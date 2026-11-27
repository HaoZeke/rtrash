//! Sealed per-root catalog of `.trashinfo` bodies.
//!
//! `list` (and everything that collects the same records) reuses the blob
//! when every name in `info/` and `files/` still has the same kind, device,
//! inode, size, timestamps and record hash, and every modification time
//! is older than the blob by the coarse-clock margin. Anything else is a
//! full read of the records: a changed name, a rewrite by another tool
//! (including one that keeps the timestamps), a clock with no nanoseconds,
//! a short or undecryptable blob, or `RTRASH_NO_CACHE`.
//!
//! The blob is XChaCha20-Poly1305 under `$XDG_CACHE_HOME/rtrash`. The
//! 32-byte key is mode 0600 at `$XDG_DATA_HOME/rtrash/cache.key`. Put,
//! restore, rm and empty delete the blob before they change the trash, so
//! a hit cannot replay entries from before that command.

use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
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
    /// Newest modification time in the stamp. ctime stays in `bytes`:
    /// `utimensat` cannot move it backward, and a rewrite that leaves both
    /// times unchanged changes the record hash instead.
    max_m: (i64, i64),
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
    if after != *before || !mtime_settled(&after) || !bodies_match(&after.bytes, saved) {
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
    let Some(parent) = path.parent() else {
        return;
    };
    if !ensure_private_dir(parent) {
        return;
    }
    if crate::file_replace::write_own(&path, &blob).is_err() {
        return;
    }
    // Opened with O_NOFOLLOW. A link planted at this path is not written
    // through, and a file anyone else can read is not kept.
    if !private_blob(&path, &after) {
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
    let parent = path.parent()?;
    if !dir_is_private(parent) {
        return None;
    }
    let (raw, meta) = read_private(&path)?;
    if !older_than_file(&live, &meta) {
        return None;
    }
    if raw.len() < MAGIC.len() + NONCE_LEN + 16 || &raw[..4] != MAGIC {
        return None;
    }
    let key = read_key()?;
    let plain = open(&key, &raw[4..4 + NONCE_LEN], &raw[4 + NONCE_LEN..])?;
    let saved = decode(dir, &live, &plain)?;
    bodies_match(&live.bytes, &saved).then_some(saved)
}

fn older_than_file(stamp: &Stamp, meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    let blob = (meta.mtime(), meta.mtime_nsec());
    // The blob's own mtime is the clock the file system stamped it with.
    // A source mtime within the margin can share that coarse tick, so it
    // is not evidence the records were quiet while the blob was written.
    before(stamp.max_m, sub_ns(blob, margin_ns()))
}

/// Newest modification time is at least the margin behind the system clock.
/// The post-write check against the blob is the one that decides; this
/// only skips writing a blob that check would delete.
fn mtime_settled(stamp: &Stamp) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs() as i64, d.subsec_nanos() as i64))
        .unwrap_or((i64::MAX, 0));
    before(stamp.max_m, sub_ns(now, margin_ns()))
}

fn sub_ns(t: (i64, i64), margin: u128) -> (i64, i64) {
    let ns = i128::from(t.0) * 1_000_000_000 + i128::from(t.1) - (margin as i128);
    (
        ns.div_euclid(1_000_000_000) as i64,
        ns.rem_euclid(1_000_000_000) as i64,
    )
}

/// How long a file stamp can stay still while its nanosecond field is not
/// zero. Linux `CLOCK_REALTIME_COARSE` is that clock (about 4 ms here) and
/// a blob written a few ticks later is still inside the race, so the
/// margin is twice the measured tick and at least twice 10 ms.
fn margin_ns() -> u128 {
    coarse_tick_ns().max(10_000_000).saturating_mul(2)
}

fn coarse_tick_ns() -> u128 {
    #[cfg(target_os = "linux")]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a writable timespec and the clock id is a constant.
        let rc = unsafe { libc::clock_getres(libc::CLOCK_REALTIME_COARSE, &mut ts) };
        if rc == 0 {
            let ns = (ts.tv_sec as u128)
                .saturating_mul(1_000_000_000)
                .saturating_add(ts.tv_nsec as u128);
            if ns > 0 {
                return ns;
            }
        }
    }
    10_000_000
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
    let mut bytes = Vec::new();
    for row in &rows {
        max_m = later(max_m, (row.mtime, row.mtime_n));
        push_row(&mut bytes, row);
    }
    Some(Stamp { bytes, max_m })
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
    hash: u128,
}

