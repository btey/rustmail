//! mbox support (mboxrd and the common mboxo/mboxcl variants).
//! Mailbox contents are only ever read: nothing here opens an mbox for
//! writing. Folder management creates, renames and deletes whole mbox files.

use super::maildir::within_root;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

const MAX_DEPTH: usize = 8;
/// Whole-file parse limit; larger mailboxes should be split or converted.
pub const MAX_MBOX_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_MESSAGE_BYTES: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Folder {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Flags {
    pub seen: bool,
    pub flagged: bool,
    pub replied: bool,
    pub draft: bool,
    pub deleted: bool,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub offset: u64,
    pub raw: Vec<u8>,
    pub key: String,
    pub flags: Flags,
}

/// FNV-1a: stable across Rust versions, unlike `DefaultHasher`.
fn fnv(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

fn looks_like_mbox(path: &Path) -> bool {
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 5];
    match file.read(&mut head) {
        Ok(0) => true, // an empty mbox is a valid, empty folder
        Ok(n) => &head[..n.min(5)] == b"From " && n == 5,
        Err(_) => false,
    }
}

/// An mbox root is either one file (the account's only folder) or a
/// directory tree whose regular files are mbox folders. Mozilla-style
/// `name.sbd` directories hold the children of `name`.
pub fn discover(root: &Path) -> io::Result<Vec<Folder>> {
    let meta = fs::symlink_metadata(root)?;
    if meta.is_file() {
        return Ok(vec![Folder {
            name: "INBOX".into(),
            path: root.to_path_buf(),
        }]);
    }
    let mut out = Vec::new();
    walk(root, "", 0, &mut out)?;
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn walk(dir: &Path, prefix: &str, depth: usize, out: &mut Vec<Folder>) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let mut items = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .collect::<Vec<_>>();
    items.sort_by_key(|entry| entry.file_name());
    for item in items {
        let Ok(kind) = item.file_type() else { continue };
        if kind.is_symlink() {
            continue;
        }
        let file_name = item.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if name.starts_with('.')
            || name.ends_with(".lock")
            || name.ends_with(".msf")
            || name.ends_with(".idx")
            || name.ends_with('~')
        {
            continue;
        }
        let path = item.path();
        if kind.is_dir() {
            let (base, is_sbd) = match name.strip_suffix(".sbd") {
                Some(base) => (base, true),
                None => (name, false),
            };
            let full = if prefix.is_empty() {
                base.to_owned()
            } else {
                format!("{prefix}/{base}")
            };
            let _ = is_sbd;
            walk(&path, &full, depth + 1, out)?;
        } else if kind.is_file() && looks_like_mbox(&path) {
            let display = if prefix.is_empty() && name.eq_ignore_ascii_case("inbox") {
                "INBOX".to_owned()
            } else if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            out.push(Folder {
                name: display,
                path,
            });
        }
    }
    Ok(())
}

pub fn fingerprint(path: &Path) -> String {
    fs::metadata(path)
        .map(|meta| {
            let modified = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            format!("{}:{modified}", meta.len())
        })
        .unwrap_or_default()
}

pub fn read_file(path: &Path) -> io::Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "mbox path is not a regular file",
        ));
    }
    if meta.len() > MAX_MBOX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "mbox file is larger than 1 GiB; split it before opening it in Rustmail",
        ));
    }
    fs::read(path)
}

/// Remove one level of `>` quoting from `>From ` lines (mboxrd).
fn unescape(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut line_start = true;
    let mut i = 0;
    while i < body.len() {
        if line_start && body[i] == b'>' {
            let mut j = i;
            while j < body.len() && body[j] == b'>' {
                j += 1;
            }
            if body[j..].starts_with(b"From ") {
                // Drop exactly one '>'.
                out.extend_from_slice(&body[i + 1..j]);
                i = j;
                line_start = false;
                continue;
            }
        }
        let byte = body[i];
        out.push(byte);
        line_start = byte == b'\n';
        i += 1;
    }
    out
}

fn header_block(raw: &[u8]) -> &[u8] {
    let end = raw
        .windows(2)
        .position(|pair| pair == b"\n\n")
        .or_else(|| raw.windows(4).position(|quad| quad == b"\r\n\r\n"))
        .unwrap_or(raw.len());
    &raw[..end.min(raw.len())]
}

