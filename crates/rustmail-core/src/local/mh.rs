//! MH folders (nmh / Claws Mail layout): one directory per folder, one file
//! per message named by a decimal number. Read state is kept in the optional
//! `.mh_sequences` file (`unseen`, `flagged`); other sequences are preserved.
//! Claws Mail's own binary `.claws_mark` is not read or written.
//!
//! Everything here is plain blocking filesystem work. Safety rules match the
//! Maildir module: symbolic links are never followed or written through and
//! every destructive operation checks the target stays inside the root.

use super::maildir::{Entry, Flags, within_root};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const MAX_DEPTH: usize = 8;
const SEQUENCES: &str = ".mh_sequences";
const MARKERS: [&str; 3] = [SEQUENCES, ".claws_mark", ".sylpheed_mark"];

#[derive(Debug, Clone)]
pub struct Folder {
    /// Display name; `/` separates hierarchy levels. A root holding messages
    /// itself is INBOX.
    pub name: String,
    pub dir: PathBuf,
}

fn message_number(name: &str) -> Option<u32> {
    if name.is_empty() || name.starts_with('0') || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse().ok()
}

fn is_regular(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|meta| meta.is_file())
        .unwrap_or(false)
}

/// A directory is a folder when it holds numbered messages or an MH marker file.
pub fn is_mh_folder(dir: &Path) -> bool {
    if MARKERS.iter().any(|marker| is_regular(&dir.join(marker))) {
        return true;
    }
    let Ok(read) = fs::read_dir(dir) else {
        return false;
    };
    read.filter_map(|item| item.ok()).any(|item| {
        item.file_type().map(|kind| kind.is_file()).unwrap_or(false)
            && item.file_name().to_str().and_then(message_number).is_some()
    })
}

pub fn discover(root: &Path) -> io::Result<Vec<Folder>> {
    let mut folders = Vec::new();
    if is_mh_folder(root) {
        folders.push(Folder {
            name: "INBOX".into(),
            dir: root.to_path_buf(),
        });
    }
    walk(root, "", 0, &mut folders)?;
    folders.sort_by(|a, b| a.name.cmp(&b.name));
    folders.dedup_by(|a, b| a.name == b.name);
    Ok(folders)
}

fn walk(dir: &Path, prefix: &str, depth: usize, out: &mut Vec<Folder>) -> io::Result<()> {
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
        if name.starts_with('.') || name.contains('\\') {
            continue;
        }
        let path = dir.join(&child);
        let full = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if is_mh_folder(&path) {
            out.push(Folder {
                name: full.clone(),
                dir: path.clone(),
            });
        }
        walk(&path, &full, depth + 1, out)?;
    }
    Ok(())
}

fn stamp(path: &Path) -> u128 {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

/// Cheap change detector: the directory and its sequences file.
pub fn fingerprint(dir: &Path) -> String {
    format!("{}:{}", stamp(dir), stamp(&dir.join(SEQUENCES)))
}

type Sequences = Vec<(String, BTreeSet<u32>)>;

fn parse_sequences(text: &str) -> Sequences {
    let mut out: Sequences = Vec::new();
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let mut set = BTreeSet::new();
        for token in rest.split_whitespace() {
            match token.split_once('-') {
                Some((a, b)) => {
                    if let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) {
                        // Bound the span so a hostile file cannot exhaust memory.
                        if a <= b && b - a < 1_000_000 {
                            set.extend(a..=b);
                        }
                    }
                }
                None => {
                    if let Ok(n) = token.parse::<u32>() {
                        set.insert(n);
                    }
                }
            }
        }
        match out.iter_mut().find(|(existing, _)| existing == name) {
            Some((_, existing)) => existing.extend(set),
            None => out.push((name.to_owned(), set)),
        }
    }
    out
}

fn format_set(set: &BTreeSet<u32>) -> String {
    let mut parts = Vec::new();
    let mut iter = set.iter().copied().peekable();
    while let Some(start) = iter.next() {
        let mut end = start;
        while iter.peek() == Some(&(end + 1)) {
            end = iter.next().unwrap_or(end);
        }
        parts.push(if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        });
    }
    parts.join(" ")
}

