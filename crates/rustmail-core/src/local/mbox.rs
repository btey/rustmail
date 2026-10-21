//! mbox support (mboxrd and the common mboxo/mboxcl variants).
//!
//! Reading parses the whole file. Writing (flag changes, removals, moves,
//! delivery) rewrites the file: every message that is not edited keeps its
//! exact bytes, the new file is written next to the old one and renamed over
//! it, and a `<file>.lock` dot-lock is held meanwhile. Another program that
//! appends between our read and the rename is detected by a fingerprint check
//! and the write fails (the caller retries); there is no way to exclude a
//! program that ignores the dot-lock completely. Folder management creates,
//! renames and deletes whole mbox files.

use super::maildir::within_root;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
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

/// One `From ` separated chunk of the file. `end` is the start of the next
/// chunk (or the file's end), so a chunk includes its trailing blank line.
struct Chunk {
    start: usize,
    body_start: usize,
    end: usize,
    /// `None` for chunks `parse` skips (empty or oversized).
    message: Option<Message>,
}

/// A separator is a line starting with `From ` at the start of the file or
/// right after a blank line.
fn chunks(bytes: &[u8]) -> Vec<Chunk> {
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
    let mut out = Vec::with_capacity(starts.len());
    for (index, (start, body_start)) in starts.iter().enumerate() {
        let next = starts.get(index + 1).map(|(next, _)| *next);
        let chunk_end = next.unwrap_or(bytes.len());
        let mut end = chunk_end;
        // The blank line before the next separator is part of the separator.
        if end > *body_start && bytes[..end].ends_with(b"\r\n") && next.is_some() {
            end -= 2;
        } else if end > *body_start && bytes[..end].ends_with(b"\n") && next.is_some() {
            end -= 1;
        }
        let message = if end <= *body_start {
            None
        } else {
            let raw = unescape(&bytes[*body_start..end]);
            if raw.is_empty() || raw.len() > MAX_MESSAGE_BYTES {
                None
            } else {
                let identity = header_value(header_block(&raw), "Message-ID")
                    .map(|id| fnv(id.as_bytes()))
                    .unwrap_or_else(|| fnv(&raw[..raw.len().min(2048)]));
                Some(Message {
                    offset: *start as u64,
                    key: format!("{start}:{identity:016x}"),
                    flags: flags_of(&raw),
                    raw,
                })
            }
        };
        out.push(Chunk {
            start: *start,
            body_start: *body_start,
            end: chunk_end,
            message,
        });
    }
    out
}

/// Split an mbox into messages.
pub fn parse(bytes: &[u8]) -> Vec<Message> {
    chunks(bytes)
        .into_iter()
        .filter_map(|chunk| chunk.message)
        .collect()
}

/// What to do with one message of an mbox.
#[derive(Debug, Clone)]
pub enum Edit {
    /// Change the read and/or starred mark (`Status`/`X-Status`); the other
    /// flags of the message stay as they are.
    Set {
        seen: Option<bool>,
        flagged: Option<bool>,
    },
    Remove,
}

#[derive(Debug, Default)]
pub struct Outcome {
    /// `(old key, new key)` for every kept message whose key changed (keys
    /// contain the byte offset, so an edit shifts the ones after it).
    pub remap: Vec<(String, String)>,
}

const LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(300);

/// A `<file>.lock` dot-lock, removed on drop.
struct DotLock(PathBuf);

impl DotLock {
    fn acquire(path: &Path) -> io::Result<Self> {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid("bad mbox path"))?;
        let lock = path.with_file_name(format!("{name}.lock"));
        for _ in 0..2 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock)
            {
                Ok(_) => return Ok(Self(lock)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let stale = fs::symlink_metadata(&lock)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|time| time.elapsed().ok())
                        .is_some_and(|age| age > LOCK_STALE);
                    if !stale || fs::remove_file(&lock).is_err() {
                        break;
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "the mbox file is locked by another program",
        ))
    }
}

impl Drop for DotLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn ends_with_blank_line(bytes: &[u8]) -> bool {
    bytes.ends_with(b"\n\n") || bytes.ends_with(b"\n\r\n")
}

/// The line ending the message's headers use.
fn eol_of(block: &[u8]) -> &'static [u8] {
    if block.windows(2).any(|pair| pair == b"\r\n") {
        b"\r\n"
    } else {
        b"\n"
    }
}

