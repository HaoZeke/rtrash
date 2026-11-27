//! Sealed per-root catalog of `.trashinfo` bodies.
//!
//! `list` (and everything that collects the same records) reuses the blob
//! when every name in `info/` and `files/` still has the same kind, device,
//! inode, size and timestamps, and each of those times is strictly older
//! than the blob. Anything else is a full read of the records: a changed
//! name, a rewrite by another tool, a coarse clock that stores no
//! nanoseconds, a short or undecryptable blob, or `RTRASH_NO_CACHE`.
//!
//! The blob is XChaCha20-Poly1305 under `$XDG_CACHE_HOME/rtrash`. The
//! 32-byte key is mode 0600 at `$XDG_DATA_HOME/rtrash/cache.key`. Put,
//! restore, rm and empty delete the blob before they change the trash, so
//! a hit cannot replay entries from before that command.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

use crate::trashdir::TrashDir;

const MAGIC: &[u8; 4] = b"RTR1";
const VERSION: u32 = 1;
const NONCE_LEN: usize = 24;
const KEY_LEN: usize = 32;
/// A record body or a path stored in the blob, matching [`crate::info::RECORD_CAP`].
const BODY_CAP: usize = 1 << 20;
const NAME_CAP: usize = 1 << 16;
const COUNT_CAP: usize = 10_000_000;

/// One record as it was read, so a later collect can parse it again
/// without opening the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Record {
    pub name: String,
    pub file_name: Vec<u8>,
    pub body: Vec<u8>,
}

/// The records and the stray-name count of one trash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Saved {
    pub records: Vec<Record>,
    pub strays: u64,
}

/// What [`stamp`] saw. Compared as bytes; the times say whether the
/// blob is newer than every name it covers.
pub(crate) struct Stamp {
    bytes: Vec<u8>,
    max_m: (i64, i64),
    max_c: (i64, i64),
}

impl PartialEq for Stamp {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for Stamp {}

/// Whether this process should read or write a catalog.
pub(crate) fn enabled() -> bool {
    if no_cache() {
        return false;
    }
    // Library tests share a process. They opt in by pointing the cache at
    // a directory of their own; every other test keeps scanning.
    #[cfg(test)]
    {
        override_dirs().is_some()
    }
    #[cfg(not(test))]
    {
        true
    }
}

/// `1`, `true` or `yes`, in any case.
fn no_cache() -> bool {
    std::env::var("RTRASH_NO_CACHE")
        .ok()
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

/// The catalog for `dir`, when the blob still describes it.
pub(crate) fn load(dir: &TrashDir) -> Option<Saved> {
    if !enabled() {
        return None;
    }
    let saved = load_inner(dir)?;
    note("hit");
    Some(saved)
}

/// One collect that did not use a catalog. Ignored when the catalog is off,
/// so a test log only counts collects that were allowed to hit.
pub(crate) fn note_scan() {
    if enabled() {
        note("scan");
    }
}

/// Stamp of `info/` and `files/`, or `None` when it cannot be trusted
/// (missing, unreadable, not a real directory, or a clock with no
/// nanoseconds).
pub(crate) fn stamp(dir: &TrashDir) -> Option<Stamp> {
    if !enabled() {
        return None;
    }
    stamp_of(dir)
}

/// Write `saved` when `info/` and `files/` still match `before`.
pub(crate) fn store_if_unchanged(dir: &TrashDir, before: &Stamp, saved: &Saved) {
    if !enabled() {
        return;
    }
    let Some(after) = stamp_of(dir) else {
        return;
    };
    if after != *before {
        return;
    }
    let Some(plain) = encode(dir, &after, saved) else {
        return;
    };
    let Some(key) = load_or_create_key() else {
        return;
    };
    let Some(blob) = seal(&key, &plain) else {
        return;
    };
    let Some(path) = cache_path(dir) else {
        return;
    };
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return;
        }
        let _ = fs::set_permissions(parent, mode(0o700));
    }
    if crate::file_replace::write_own(&path, &blob).is_err() {
        return;
    }
    if fs::set_permissions(&path, mode(0o600)).is_err() {
        let _ = fs::remove_file(&path);
    }
}

/// Delete the catalog of `dir`. A missing blob is not an error.
pub(crate) fn invalidate(dir: &TrashDir) {
    invalidate_root(&dir.root);
}