/// Claws Mail's per-folder mark file: a `u32` version, then `(message number,
/// flags)` pairs of `u32`, in the byte order of the machine that wrote it.
const CLAWS_MARK: &str = ".claws_mark";
const CLAWS_MARK_VERSION: u32 = 2;
/// Claws message flag bits (`MSG_NEW`, `MSG_UNREAD`, `MSG_MARKED`).
const CLAWS_NEW: u32 = 1 << 0;
const CLAWS_UNREAD: u32 = 1 << 1;
const CLAWS_MARKED: u32 = 1 << 2;
/// A mark file is 8 bytes per message; anything bigger is not read.
const MAX_CLAWS_MARK_BYTES: u64 = 16 * 1024 * 1024;

/// The read and starred marks of a folder that Claws Mail kept in its own
/// `.claws_mark` file, as `unseen` and `flagged` sequences. Only used while the
/// folder has no `.mh_sequences` yet; the first change made here writes one,
/// which then takes over. A message the file does not mention counts as unread
/// (Claws shows such a message as new). Replied, forwarded and deleted marks
/// are not carried over. `None` when there is no readable mark file of the
/// version this reads (2).
fn claws_marks(dir: &Path) -> Option<Sequences> {
    let path = dir.join(CLAWS_MARK);
    if !is_regular(&path) || fs::metadata(&path).ok()?.len() > MAX_CLAWS_MARK_BYTES {
        return None;
    }
    let bytes = fs::read(&path).ok()?;
    let head: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    let little = if u32::from_le_bytes(head) == CLAWS_MARK_VERSION {
        true
    } else if u32::from_be_bytes(head) == CLAWS_MARK_VERSION {
        false
    } else {
        return None;
    };
    let word = |chunk: &[u8]| {
        let raw: [u8; 4] = chunk.try_into().unwrap_or_default();
        if little {
            u32::from_le_bytes(raw)
        } else {
            u32::from_be_bytes(raw)
        }
    };
    let mut flags: std::collections::BTreeMap<u32, u32> = std::collections::BTreeMap::new();
    for pair in bytes[4..].as_chunks::<8>().0 {
        flags.insert(word(&pair[..4]), word(&pair[4..]));
    }
    let mut unseen = BTreeSet::new();
    for item in fs::read_dir(dir).ok()?.filter_map(|item| item.ok()) {
        if !item.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let Some(number) = item.file_name().to_str().and_then(message_number) else {
            continue;
        };
        if flags
            .get(&number)
            .is_none_or(|flags| flags & (CLAWS_NEW | CLAWS_UNREAD) != 0)
        {
            unseen.insert(number);
        }
    }
    let flagged = flags
        .iter()
        .filter(|(_, flags)| **flags & CLAWS_MARKED != 0)
        .map(|(number, _)| *number)
        .collect();
    Some(vec![("unseen".into(), unseen), ("flagged".into(), flagged)])
}

fn read_sequences(dir: &Path) -> Sequences {
    let path = dir.join(SEQUENCES);
    if !is_regular(&path) {
        return claws_marks(dir).unwrap_or_default();
    }
    fs::read(&path)
        .map(|bytes| parse_sequences(&String::from_utf8_lossy(&bytes)))
        .unwrap_or_default()
}

fn write_sequences(dir: &Path, sequences: &Sequences) -> io::Result<()> {
    let target = dir.join(SEQUENCES);
    if fs::symlink_metadata(&target)
        .map(|meta| !meta.is_file())
        .unwrap_or(false)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            ".mh_sequences is not a regular file",
        ));
    }
    let mut text = String::new();
    for (name, set) in sequences.iter().filter(|(_, set)| !set.is_empty()) {
        text.push_str(&format!("{name}: {}\n", format_set(set)));
    }
    let tmp = dir.join(format!(".mh_sequences.rustmail-{}", std::process::id()));
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&tmp, &target)
}

fn sequence_mut<'a>(sequences: &'a mut Sequences, name: &str) -> &'a mut BTreeSet<u32> {
    if let Some(index) = sequences.iter().position(|(existing, _)| existing == name) {
        return &mut sequences[index].1;
    }
    sequences.push((name.to_owned(), BTreeSet::new()));
    let last = sequences.len() - 1;
    &mut sequences[last].1
}

