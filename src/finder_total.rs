//! Running totals and history for the Finder trashes, behind
//! `trash-stats --total` and `--history` on macOS.
//!
//! Finder shows every file in a trash, so nothing goes inside one. Each
//! trash gets `KEY.total` and `KEY.history` under
//! `$XDG_STATE_HOME/rtrash/finder-totals`, KEY being the trash path with
//! `%` and `/` percent-encoded, or for a trash on another volume
//! `uuid-UUID-PATH` with PATH below the volume's mount point. A total (`v2 BYTES ITEMS INODES STAMP`; `v1` files lack INODES) carries
//! a fingerprint of the names in the trash (`.DS_Store` left out), which
//! changes whenever Finder or rtrash adds, removes or renames an item, so a total that another tool
//! made out of date reads as stale.
//!
//! A command that may change a trash takes a [`Snapshot`] of the trash
//! times before it and calls [`log`] after it: each trash whose time moved
//! gets a new total and a history line. Put, rm, empty, restore and undo
//! report each item they add or remove with [`carry`]; when the total was
//! current before the command, the new one is the old plus those changes,
//! with no walk; the inodes of each item (itself and everything below it)
//! carry the same way. A failed removal or a trash without a current total
//! measures the trash once.

use std::fs;
use std::path::{Path, PathBuf};

fn state_dir() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".local/state"))
        })?;
    Some(state.join("rtrash").join("finder-totals"))
}

fn enc(p: &Path) -> String {
    p.to_string_lossy().replace('%', "%25").replace('/', "%2F")
}

/// The file name stem for `trash`'s total and history: the encoded path
/// for the home trash, and for a trash on another volume the volume's
/// UUID plus the path below its mount point, so a drive mounted again
/// at another place keeps its total. Without a UUID, the path.
fn key_with(trash: &Path, vol: Option<(&str, &Path)>) -> String {
    match vol.and_then(|(u, m)| Some((u, trash.strip_prefix(m).ok()?))) {
        Some((uuid, rel)) => format!("uuid-{uuid}-{}", enc(rel)),
        None => enc(trash),
    }
}

fn key(trash: &Path) -> String {
    if crate::finder::home_trash().as_deref() == Some(trash) {
        return enc(trash);
    }
    let mount = mount_point(trash);
    let uuid = mount.as_deref().and_then(volume_uuid);
    key_with(trash, uuid.as_deref().zip(mount.as_deref()))
}

/// The top of the file system `p` is on: the last ancestor on the same
/// device.
fn mount_point(p: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let dev = fs::metadata(p).ok()?.dev();
    let mut top = p.to_path_buf();
    while let Some(up) = top.parent() {
        if fs::metadata(up).ok()?.dev() != dev {
            break;
        }
        top = up.to_path_buf();
    }
    Some(top)
}

/// macOS: the volume UUID of the file system mounted at `mount`.
#[cfg(target_os = "macos")]
fn volume_uuid(mount: &Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(mount.as_os_str().as_bytes()).ok()?;
    let mut req = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_UUID,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    // A u32 length, then the 16-byte UUID.
    let mut buf = [0u8; 4 + 16];
    // SAFETY: `c` is a valid C string, `req` a valid attrlist and `buf`
    // a writable buffer of the size given.
    let r = unsafe {
        libc::getattrlist(
            c.as_ptr(),
            (&mut req as *mut libc::attrlist).cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            0,
        )
    };
    let id = &buf[4..];
    (r == 0 && id.iter().any(|&b| b != 0)).then(|| id.iter().map(|b| format!("{b:02x}")).collect())
}

/// Linux: the file system UUID udev lists for the device `mount` is on.
#[cfg(target_os = "linux")]
fn volume_uuid(mount: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let dev = fs::metadata(mount).ok()?.dev();
    fs::read_dir("/dev/disk/by-uuid")
        .ok()?
        .flatten()
        .find(|e| fs::metadata(e.path()).is_ok_and(|m| m.rdev() == dev))
        .map(|e| e.file_name().to_string_lossy().into_owned())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn volume_uuid(_mount: &Path) -> Option<String> {
    None
}

fn file(trash: &Path, ext: &str) -> Option<PathBuf> {
    Some(state_dir()?.join(format!("{}.{ext}", key(trash))))
}

/// Finder's own record file, which it rewrites at will.
const DS_STORE: &str = ".DS_Store";

/// A fingerprint of the names in the trash, `.DS_Store` left out: any
/// item added, removed or renamed changes it, while Finder rewriting
/// `.DS_Store` (which moves the directory's time) does not. One readdir,
/// no stat.
fn stamp(trash: &Path) -> Option<u128> {
    let mut names: Vec<std::ffi::OsString> = fs::read_dir(trash)
        .ok()?
        .flatten()
        .map(|e| e.file_name())
        .filter(|n| n != DS_STORE)
        .collect();
    names.sort();
    // FNV-1a, 128 bits.
    let mut h: u128 = 0x6c62272e07bb014262b821756295c58d;
    for n in &names {
        for b in n.as_encoded_bytes().iter().chain([&0u8]) {
            h ^= u128::from(*b);
            h = h.wrapping_mul(0x0000000001000000000000000000013B);
        }
    }
    Some(h ^ names.len() as u128)
}

/// Inodes in the trash as the totals count them: `.DS_Store` left out.
fn trash_inodes(trash: &Path) -> u64 {
    let ds = u64::from(trash.join(DS_STORE).symlink_metadata().is_ok());
    crate::trash_stats::count_inodes(trash).saturating_sub(ds)
}

/// A stored total: bytes, items, and inodes (files and directories in
/// the trash, as `trash-stats` counts them) when known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Total {
    pub bytes: u64,
    pub items: u64,
    pub inodes: Option<u64>,
}

