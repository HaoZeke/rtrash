//! Which live processes hold a path, the question lsof and fuser answer.
//!
//! Linux reads `/proc/PID/{fd,cwd,root,exe,maps}`; macOS asks libproc for
//! each process's open vnodes, working and root directories and executable.
//! Without root, other users' processes cannot be read; the snapshot counts
//! them so a caller can say the answer is partial.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One way one process holds one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    pub pid: u32,
    pub uid: Option<u32>,
    pub command: String,
    /// `fd N`, `cwd`, `root`, `exe` or `mmap`.
    pub how: String,
    pub path: PathBuf,
    /// Every name the process goes by: its short command name (which
    /// Linux cuts to 15 bytes), and the file names of its executable
    /// and of its first argument.
    pub names: Vec<String>,
}

impl Hold {
    /// Does the process go by `name`?
    pub fn is(&self, name: &str) -> bool {
        self.command == name || self.names.iter().any(|n| n == name)
    }
}

/// The file name of a path given as bytes or text.
fn base_name(p: &Path) -> Option<String> {
    p.file_name().map(|f| f.to_string_lossy().into_owned())
}

/// Every hold of every readable process at one moment, keyed by path.
#[derive(Debug, Default)]
pub struct Snapshot {
    held: BTreeMap<PathBuf, Vec<Hold>>,
    /// Processes whose descriptors could not be read (another user's, without root).
    pub unreadable: usize,
    /// False where rtrash has no way to list holders.
    pub supported: bool,
    roots: Vec<PathBuf>,
    /// Read memory maps (Linux only; libproc gives none).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    maps: bool,
}

impl Snapshot {
    /// Scan the running processes, leaving out this one.
    pub fn take() -> Snapshot {
        Snapshot::take_under(&[])
    }

    /// Like `take`, but keep only holds on or below one of `roots`
    /// (all of them when `roots` is empty), which saves most of the work.
    pub fn take_under(roots: &[PathBuf]) -> Snapshot {
        Snapshot::scan_under(roots, true)
    }

    /// Like `take_under`; with `maps` false, memory maps are not read,
    /// which on Linux is about half the cost of a scan.
    pub fn scan_under(roots: &[PathBuf], maps: bool) -> Snapshot {
        let mut s = Snapshot {
            supported: SUPPORTED,
            maps,
            roots: roots.iter().map(|r| resolve(r)).collect(),
            ..Snapshot::default()
        };
        scan(&mut s);
        s
    }

    fn wanted(&self, path: &Path) -> bool {
        self.roots.is_empty() || self.roots.iter().any(|r| path.starts_with(r))
    }

    fn add(&mut self, hold: Hold) {
        if !self.wanted(&hold.path) {
            return;
        }
        let list = self.held.entry(hold.path.clone()).or_default();
        if !list.iter().any(|h| h.pid == hold.pid && h.how == hold.how) {
            list.push(hold);
        }
    }

    /// Holds on `path` or anything below it.
    /// Paths compare by component, so everything below `path` sorts
    /// right after it and before any sibling.
    pub fn under(&self, path: &Path) -> Vec<&Hold> {
        let key = resolve(path);
        self.held
            .range(key.clone()..)
            .take_while(|(p, _)| p.starts_with(&key))
            .flat_map(|(_, v)| v.iter())
            .collect()
    }

    /// True when a live process holds `path` or anything below it.
    pub fn holds(&self, path: &Path) -> bool {
        let key = resolve(path);
        self.held
            .range(key.clone()..)
            .next()
            .is_some_and(|(p, _)| p.starts_with(&key))
    }
}

/// The path with its parent's symlinks resolved, as the kernel reports it,
/// but the last component kept (a symlink is not its target).
pub fn resolve(path: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    match (abs.parent(), abs.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .map(|p| p.join(name))
            .unwrap_or(abs),
        _ => abs,
    }
}

/// Is `pid` a live process (EPERM still means it exists)?
pub fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks that the process exists.
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(target_os = "linux")]
const SUPPORTED: bool = true;

