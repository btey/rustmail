//! Maildir (and Maildir++) access. Everything here is plain blocking
//! filesystem work; callers run it off the async executor.
//!
//! Safety rules: symbolic links are never followed or written through, and
//! every destructive operation first checks that the target lives inside the
//! account root after resolving the parent directory.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_DEPTH: usize = 8;

/// Maildir info-suffix flags (`:2,` + sorted letters).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    pub seen: bool,
    pub replied: bool,
    pub flagged: bool,
    pub draft: bool,
    pub trashed: bool,
    pub passed: bool,
}

impl Flags {
    pub fn parse(letters: &str) -> Self {
        let mut flags = Self::default();
        for letter in letters.chars() {
            match letter {
                'S' => flags.seen = true,
                'R' => flags.replied = true,
                'F' => flags.flagged = true,
                'D' => flags.draft = true,
                'T' => flags.trashed = true,
                'P' => flags.passed = true,
                _ => {}
            }
        }
        flags
    }

    /// Letters in the ASCII order the Maildir specification requires.
    pub fn letters(self) -> String {
        let mut out = String::new();
        for (set, letter) in [
            (self.draft, 'D'),
            (self.flagged, 'F'),
            (self.passed, 'P'),
            (self.replied, 'R'),
            (self.seen, 'S'),
            (self.trashed, 'T'),
        ] {
            if set {
                out.push(letter);
            }
        }
        out
    }
}

/// Split a Maildir file name into its unique part and flags.
pub fn parse_name(name: &str) -> (String, Flags) {
    match name.split_once(':') {
        Some((key, info)) => {
            let flags = info
                .strip_prefix("2,")
                .map(Flags::parse)
                .unwrap_or_default();
            (key.to_owned(), flags)
        }
        None => (name.to_owned(), Flags::default()),
    }
}

pub fn file_name(key: &str, flags: Flags) -> String {
    format!("{key}:2,{}", flags.letters())
}

#[derive(Debug, Clone)]
pub struct Folder {
    /// Display name; `/` separates hierarchy levels. The root Maildir is INBOX.
    pub name: String,
    pub dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub key: String,
    pub path: PathBuf,
    pub in_new: bool,
    pub flags: Flags,
    pub size: u64,
}

pub fn is_maildir(dir: &Path) -> bool {
    ["cur", "new"].iter().all(|sub| {
        fs::symlink_metadata(dir.join(sub))
            .map(|meta| meta.is_dir())
            .unwrap_or(false)
    })
}

/// Find every Maildir below `root`. A Maildir++ tree keeps sub-folders as
/// `.Parent.Child` siblings inside the root Maildir; nested plain Maildirs
/// (one directory per folder) are found at any depth.
pub fn discover(root: &Path) -> io::Result<Vec<Folder>> {
    let mut folders = Vec::new();
    if is_maildir(root) {
        folders.push(Folder {
            name: "INBOX".into(),
            dir: root.to_path_buf(),
        });
    }
    walk(root, "", is_maildir(root), 0, &mut folders)?;
    folders.sort_by(|a, b| a.name.cmp(&b.name));
    folders.dedup_by(|a, b| a.name == b.name);
    Ok(folders)
}

fn walk(
    dir: &Path,
    prefix: &str,
    parent_is_maildir: bool,
    depth: usize,
    out: &mut Vec<Folder>,
) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let mut children = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_type()
                .map(|kind| kind.is_dir() && !kind.is_symlink())
                .unwrap_or(false)
        })
        .map(|entry| entry.file_name())
        .collect::<Vec<_>>();
    children.sort();
    for child in children {
        let Some(name) = child.to_str() else { continue };
        if matches!(name, "cur" | "new" | "tmp" | "." | "..") {
            continue;
        }
        let path = dir.join(&child);
        let (display, maildirpp) = if parent_is_maildir && name.starts_with('.') && name.len() > 1 {
            (name[1..].replace('.', "/"), true)
        } else {
            (name.to_owned(), false)
        };
        if display.is_empty() || display.split('/').any(str::is_empty) {
            continue;
        }
        let full = if prefix.is_empty() || maildirpp {
            display
        } else {
            format!("{prefix}/{display}")
        };
        let child_is_maildir = is_maildir(&path);
        if child_is_maildir {
            out.push(Folder {
                name: full.clone(),
                dir: path.clone(),
            });
        }
        // Maildir++ sub-folders are siblings, never nested inside the child.
        if !maildirpp {
            walk(&path, &full, child_is_maildir, depth + 1, out)?;
        }
    }
    Ok(())
}