/// [`invalidate`] for a trash root named by a record path's grandparent.
pub(crate) fn invalidate_root(root: &Path) {
    if let Some(path) = cache_path_of(root) {
        let _ = fs::remove_file(path);
    }
}

fn load_inner(dir: &TrashDir) -> Option<Saved> {
    let live = stamp_of(dir)?;
    let path = cache_path(dir)?;
    let meta = fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || !older_than_file(&live, &meta) {
        return None;
    }
    let raw = fs::read(&path).ok()?;
    if raw.len() < MAGIC.len() + NONCE_LEN + 16 || &raw[..4] != MAGIC {
        return None;
    }
    let key = read_key()?;
    let plain = open(&key, &raw[4..4 + NONCE_LEN], &raw[4 + NONCE_LEN..])?;
    decode(dir, &live, &plain)
}

fn older_than_file(stamp: &Stamp, meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    let file = (meta.mtime(), meta.mtime_nsec());
    before(stamp.max_m, file) && before(stamp.max_c, file)
}

fn before(earlier: (i64, i64), later: (i64, i64)) -> bool {
    let ns = |t: (i64, i64)| i128::from(t.0) * 1_000_000_000 + i128::from(t.1);
    ns(earlier) < ns(later)
}

fn stamp_of(dir: &TrashDir) -> Option<Stamp> {
    let mut rows = Vec::new();
    rows.push(dir_row(0, &dir.info())?);
    rows.push(dir_row(1, &dir.files())?);
    push_children(2, &dir.info(), &mut rows)?;
    push_children(3, &dir.files(), &mut rows)?;
    // A filesystem that reports no nanoseconds cannot tell two writes in
    // the same second apart. Listing then reads the records.
    let coarse = rows.iter().all(|r| r.mtime_n == 0 && r.ctime_n == 0);
    if coarse {
        return None;
    }
    rows.sort_by(|a, b| a.slot.cmp(&b.slot).then(a.name.cmp(&b.name)));
    let mut max_m = (i64::MIN, 0);
    let mut max_c = (i64::MIN, 0);
    let mut bytes = Vec::new();
    for row in &rows {
        max_m = later(max_m, (row.mtime, row.mtime_n));
        max_c = later(max_c, (row.ctime, row.ctime_n));
        push_row(&mut bytes, row);
    }
    Some(Stamp {
        bytes,
        max_m,
        max_c,
    })
}

fn later(a: (i64, i64), b: (i64, i64)) -> (i64, i64) {
    if before(a, b) {
        b
    } else {
        a
    }
}

struct Row {
    slot: u8,
    name: Vec<u8>,
    kind: u8,
    dev: u64,
    ino: u64,
    len: u64,
    mtime: i64,
    mtime_n: i64,
    ctime: i64,
    ctime_n: i64,
}

fn dir_row(slot: u8, path: &Path) -> Option<Row> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.is_dir() {
        return None;
    }
    Some(row_from(slot, Vec::new(), &meta))
}

fn push_children(slot: u8, dir: &Path, rows: &mut Vec<Row>) -> Option<()> {
    let rd = fs::read_dir(dir).ok()?;
    for ent in rd {
        let ent = ent.ok()?;
        let meta = ent.metadata().ok()?;
        // `DirEntry::metadata` does not follow a symlink. A name that
        // cannot be statted fails the whole stamp, so a half-seen
        // directory is never stored.
        let name = {
            use std::os::unix::ffi::OsStrExt;
            ent.file_name().as_bytes().to_vec()
        };
        rows.push(row_from(slot, name, &meta));
    }
    Some(())
}

fn row_from(slot: u8, name: Vec<u8>, meta: &fs::Metadata) -> Row {
    use std::os::unix::fs::MetadataExt;
    let ft = meta.file_type();
    let kind = if ft.is_symlink() {
        1
    } else if ft.is_dir() {
        2
    } else if ft.is_file() {
        3
    } else {
        4
    };
    Row {
        slot,
        name,
        kind,
        dev: meta.dev(),
        ino: meta.ino(),
        len: meta.len(),
        mtime: meta.mtime(),
        mtime_n: meta.mtime_nsec(),
        ctime: meta.ctime(),
        ctime_n: meta.ctime_nsec(),
    }
}