#[cfg(target_os = "linux")]
fn scan(s: &mut Snapshot) {
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    let me = std::process::id();
    let Ok(dir) = fs::read_dir("/proc") else {
        s.supported = false;
        return;
    };
    for entry in dir.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == me {
            continue;
        }
        let base = entry.path();
        let mut found: Vec<(String, PathBuf)> = Vec::new();
        let mut denied = false;
        for link in ["cwd", "root", "exe"] {
            match fs::read_link(base.join(link)) {
                Ok(p) => {
                    if let Some(p) = live_path(p) {
                        if !(link == "root" && p == Path::new("/")) && s.wanted(&p) {
                            found.push((link.to_string(), p));
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => denied = true,
                Err(_) => {}
            }
        }
        match fs::read_dir(base.join("fd")) {
            Ok(fds) => {
                for fd in fds.flatten() {
                    if let Ok(p) = fs::read_link(fd.path()) {
                        if let Some(p) = live_path(p) {
                            if s.wanted(&p) {
                                let n = fd.file_name().to_string_lossy().into_owned();
                                found.push((format!("fd {n}"), p));
                            }
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => denied = true,
            Err(_) => {}
        }
        let maps = if s.maps {
            fs::read(base.join("maps"))
        } else {
            Err(std::io::ErrorKind::Unsupported.into())
        };
        if let Ok(maps) = maps {
            let mut seen: Vec<&[u8]> = Vec::new();
            for line in maps.split(|&b| b == b'\n') {
                // address perms offset dev inode path; the path may hold spaces.
                let Some(at) = line.iter().position(|&b| b == b'/') else {
                    continue;
                };
                let p = &line[at..];
                let os = <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(p);
                if !s.wanted(Path::new(os)) || seen.contains(&p) {
                    continue;
                }
                seen.push(p);
                if let Some(p) = live_path(PathBuf::from(os)) {
                    found.push(("mmap".to_string(), p));
                }
            }
        }
        if !found.is_empty() {
            // Name and owner only for a process that holds something.
            let uid = fs::metadata(&base).ok().map(|m| m.uid());
            let command = fs::read_to_string(base.join("comm"))
                .map(|c| c.trim_end_matches('\n').to_string())
                .unwrap_or_default();
            let mut names = Vec::new();
            if let Some(n) = fs::read_link(base.join("exe"))
                .ok()
                .and_then(live_path)
                .and_then(|e| base_name(&e))
            {
                names.push(n);
            }
            if let Ok(cmdline) = fs::read(base.join("cmdline")) {
                let argv0 = cmdline.split(|&b| b == 0).next().unwrap_or_default();
                let argv0 = <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(argv0);
                if let Some(n) = base_name(Path::new(argv0)) {
                    if !names.contains(&n) {
                        names.push(n);
                    }
                }
            }
            for (how, path) in found {
                s.add(Hold {
                    pid,
                    uid,
                    command: command.clone(),
                    how,
                    path,
                    names: names.clone(),
                });
            }
        }
        if denied {
            s.unreadable += 1;
        }
    }
}

/// Absolute paths only (not `socket:[N]` or `anon_inode:...`), with the
/// kernel's ` (deleted)` mark dropped.
#[cfg(target_os = "linux")]
fn live_path(p: PathBuf) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let b = p.as_os_str().as_bytes();
    if b.first() != Some(&b'/') {
        return None;
    }
    let b = b.strip_suffix(b" (deleted)").unwrap_or(b);
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(b)))
}

#[cfg(target_os = "macos")]
const SUPPORTED: bool = true;

/// `struct proc_fileinfo` from sys/proc_info.h (libc does not carry it).
#[cfg(target_os = "macos")]
#[repr(C)]
struct ProcFileinfo {
    fi_openflags: u32,
    fi_status: u32,
    fi_offset: i64,
    fi_type: i32,
    fi_guardflags: u32,
}

/// `struct vnode_fdinfowithpath` from sys/proc_info.h.
#[cfg(target_os = "macos")]
#[repr(C)]
struct VnodeFdinfoWithPath {
    pfi: ProcFileinfo,
    pvip: libc::vnode_info_path,
}

#[cfg(target_os = "macos")]
const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;

#[cfg(target_os = "macos")]
fn vip_path(v: &libc::vnode_info_path) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: the 32x32 array is MAXPATHLEN contiguous bytes.
    let bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(v.vip_path.as_ptr().cast::<u8>(), 1024) };
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let b = &bytes[..end];
    (b.first() == Some(&b'/')).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(b)))
}