/// Cheap change detector: both directories' modification times.
pub fn fingerprint(dir: &Path) -> String {
    let stamp = |sub: &str| {
        fs::metadata(dir.join(sub))
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    };
    format!("{}:{}", stamp("cur"), stamp("new"))
}

/// All messages currently in a Maildir (cur + new), ignoring symlinks and
/// hidden files.
pub fn list(dir: &Path) -> io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for (sub, in_new) in [("cur", false), ("new", true)] {
        let Ok(read) = fs::read_dir(dir.join(sub)) else {
            continue;
        };
        for item in read.filter_map(|item| item.ok()) {
            let Ok(kind) = item.file_type() else { continue };
            if !kind.is_file() {
                continue;
            }
            let file = item.file_name();
            let Some(name) = file.to_str() else { continue };
            if name.starts_with('.') {
                continue;
            }
            let (key, mut flags) = parse_name(name);
            if key.is_empty() {
                continue;
            }
            if in_new {
                // Files in new/ are by definition unseen.
                flags.seen = false;
            }
            entries.push(Entry {
                key,
                path: item.path(),
                in_new,
                flags,
                size: item.metadata().map(|meta| meta.len()).unwrap_or(0),
            });
        }
    }
    entries.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(entries)
}

/// Find a message by its unique key (its name changes whenever flags do).
pub fn locate(dir: &Path, key: &str) -> Option<Entry> {
    list(dir).ok()?.into_iter().find(|entry| entry.key == key)
}

pub fn ensure_maildir(dir: &Path) -> io::Result<()> {
    for sub in ["cur", "new", "tmp"] {
        fs::create_dir_all(dir.join(sub))?;
    }
    Ok(())
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn new_key() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{}.M{}P{}Q{}.rustmail",
        now.as_secs(),
        now.subsec_micros(),
        std::process::id(),
        counter
    )
}

/// Resolve `path`'s parent directory and require it to stay inside `root`.
pub fn within_root(root: &Path, path: &Path) -> io::Result<()> {
    let root = fs::canonicalize(root)?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    let parent = fs::canonicalize(parent)?;
    if parent.starts_with(&root) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "path is outside the account's mail folder",
        ))
    }
}

fn require_regular(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_file() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular mail file",
        ))
    }
}

/// Write a new message into `dir` (tmp → cur) and return its key and path.
pub fn deliver(root: &Path, dir: &Path, raw: &[u8], flags: Flags) -> io::Result<(String, PathBuf)> {
    ensure_maildir(dir)?;
    within_root(root, &dir.join("tmp").join("x"))?;
    let key = new_key();
    let tmp = dir.join("tmp").join(&key);
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(raw)?;
        file.sync_all()?;
    }
    let target = dir.join("cur").join(file_name(&key, flags));
    fs::rename(&tmp, &target)?;
    Ok((key, target))
}

/// Rename a message in place to carry `flags` (moving it out of new/).
pub fn set_flags(root: &Path, entry: &Entry, flags: Flags) -> io::Result<PathBuf> {
    require_regular(&entry.path)?;
    within_root(root, &entry.path)?;
    let maildir = maildir_of(&entry.path)?;
    let target = maildir.join("cur").join(file_name(&entry.key, flags));
    if target != entry.path {
        fs::rename(&entry.path, &target)?;
    }
    Ok(target)
}

/// Move a message to another Maildir, keeping its flags.
pub fn move_to(
    root: &Path,
    entry: &Entry,
    target_dir: &Path,
    flags: Flags,
) -> io::Result<(String, PathBuf)> {
    require_regular(&entry.path)?;
    within_root(root, &entry.path)?;
    ensure_maildir(target_dir)?;
    within_root(root, &target_dir.join("cur").join("x"))?;
    let key = if locate(target_dir, &entry.key).is_some() {
        new_key()
    } else {
        entry.key.clone()
    };
    let target = target_dir.join("cur").join(file_name(&key, flags));
    if fs::rename(&entry.path, &target).is_err() {
        // Different filesystem: copy, flush, then remove the original.
        let raw = fs::read(&entry.path)?;
        let tmp = target_dir.join("tmp").join(&key);
        fs::write(&tmp, &raw)?;
        fs::rename(&tmp, &target)?;
        fs::remove_file(&entry.path)?;
    }
    Ok((key, target))
}

pub fn remove(root: &Path, entry: &Entry) -> io::Result<()> {
    require_regular(&entry.path)?;
    within_root(root, &entry.path)?;
    fs::remove_file(&entry.path)
}

/// The Maildir directory containing `cur/` or `new/` that holds `path`.
pub fn maildir_of(path: &Path) -> io::Result<PathBuf> {
    path.parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not inside a Maildir"))
}