fn push_row(out: &mut Vec<u8>, row: &Row) {
    out.push(row.slot);
    out.push(row.kind);
    push_u64(out, row.dev);
    push_u64(out, row.ino);
    push_u64(out, row.len);
    push_i64(out, row.mtime);
    push_i64(out, row.mtime_n);
    push_i64(out, row.ctime);
    push_i64(out, row.ctime_n);
    push_bytes(out, &row.name);
}

fn encode(dir: &TrashDir, stamp: &Stamp, saved: &Saved) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    if saved.records.len() > COUNT_CAP {
        return None;
    }
    let mut b = Vec::new();
    push_u32(&mut b, VERSION);
    push_bytes(&mut b, dir.root.as_os_str().as_bytes());
    match &dir.topdir {
        Some(top) => {
            b.push(1);
            push_bytes(&mut b, top.as_os_str().as_bytes());
        }
        None => b.push(0),
    }
    push_bytes(&mut b, &stamp.bytes);
    push_u64(&mut b, saved.strays);
    push_u32(&mut b, saved.records.len() as u32);
    let mut records = saved.records.clone();
    records.sort_by(|a, c| a.file_name.cmp(&c.file_name));
    for rec in &records {
        if rec.body.len() > BODY_CAP || rec.name.len() > NAME_CAP || rec.file_name.len() > NAME_CAP
        {
            return None;
        }
        push_bytes(&mut b, rec.name.as_bytes());
        push_bytes(&mut b, &rec.file_name);
        push_bytes(&mut b, &rec.body);
    }
    Some(b)
}

fn decode(dir: &TrashDir, live: &Stamp, plain: &[u8]) -> Option<Saved> {
    use std::os::unix::ffi::OsStrExt;
    let mut c = Cur { b: plain, i: 0 };
    if c.u32()? != VERSION {
        return None;
    }
    if c.bytes()? != dir.root.as_os_str().as_bytes() {
        return None;
    }
    match c.u8()? {
        0 => {
            if dir.topdir.is_some() {
                return None;
            }
        }
        1 => {
            let top = c.bytes()?;
            let same = dir
                .topdir
                .as_ref()
                .is_some_and(|p| p.as_os_str().as_bytes() == top);
            if !same {
                return None;
            }
        }
        _ => return None,
    }
    if c.bytes()? != live.bytes {
        return None;
    }
    let strays = c.u64()?;
    let n = c.u32()? as usize;
    if n > COUNT_CAP {
        return None;
    }
    let mut records = Vec::with_capacity(n);
    for _ in 0..n {
        let name = c.bytes()?;
        if name.len() > NAME_CAP {
            return None;
        }
        let name = std::str::from_utf8(name).ok()?.to_string();
        let file_name = c.bytes()?.to_vec();
        if file_name.len() > NAME_CAP {
            return None;
        }
        let body = c.bytes()?.to_vec();
        if body.len() > BODY_CAP {
            return None;
        }
        records.push(Record {
            name,
            file_name,
            body,
        });
    }
    if c.i != c.b.len() {
        return None;
    }
    Some(Saved { records, strays })
}

struct Cur<'a> {
    b: &'a [u8],
    i: usize,
}

impl Cur<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.b.get(self.i..self.i + n)?;
        self.i += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(*self.take(1)?.first()?)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn bytes(&mut self) -> Option<&[u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}

fn push_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn push_u64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn push_i64(b: &mut Vec<u8>, v: i64) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn push_bytes(b: &mut Vec<u8>, v: &[u8]) {
    push_u32(b, v.len() as u32);
    b.extend_from_slice(v);
}

fn seal(key: &[u8; KEY_LEN], plain: &[u8]) -> Option<Vec<u8>> {
    let mut nonce_bytes = [0u8; NONCE_LEN];
    fill_random(&mut nonce_bytes).ok()?;
    let cipher = XChaCha20Poly1305::new_from_slice(key).ok()?;
    let nonce = XNonce::from_slice(&nonce_bytes);
    let ct = cipher.encrypt(nonce, plain).ok()?;
    let mut out = Vec::with_capacity(4 + NONCE_LEN + ct.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Some(out)
}

fn open(key: &[u8; KEY_LEN], nonce: &[u8], ct: &[u8]) -> Option<Vec<u8>> {
    if nonce.len() != NONCE_LEN {
        return None;
    }
    let cipher = XChaCha20Poly1305::new_from_slice(key).ok()?;
    let nonce = XNonce::from_slice(nonce);
    cipher.decrypt(nonce, ct).ok()
}

fn cache_path(dir: &TrashDir) -> Option<PathBuf> {
    cache_path_of(&dir.root)
}

fn cache_path_of(root: &Path) -> Option<PathBuf> {
    Some(cache_dir()?.join(format!("c-{}.bin", cache_id(root))))
}

fn cache_id(root: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut h = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58du128;
    for byte in b"rtrash-catalog-v1"
        .iter()
        .chain(root.as_os_str().as_bytes())
    {
        h ^= u128::from(*byte);
        h = h.wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013B);
    }
    format!("{h:032x}")
}