fn apply_flags(dir: &Path, number: u32, flags: Flags) -> io::Result<()> {
    let mut sequences = read_sequences(dir);
    let had_unseen = sequences.iter().any(|(name, _)| name == "unseen");
    let before = sequences.clone();
    if flags.seen {
        if had_unseen {
            sequence_mut(&mut sequences, "unseen").remove(&number);
        }
    } else {
        sequence_mut(&mut sequences, "unseen").insert(number);
    }
    if flags.flagged {
        sequence_mut(&mut sequences, "flagged").insert(number);
    } else if sequences.iter().any(|(name, _)| name == "flagged") {
        sequence_mut(&mut sequences, "flagged").remove(&number);
    }
    if sequences == before {
        return Ok(());
    }
    write_sequences(dir, &sequences)
}

fn drop_from_sequences(dir: &Path, number: u32) -> io::Result<()> {
    let mut sequences = read_sequences(dir);
    let mut changed = false;
    for (_, set) in &mut sequences {
        changed |= set.remove(&number);
    }
    if changed {
        write_sequences(dir, &sequences)?;
    }
    Ok(())
}

/// All messages in a folder, ignoring symlinks, dot-files and non-numeric names.
pub fn list(dir: &Path) -> io::Result<Vec<Entry>> {
    let sequences = read_sequences(dir);
    let set_of = |name: &str| {
        sequences
            .iter()
            .find(|(existing, _)| existing == name)
            .map(|(_, set)| set.clone())
            .unwrap_or_default()
    };
    let unseen = set_of("unseen");
    let flagged = set_of("flagged");
    let mut entries = Vec::new();
    for item in fs::read_dir(dir)?.filter_map(|item| item.ok()) {
        let Ok(kind) = item.file_type() else { continue };
        if !kind.is_file() {
            continue;
        }
        let file = item.file_name();
        let Some(name) = file.to_str() else { continue };
        let Some(number) = message_number(name) else {
            continue;
        };
        entries.push((
            number,
            Entry {
                key: name.to_owned(),
                path: item.path(),
                in_new: false,
                flags: Flags {
                    seen: !unseen.contains(&number),
                    flagged: flagged.contains(&number),
                    ..Flags::default()
                },
                size: item.metadata().map(|meta| meta.len()).unwrap_or(0),
            },
        ));
    }
    entries.sort_by_key(|(number, _)| *number);
    Ok(entries.into_iter().map(|(_, entry)| entry).collect())
}

pub fn locate(dir: &Path, key: &str) -> Option<Entry> {
    message_number(key)?;
    list(dir).ok()?.into_iter().find(|entry| entry.key == key)
}

/// Create a folder (and its parents) with an empty marker so it is discovered.
pub fn ensure_folder(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let marker = dir.join(SEQUENCES);
    if fs::symlink_metadata(&marker).is_err() {
        fs::File::create(marker)?;
    }
    Ok(())
}

/// Fail unless the nearest existing ancestor of `dir` resolves inside `root`.
fn guard_ancestor(root: &Path, dir: &Path) -> io::Result<()> {
    let root = fs::canonicalize(root)?;
    let mut existing = dir;
    while fs::symlink_metadata(existing).is_err() {
        existing = dir_of(existing)?;
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

/// Create a folder only if its nearest existing ancestor resolves inside `root`.
pub fn create_folder(root: &Path, dir: &Path) -> io::Result<()> {
    guard_ancestor(root, dir)?;
    ensure_folder(dir)?;
    within_root(&fs::canonicalize(root)?, &dir.join("x"))
}

fn number_of(entry: &Entry) -> io::Result<u32> {
    message_number(&entry.key)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a numbered MH message"))
}

fn require_regular(path: &Path) -> io::Result<()> {
    if is_regular(path) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular mail file",
        ))
    }
}

fn dir_of(path: &Path) -> io::Result<&Path> {
    path.parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))
}

/// Write a new message as the next free number and return its key and path.
pub fn deliver(root: &Path, dir: &Path, raw: &[u8], flags: Flags) -> io::Result<(String, PathBuf)> {
    create_folder(root, dir)?;
    let mut next = list(dir)?
        .iter()
        .filter_map(|entry| message_number(&entry.key))
        .max()
        .unwrap_or(0)
        + 1;
    loop {
        let path = dir.join(next.to_string());
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(raw)?;
                file.sync_all()?;
                if let Err(error) = apply_flags(dir, next, flags) {
                    let _ = fs::remove_file(&path);
                    return Err(error);
                }
                return Ok((next.to_string(), path));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => next += 1,
            Err(error) => return Err(error),
        }
    }
}