/// Directory of the folder called `name` for a new Maildir created by us:
/// `.A.B` inside a Maildir++ root, or `A/B` under a plain nested root.
pub fn dir_for_new_folder(root: &Path, name: &str) -> Option<PathBuf> {
    let clean = name.split('/').all(|part| {
        !part.is_empty() && part != "." && part != ".." && !part.contains(['\0', '\\'])
    });
    if !clean || name.is_empty() {
        return None;
    }
    if name.eq_ignore_ascii_case("INBOX") && is_maildir(root) {
        return Some(root.to_path_buf());
    }
    if is_maildir(root) {
        if name.contains('.') {
            return None;
        }
        Some(root.join(format!(".{}", name.replace('/', "."))))
    } else {
        let mut path = root.to_path_buf();
        for part in name.split('/') {
            path.push(OsStr::new(part));
        }
        Some(path)
    }
}

/// Maildirs making up the folder `name` and everything below it, as
/// `(folder name, directory)` pairs. In plain nested trees the children live
/// inside the folder's directory; in Maildir++ they are sibling directories.
fn folder_tree(root: &Path, name: &str) -> io::Result<Vec<Folder>> {
    let prefix = format!("{name}/");
    Ok(discover(root)?
        .into_iter()
        .filter(|folder| folder.name == name || folder.name.starts_with(&prefix))
        .collect())
}

fn require_real_dir(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidInput, "not a folder"))
    }
}

/// Rename the Maildir `old` (and its sub-folders) to `new`; both are
/// `/`-separated folder names. Nothing is overwritten.
pub fn rename_folder(root: &Path, old: &str, new: &str) -> io::Result<()> {
    if old.eq_ignore_ascii_case("INBOX") || new.eq_ignore_ascii_case("INBOX") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INBOX cannot be renamed",
        ));
    }
    let invalid = |what| io::Error::new(io::ErrorKind::InvalidInput, what);
    let tree = folder_tree(root, old)?;
    if tree.is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "folder not found"));
    }
    let mut moves = Vec::new();
    if is_maildir(root) {
        // Maildir++: every folder in the tree is its own sibling directory.
        for folder in &tree {
            let target_name = format!("{new}{}", &folder.name[old.len()..]);
            let target = dir_for_new_folder(root, &target_name)
                .ok_or_else(|| invalid("that folder name cannot be used in a Maildir"))?;
            moves.push((folder.dir.clone(), target));
        }
    } else {
        let source = dir_for_new_folder(root, old).ok_or_else(|| invalid("invalid folder name"))?;
        let target = dir_for_new_folder(root, new)
            .ok_or_else(|| invalid("that folder name cannot be used in a Maildir"))?;
        if target.starts_with(&source) {
            return Err(invalid("a folder cannot be moved into itself"));
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        moves.push((source, target));
    }
    for (from, to) in &moves {
        require_real_dir(from)?;
        within_root(root, from)?;
        within_root(root, to)?;
        if fs::symlink_metadata(to).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a folder with that name already exists",
            ));
        }
    }
    for (from, to) in moves {
        fs::rename(from, to)?;
    }
    Ok(())
}

/// Delete the Maildir `name` together with its sub-folders and every message
/// in them. The root Maildir (INBOX) is never removed.
pub fn delete_folder(root: &Path, name: &str) -> io::Result<()> {
    if name.eq_ignore_ascii_case("INBOX") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INBOX cannot be deleted",
        ));
    }
    let tree = folder_tree(root, name)?;
    if tree.is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "folder not found"));
    }
    let mut dirs = Vec::new();
    if is_maildir(root) {
        dirs.extend(tree.into_iter().map(|folder| folder.dir));
    } else {
        dirs.push(
            dir_for_new_folder(root, name).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid folder name")
            })?,
        );
    }
    for dir in &dirs {
        require_real_dir(dir)?;
        within_root(root, dir)?;
        if fs::canonicalize(dir)? == fs::canonicalize(root)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the account root cannot be deleted",
            ));
        }
    }
    for dir in dirs {
        // remove_dir_all unlinks symlinks instead of following them.
        fs::remove_dir_all(dir)?;
    }
    Ok(())
}