fn cache_dir() -> Option<PathBuf> {
    if let Some((cache, _)) = override_dirs() {
        return Some(cache);
    }
    xdg("XDG_CACHE_HOME", ".cache").map(|p| p.join("rtrash"))
}

fn key_path() -> Option<PathBuf> {
    if let Some((_, data)) = override_dirs() {
        return Some(data.join("cache.key"));
    }
    xdg("XDG_DATA_HOME", ".local/share").map(|p| p.join("rtrash/cache.key"))
}

fn xdg(var: &str, home_rel: &str) -> Option<PathBuf> {
    if let Some(v) = std::env::var_os(var) {
        if !v.is_empty() {
            return Some(PathBuf::from(v));
        }
    }
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join(home_rel))
}

fn read_key() -> Option<[u8; KEY_LEN]> {
    let bytes = fs::read(key_path()?).ok()?;
    <[u8; KEY_LEN]>::try_from(bytes).ok()
}

fn load_or_create_key() -> Option<[u8; KEY_LEN]> {
    let path = key_path()?;
    match fs::read(&path) {
        Ok(bytes) => match <[u8; KEY_LEN]>::try_from(bytes) {
            Ok(key) => Some(key),
            Err(_) => {
                let _ = fs::remove_file(&path);
                create_key(&path)
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => create_key(&path),
        Err(_) => None,
    }
}

fn create_key(path: &Path) -> Option<[u8; KEY_LEN]> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut key = [0u8; KEY_LEN];
    fill_random(&mut key).ok()?;
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return None;
        }
        let _ = fs::set_permissions(parent, mode(0o700));
    }
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true).mode(0o600);
    match opts.open(path) {
        Ok(mut f) => {
            if f.write_all(&key).is_err() || f.sync_all().is_err() {
                let _ = fs::remove_file(path);
                return None;
            }
            if fs::set_permissions(path, mode(0o600)).is_err() {
                let _ = fs::remove_file(path);
                return None;
            }
            Some(key)
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => read_key(),
        Err(_) => None,
    }
}

fn fill_random(buf: &mut [u8]) -> io::Result<()> {
    let mut f = fs::File::open("/dev/urandom")?;
    std::io::Read::read_exact(&mut f, buf)
}

fn mode(bits: u32) -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    fs::Permissions::from_mode(bits)
}

fn note(kind: &'static str) {
    #[cfg(test)]
    NOTES.with(|n| n.borrow_mut().push(kind));
    if !cfg!(debug_assertions) {
        return;
    }
    let Ok(path) = std::env::var("RTRASH_TEST_CATALOG_LOG") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    let mut file = match OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let _ = writeln!(file, "{kind}");
}

#[cfg(test)]
thread_local! {
    static NOTES: std::cell::RefCell<Vec<&'static str>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static OVERRIDE: std::cell::RefCell<Option<(PathBuf, PathBuf)>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn override_dirs() -> Option<(PathBuf, PathBuf)> {
    OVERRIDE.with(|o| o.borrow().clone())
}

