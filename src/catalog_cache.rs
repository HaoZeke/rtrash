//! Compact sealed per-root catalog so `list` / HUD skip a full `.trashinfo` walk.
//!
//! The blob lives under `$XDG_CACHE_HOME/rtrash/`. The 32-byte key is
//! `$XDG_DATA_HOME/rtrash/cache.key` (mode 0600). This is not a git vault:
//! decrypt is one XChaCha20-Poly1305 open, not GPG.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

use crate::list::Entry;
use crate::trashdir::TrashDir;

const MAGIC: &[u8; 4] = b"RTR1";
const NONCE_LEN: usize = 24;
const KEY_LEN: usize = 32;
const ENV_DISABLE: &str = "RTRASH_NO_CACHE";

/// True when the operator asked to skip the cache.
pub fn disabled() -> bool {
    matches!(
        std::env::var(ENV_DISABLE).as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// Load a cached catalog if the root's `info/` + `files/` fingerprint still matches.
pub fn load(dir: &TrashDir) -> Option<Vec<Entry>> {
    if disabled() {
        return None;
    }
    let fp = fingerprint(dir)?;
    let raw = fs::read(cache_path(dir)).ok()?;
    if raw.len() < MAGIC.len() + NONCE_LEN + 16 {
        return None;
    }
    if &raw[..4] != MAGIC {
        return None;
    }
    let nonce = XNonce::from_slice(&raw[4..4 + NONCE_LEN]);
    let cipher = XChaCha20Poly1305::new_from_slice(&load_or_create_key()?).ok()?;
    let plain = cipher.decrypt(nonce, &raw[4 + NONCE_LEN..]).ok()?;
    decode(dir, &fp, &plain)
}

/// Write the compact sealed blob for this root.
pub fn store(dir: &TrashDir, entries: &[Entry]) {
    if disabled() {
        return;
    }
    let Some(fp) = fingerprint(dir) else {
        return;
    };
    let Some(key) = load_or_create_key() else {
        return;
    };
    let Ok(plain) = encode(&fp, entries) else {
        return;
    };
    let mut nonce_bytes = [0u8; NONCE_LEN];
    if fill_random(&mut nonce_bytes).is_err() {
        return;
    }
    let nonce = XNonce::from_slice(&nonce_bytes);
    let Ok(cipher) = XChaCha20Poly1305::new_from_slice(&key) else {
        return;
    };
    let Ok(ct) = cipher.encrypt(nonce, plain.as_ref()) else {
        return;
    };
    let path = cache_path(dir);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("tmp");
    let mut out = Vec::with_capacity(4 + NONCE_LEN + ct.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    if fs::write(&tmp, &out).is_ok() {
        let _ = fs::rename(&tmp, path);
    } else {
        let _ = fs::remove_file(&tmp);
    }
}

/// Drop the sealed blob so the next collect walks.
pub fn invalidate(dir: &TrashDir) {
    let _ = fs::remove_file(cache_path(dir));
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Finger {
    info_m: i64,
    info_n: i64,
    files_m: i64,
    files_n: i64,
    info_count: u32,
    files_count: u32,
}

fn fingerprint(dir: &TrashDir) -> Option<Finger> {
    let info = fs::metadata(dir.info()).ok()?;
    let files = fs::metadata(dir.files()).ok()?;
    Some(Finger {
        info_m: info.mtime(),
        info_n: info.mtime_nsec(),
        files_m: files.mtime(),
        files_n: files.mtime_nsec(),
        info_count: 0,
        files_count: 0,
    })
}

fn encode(fp: &Finger, entries: &[Entry]) -> io::Result<Vec<u8>> {
    let mut b = Vec::new();
    write_i64(&mut b, fp.info_m);
    write_i64(&mut b, fp.info_n);
    write_i64(&mut b, fp.files_m);
    write_i64(&mut b, fp.files_n);
    write_u32(&mut b, fp.info_count);
    write_u32(&mut b, fp.files_count);
    write_u32(&mut b, entries.len() as u32);
    for e in entries {
        b.push(u8::from(e.orphan));
        write_i64(&mut b, e.epoch);
        write_str(&mut b, &e.name)?;
        write_str(&mut b, &e.original.to_string_lossy())?;
        write_str(&mut b, e.date.as_deref().unwrap_or(""))?;
    }
    Ok(b)
}

fn decode(dir: &TrashDir, live: &Finger, plain: &[u8]) -> Option<Vec<Entry>> {
    let mut i = 0;
    let stored = Finger {
        info_m: read_i64(plain, &mut i)?,
        info_n: read_i64(plain, &mut i)?,
        files_m: read_i64(plain, &mut i)?,
        files_n: read_i64(plain, &mut i)?,
        info_count: read_u32(plain, &mut i)?,
        files_count: read_u32(plain, &mut i)?,
    };
    if stored != *live {
        return None;
    }
    let n = read_u32(plain, &mut i)? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let orphan = *plain.get(i)? != 0;
        i += 1;
        let epoch = read_i64(plain, &mut i)?;
        let name = read_str(plain, &mut i)?;
        let original = PathBuf::from(read_str(plain, &mut i)?);
        let date_raw = read_str(plain, &mut i)?;
        let date = if date_raw.is_empty() {
            None
        } else {
            Some(date_raw)
        };
        out.push(Entry {
            date,
            epoch,
            original,
            name,
            dir: dir.clone(),
            orphan,
        });
    }
    Some(out)
}

fn write_i64(b: &mut Vec<u8>, v: i64) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn write_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn write_str(b: &mut Vec<u8>, s: &str) -> io::Result<()> {
    let bytes = s.as_bytes();
    if bytes.len() > u16::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "catalog string too long",
        ));
    }
    b.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    b.extend_from_slice(bytes);
    Ok(())
}
fn read_i64(b: &[u8], i: &mut usize) -> Option<i64> {
    let s = b.get(*i..*i + 8)?;
    *i += 8;
    Some(i64::from_le_bytes(s.try_into().ok()?))
}
fn read_u32(b: &[u8], i: &mut usize) -> Option<u32> {
    let s = b.get(*i..*i + 4)?;
    *i += 4;
    Some(u32::from_le_bytes(s.try_into().ok()?))
}
fn read_str(b: &[u8], i: &mut usize) -> Option<String> {
    let slen = b.get(*i..*i + 2)?;
    *i += 2;
    let n = u16::from_le_bytes(slen.try_into().ok()?) as usize;
    let s = b.get(*i..*i + n)?;
    *i += n;
    String::from_utf8(s.to_vec()).ok()
}

