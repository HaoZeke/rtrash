//! `rtrash du --devices`: size, use and free space of each mounted file
//! system, as `df` and `gdu -d` show them.
//!
//! Like `df`, the table leaves out pseudo file systems, file systems with
//! no blocks, and second mounts of a device already listed (bind mounts).

use std::io;
use std::path::PathBuf;

/// One mounted file system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// What is mounted: a device node, a volume or a label.
    pub source: String,
    pub fstype: String,
    pub mount: PathBuf,
    /// Bytes in the file system.
    pub size: u64,
    /// Bytes in use (size less every free byte).
    pub used: u64,
    /// Bytes free to an ordinary user.
    pub avail: u64,
}

impl Device {
    /// Use as a whole percent, rounded up as `df` rounds it: used over
    /// used plus what a user may still fill.
    pub fn percent(&self) -> u64 {
        let room = self.used.saturating_add(self.avail);
        if room == 0 {
            return 0;
        }
        (self.used as u128 * 100).div_ceil(room as u128) as u64
    }
}

/// A mount table line: source, mount point and type.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub source: String,
    pub mount: PathBuf,
    pub fstype: String,
}

/// The mounts in a `/proc/self/mounts` table that are not pseudo file
/// systems, in table order, each mount point once (the last mount on it,
/// the one in view).
#[cfg(unix)]
pub fn mounts_from_table(content: &str) -> Vec<Mount> {
    let mut out: Vec<Mount> = Vec::new();
    for line in content.lines() {
        let mut f = line.split_whitespace();
        let (Some(src), Some(mp), Some(fstype)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        if crate::trashdir::is_pseudo_fs(fstype) {
            continue;
        }
        let m = Mount {
            source: crate::trashdir::unescape_mount(src)
                .to_string_lossy()
                .into_owned(),
            mount: crate::trashdir::unescape_mount(mp),
            fstype: fstype.to_string(),
        };
        // A later mount on the same point hides the earlier one.
        match out.iter_mut().find(|o| o.mount == m.mount) {
            Some(o) => *o = m,
            None => out.push(m),
        }
    }
    out
}

/// Size, use and free space of the file system at `mount`, and its
/// device number (to drop bind mounts). Linux and macOS read a mount
/// table for [`measure`]; other Unix systems list no devices.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn space(mount: &std::path::Path) -> io::Result<(u64, u64, u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let dev = std::fs::metadata(mount)?.dev();
    let c = std::ffi::CString::new(mount.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is NUL-terminated and `s` is a plain C struct.
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let unit = if s.f_frsize > 0 {
        s.f_frsize as u64
    } else {
        s.f_bsize as u64
    };
    let size = (s.f_blocks as u64).saturating_mul(unit);
    let free = (s.f_bfree as u64).saturating_mul(unit);
    let avail = (s.f_bavail as u64).saturating_mul(unit);
    Ok((size, size.saturating_sub(free), avail, dev))
}

/// Keep the mounts with blocks, once per device, measured.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn measure(mounts: Vec<Mount>) -> Vec<Device> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in mounts {
        // A dead network share must not hang the table (glib #605).
        let measured = crate::mount_probe::probe_typed(&m.mount, &m.fstype, space);
        let Some(Ok((size, used, avail, dev))) = measured else {
            continue;
        };
        if size == 0 || !seen.insert(dev) {
            continue;
        }
        out.push(Device {
            source: m.source,
            fstype: m.fstype,
            mount: m.mount,
            size,
            used,
            avail,
        });
    }
    out
}

/// The mounted file systems, sorted by mount point.
#[cfg(target_os = "linux")]
pub fn devices() -> io::Result<Vec<Device>> {
    let table = crate::trashdir::read_mount_table()?;
    Ok(sorted(measure(mounts_from_table(&table))))
}