/// Record `flags` in the folder's `.mh_sequences`.
pub fn set_flags(root: &Path, entry: &Entry, flags: Flags) -> io::Result<PathBuf> {
    require_regular(&entry.path)?;
    within_root(root, &entry.path)?;
    apply_flags(dir_of(&entry.path)?, number_of(entry)?, flags)?;
    Ok(entry.path.clone())
}

/// Move a message to another folder under a new number, keeping its flags.
pub fn move_to(
    root: &Path,
    entry: &Entry,
    target_dir: &Path,
    flags: Flags,
) -> io::Result<(String, PathBuf)> {
    require_regular(&entry.path)?;
    within_root(root, &entry.path)?;
    let raw = fs::read(&entry.path)?;
    let delivered = deliver(root, target_dir, &raw, flags)?;
    if let Err(error) = remove(root, entry) {
        // Never leave two copies behind.
        let _ = fs::remove_file(&delivered.1);
        return Err(error);
    }
    Ok(delivered)
}

pub fn remove(root: &Path, entry: &Entry) -> io::Result<()> {
    require_regular(&entry.path)?;
    within_root(root, &entry.path)?;
    let number = number_of(entry)?;
    fs::remove_file(&entry.path)?;
    drop_from_sequences(dir_of(&entry.path)?, number)
}

/// Directory of the folder called `name` under `root`.
pub fn dir_for_new_folder(root: &Path, name: &str) -> Option<PathBuf> {
    let clean = !name.is_empty()
        && name.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && !part.contains(['\0', '\\'])
                && message_number(part).is_none()
        });
    if !clean {
        return None;
    }
    if name.eq_ignore_ascii_case("INBOX") && is_mh_folder(root) {
        return Some(root.to_path_buf());
    }
    let mut path = root.to_path_buf();
    for part in name.split('/') {
        path.push(part);
    }
    Some(path)
}

/// The discovered folder called `name`, never INBOX (the root itself).
fn find_folder(root: &Path, name: &str) -> io::Result<Folder> {
    if name.eq_ignore_ascii_case("INBOX") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INBOX cannot be renamed or deleted",
        ));
    }
    let folder = discover(root)?
        .into_iter()
        .find(|folder| folder.name == name)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "folder not found"))?;
    if !fs::symlink_metadata(&folder.dir)?.is_dir() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a folder"));
    }
    within_root(root, &folder.dir)?;
    Ok(folder)
}

/// Rename the MH folder `old` to `new` (`/`-separated names). Its
/// sub-folders move with it and nothing is overwritten.
pub fn rename_folder(root: &Path, old: &str, new: &str) -> io::Result<()> {
    let from = find_folder(root, old)?.dir;
    if new.eq_ignore_ascii_case("INBOX") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INBOX cannot be renamed or deleted",
        ));
    }
    let to = dir_for_new_folder(root, new).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "that folder name cannot be used",
        )
    })?;
    if to.starts_with(&from) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a folder cannot be moved into itself",
        ));
    }
    if fs::symlink_metadata(&to).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a folder with that name already exists",
        ));
    }
    let parent = dir_of(&to)?;
    guard_ancestor(root, parent)?;
    fs::create_dir_all(parent)?;
    within_root(root, &to)?;
    fs::rename(&from, &to)
}