#[cfg(not(test))]
fn override_dirs() -> Option<(PathBuf, PathBuf)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(1);

    struct Isolated {
        root: PathBuf,
        dir: TrashDir,
    }

    impl Isolated {
        fn open() -> Self {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let root =
                std::env::temp_dir().join(format!("rtrash-catalog-{n}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            let dir = TrashDir {
                root: root.join("trash"),
                topdir: None,
            };
            fs::create_dir_all(dir.info()).unwrap();
            fs::create_dir_all(dir.files()).unwrap();
            fs::create_dir_all(root.join("cache")).unwrap();
            fs::create_dir_all(root.join("data")).unwrap();
            OVERRIDE.with(|o| {
                *o.borrow_mut() = Some((root.join("cache"), root.join("data")));
            });
            NOTES.with(|n| n.borrow_mut().clear());
            Isolated { root, dir }
        }
    }

    impl Drop for Isolated {
        fn drop(&mut self) {
            OVERRIDE.with(|o| *o.borrow_mut() = None);
            NOTES.with(|n| n.borrow_mut().clear());
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn write_record(dir: &TrashDir, name: &str, path: &str) {
        let body = format!("[Trash Info]\nPath={path}\nDeletionDate=2026-01-02T03:04:05\n");
        fs::write(dir.info().join(format!("{name}.trashinfo")), body).unwrap();
        fs::write(dir.files().join(name), b"payload").unwrap();
    }

    #[test]
    fn a_round_trip_is_sealed_and_a_rewrite_misses() {
        let iso = Isolated::open();
        write_record(&iso.dir, "hello.txt", "/tmp/hello.txt");
        let before = stamp(&iso.dir).expect("stamp");
        let saved = Saved {
            records: vec![Record {
                name: "hello.txt".into(),
                file_name: b"hello.txt.trashinfo".to_vec(),
                body: b"[Trash Info]\nPath=/tmp/hello.txt\n".to_vec(),
            }],
            strays: 0,
        };
        store_if_unchanged(&iso.dir, &before, &saved);
        let loaded = load(&iso.dir).expect("hit");
        assert_eq!(loaded, saved);
        let blob = fs::read(cache_path(&iso.dir).unwrap()).unwrap();
        assert!(blob.starts_with(b"RTR1"));
        assert!(
            !blob
                .windows(b"/tmp/hello.txt".len())
                .any(|w| w == b"/tmp/hello.txt"),
            "sealed blob must not contain Path= plaintext"
        );
        let key = key_path().unwrap();
        assert_eq!(
            fs::metadata(&key).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::write(
            iso.dir.info().join("hello.txt.trashinfo"),
            b"rewritten-by-another-tool",
        )
        .unwrap();
        assert!(load(&iso.dir).is_none(), "an external rewrite must miss");
        invalidate(&iso.dir);
        assert!(load(&iso.dir).is_none());
    }

    #[test]
    fn a_damaged_blob_is_not_a_catalog() {
        let iso = Isolated::open();
        write_record(&iso.dir, "a.txt", "/tmp/a.txt");
        let before = stamp(&iso.dir).unwrap();
        store_if_unchanged(
            &iso.dir,
            &before,
            &Saved {
                records: vec![Record {
                    name: "a.txt".into(),
                    file_name: b"a.txt.trashinfo".to_vec(),
                    body: fs::read(iso.dir.info().join("a.txt.trashinfo")).unwrap(),
                }],
                strays: 0,
            },
        );
        let path = cache_path(&iso.dir).unwrap();
        let mut raw = fs::read(&path).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0xff;
        fs::write(&path, &raw).unwrap();
        assert!(load(&iso.dir).is_none());
    }

    #[test]
    fn collect_hits_the_second_time_and_sees_an_external_edit() {
        let iso = Isolated::open();
        write_record(&iso.dir, "item.txt", "/tmp/catalog-before");
        let first = crate::list::collect(std::slice::from_ref(&iso.dir));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].original, PathBuf::from("/tmp/catalog-before"));
        let notes = NOTES.with(|n| n.borrow().clone());
        assert_eq!(notes, ["scan"], "the first collect reads the records");
        NOTES.with(|n| n.borrow_mut().clear());
        let second = crate::list::collect(std::slice::from_ref(&iso.dir));
        assert_eq!(second[0].original, first[0].original);
        let notes = NOTES.with(|n| n.borrow().clone());
        assert_eq!(notes, ["hit"], "the second collect must use the catalog");
        fs::write(
            iso.dir.info().join("item.txt.trashinfo"),
            b"[Trash Info]\nPath=/tmp/catalog-after-external\nDeletionDate=2026-01-02T03:04:05\n",
        )
        .unwrap();
        NOTES.with(|n| n.borrow_mut().clear());
        let third = crate::list::collect(std::slice::from_ref(&iso.dir));
        assert_eq!(
            third[0].original,
            PathBuf::from("/tmp/catalog-after-external")
        );
        let notes = NOTES.with(|n| n.borrow().clone());
        assert_eq!(notes, ["scan"], "an external edit must not hit");
    }
}