/// Linux without a readable mount table (Termux and other Android
/// sandboxes, trash-cli #348): the file systems of `/`, the home directory
/// and the current directory, each found by its device, and a note that
/// says the list is partial. Elsewhere the error stands.
#[cfg(target_os = "linux")]
fn without_table(prog: &str, e: io::Error) -> io::Result<Vec<Device>> {
    eprintln!(
        "{prog}: cannot read the mount table ({e}); listing only the file systems of /, \
         the home directory and the current directory"
    );
    let mut at = vec![PathBuf::from("/")];
    at.extend(crate::util::home_dir());
    at.extend(std::env::current_dir().ok());
    let mounts = at
        .iter()
        .map(PathBuf::as_path)
        .filter_map(crate::trashdir::mount_top_by_walk)
        .map(|mount| Mount {
            source: String::new(),
            mount,
            fstype: String::new(),
        })
        .collect();
    Ok(sorted(measure(mounts)))
}

#[cfg(not(target_os = "linux"))]
fn without_table(_prog: &str, e: io::Error) -> io::Result<Vec<Device>> {
    Err(e)
}

#[cfg(target_os = "macos")]
pub fn devices() -> io::Result<Vec<Device>> {
    use std::ffi::CStr;
    let mut buf: *mut libc::statfs = std::ptr::null_mut();
    // SAFETY: getmntinfo points `buf` at an array it owns, of the length
    // it returns.
    let n = unsafe { libc::getmntinfo(&mut buf, libc::MNT_NOWAIT) };
    if n <= 0 || buf.is_null() {
        return Err(io::Error::last_os_error());
    }
    let all = unsafe { std::slice::from_raw_parts(buf, n as usize) };
    let text = |b: &[libc::c_char]| {
        // SAFETY: the kernel NUL-terminates these fixed-size fields.
        unsafe { CStr::from_ptr(b.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    };
    let mounts = all
        .iter()
        .filter(|s| s.f_flags & libc::MNT_DONTBROWSE as u32 == 0)
        .map(|s| Mount {
            source: text(&s.f_mntfromname),
            mount: PathBuf::from(text(&s.f_mntonname)),
            fstype: text(&s.f_fstypename),
        })
        .filter(|m| !matches!(m.fstype.as_str(), "devfs" | "autofs" | "nullfs"))
        .collect();
    Ok(sorted(measure(mounts)))
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub fn devices() -> io::Result<Vec<Device>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "listing devices is supported on Linux, macOS and Windows",
    ))
}

#[cfg(windows)]
pub fn devices() -> io::Result<Vec<Device>> {
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetVolumeInformationW};
    let mut out = Vec::new();
    // The drives, then the folders volumes are mounted on.
    for root in crate::trash_history::bin_roots() {
        let w = crate::util::win_path(&root);
        let (mut avail, mut size, mut free) = (0u64, 0u64, 0u64);
        // SAFETY: `w` is NUL-terminated; the outputs are plain integers.
        if unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut size, &mut free) } == 0 {
            continue;
        }
        let mut label = [0u16; 261];
        let mut fs = [0u16; 261];
        // SAFETY: both buffers are as long as the lengths passed.
        let ok = unsafe {
            GetVolumeInformationW(
                w.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        let text = |b: &[u16]| {
            let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            String::from_utf16_lossy(&b[..n])
        };
        out.push(Device {
            source: if ok != 0 { text(&label) } else { String::new() },
            fstype: if ok != 0 { text(&fs) } else { String::new() },
            mount: root,
            size,
            used: size.saturating_sub(free),
            avail,
        });
    }
    Ok(sorted(out))
}

/// [`devices`], or in debug builds the one fake file system
/// `RTRASH_TEST_FS` describes, so tests see known numbers. On Linux
/// without a mount table, a partial list (see [`without_table`]).
pub fn listed(prog: &str) -> io::Result<Vec<Device>> {
    match crate::space::test_fs() {
        Some((mount, s)) => Ok(vec![Device {
            source: "rtrash-test".into(),
            fstype: "test".into(),
            mount,
            size: s.total,
            used: s.total.saturating_sub(s.avail),
            avail: s.avail,
        }]),
        None => devices().or_else(|e| without_table(prog, e)),
    }
}