fn dir_row(slot: u8, path: &Path) -> Option<Row> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.is_dir() {
        return None;
    }
    Some(row_from(slot, Vec::new(), &meta, 0))
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
        // Records are tiny. Their hash is what a same-tick rewrite changes
        // when the coarse clock leaves mtime, ctime, inode and size alone.
        // A symlink record is hashed too, through the target `read_bounded`
        // follows, so a saved body of that target cannot hide behind a zero
        // hash. `files/` is not hashed: a payload there can be the user's data.
        let ft = meta.file_type();
        let hash = if slot == 2 && is_record_name(&name) && (ft.is_file() || ft.is_symlink()) {
            hash_record(&ent.path())?
        } else {
            0
        };
        rows.push(row_from(slot, name, &meta, hash));
    }
    Some(())
}

fn is_record_name(name: &[u8]) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let fname = crate::util::str_from_os_ref(std::ffi::OsStr::from_bytes(name));
    crate::util::trash_item_name(&fname).is_some()
}

fn hash_record(path: &Path) -> Option<u128> {
    let body = crate::info::read_bounded(path, crate::info::RECORD_CAP).ok()?;
    Some(fnv128(b"rtrash-record-v1", &body))
}

fn row_from(slot: u8, name: Vec<u8>, meta: &fs::Metadata, hash: u128) -> Row {
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
        hash,
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
    push_u128(out, row.hash);
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
fn push_u128(b: &mut Vec<u8>, v: u128) {
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
    let h = fnv128(b"rtrash-catalog-v1", root.as_os_str().as_bytes());
    format!("{h:032x}")
}

fn fnv128(prefix: &[u8], bytes: &[u8]) -> u128 {
    let mut h = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58du128;
    for byte in prefix.iter().chain(bytes) {
        h ^= u128::from(*byte);
        h = h.wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013B);
    }
    h
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
    let path = key_path()?;
    let parent = path.parent()?;
    if !dir_is_private(parent) {
        return None;
    }
    let (bytes, _) = read_private(&path)?;
    <[u8; KEY_LEN]>::try_from(bytes).ok()
}

fn load_or_create_key() -> Option<[u8; KEY_LEN]> {
    let path = key_path()?;
    let parent = path.parent()?;
    if !ensure_private_dir(parent) {
        return None;
    }
    if let Some(key) = read_key() {
        return Some(key);
    }
    // A symlink is removed as a link. A loose file of ours is replaced.
    // Someone else's file is left where it is, and there is no key.
    if path_exists(&path) && !discard_untrusted(&path) {
        return None;
    }
    create_key(&path)
}

fn path_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Remove a symlink or a regular file this user owns at `path`.
fn discard_untrusted(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = fs::symlink_metadata(path) else {
        return true;
    };
    let ours = meta.file_type().is_symlink() || (meta.is_file() && meta.uid() == our_uid());
    ours && fs::remove_file(path).is_ok()
}

fn create_key(path: &Path) -> Option<[u8; KEY_LEN]> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut key = [0u8; KEY_LEN];
    fill_random(&mut key).ok()?;
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true).mode(0o600);
    match opts.open(path) {
        Ok(mut f) => {
            let bad = f.write_all(&key).is_err()
                || f.sync_all().is_err()
                || f.set_permissions(mode(0o600)).is_err()
                || !f.metadata().is_ok_and(|m| private_file(&m));
            if bad {
                let _ = fs::remove_file(path);
                return None;
            }
            Some(key)
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => read_key(),
        Err(_) => None,
    }
}

/// `path` is this user's real directory, and group and other have no
/// permission on it. A symlink is not one. When `tighten` is set, a
/// directory we own is chmodded to 0700; a chmod that does not land is
/// a refusal.
fn ensure_private_dir(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => false,
        Ok(_) => tighten_dir(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .mode(0o700)
                .recursive(true)
                .create(path)
                .is_ok()
                && tighten_dir(path)
        }
        Err(_) => false,
    }
}