/// Delete the MH folder `name` with its sub-folders and every file in them.
pub fn delete_folder(root: &Path, name: &str) -> io::Result<()> {
    let dir = find_folder(root, name)?.dir;
    if fs::canonicalize(&dir)? == fs::canonicalize(root)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the account root cannot be deleted",
        ));
    }
    fs::remove_dir_all(&dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(dir: &Path, name: &str, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn sequences_roundtrip_ranges_and_keep_unknown_ones() {
        let parsed = parse_sequences("unseen: 1-3 5\ncur: 2\nflagged: 9\n");
        assert_eq!(format_set(&parsed[0].1), "1-3 5");
        assert_eq!(parsed.len(), 3);
        assert!(parse_sequences("x: 1-99999999999\n")[0].1.is_empty());
    }

    #[test]
    fn discovers_nested_folders_and_ignores_junk() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        put(&root.join("inbox"), "1", "x");
        put(&root.join("Lists/rust"), "4", "x");
        put(&root.join("Lists/rust"), "notes.txt", "x");
        put(&root.join("empty"), "readme", "x");
        put(&root.join(".hidden"), "1", "x");
        let names: Vec<_> = discover(root)
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, ["Lists/rust", "inbox"]);
        let rust = locate(&root.join("Lists/rust"), "4").unwrap();
        assert!(rust.flags.seen);
        assert!(list(&root.join("Lists/rust")).unwrap().len() == 1);
        put(root, "2", "x");
        assert_eq!(discover(root).unwrap()[0].name, "INBOX");
    }

    #[test]
    fn flags_move_and_remove_write_back() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Mail");
        let inbox = root.join("inbox");
        let trash = root.join("Trash");
        put(&inbox, "1", "Subject: a\n\nA\n");
        put(&inbox, "2", "Subject: b\n\nB\n");
        fs::write(inbox.join(SEQUENCES), "unseen: 1-2\nflagged: 2\n").unwrap();
        let first = locate(&inbox, "1").unwrap();
        assert!(!first.flags.seen && !first.flags.flagged);
        assert!(locate(&inbox, "2").unwrap().flags.flagged);

        set_flags(
            &root,
            &first,
            Flags {
                seen: true,
                flagged: true,
                ..Flags::default()
            },
        )
        .unwrap();
        let first = locate(&inbox, "1").unwrap();
        assert!(first.flags.seen && first.flags.flagged);
        assert!(!locate(&inbox, "2").unwrap().flags.seen);

        let (key, path) = move_to(&root, &first, &trash, first.flags).unwrap();
        assert_eq!(key, "1");
        assert!(path.exists() && !inbox.join("1").exists());
        assert!(locate(&trash, "1").unwrap().flags.flagged);
        let second = locate(&inbox, "2").unwrap();
        remove(&root, &second).unwrap();
        assert!(locate(&inbox, "2").is_none());
        assert!(
            !fs::read_to_string(inbox.join(SEQUENCES))
                .unwrap()
                .contains('2')
        );
    }

    fn claws_marks_file(entries: &[(u32, u32)], little: bool) -> Vec<u8> {
        let word = |value: u32| {
            if little {
                value.to_le_bytes()
            } else {
                value.to_be_bytes()
            }
        };
        let mut bytes = word(2).to_vec();
        for (number, flags) in entries {
            bytes.extend(word(*number));
            bytes.extend(word(*flags));
        }
        bytes
    }

    #[test]
    fn claws_marks_give_the_read_and_starred_state_until_sequences_exist() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Mail");
        let inbox = root.join("inbox");
        for number in ["1", "2", "3", "4"] {
            put(&inbox, number, "Subject: x\n\nx\n");
        }
        // 1 read, 2 read and starred, 3 unread, 4 not in the file (new).
        fs::write(
            inbox.join(CLAWS_MARK),
            claws_marks_file(&[(1, 0), (2, CLAWS_MARKED), (3, CLAWS_UNREAD)], true),
        )
        .unwrap();
        let state = |dir: &Path, key: &str| {
            let entry = locate(dir, key).unwrap();
            (entry.flags.seen, entry.flags.flagged)
        };
        assert_eq!(state(&inbox, "1"), (true, false));
        assert_eq!(state(&inbox, "2"), (true, true));
        assert_eq!(state(&inbox, "3"), (false, false));
        assert_eq!(state(&inbox, "4"), (false, false));
        // Reading a message here writes .mh_sequences from the marks, so the
        // other messages keep what Claws knew.
        let third = locate(&inbox, "3").unwrap();
        set_flags(
            &root,
            &third,
            Flags {
                seen: true,
                ..Flags::default()
            },
        )
        .unwrap();
        assert!(inbox.join(SEQUENCES).exists());
        assert_eq!(state(&inbox, "2"), (true, true));
        assert_eq!(state(&inbox, "3"), (true, false));
        assert_eq!(state(&inbox, "4"), (false, false));
        // From now on the sequences win over the (older) mark file.
        fs::write(
            inbox.join(CLAWS_MARK),
            claws_marks_file(&[(4, CLAWS_MARKED)], true),
        )
        .unwrap();
        assert_eq!(state(&inbox, "4"), (false, false));
    }

    #[test]
    fn claws_marks_of_another_byte_order_or_version_are_handled_safely() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("inbox");
        put(&dir, "1", "Subject: x\n\nx\n");
        put(&dir, "2", "Subject: x\n\nx\n");
        // Written on a big-endian machine.
        fs::write(
            dir.join(CLAWS_MARK),
            claws_marks_file(&[(1, CLAWS_MARKED), (2, 0)], false),
        )
        .unwrap();
        let first = locate(&dir, "1").unwrap();
        let second = locate(&dir, "2").unwrap();
        assert!(first.flags.flagged && second.flags.seen);
        // Another version, a truncated file and garbage are ignored.
        let mut other = claws_marks_file(&[(1, CLAWS_MARKED)], true);
        other[0] = 3;
        for bytes in [other, vec![2, 0], b"not a mark file".to_vec(), Vec::new()] {
            fs::write(dir.join(CLAWS_MARK), bytes).unwrap();
            let entry = locate(&dir, "1").unwrap();
            assert!(entry.flags.seen && !entry.flags.flagged);
        }
        // A symlinked mark file is never followed.
        #[cfg(unix)]
        {
            let outside = temp.path().join("outside");
            fs::write(&outside, claws_marks_file(&[(1, CLAWS_MARKED)], true)).unwrap();
            fs::remove_file(dir.join(CLAWS_MARK)).unwrap();
            std::os::unix::fs::symlink(&outside, dir.join(CLAWS_MARK)).unwrap();
            assert!(!locate(&dir, "1").unwrap().flags.flagged);
        }
    }

    #[test]
    fn deliver_uses_the_next_free_number() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        put(&root.join("Sent"), "7", "x");
        let (key, _) = deliver(
            root,
            &root.join("Sent"),
            b"Subject: z\n\n",
            Flags::default(),
        )
        .unwrap();
        assert_eq!(key, "8");
        let (key, _) = deliver(root, &root.join("New"), b"y", Flags::default()).unwrap();
        assert_eq!(key, "1");
        assert!(dir_for_new_folder(root, "../evil").is_none());
        assert!(dir_for_new_folder(root, "a/.b").is_none());
        assert!(dir_for_new_folder(root, "12").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_escapes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        put(&root.join("inbox"), "1", "x");
        put(&outside, "1", "secret");
        std::os::unix::fs::symlink(&outside, root.join("Evil")).unwrap();
        assert!(discover(&root).unwrap().iter().all(|f| f.name != "Evil"));
        let error = deliver(&root, &root.join("Evil"), b"data", Flags::default()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        let entry = Entry {
            key: "1".into(),
            path: outside.join("1"),
            in_new: false,
            flags: Flags::default(),
            size: 6,
        };
        assert!(remove(&root, &entry).is_err());
        assert!(set_flags(&root, &entry, Flags::default()).is_err());
        assert!(outside.join("1").exists());
        std::os::unix::fs::symlink(outside.join("1"), root.join("inbox/2")).unwrap();
        assert!(
            list(&root.join("inbox"))
                .unwrap()
                .iter()
                .all(|e| e.key != "2")
        );
    }

    #[test]
    fn rename_and_delete_nested_folders() {
        let root = std::env::temp_dir().join(format!("rustmail-mh-rename-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        put(&root.join("Work"), "1", "Subject: a\n\nx\n");
        put(&root.join("Work/Sub"), "1", "Subject: b\n\nx\n");
        create_folder(&root, &root.join("Keep")).unwrap();

        assert!(rename_folder(&root, "INBOX", "X").is_err());
        assert!(rename_folder(&root, "Work", "Keep").is_err());
        assert!(rename_folder(&root, "Work", "Work/Sub/In").is_err());
        rename_folder(&root, "Work", "Archive/Work2").unwrap();
        let names: Vec<_> = discover(&root)
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert!(names.contains(&"Archive/Work2".to_owned()));
        assert!(names.contains(&"Archive/Work2/Sub".to_owned()));
        assert!(!names.contains(&"Work".to_owned()));

        delete_folder(&root, "Archive/Work2").unwrap();
        assert!(!root.join("Archive/Work2").exists());
        assert!(delete_folder(&root, "INBOX").is_err());
        assert!(root.join("Keep").exists());
        let _ = fs::remove_dir_all(&root);
    }
}