/// Rewrite one header (`Status`/`X-Status`) of `raw` (headers + body, still
/// mboxrd-escaped) so that its letters are `letters` plus any letters it
/// already had that are not in `managed` (such as `O`). An empty result drops
/// the header.
fn set_header_letters(raw: &[u8], name: &str, managed: &[u8], letters: &[u8]) -> Vec<u8> {
    let block_len = header_block(raw).len();
    let block = &raw[..block_len];
    let eol = eol_of(block);
    let prefix = format!("{}:", name.to_ascii_lowercase());
    let mut out = Vec::with_capacity(raw.len() + 16);
    let mut found = false;
    let mut cursor = 0;
    for line in block.split_inclusive(|byte| *byte == b'\n') {
        let text = String::from_utf8_lossy(line).to_ascii_lowercase();
        if text.starts_with(&prefix) {
            // Keep unmanaged letters; drop duplicate headers.
            if !found {
                found = true;
                let value = &line[prefix.len()..];
                let mut merged = letters.to_vec();
                merged.extend(
                    value
                        .iter()
                        .copied()
                        .filter(|byte| byte.is_ascii_alphabetic() && !managed.contains(byte)),
                );
                if !merged.is_empty() {
                    out.extend_from_slice(name.as_bytes());
                    out.extend_from_slice(b": ");
                    out.extend_from_slice(&merged);
                    out.extend_from_slice(if line.ends_with(b"\r\n") {
                        b"\r\n"
                    } else if line.ends_with(b"\n") {
                        b"\n"
                    } else {
                        b""
                    });
                }
            }
        } else {
            out.extend_from_slice(line);
        }
        cursor += line.len();
    }
    debug_assert_eq!(cursor, block_len);
    if !found && !letters.is_empty() {
        let header = [name.as_bytes(), b": ", letters].concat();
        if block.is_empty() {
            // No headers at all: the blank line already starts the body.
            out.extend_from_slice(&header);
            out.extend_from_slice(eol);
        } else {
            if !out.ends_with(b"\n") {
                out.extend_from_slice(eol);
            }
            out.extend_from_slice(&header);
        }
        out.extend_from_slice(&raw[block_len..]);
        return out;
    }
    // The block excludes the newline that ends its last header.
    if out.ends_with(b"\r\n") {
        out.truncate(out.len() - 2);
    } else if out.ends_with(b"\n") {
        out.truncate(out.len() - 1);
    }
    out.extend_from_slice(&raw[block_len..]);
    out
}

/// `raw` with `Status` and `X-Status` set from `flags`.
fn with_flags(raw: &[u8], flags: &Flags) -> Vec<u8> {
    let mut status = Vec::new();
    if flags.seen {
        status.push(b'R');
    }
    let mut x_status = Vec::new();
    for (on, letter) in [
        (flags.replied, b'A'),
        (flags.flagged, b'F'),
        (flags.deleted, b'D'),
        (flags.draft, b'T'),
    ] {
        if on {
            x_status.push(letter);
        }
    }
    let raw = set_header_letters(raw, "Status", b"R", &status);
    set_header_letters(&raw, "X-Status", b"AFDT", &x_status)
}

/// Write `bytes` over `path` atomically (temp file in the same directory,
/// renamed into place). Fails when the file changed since `expected`.
fn replace_file(path: &Path, bytes: &[u8], expected: &str) -> io::Result<()> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("bad mbox path"))?;
    let temp = path.with_file_name(format!(".{name}.rustmail-tmp"));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)?;
        if let Ok(meta) = fs::metadata(path) {
            let _ = fs::set_permissions(&temp, meta.permissions());
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if fingerprint(path) != expected {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "the mbox file changed while it was being updated",
            ));
        }
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Apply `edits` (by message key) to the mbox at `path`. Messages that are
/// not edited are written back byte for byte. Keys that are not in the file
/// are ignored.
pub fn edit(path: &Path, edits: &HashMap<String, Edit>) -> io::Result<Outcome> {
    let _lock = DotLock::acquire(path)?;
    let before = fingerprint(path);
    let bytes = read_file(path)?;
    let parsed = chunks(&bytes);
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    // Old key of each chunk that was written, in order.
    let mut written: Vec<Option<String>> = Vec::new();
    let mut last_removed = false;
    for chunk in &parsed {
        let key = chunk.message.as_ref().map(|message| message.key.clone());
        let action = key.as_ref().and_then(|key| edits.get(key));
        last_removed = matches!(action, Some(Edit::Remove));
        if last_removed {
            continue;
        }
        let original = &bytes[chunk.start..chunk.end];
        // Make sure the previous message is followed by a blank line.
        if !out.is_empty() && !ends_with_blank_line(&out) {
            if !out.ends_with(b"\n") {
                out.push(b'\n');
            }
            out.push(b'\n');
        }
        match action {
            Some(Edit::Set { seen, flagged }) => {
                let mut flags = chunk
                    .message
                    .as_ref()
                    .map(|message| message.flags.clone())
                    .unwrap_or_default();
                flags.seen = seen.unwrap_or(flags.seen);
                flags.flagged = flagged.unwrap_or(flags.flagged);
                let head = chunk.body_start - chunk.start;
                out.extend_from_slice(&original[..head]);
                out.extend_from_slice(&with_flags(&original[head..], &flags));
            }
            _ => out.extend_from_slice(original),
        }
        written.push(key);
    }
    if last_removed {
        while out.ends_with(b"\n\n") {
            out.pop();
        }
    }
    // Anything before the first separator is not a message; keep it.
    if let Some(first) = parsed.first()
        && first.start > 0
    {
        let mut with_preamble = bytes[..first.start].to_vec();
        with_preamble.extend_from_slice(&out);
        out = with_preamble;
    }
    if parsed.is_empty() {
        out = bytes.clone();
    }
    if !out.is_empty() && !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    let rewritten = chunks(&out);
    replace_file(path, &out, &before)?;
    let mut outcome = Outcome::default();
    for (old, new) in written.iter().zip(rewritten.iter()) {
        if let (Some(old), Some(new)) = (old, new.message.as_ref())
            && *old != new.key
        {
            outcome.remap.push((old.clone(), new.key.clone()));
        }
    }
    Ok(outcome)
}