fn tighten_dir(path: &Path) -> bool {
    let Ok(file) = open_dir(path) else {
        return false;
    };
    if dir_file_private(&file) {
        return true;
    }
    file.set_permissions(mode(0o700)).is_ok() && dir_file_private(&file)
}

fn dir_is_private(path: &Path) -> bool {
    open_dir(path).is_ok_and(|f| dir_file_private(&f))
}

fn dir_file_private(file: &fs::File) -> bool {
    let Ok(meta) = file.metadata() else {
        return false;
    };
    meta.is_dir() && meta.uid() == our_uid() && meta.permissions().mode() & 0o077 == 0
}

fn private_blob(path: &Path, stamp: &Stamp) -> bool {
    let Ok(file) = open_nofollow(path) else {
        return false;
    };
    if file.set_permissions(mode(0o600)).is_err() {
        return false;
    }
    let Ok(meta) = file.metadata() else {
        return false;
    };
    private_file(&meta) && older_than_file(stamp, &meta)
}

fn read_private(path: &Path) -> Option<(Vec<u8>, fs::Metadata)> {
    let file = open_nofollow(path).ok()?;
    let meta = file.metadata().ok()?;
    if !private_file(&meta) {
        return None;
    }
    let mut raw = Vec::new();
    file.take(1 << 28).read_to_end(&mut raw).ok()?;
    Some((raw, meta))
}

fn private_file(meta: &fs::Metadata) -> bool {
    meta.file_type().is_file()
        && meta.uid() == our_uid()
        && meta.permissions().mode() & 0o077 == 0
        && meta.permissions().mode() & 0o400 != 0
}

fn our_uid() -> u32 {
    unsafe { libc::geteuid() as u32 }
}

fn open_dir(path: &Path) -> io::Result<fs::File> {
    open_flags(
        path,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
    )
}