#[cfg(target_os = "macos")]
fn scan(s: &mut Snapshot) {
    use std::mem::{size_of, zeroed};
    let me = std::process::id();
    // SAFETY: libproc calls fill caller-sized buffers and return the byte
    // or entry count written; every struct is plain data.
    unsafe {
        let n = libc::proc_listallpids(std::ptr::null_mut(), 0);
        if n <= 0 {
            s.supported = false;
            return;
        }
        let mut pids = vec![0 as libc::c_int; n as usize * 2 + 64];
        let bytes = libc::c_int::try_from(pids.len() * size_of::<libc::c_int>()).unwrap_or(0);
        let got = libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes);
        if got <= 0 {
            s.supported = false;
            return;
        }
        pids.truncate(got as usize);
        for &pid in &pids {
            if pid <= 0 || pid as u32 == me {
                continue;
            }
            let mut bsd: libc::proc_bsdinfo = zeroed();
            let sz = size_of::<libc::proc_bsdinfo>() as libc::c_int;
            let (uid, mut command) = if libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut bsd as *mut libc::proc_bsdinfo).cast(),
                sz,
            ) == sz
            {
                let name: Vec<u8> = bsd
                    .pbi_comm
                    .iter()
                    .take_while(|&&c| c != 0)
                    .map(|&c| c as u8)
                    .collect();
                (
                    Some(bsd.pbi_uid),
                    String::from_utf8_lossy(&name).into_owned(),
                )
            } else {
                (None, String::new())
            };
            let mut denied = false;
            let mut buf = vec![0u8; 4096];
            let len = libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32);
            let exe = if len > 0 {
                use std::os::unix::ffi::OsStrExt;
                Some(PathBuf::from(std::ffi::OsStr::from_bytes(
                    &buf[..len as usize],
                )))
            } else {
                None
            };
            if command.is_empty() {
                if let Some(e) = &exe {
                    command = e
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_default();
                }
            }
            let pid_u = pid as u32;
            let names: Vec<String> = exe.as_deref().and_then(base_name).into_iter().collect();
            let hold = |how: &str, path: PathBuf| Hold {
                pid: pid_u,
                uid,
                command: command.clone(),
                how: how.to_string(),
                path,
                names: names.clone(),
            };
            let mut holds = Vec::new();
            if let Some(e) = exe {
                holds.push(hold("exe", e));
            }
            let mut vp: libc::proc_vnodepathinfo = zeroed();
            let sz = size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
            if libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                (&mut vp as *mut libc::proc_vnodepathinfo).cast(),
                sz,
            ) == sz
            {
                if let Some(p) = vip_path(&vp.pvi_cdir) {
                    holds.push(hold("cwd", p));
                }
                if let Some(p) = vip_path(&vp.pvi_rdir) {
                    if p != Path::new("/") {
                        holds.push(hold("root", p));
                    }
                }
            } else {
                denied = true;
            }
            let need = libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0);
            if need > 0 {
                let count = need as usize / size_of::<libc::proc_fdinfo>() + 16;
                let mut fds: Vec<libc::proc_fdinfo> = Vec::with_capacity(count);
                let cap = (count * size_of::<libc::proc_fdinfo>()) as libc::c_int;
                let got =
                    libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), cap);
                if got > 0 {
                    fds.set_len(got as usize / size_of::<libc::proc_fdinfo>());
                    for fd in &fds {
                        if fd.proc_fdtype != libc::PROX_FDTYPE_VNODE as u32 {
                            continue;
                        }
                        let mut vi: VnodeFdinfoWithPath = zeroed();
                        let sz = size_of::<VnodeFdinfoWithPath>() as libc::c_int;
                        if libc::proc_pidfdinfo(
                            pid,
                            fd.proc_fd,
                            PROC_PIDFDVNODEPATHINFO,
                            (&mut vi as *mut VnodeFdinfoWithPath).cast(),
                            sz,
                        ) == sz
                        {
                            if let Some(p) = vip_path(&vi.pvip) {
                                holds.push(hold(&format!("fd {}", fd.proc_fd), p));
                            }
                        }
                    }
                } else {
                    denied = true;
                }
            } else {
                denied = true;
            }
            for h in holds {
                s.add(h);
            }
            if denied {
                s.unreadable += 1;
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const SUPPORTED: bool = false;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn scan(_s: &mut Snapshot) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_open_file_and_its_parents_are_held_but_a_sibling_is_not() {
        if !SUPPORTED {
            return;
        }
        let dir = std::env::temp_dir().join(format!("rtrash-holders-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("a")).unwrap();
        let file = dir.join("a/held");
        std::fs::write(&file, b"x").unwrap();
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exec 3<\"$1\"; exec sleep 30")
            .arg("sh")
            .arg(&file)
            .spawn()
            .unwrap();
        let mut found = false;
        for _ in 0..100 {
            let snap = Snapshot::take();
            if snap
                .under(&file)
                .iter()
                .any(|h| h.pid == child.id() && h.how.starts_with("fd "))
            {
                assert!(snap.holds(&dir.join("a")));
                assert!(snap.holds(&dir));
                assert!(!snap.holds(&dir.join("a-b")));
                found = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(found, "the child's descriptor was never seen");
    }

    #[test]
    fn this_process_is_alive_and_pid_zero_is_not() {
        assert!(alive(std::process::id()));
        assert!(!alive(0));
    }
}