/// Lexical check used before touching user-supplied relative paths.
pub fn is_safe_relative(path: &Path) -> bool {
    path.components()
        .all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(dir: &Path) {
        ensure_maildir(dir).unwrap();
    }

    #[test]
    fn names_roundtrip_flags_in_order() {
        let (key, flags) = parse_name("123.M1.host:2,SRF");
        assert_eq!(key, "123.M1.host");
        assert!(flags.seen && flags.replied && flags.flagged && !flags.draft);
        assert_eq!(file_name(&key, flags), "123.M1.host:2,FRS");
        assert_eq!(parse_name("plain").0, "plain");
    }

    #[test]
    fn discovers_maildirpp_and_nested_layouts() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        mk(root);
        mk(&root.join(".Work.Reports"));
        mk(&root.join(".Sent"));
        let names: Vec<_> = discover(root)
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, ["INBOX", "Sent", "Work/Reports"]);

        let nested = temp.path().join("nested");
        mk(&nested.join("inbox"));
        mk(&nested.join("Lists/rust"));
        let names: Vec<_> = discover(&nested)
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, ["Lists/rust", "inbox"]);
    }

    #[test]
    fn deliver_move_and_remove_stay_inside_the_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        mk(&root);
        let other = root.join(".Archive");
        mk(&other);
        let (key, path) = deliver(&root, &root, b"Subject: x\n\nbody\n", Flags::default()).unwrap();
        assert!(path.exists());
        let entry = locate(&root, &key).unwrap();
        let (_, moved) = move_to(&root, &entry, &other, entry.flags).unwrap();
        assert!(moved.starts_with(&other) && moved.exists() && !path.exists());
        let entry = locate(&other, &key).unwrap();
        remove(&root, &entry).unwrap();
        assert!(locate(&other, &key).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_escapes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        mk(&root);
        mk(&outside);
        std::fs::write(outside.join("cur/secret:2,S"), "x").unwrap();
        // A folder that is a symlink to somewhere else.
        std::os::unix::fs::symlink(&outside, root.join(".Evil")).unwrap();
        assert!(discover(&root).unwrap().iter().all(|f| f.name != "Evil"));
        // Writing into it is refused even if asked directly.
        let error = deliver(&root, &root.join(".Evil"), b"data", Flags::default()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        // An entry whose file lives outside the root cannot be removed/renamed.
        let entry = Entry {
            key: "secret".into(),
            path: outside.join("cur/secret:2,S"),
            in_new: false,
            flags: Flags::default(),
            size: 1,
        };
        assert!(remove(&root, &entry).is_err());
        assert!(
            set_flags(
                &root,
                &entry,
                Flags {
                    seen: true,
                    ..Default::default()
                }
            )
            .is_err()
        );
        assert!(outside.join("cur/secret:2,S").exists());
        // Symlinked mail files are skipped by listing.
        std::os::unix::fs::symlink(outside.join("cur/secret:2,S"), root.join("cur/link:2,S"))
            .unwrap();
        assert!(list(&root).unwrap().iter().all(|e| e.key != "link"));
    }

    #[test]
    fn new_folder_paths_reject_traversal() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        mk(root);
        assert!(dir_for_new_folder(root, "../evil").is_none());
        assert!(dir_for_new_folder(root, "a/../b").is_none());
        assert!(dir_for_new_folder(root, "").is_none());
        assert_eq!(
            dir_for_new_folder(root, "Work").unwrap(),
            root.join(".Work")
        );
    }

    #[test]
    fn rename_and_delete_maildirpp_tree() {
        let tmp = std::env::temp_dir().join(format!("rm-mdpp-{}", new_key()));
        mk(&tmp);
        mk(&tmp.join(".A"));
        mk(&tmp.join(".A.B"));
        rename_folder(&tmp, "A", "X").unwrap();
        assert!(tmp.join(".X").is_dir() && tmp.join(".X.B").is_dir());
        assert!(!tmp.join(".A").exists());
        mk(&tmp.join(".Y"));
        assert!(rename_folder(&tmp, "X", "Y").is_err());
        assert!(rename_folder(&tmp, "INBOX", "Z").is_err());
        delete_folder(&tmp, "X").unwrap();
        assert!(!tmp.join(".X").exists() && !tmp.join(".X.B").exists());
        assert!(tmp.join(".Y").is_dir());
        assert!(delete_folder(&tmp, "INBOX").is_err());
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn rename_and_delete_nested_tree() {
        let tmp = std::env::temp_dir().join(format!("rm-nest-{}", new_key()));
        fs::create_dir_all(&tmp).unwrap();
        mk(&tmp.join("A"));
        mk(&tmp.join("A").join("B"));
        rename_folder(&tmp, "A", "X").unwrap();
        assert!(tmp.join("X").join("B").join("cur").is_dir());
        assert!(rename_folder(&tmp, "X", "X/Z").is_err());
        delete_folder(&tmp, "X/B").unwrap();
        assert!(tmp.join("X").is_dir() && !tmp.join("X").join("B").exists());
        fs::remove_dir_all(&tmp).unwrap();
    }
}