fn parse(body: &str) -> Option<(Total, u128)> {
    let mut f = body.split_whitespace();
    let v = f.next()?;
    let bytes = f.next()?.parse().ok()?;
    let items = f.next()?.parse().ok()?;
    let inodes = match v {
        "v1" => None,
        "v2" => Some(f.next()?.parse().ok()?),
        _ => return None,
    };
    let s = f.next()?.parse().ok()?;
    Some((
        Total {
            bytes,
            items,
            inodes,
        },
        s,
    ))
}

/// The stored total of `trash`, with whether it is current. Reads one
/// small file and never walks the trash.
pub fn stored_total(trash: &Path) -> Option<(Total, bool)> {
    let (t, s) = parse(&fs::read_to_string(file(trash, "total")?).ok()?)?;
    Some((t, stamp(trash) == Some(s)))
}

/// [`stored_total`] as bytes, items and whether current.
pub fn stored(trash: &Path) -> Option<(u64, u64, bool)> {
    stored_total(trash).map(|(t, c)| (t.bytes, t.items, c))
}

/// Walk `trash` once (its items and inodes, as `trash-stats` counts
/// them) and store the total. Best effort.
pub fn measure(trash: &Path) -> Total {
    let before = stamp(trash);
    let (bytes, items) = crate::finder_shared::size_of(trash);
    let t = Total {
        bytes,
        items,
        inodes: Some(trash_inodes(trash)),
    };
    // A change during the walk leaves the total stale.
    if before.is_some() && stamp(trash) == before {
        store(trash, &t);
    }
    t
}

fn store(trash: &Path, t: &Total) {
    if let (Some(s), Some(p)) = (stamp(trash), file(trash, "total")) {
        if let Some(d) = p.parent() {
            let _ = fs::create_dir_all(d);
        }
        let line = match t.inodes {
            Some(i) => format!("v2 {} {} {i} {s}\n", t.bytes, t.items),
            None => format!("v1 {} {} {s}\n", t.bytes, t.items),
        };
        let _ = crate::trashdir::atomic_write(&p, line.as_bytes());
    }
}

/// The history of `trash`, oldest first.
pub fn history(trash: &Path) -> Vec<crate::trash_history::Point> {
    file(trash, "history")
        .map(|p| crate::trash_history::read(&p))
        .unwrap_or_default()
}

/// What one command changed in one trash: bytes, items and inodes added
/// (negative: removed), and whether every change was carried.
#[derive(Debug, Clone)]
struct Change {
    trash: PathBuf,
    bytes: i128,
    items: i64,
    inodes: i128,
    whole: bool,
}

static CARRIED: std::sync::Mutex<Vec<Change>> = std::sync::Mutex::new(Vec::new());

fn note(trash: &Path, bytes: i128, items: i64, inodes: i128, whole: bool) {
    let mut c = CARRIED.lock().unwrap_or_else(|e| e.into_inner());
    match c.iter_mut().find(|x| x.trash == trash) {
        Some(x) => {
            x.bytes += bytes;
            x.items += items;
            x.inodes += inodes;
            x.whole &= whole;
        }
        None => c.push(Change {
            trash: trash.to_path_buf(),
            bytes,
            items,
            inodes,
            whole,
        }),
    }
}

/// The size on disk and inodes of an item about to move, for [`carry`].
pub fn measure_item(path: &Path) -> (u64, u64) {
    (
        crate::fastdelete::disk_usage(path),
        1 + crate::trash_stats::count_inodes(path),
    )
}

/// Report one item this command put into `trash` (`added`) or removed
/// from it, with its size and inodes from [`measure_item`].
pub fn carry(trash: &Path, (bytes, inodes): (u64, u64), added: bool) {
    let sign = if added { 1 } else { -1 };
    note(
        trash,
        sign * i128::from(bytes),
        sign as i64,
        sign * i128::from(inodes),
        true,
    );
}