fn header_value(block: &[u8], name: &str) -> Option<String> {
    let text = String::from_utf8_lossy(block);
    text.lines().find_map(|line| {
        let (field, value) = line.split_once(':')?;
        field
            .trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
}

fn flags_of(raw: &[u8]) -> Flags {
    let block = header_block(raw);
    let status = header_value(block, "Status").unwrap_or_default();
    let x_status = header_value(block, "X-Status").unwrap_or_default();
    Flags {
        seen: status.contains('R'),
        flagged: x_status.contains('F'),
        replied: x_status.contains('A'),
        draft: x_status.contains('T'),
        deleted: x_status.contains('D'),
    }
}

/// Split an mbox into messages. A separator is a line starting with `From `
/// at the start of the file or right after a blank line.
pub fn parse(bytes: &[u8]) -> Vec<Message> {
    let mut starts = Vec::new();
    let mut line_start = 0usize;
    let mut previous_blank = true;
    while line_start < bytes.len() {
        let line_end = bytes[line_start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|offset| line_start + offset + 1)
            .unwrap_or(bytes.len());
        let line = &bytes[line_start..line_end];
        if previous_blank && line.starts_with(b"From ") {
            starts.push((line_start, line_end));
        }
        previous_blank = line == b"\n" || line == b"\r\n";
        line_start = line_end;
    }
    let mut messages = Vec::with_capacity(starts.len());
    for (index, (start, body_start)) in starts.iter().enumerate() {
        let mut end = starts
            .get(index + 1)
            .map(|(next, _)| *next)
            .unwrap_or(bytes.len());
        // The blank line before the next separator is part of the separator.
        if end > *body_start && bytes[..end].ends_with(b"\r\n") && index + 1 < starts.len() {
            end -= 2;
        } else if end > *body_start && bytes[..end].ends_with(b"\n") && index + 1 < starts.len() {
            end -= 1;
        }
        if end <= *body_start {
            continue;
        }
        let raw = unescape(&bytes[*body_start..end]);
        if raw.is_empty() || raw.len() > MAX_MESSAGE_BYTES {
            continue;
        }
        let identity = header_value(header_block(&raw), "Message-ID")
            .map(|id| fnv(id.as_bytes()))
            .unwrap_or_else(|| fnv(&raw[..raw.len().min(2048)]));
        messages.push(Message {
            offset: *start as u64,
            key: format!("{start}:{identity:016x}"),
            flags: flags_of(&raw),
            raw,
        });
    }
    messages
}

fn invalid(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what)
}

fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|meta| meta.is_dir())
        .unwrap_or(false)
}

/// Sub-folders of the mbox file `path` live in the sibling `<name>.sbd`.
fn children_dir(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    Some(path.with_file_name(format!("{name}.sbd")))
}

fn valid_part(part: &str) -> bool {
    let lower = part.to_ascii_lowercase();
    !part.is_empty()
        && !part.starts_with('.')
        && !part.contains(['\0', '\\', '/'])
        && !part.ends_with('~')
        && !(lower.ends_with(".lock")
            || lower.ends_with(".msf")
            || lower.ends_with(".idx")
            || lower.ends_with(".sbd"))
}

/// Where the folder `name` belongs: an existing `<parent>.sbd` or plain
/// directory for each parent level, else a new `<parent>.sbd`. Nothing is
/// created here.
fn resolve(root: &Path, name: &str) -> io::Result<(PathBuf, PathBuf)> {
    let parts: Vec<&str> = name.split('/').collect();
    if !parts.iter().all(|part| valid_part(part)) {
        return Err(invalid("that folder name cannot be used in an mbox tree"));
    }
    let (leaf, parents) = parts
        .split_last()
        .ok_or_else(|| invalid("empty folder name"))?;
    let mut dir = root.to_path_buf();
    for part in parents {
        let sbd = dir.join(format!("{part}.sbd"));
        let plain = dir.join(part);
        dir = if is_real_dir(&sbd) {
            sbd
        } else if is_real_dir(&plain) {
            plain
        } else {
            sbd
        };
    }
    let file = dir.join(leaf);
    Ok((dir, file))
}

/// Fail unless the nearest existing ancestor of `dir` resolves inside `root`.
fn guard_ancestor(root: &Path, dir: &Path) -> io::Result<()> {
    let root = fs::canonicalize(root)?;
    let mut existing = dir;
    while fs::symlink_metadata(existing).is_err() {
        existing = existing
            .parent()
            .ok_or_else(|| invalid("path has no parent"))?;
    }
    if fs::canonicalize(existing)?.starts_with(&root) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "path is outside the account's mail folder",
        ))
    }
}

fn require_tree_root(root: &Path) -> io::Result<()> {
    if is_real_dir(root) {
        Ok(())
    } else {
        Err(invalid("a single mbox file has no folder tree to change"))
    }
}

fn find_folder(root: &Path, name: &str) -> io::Result<Folder> {
    require_tree_root(root)?;
    if name.eq_ignore_ascii_case("INBOX") {
        return Err(invalid("INBOX cannot be renamed or deleted"));
    }
    let folder = discover(root)?
        .into_iter()
        .find(|folder| folder.name == name)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "folder not found"))?;
    if !fs::symlink_metadata(&folder.path)?.is_file() {
        return Err(invalid("not a mailbox file"));
    }
    within_root(root, &folder.path)?;
    Ok(folder)
}