/// `v` in mount point order, for each `devices` that lists any.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
fn sorted(mut v: Vec<Device>) -> Vec<Device> {
    v.sort_by(|a, b| a.mount.cmp(&b.mount));
    v
}

/// The table `du --devices` prints.
pub fn table(devs: &[Device]) -> String {
    use crate::util::format_bytes;
    let src_w = devs
        .iter()
        .map(|d| d.source.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(6, 40);
    let typ_w = devs
        .iter()
        .map(|d| d.fstype.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(4, 12);
    let mut s = format!(
        "{:>10}  {:>10}  {:>10}  {:>4}  {:<typ_w$}  {:<src_w$}  MOUNTED ON\n",
        "SIZE", "USED", "AVAIL", "USE%", "TYPE", "DEVICE"
    );
    for d in devs {
        s.push_str(&format!(
            "{:>10}  {:>10}  {:>10}  {:>3}%  {:<typ_w$}  {:<src_w$}  {}\n",
            format_bytes(d.size),
            format_bytes(d.used),
            format_bytes(d.avail),
            d.percent(),
            crate::util::term_text(&d.fstype),
            crate::util::term_text(&d.source),
            crate::util::term_path(&d.mount),
        ));
    }
    s
}

/// The JSON object `du --devices --json` prints.
pub fn json(devs: &[Device]) -> String {
    use crate::util::json_escape;
    let rows: Vec<String> = devs
        .iter()
        .map(|d| {
            format!(
                "{{\"mount\":\"{}\",\"device\":\"{}\",\"type\":\"{}\",\"size\":{},\"used\":{},\"available\":{},\"use_percent\":{}}}",
                json_escape(&d.mount.to_string_lossy()),
                json_escape(&d.source),
                json_escape(&d.fstype),
                d.size,
                d.used,
                d.avail,
                d.percent()
            )
        })
        .collect();
    format!("{{\"devices\":[{}]}}", rows.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(used: u64, avail: u64) -> Device {
        Device {
            source: "/dev/sda1".into(),
            fstype: "ext4".into(),
            mount: PathBuf::from("/"),
            size: used + avail + 50,
            used,
            avail,
        }
    }

    #[test]
    fn use_rounds_up_like_df() {
        assert_eq!(dev(0, 100).percent(), 0);
        assert_eq!(dev(1, 999).percent(), 1);
        assert_eq!(dev(500, 500).percent(), 50);
        assert_eq!(dev(100, 0).percent(), 100);
        assert_eq!(dev(0, 0).percent(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn the_table_drops_pseudo_file_systems_and_unescapes_names() {
        let t = "/dev/sda1 / ext4 rw 0 0\n\
                 proc /proc proc rw 0 0\n\
                 cgroup2 /sys/fs/cgroup cgroup2 rw 0 0\n\
                 /dev/sdb1 /media/my\\040disk vfat rw 0 0\n\
                 tmpfs /dev/shm tmpfs rw 0 0\n\
                 shm /dev/shm tmpfs rw 0 0\n";
        let m = mounts_from_table(t);
        let mp: Vec<_> = m
            .iter()
            .map(|m| m.mount.to_string_lossy().into_owned())
            .collect();
        assert_eq!(mp, ["/", "/media/my disk", "/dev/shm"]);
        assert_eq!(m[1].fstype, "vfat");
        assert_eq!(m[1].source, "/dev/sdb1");
        assert_eq!(m[2].source, "shm", "the mount in view");
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn the_root_file_system_is_listed_with_its_space() {
        let devs = devices().unwrap();
        assert!(!devs.is_empty());
        for d in &devs {
            assert!(d.size > 0 && d.used <= d.size, "{d:?}");
        }
        let text = table(&devs);
        assert!(text.starts_with("      SIZE"), "{text}");
        assert!(json(&devs).starts_with("{\"devices\":[{\"mount\":"));
    }
}