/// The message with `key` exactly as stored (separator line and escaped
/// body, without the blank line that follows it).
pub fn extract(path: &Path, key: &str) -> io::Result<Option<Vec<u8>>> {
    let bytes = read_file(path)?;
    Ok(chunks(&bytes)
        .into_iter()
        .find(|chunk| {
            chunk
                .message
                .as_ref()
                .is_some_and(|message| message.key == key)
        })
        .map(|chunk| {
            let mut piece = bytes[chunk.start..chunk.end].to_vec();
            while piece.ends_with(b"\n\n") || piece.ends_with(b"\r\n\r\n") {
                let cut = if piece.ends_with(b"\r\n\r\n") { 2 } else { 1 };
                piece.truncate(piece.len() - cut);
            }
            piece
        }))
}

/// Append a stored message (as returned by `extract`) to the mbox at `path`
/// and return its new key.
pub fn append(path: &Path, piece: &[u8]) -> io::Result<String> {
    let _lock = DotLock::acquire(path)?;
    let before = fingerprint(path);
    let mut bytes = read_file(path)?;
    if !bytes.is_empty() {
        if !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        if !ends_with_blank_line(&bytes) {
            bytes.push(b'\n');
        }
    }
    bytes.extend_from_slice(piece);
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    replace_file(path, &bytes, &before)?;
    chunks(&bytes)
        .into_iter()
        .rev()
        .find_map(|chunk| chunk.message.map(|message| message.key))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "appended message not found"))
}

/// Escape `>*From ` lines (mboxrd) and normalise line endings to `\n`.
fn escape(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + 64);
    let mut line_start = true;
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'\r' && raw.get(i + 1) == Some(&b'\n') {
            i += 1;
            continue;
        }
        if line_start {
            let mut j = i;
            while j < raw.len() && raw[j] == b'>' {
                j += 1;
            }
            if raw[j..].starts_with(b"From ") {
                out.push(b'>');
            }
        }
        out.push(raw[i]);
        line_start = raw[i] == b'\n';
        i += 1;
    }
    out
}