/// Create an empty mbox folder called `name` (`/`-separated), making
/// `<parent>.sbd` directories as needed. An existing folder is never touched.
pub fn create_folder(root: &Path, name: &str) -> io::Result<()> {
    require_tree_root(root)?;
    let (dir, file) = resolve(root, name)?;
    guard_ancestor(root, &dir)?;
    fs::create_dir_all(&dir)?;
    within_root(root, &file)?;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&file)
        .map(drop)
}

/// Rename the mbox folder `old` to `new`; its `.sbd` sub-folders move too.
pub fn rename_folder(root: &Path, old: &str, new: &str) -> io::Result<()> {
    let from = find_folder(root, old)?.path;
    if new.eq_ignore_ascii_case("INBOX") {
        return Err(invalid("INBOX cannot be renamed or deleted"));
    }
    let (dir, to) = resolve(root, new)?;
    let from_children = children_dir(&from).filter(|path| is_real_dir(path));
    let to_children = children_dir(&to).ok_or_else(|| invalid("bad folder name"))?;
    if let Some(from_children) = &from_children
        && dir.starts_with(from_children)
    {
        return Err(invalid("a folder cannot be moved into itself"));
    }
    let exists = |path: &Path| fs::symlink_metadata(path).is_ok();
    if exists(&to) || (from_children.is_some() && exists(&to_children)) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a folder with that name already exists",
        ));
    }
    guard_ancestor(root, &dir)?;
    fs::create_dir_all(&dir)?;
    within_root(root, &to)?;
    fs::rename(&from, &to)?;
    if let Some(from_children) = from_children
        && let Err(error) = fs::rename(&from_children, &to_children)
    {
        let _ = fs::rename(&to, &from);
        return Err(error);
    }
    Ok(())
}

/// Delete the mbox folder `name`, its sub-folders and every message in them.
pub fn delete_folder(root: &Path, name: &str) -> io::Result<()> {
    let path = find_folder(root, name)?.path;
    if let Some(children) = children_dir(&path).filter(|path| is_real_dir(path)) {
        fs::remove_dir_all(children)?;
    }
    fs::remove_file(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_messages_and_unescapes_from_lines() {
        let mbox = b"From a@x Mon Jan  1 00:00:00 2024\nSubject: one\nMessage-ID: <1@x>\nStatus: RO\n\nhello\n>From quoted\n>>From twice\n\nFrom b@x Mon Jan  1 00:00:00 2024\nSubject: two\nX-Status: F\n\nbye\n";
        let messages = parse(mbox);
        assert_eq!(messages.len(), 2);
        let first = String::from_utf8_lossy(&messages[0].raw);
        assert!(first.contains("\nFrom quoted\n"));
        assert!(first.contains("\n>From twice\n"));
        assert!(first.starts_with("Subject: one"));
        assert!(messages[0].flags.seen);
        assert!(messages[1].flags.flagged);
        assert!(!messages[1].flags.seen);
        assert_ne!(messages[0].key, messages[1].key);
    }

    #[test]
    fn keys_survive_reparse() {
        let mbox = b"From a@x Mon Jan  1 00:00:00 2024\nMessage-ID: <1@x>\n\nhi\n";
        assert_eq!(parse(mbox)[0].key, parse(mbox)[0].key);
    }

    #[test]
    fn create_rename_and_delete_folders() {
        let root =
            std::env::temp_dir().join(format!("rustmail-mbox-rename-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("inbox"), "").unwrap();

        create_folder(&root, "Work").unwrap();
        create_folder(&root, "Work/Sub").unwrap();
        assert!(create_folder(&root, "Work").is_err());
        assert!(create_folder(&root, "../escape").is_err());
        assert!(create_folder(&root, "Bad.sbd").is_err());
        let names = |root: &Path| -> Vec<String> {
            discover(root)
                .unwrap()
                .into_iter()
                .map(|f| f.name)
                .collect()
        };
        assert_eq!(names(&root), ["INBOX", "Work", "Work/Sub"]);

        assert!(rename_folder(&root, "INBOX", "X").is_err());
        assert!(rename_folder(&root, "Work", "inbox").is_err());
        assert!(rename_folder(&root, "Work", "Work/Sub/In").is_err());
        rename_folder(&root, "Work", "Done").unwrap();
        assert_eq!(names(&root), ["Done", "Done/Sub", "INBOX"]);

        delete_folder(&root, "Done").unwrap();
        assert_eq!(names(&root), ["INBOX"]);
        assert!(delete_folder(&root, "INBOX").is_err());
        let _ = fs::remove_dir_all(&root);
    }
}