fn open_nofollow(path: &Path) -> io::Result<fs::File> {
    open_flags(path, libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
}

fn open_flags(path: &Path, flags: i32) -> io::Result<fs::File> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "interior nul"))?;
    // SAFETY: `c` is NUL-terminated. O_NOFOLLOW applies to the last
    // component. The descriptor is owned by the returned file.
    let fd = unsafe { libc::open(c.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    Ok(fs::File::from(owned))
}

/// The sealed bodies are the bytes the stamp hashed. A blob whose record
/// text is not those bytes is not a catalog, even when the stamp's times
/// still match the files.
fn bodies_match(stamp: &[u8], saved: &Saved) -> bool {
    let Some(rows) = record_hashes(stamp) else {
        return false;
    };
    if rows.len() != saved.records.len() {
        return false;
    }
    let mut used = vec![false; saved.records.len()];
    for (name, hash) in &rows {
        let Some(i) = saved.records.iter().position(|r| r.file_name == *name) else {
            return false;
        };
        if used[i] || fnv128(b"rtrash-record-v1", &saved.records[i].body) != *hash {
            return false;
        }
        used[i] = true;
    }
    used.iter().all(|u| *u)
}

fn record_hashes(stamp: &[u8]) -> Option<Vec<(Vec<u8>, u128)>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < stamp.len() {
        let rest = stamp.get(i..)?;
        // slot, kind, then seven 8-byte fields, then a length-prefixed name
        // and the record hash. 58 is everything before the name's length.
        if rest.len() < 62 + 16 {
            return None;
        }
        let slot = rest[0];
        let name_len = u32::from_le_bytes(rest[58..62].try_into().ok()?) as usize;
        if name_len > NAME_CAP {
            return None;
        }
        let name_end = 62usize.checked_add(name_len)?;
        let hash_end = name_end.checked_add(16)?;
        if rest.len() < hash_end {
            return None;
        }
        if slot == 2 {
            let name = rest[62..name_end].to_vec();
            if is_record_name(&name) {
                let hash = u128::from_le_bytes(rest[name_end..hash_end].try_into().ok()?);
                out.push((name, hash));
            }
        }
        i += hash_end;
    }
    Some(out)
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

    /// Modification times two seconds back, past the coarse-clock margin.
    /// `utimensat` cannot move ctime; the margin is on mtime.
    fn backdate_tree(dir: &TrashDir) {
        let ago = std::time::SystemTime::now() - std::time::Duration::from_secs(2);
        let set = |p: &std::path::Path| {
            let f = fs::OpenOptions::new().read(true).open(p).unwrap();
            let times = fs::FileTimes::new().set_accessed(ago).set_modified(ago);
            f.set_times(times).unwrap();
        };
        for root in [dir.info(), dir.files()] {
            for ent in fs::read_dir(&root).unwrap().flatten() {
                set(&ent.path());
            }
            set(&root);
        }
    }

    fn same_meta(before: &fs::Metadata, after: &fs::Metadata) -> bool {
        use std::os::unix::fs::MetadataExt;
        before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
            && before.ino() == after.ino()
            && before.len() == after.len()
    }

    #[test]
    fn a_round_trip_is_sealed_and_a_rewrite_misses() {
        let iso = Isolated::open();
        write_record(&iso.dir, "hello.txt", "/tmp/hello.txt");
        backdate_tree(&iso.dir);
        let before = stamp(&iso.dir).expect("stamp");
        let saved = Saved {
            records: vec![Record {
                name: "hello.txt".into(),
                file_name: b"hello.txt.trashinfo".to_vec(),
                body: fs::read(iso.dir.info().join("hello.txt.trashinfo")).unwrap(),
            }],
            strays: 0,
        };
        store_if_unchanged(&iso.dir, &before, &saved);
        let blob = cache_path(&iso.dir).expect("blob path");
        assert!(
            older_than_file(&before, &fs::metadata(&blob).unwrap()),
            "a hit requires every record mtime to be older than the blob by the coarse-clock margin"
        );
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
        backdate_tree(&iso.dir);
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
        backdate_tree(&iso.dir);
        let first = crate::list::collect(std::slice::from_ref(&iso.dir));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].original, PathBuf::from("/tmp/catalog-before"));
        let notes = NOTES.with(|n| n.borrow().clone());
        assert_eq!(notes, ["scan"], "the first collect reads the records");
        let live = stamp(&iso.dir).expect("stamp");
        let blob = cache_path(&iso.dir).expect("blob path");
        assert!(
            older_than_file(&live, &fs::metadata(&blob).unwrap()),
            "a hit requires every record mtime to be older than the blob by the coarse-clock margin"
        );
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

    fn swapped_body(old: &[u8]) -> Vec<u8> {
        let from = b"/tmp/catalog-before";
        let to = b"/tmp/catalog-rewrit";
        assert_eq!(from.len(), to.len());
        let at = old
            .windows(from.len())
            .position(|w| w == from)
            .expect("path");
        let mut body = old.to_vec();
        body[at..at + from.len()].copy_from_slice(to);
        body
    }

    fn saved_old(body: Vec<u8>) -> Saved {
        Saved {
            records: vec![Record {
                name: "item.txt".into(),
                file_name: b"item.txt.trashinfo".to_vec(),
                body,
            }],
            strays: 0,
        }
    }

    fn assert_not_the_old_path(got: Option<Saved>) {
        if let Some(got) = got {
            let body = &got.records[0].body;
            assert!(
                !body
                    .windows(b"/tmp/catalog-before".len())
                    .any(|w| w == b"/tmp/catalog-before"),
                "stale catalog served the path from before the rewrite"
            );
        }
    }

    /// The gap the coarse clock hides: the record is rewritten in its own
    /// tick, then the blob is written on a later tick. The old path must
    /// not come back.
    #[test]
    fn a_same_length_rewrite_in_one_tick_is_not_served() {
        let iso = Isolated::open();
        let info = iso.dir.info().join("item.txt.trashinfo");
        let mut stuck = false;
        for _ in 0..100 {
            write_record(&iso.dir, "item.txt", "/tmp/catalog-before");
            let before_meta = fs::symlink_metadata(&info).unwrap();
            let before = stamp(&iso.dir).expect("stamp");
            let old = fs::read(&info).unwrap();
            fs::write(&info, swapped_body(&old)).unwrap();
            if !same_meta(&before_meta, &fs::symlink_metadata(&info).unwrap()) {
                continue;
            }
            stuck = true;
            // Past the margin, so a check on the times alone would keep a
            // blob written now. The record hash is what must refuse it.
            std::thread::sleep(std::time::Duration::from_millis(50));
            store_if_unchanged(&iso.dir, &before, &saved_old(old));
            assert_not_the_old_path(load(&iso.dir));
            let listed = crate::list::collect(std::slice::from_ref(&iso.dir));
            assert_eq!(listed[0].original, PathBuf::from("/tmp/catalog-rewrit"));
            break;
        }
        assert!(
            stuck,
            "a same-tick same-length rewrite never kept its stamp"
        );
    }

    /// Unlink plus create in the same coarse tick reuses the inode and
    /// leaves the directory stamp unchanged. The new record is what a
    /// later list shows.
    #[test]
    fn an_inode_reuse_in_one_tick_is_not_served() {
        let iso = Isolated::open();
        let info = iso.dir.info().join("item.txt.trashinfo");
        let mut stuck = false;
        for _ in 0..100 {
            write_record(&iso.dir, "item.txt", "/tmp/catalog-before");
            let before = stamp(&iso.dir).expect("stamp");
            let old = fs::read(&info).unwrap();
            let file_meta = fs::symlink_metadata(&info).unwrap();
            let dir_meta = fs::symlink_metadata(iso.dir.info()).unwrap();
            fs::remove_file(&info).unwrap();
            fs::write(&info, swapped_body(&old)).unwrap();
            let file_after = fs::symlink_metadata(&info).unwrap();
            let dir_after = fs::symlink_metadata(iso.dir.info()).unwrap();
            if !same_meta(&file_meta, &file_after) || !same_meta(&dir_meta, &dir_after) {
                continue;
            }
            stuck = true;
            // Past the margin, so a check on the times alone would keep a
            // blob written now. The record hash is what must refuse it.
            std::thread::sleep(std::time::Duration::from_millis(50));
            store_if_unchanged(&iso.dir, &before, &saved_old(old));
            assert_not_the_old_path(load(&iso.dir));
            let listed = crate::list::collect(std::slice::from_ref(&iso.dir));
            assert_eq!(listed[0].original, PathBuf::from("/tmp/catalog-rewrit"));
            break;
        }
        assert!(
            stuck,
            "unlink plus create in one tick never reused the inode"
        );
    }

    fn seal_hello(iso: &Isolated) {
        let before = stamp(&iso.dir).unwrap();
        let body = fs::read(iso.dir.info().join("hello.txt.trashinfo")).unwrap();
        store_if_unchanged(
            &iso.dir,
            &before,
            &Saved {
                records: vec![Record {
                    name: "hello.txt".into(),
                    file_name: b"hello.txt.trashinfo".to_vec(),
                    body,
                }],
                strays: 0,
            },
        );
    }

    #[test]
    fn a_symlink_key_is_not_followed() {
        let iso = Isolated::open();
        write_record(&iso.dir, "hello.txt", "/tmp/hello.txt");
        backdate_tree(&iso.dir);
        let target = iso.root.join("other-key");
        fs::write(&target, [9u8; 32]).unwrap();
        fs::set_permissions(&target, mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&target, key_path().unwrap()).unwrap();
        seal_hello(&iso);
        assert_eq!(
            fs::read(&target).unwrap(),
            vec![9u8; 32],
            "opening the key followed the symlink"
        );
        assert!(
            fs::symlink_metadata(key_path().unwrap())
                .unwrap()
                .file_type()
                .is_file(),
            "the key path is still a symlink"
        );
    }

    #[test]
    fn a_group_readable_key_is_not_used() {
        let iso = Isolated::open();
        write_record(&iso.dir, "hello.txt", "/tmp/hello.txt");
        backdate_tree(&iso.dir);
        seal_hello(&iso);
        assert!(load(&iso.dir).is_some());
        fs::set_permissions(key_path().unwrap(), mode(0o644)).unwrap();
        assert!(
            load(&iso.dir).is_none(),
            "a key other users can read is used"
        );
        let mode = fs::symlink_metadata(key_path().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o644, "reading the catalog replaced the loose key");
    }

    #[test]
    fn a_symlink_blob_is_not_a_catalog() {
        let iso = Isolated::open();
        write_record(&iso.dir, "hello.txt", "/tmp/hello.txt");
        backdate_tree(&iso.dir);
        seal_hello(&iso);
        let blob = cache_path(&iso.dir).unwrap();
        let side = iso.root.join("blob-copy");
        fs::copy(&blob, &side).unwrap();
        fs::set_permissions(&side, mode(0o600)).unwrap();
        let bytes = fs::read(&side).unwrap();
        fs::remove_file(&blob).unwrap();
        std::os::unix::fs::symlink(&side, &blob).unwrap();
        assert!(load(&iso.dir).is_none(), "a symlink blob was followed");
        assert_eq!(fs::read(&side).unwrap(), bytes);
    }

    #[test]
    fn a_symlink_cache_directory_is_not_written_through() {
        let iso = Isolated::open();
        write_record(&iso.dir, "hello.txt", "/tmp/hello.txt");
        backdate_tree(&iso.dir);
        let cache = iso.root.join("cache");
        fs::remove_dir_all(&cache).unwrap();
        let other = iso.root.join("other-cache");
        fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(&other, &cache).unwrap();
        seal_hello(&iso);
        assert!(
            fs::read_dir(&other).unwrap().next().is_none(),
            "the catalog was written through the directory symlink"
        );
    }

    #[test]
    fn a_loose_cache_directory_is_not_read_until_it_is_private() {
        let iso = Isolated::open();
        write_record(&iso.dir, "hello.txt", "/tmp/hello.txt");
        backdate_tree(&iso.dir);
        seal_hello(&iso);
        assert!(load(&iso.dir).is_some());
        let cache = iso.root.join("cache");
        fs::set_permissions(&cache, mode(0o755)).unwrap();
        assert!(
            load(&iso.dir).is_none(),
            "a cache directory other users can read was used"
        );
        seal_hello(&iso);
        let bits = fs::symlink_metadata(&cache).unwrap().permissions().mode() & 0o777;
        assert_eq!(bits, 0o700, "storing left the directory readable by others");
        assert!(load(&iso.dir).is_some());
    }

    fn newest_mtime(dir: &TrashDir) -> std::time::SystemTime {
        let mut newest = std::time::SystemTime::UNIX_EPOCH;
        for root in [dir.info(), dir.files()] {
            let seen = fs::metadata(&root).unwrap().modified().unwrap();
            if seen > newest {
                newest = seen;
            }
            for ent in fs::read_dir(&root).unwrap().flatten() {
                let seen = ent.metadata().unwrap().modified().unwrap();
                if seen > newest {
                    newest = seen;
                }
            }
        }
        newest
    }

    fn set_mtime(path: &std::path::Path, modified: std::time::SystemTime) {
        let f = fs::OpenOptions::new().read(true).open(path).unwrap();
        let times = fs::FileTimes::new()
            .set_accessed(modified)
            .set_modified(modified);
        f.set_times(times).unwrap();
    }

    fn seal_item(iso: &Isolated) {
        let before = stamp(&iso.dir).unwrap();
        let body = fs::read(iso.dir.info().join("item.txt.trashinfo")).unwrap();
        store_if_unchanged(
            &iso.dir,
            &before,
            &Saved {
                records: vec![Record {
                    name: "item.txt".into(),
                    file_name: b"item.txt.trashinfo".to_vec(),
                    body,
                }],
                strays: 0,
            },
        );
        assert!(load(&iso.dir).is_some(), "the sealed catalog should hit");
    }

    /// The time gate on its own. The stamp bytes still match; only the
    /// blob's mtime is pulled back onto the records' tick.
    #[test]
    fn a_blob_within_the_margin_is_not_a_hit() {
        let iso = Isolated::open();
        write_record(&iso.dir, "item.txt", "/tmp/catalog-before");
        backdate_tree(&iso.dir);
        seal_item(&iso);
        let blob = cache_path(&iso.dir).unwrap();
        set_mtime(&blob, newest_mtime(&iso.dir));
        assert!(
            load(&iso.dir).is_none(),
            "a blob that is not older than the records by the margin was a hit"
        );
    }

    /// The stamp compare on its own. Mtimes are put back, so the time gate
    /// would allow a hit; the new inode and ctime are only in the stamp.
    #[test]
    fn a_replaced_payload_with_its_mtime_put_back_is_not_a_hit() {
        let iso = Isolated::open();
        write_record(&iso.dir, "item.txt", "/tmp/catalog-before");
        backdate_tree(&iso.dir);
        seal_item(&iso);
        let payload = iso.dir.files().join("item.txt");
        let info = iso.dir.info();
        let files = iso.dir.files();
        let payload_m = fs::metadata(&payload).unwrap().modified().unwrap();
        let info_m = fs::metadata(&info).unwrap().modified().unwrap();
        let files_m = fs::metadata(&files).unwrap().modified().unwrap();
        let bytes = fs::read(&payload).unwrap();
        fs::remove_file(&payload).unwrap();
        fs::write(&payload, &bytes).unwrap();
        set_mtime(&payload, payload_m);
        set_mtime(&info, info_m);
        set_mtime(&files, files_m);
        assert!(
            load(&iso.dir).is_none(),
            "a payload replaced under the same mtime was served from the catalog"
        );
    }

    /// `backdate_tree` follows links. The link's own mtime has to move too,
    /// or the time gate refuses a catalog the link check was supposed to.
    fn backdate_link(path: &std::path::Path) {
        let ago = std::time::SystemTime::now() - std::time::Duration::from_secs(2);
        let dur = ago.duration_since(std::time::UNIX_EPOCH).unwrap();
        let ts = libc::timespec {
            tv_sec: dur.as_secs() as _,
            tv_nsec: dur.subsec_nanos() as _,
        };
        let times = [ts, ts];
        let bytes = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str());
        let c = CString::new(bytes).unwrap();
        // SAFETY: `c` is NUL-terminated and `times` is two const timespecs.
        // AT_SYMLINK_NOFOLLOW sets the link itself, not the target.
        let rc = unsafe {
            libc::utimensat(
                libc::AT_FDCWD,
                c.as_ptr(),
                times.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        assert_eq!(rc, 0, "{}", io::Error::last_os_error());
    }

    #[test]
    fn a_symlink_record_never_seals_a_catalog() {
        let iso = Isolated::open();
        write_record(&iso.dir, "hello.txt", "/tmp/hello.txt");
        let link = iso.dir.info().join("link.trashinfo");
        std::os::unix::fs::symlink("hello.txt.trashinfo", &link).unwrap();
        backdate_tree(&iso.dir);
        backdate_link(&link);
        let _ = crate::list::collect(std::slice::from_ref(&iso.dir));
        assert!(
            !cache_path(&iso.dir).unwrap().is_file(),
            "a symlink record was sealed"
        );
        NOTES.with(|n| n.borrow_mut().clear());
        let _ = crate::list::collect(std::slice::from_ref(&iso.dir));
        let notes = NOTES.with(|n| n.borrow().clone());
        assert_eq!(notes, ["scan"], "a symlink record must not hit");
    }

    #[test]
    fn an_unreadable_record_never_seals_a_catalog() {
        let iso = Isolated::open();
        write_record(&iso.dir, "hello.txt", "/tmp/hello.txt");
        let info = iso.dir.info().join("hello.txt.trashinfo");
        fs::set_permissions(&info, mode(0o000)).unwrap();
        assert!(
            stamp(&iso.dir).is_none(),
            "an unreadable record still produced a stamp"
        );
        let _ = crate::list::collect(std::slice::from_ref(&iso.dir));
        assert!(
            !cache_path(&iso.dir).unwrap().is_file(),
            "an unreadable record was sealed"
        );
        NOTES.with(|n| n.borrow_mut().clear());
        let _ = crate::list::collect(std::slice::from_ref(&iso.dir));
        let notes = NOTES.with(|n| n.borrow().clone());
        assert_eq!(notes, ["scan"], "an unreadable record must not hit");
    }
}