/// Deliver a new message (for example the copy of one just sent) to the mbox
/// at `path` and return its key.
pub fn deliver(path: &Path, raw: &[u8], flags: &Flags) -> io::Result<String> {
    let mut body = with_flags(&escape(raw), flags);
    if !body.ends_with(b"\n") {
        body.push(b'\n');
    }
    let mut piece = format!(
        "From MAILER-DAEMON {}\n",
        chrono::Utc::now().format("%a %b %e %H:%M:%S %Y")
    )
    .into_bytes();
    piece.extend_from_slice(&body);
    append(path, &piece)
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

    fn sample() -> Vec<u8> {
        b"From a@x Mon Jan  1 00:00:00 2024\nSubject: one\nMessage-ID: <1@x>\nStatus: O\nX-Keep: yes\n\nhello\n>From quoted\n\nFrom b@x Mon Jan  1 00:00:00 2024\r\nSubject: two\r\nMessage-ID: <2@x>\r\n\r\nbye\r\n\r\nFrom c@x Mon Jan  1 00:00:00 2024\nSubject: three\n\nlast".to_vec()
    }

    fn temp_mbox(name: &str, bytes: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rustmail-mbox-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("box");
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn flag_edits_change_only_the_status_headers() {
        let path = temp_mbox("flags", &sample());
        let before = parse(&sample());
        let edits = HashMap::from([
            (
                before[0].key.clone(),
                Edit::Set {
                    seen: Some(true),
                    flagged: Some(true),
                },
            ),
            (
                before[1].key.clone(),
                Edit::Set {
                    seen: Some(true),
                    flagged: None,
                },
            ),
        ]);
        let outcome = edit(&path, &edits).unwrap();
        let text = fs::read(&path).unwrap();
        let after = parse(&text);
        assert_eq!(after.len(), 3);
        assert!(after[0].flags.seen && after[0].flags.flagged);
        assert!(after[1].flags.seen && !after[1].flags.flagged);
        assert!(!after[2].flags.seen);
        let first = String::from_utf8_lossy(&after[0].raw).into_owned();
        // `O` and unrelated headers survive; the quoted From line is intact.
        assert!(first.contains("Status: RO\n"), "{first:?}");
        assert!(first.contains("X-Keep: yes"));
        assert!(first.contains("\nFrom quoted\n"));
        // CRLF stays CRLF in the message that was edited.
        assert!(String::from_utf8_lossy(&text).contains("Subject: two\r\n"));
        assert!(String::from_utf8_lossy(&text).contains("Status: R\r\n"));
        // The last message is byte for byte what it was.
        assert!(text.ends_with(b"Subject: three\n\nlast\n"));
        // Every changed key is reported.
        for (old, new) in &outcome.remap {
            assert!(before.iter().any(|m| &m.key == old));
            assert!(after.iter().any(|m| &m.key == new));
        }
        // Unseen again drops R but keeps O.
        let edits = HashMap::from([(
            after[0].key.clone(),
            Edit::Set {
                seen: Some(false),
                flagged: Some(false),
            },
        )]);
        edit(&path, &edits).unwrap();
        let again = parse(&fs::read(&path).unwrap());
        assert!(!again[0].flags.seen && !again[0].flags.flagged);
        assert!(String::from_utf8_lossy(&again[0].raw).contains("Status: O\n"));
        assert!(!String::from_utf8_lossy(&again[0].raw).contains("X-Status"));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn remove_extract_and_append_round_trip() {
        let path = temp_mbox("move", &sample());
        let target = path.with_file_name("other");
        fs::write(&target, "").unwrap();
        let before = parse(&sample());

        let piece = extract(&path, &before[1].key).unwrap().unwrap();
        assert!(piece.starts_with(b"From b@x"));
        let new_key = append(&target, &piece).unwrap();
        assert_eq!(parse(&fs::read(&target).unwrap())[0].key, new_key);
        append(&target, &extract(&path, &before[2].key).unwrap().unwrap()).unwrap();
        assert_eq!(parse(&fs::read(&target).unwrap()).len(), 2);

        let edits = HashMap::from([(before[1].key.clone(), Edit::Remove)]);
        let outcome = edit(&path, &edits).unwrap();
        let after = parse(&fs::read(&path).unwrap());
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].key, before[0].key);
        assert_eq!(
            outcome.remap,
            vec![(before[2].key.clone(), after[1].key.clone())]
        );
        // Removing the last message leaves a well-formed file.
        let edits = HashMap::from([(after[1].key.clone(), Edit::Remove)]);
        edit(&path, &edits).unwrap();
        let text = fs::read(&path).unwrap();
        assert_eq!(parse(&text).len(), 1);
        assert!(text.ends_with(b"\n>From quoted\n"));
        let edits = HashMap::from([(parse(&text)[0].key.clone(), Edit::Remove)]);
        edit(&path, &edits).unwrap();
        assert!(fs::read(&path).unwrap().is_empty());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn deliver_escapes_and_sets_status() {
        let path = temp_mbox("deliver", b"");
        let raw = b"Subject: sent\r\nMessage-ID: <s@x>\r\n\r\nFrom me\r\n>From you\r\n";
        let key = deliver(
            &path,
            raw,
            &Flags {
                seen: true,
                ..Default::default()
            },
        )
        .unwrap();
        let again = deliver(&path, raw, &Flags::default()).unwrap();
        let messages = parse(&fs::read(&path).unwrap());
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].key, key);
        assert_eq!(messages[1].key, again);
        assert!(messages[0].flags.seen && !messages[1].flags.seen);
        let body = String::from_utf8_lossy(&messages[0].raw).into_owned();
        assert!(body.contains("\nFrom me\n>From you\n"), "{body:?}");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_held_dot_lock_blocks_writes_and_leaves_the_file_alone() {
        let path = temp_mbox("lock", &sample());
        let lock = path.with_file_name("box.lock");
        fs::write(&lock, "").unwrap();
        let key = parse(&sample())[0].key.clone();
        let error = edit(&path, &HashMap::from([(key, Edit::Remove)])).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(fs::read(&path).unwrap(), sample());
        fs::remove_file(&lock).unwrap();
        assert!(!path.with_file_name(".box.rustmail-tmp").exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