fn cache_path(dir: &TrashDir) -> PathBuf {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    dir.root.hash(&mut h);
    let name = format!("c-{:016x}.bin", h.finish());
    cache_dir().join(name)
}

fn cache_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("rtrash");
        }
    }
    dirs_fallback_home().join(".cache/rtrash")
}

fn key_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("rtrash/cache.key");
        }
    }
    dirs_fallback_home().join(".local/share/rtrash/cache.key")
}

fn dirs_fallback_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn load_or_create_key() -> Option<[u8; KEY_LEN]> {
    let path = key_path();
    if let Ok(mut f) = fs::File::open(&path) {
        let mut key = [0u8; KEY_LEN];
        if f.read_exact(&mut key).is_ok() {
            return Some(key);
        }
    }
    let mut key = [0u8; KEY_LEN];
    fill_random(&mut key).ok()?;
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true).mode(0o600);
    match opts.open(&path) {
        Ok(mut f) => {
            if f.write_all(&key).is_err() {
                let _ = fs::remove_file(&path);
                return None;
            }
            Some(key)
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let mut f = fs::File::open(&path).ok()?;
            let mut key = [0u8; KEY_LEN];
            f.read_exact(&mut key).ok()?;
            Some(key)
        }
        Err(_) => None,
    }
}

fn fill_random(buf: &mut [u8]) -> io::Result<()> {
    let mut f = fs::File::open("/dev/urandom")?;
    f.read_exact(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(1);
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn isolated_dir() -> (PathBuf, TrashDir, std::sync::MutexGuard<'static, ()>) {
        let _guard = ENV.lock().expect("env lock");
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "rtrash-cache-{}-{}",
            std::process::id(),
            n
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("files")).unwrap();
        fs::create_dir_all(root.join("info")).unwrap();
        // Isolated from the host cache. 1.77 allows set_var; 1.98 wants unsafe.
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", root.join("xdg-cache"));
            std::env::set_var("XDG_DATA_HOME", root.join("xdg-data"));
        }
        (
            root.clone(),
            TrashDir {
                root,
                topdir: None,
            },
            _guard,
        )
    }

    #[test]
    fn store_then_load_roundtrip() {
        let (root, dir, _env) = isolated_dir();
        let entries = vec![Entry {
            date: Some("2026-01-02T03:04:05".into()),
            epoch: 1_767_000_000,
            original: PathBuf::from("/tmp/hello.txt"),
            name: "hello.txt".into(),
            dir: dir.clone(),
            orphan: false,
        }];
        store(&dir, &entries);
        let loaded = load(&dir).expect("cache hit");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "hello.txt");
        assert_eq!(loaded[0].original, PathBuf::from("/tmp/hello.txt"));
        assert_eq!(loaded[0].epoch, 1_767_000_000);
        let blob = fs::read(cache_path(&dir)).unwrap();
        let text = String::from_utf8_lossy(&blob);
        assert!(
            !text.contains("/tmp/hello.txt"),
            "sealed blob must not contain Path= plaintext"
        );
        let key = key_path();
        let mode = fs::metadata(&key).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);
        invalidate(&dir);
        assert!(load(&dir).is_none());
        let _ = fs::remove_dir_all(root);
    }
}