/// Report a change to `trash` that carries no sizes, so [`log`]
/// measures it.
pub fn uncarried(trash: &Path) {
    note(trash, 0, 0, 0, false);
}

/// The times of the Finder trashes before a command, each with its total
/// when that was current; None where the Finder Trash is not the
/// default, so the command logs nothing.
pub struct Snapshot(Option<Vec<SnapRow>>);

/// A trash, its stamp and its total when current.
type SnapRow = (PathBuf, Option<u128>, Option<Total>);

pub fn snapshot() -> Snapshot {
    if !crate::finder::is_default(false) {
        return Snapshot(None);
    }
    snapshot_always()
}

/// [`snapshot`] whatever the default trash is, for `rtrash finder`.
pub fn snapshot_always() -> Snapshot {
    Snapshot(Some(
        crate::finder::trashes()
            .into_iter()
            .map(|t| {
                let s = stamp(&t);
                let cur = stored_total(&t).filter(|x| x.1).map(|x| x.0);
                (t, s, cur)
            })
            .collect(),
    ))
}

fn add(v: u64, d: i128) -> u64 {
    (i128::from(v) + d).clamp(0, i128::from(u64::MAX)) as u64
}

/// The total after `c`, from the total before it.
fn carried_total(old: Total, c: &Change) -> Total {
    Total {
        bytes: add(old.bytes, c.bytes),
        items: add(old.items, i128::from(c.items)),
        inodes: old.inodes.map(|i| add(i, c.inodes)),
    }
}

/// After a command: give each trash whose time moved since `before` a
/// new total (carried when it can be, else measured) and a history line
/// under `event`.
pub fn log(event: &str, before: &Snapshot) {
    let Some(before) = &before.0 else {
        return;
    };
    let carried = std::mem::take(&mut *CARRIED.lock().unwrap_or_else(|e| e.into_inner()));
    for t in crate::finder::trashes() {
        let now = stamp(&t);
        let snap = before.iter().find(|x| x.0 == t);
        let was = snap.and_then(|x| x.1);
        if now.is_none() || now == was {
            continue;
        }
        let old = snap.and_then(|x| x.2);
        let c = carried.iter().find(|x| x.trash == t && x.whole);
        let total = match (old, c) {
            (Some(old), Some(c)) => {
                let n = carried_total(old, c);
                store(&t, &n);
                n
            }
            _ => measure(&t),
        };
        if let Some(p) = file(&t, "history") {
            crate::trash_history::append(&p, event, total.bytes, total.items);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_encode_paths_and_volume_uuids() {
        assert_eq!(
            key_with(Path::new("/Users/a/.Trash"), None),
            "%2FUsers%2Fa%2F.Trash"
        );
        assert_eq!(
            key_with(Path::new("/V/50%/.Trashes/501"), None),
            "%2FV%2F50%25%2F.Trashes%2F501"
        );
        // The same drive at two mount points gives one key.
        let u = "0a1b";
        assert_eq!(
            key_with(
                Path::new("/Volumes/USB/.Trashes/501"),
                Some((u, Path::new("/Volumes/USB")))
            ),
            "uuid-0a1b-.Trashes%2F501"
        );
        assert_eq!(
            key_with(
                Path::new("/Volumes/USB 1/.Trashes/501"),
                Some((u, Path::new("/Volumes/USB 1")))
            ),
            "uuid-0a1b-.Trashes%2F501"
        );
        // A trash outside the mount point keeps its path.
        assert_eq!(
            key_with(Path::new("/x/.Trashes/501"), Some((u, Path::new("/y")))),
            "%2Fx%2F.Trashes%2F501"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mount_point_climbs_to_the_file_system_top() {
        let m = mount_point(Path::new("/proc/self")).unwrap();
        assert_eq!(m, Path::new("/proc"));
    }

    #[test]
    fn totals_parse_both_versions_and_carry_changes() {
        let t = |b, n, i| Total {
            bytes: b,
            items: n,
            inodes: i,
        };
        assert_eq!(parse("v1 10 2 99"), Some((t(10, 2, None), 99)));
        assert_eq!(parse("v2 10 2 7 99"), Some((t(10, 2, Some(7)), 99)));
        assert_eq!(parse("v0 10 2 99"), None);
        let c = Change {
            trash: PathBuf::from("/t"),
            bytes: -4,
            items: -1,
            inodes: -3,
            whole: true,
        };
        assert_eq!(carried_total(t(10, 2, Some(7)), &c), t(6, 1, Some(4)));
        assert_eq!(carried_total(t(1, 0, None), &c), t(0, 0, None));
    }
}
