//! Optional mpifileutils `drm` backend for permanently deleting large trash payloads.
//!
//! On HPC parallel filesystems (Lustre, GPFS, BeeGFS, ...) a single client
//! walking a tree with `unlinkat` is bounded by metadata-server round trips;
//! `drm` spreads the walk over MPI ranks. This module decides when to hand a
//! trash payload to `LAUNCHER -np N drm -- PATH` and falls back to the native
//! [`crate::fastdelete::remove_path`] walker otherwise.
//!
//! Safety contract:
//! - Only a directory that is a direct child of a trash `files/` directory, and
//!   not a symlink, is ever passed to drm (see [`trash_target`]). User-supplied
//!   paths and symlink targets never reach drm.
//! - drm is spawned with an argv vector (no shell) and `--` before the path.
//! - Success means drm exited 0 **and** the path is gone; callers remove the
//!   matching `.trashinfo` only after this module returns `Ok`.
//! - Any drm failure (spawn error, non-zero exit, path still present) falls
//!   back to the native walker; the drm exit status is reported on stderr and
//!   carried in the error if the fallback also fails.
//!
//! Configuration: `[drm]` in `$XDG_CONFIG_HOME/rtrash/config.toml`, overridden
//! by `RTRASH_DRM`, `RTRASH_DRM_NP`, `RTRASH_DRM_LAUNCHER`,
//! `RTRASH_DRM_LAUNCHER_ARGS`, `RTRASH_DRM_MIN_ENTRIES` and
//! `RTRASH_DRM_LOCAL_MIN_ENTRIES`.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Mutex, OnceLock};

/// Entries (files + directories) a payload on a parallel/network filesystem
/// needs before `auto` pays MPI start-up.
pub const DEFAULT_MIN_ENTRIES: u64 = 10_000;
/// Entries a payload on a local filesystem needs before `auto` uses drm.
pub const DEFAULT_LOCAL_MIN_ENTRIES: u64 = 1_000_000;
/// Upper bound on the default rank count.
const DEFAULT_NP_CAP: u32 = 16;

const BTRFS_SUBVOL_ROOT_INO: u64 = 256;

/// `RTRASH_DRM` / `[drm] mode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Use drm on parallel/network filesystems above the size threshold.
    Auto,
    /// Use drm for every directory payload when the tools are present.
    Always,
    /// Never use drm.
    Never,
}

impl Mode {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Mode::Auto),
            "always" | "on" | "yes" | "1" | "true" => Some(Mode::Always),
            "never" | "off" | "no" | "0" | "false" => Some(Mode::Never),
            _ => None,
        }
    }
}

/// Resolved drm settings (config file, then environment).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub mode: Mode,
    /// MPI ranks (`-np` / `-n`).
    pub np: u32,
    /// Launcher name or path; `None` probes `mpirun`, `mpiexec`, then `srun`
    /// (the last only inside a Slurm allocation).
    pub launcher: Option<String>,
    /// Extra launcher arguments placed before the rank flag.
    pub launcher_args: Vec<String>,
    /// Threshold on parallel/network filesystems.
    pub min_entries: u64,
    /// Threshold on local filesystems (never below `min_entries`).
    pub local_min_entries: u64,
}

impl Default for Config {
    fn default() -> Self {
        let np = std::thread::available_parallelism()
            .map(|n| (n.get() as u32).min(DEFAULT_NP_CAP))
            .unwrap_or(4)
            .max(1);
        Config {
            mode: Mode::Auto,
            np,
            launcher: None,
            launcher_args: Vec::new(),
            min_entries: DEFAULT_MIN_ENTRIES,
            local_min_entries: DEFAULT_LOCAL_MIN_ENTRIES,
        }
    }
}

impl Config {
    /// Build from optional `config.toml` text and an environment lookup.
    /// Environment wins over the file. Returns warnings for bad values.
    pub fn from_sources(
        file_text: Option<&str>,
        env: impl Fn(&str) -> Option<String>,
    ) -> (Self, Vec<String>) {
        let mut cfg = Config::default();
        let mut warnings = Vec::new();
        if let Some(text) = file_text {
            for (k, v) in parse_drm_section(text) {
                cfg.apply(&k, &v, &format!("config.toml [drm] {k}"), &mut warnings);
            }
        }
        for (var, key) in [
            ("RTRASH_DRM", "mode"),
            ("RTRASH_DRM_NP", "np"),
            ("RTRASH_DRM_LAUNCHER", "launcher"),
            ("RTRASH_DRM_LAUNCHER_ARGS", "launcher_args"),
            ("RTRASH_DRM_MIN_ENTRIES", "min_entries"),
            ("RTRASH_DRM_LOCAL_MIN_ENTRIES", "local_min_entries"),
        ] {
            if let Some(v) = env(var) {
                cfg.apply(key, &v, var, &mut warnings);
            }
        }
        (cfg, warnings)
    }

    fn apply(&mut self, key: &str, val: &str, src: &str, warnings: &mut Vec<String>) {
        let val = val.trim();
        match key {
            "mode" => match Mode::parse(val) {
                Some(m) => self.mode = m,
                None => warnings.push(format!(
                    "{src}: expected auto|always|never, got `{val}` (ignored)"
                )),
            },
            "np" => match val.parse::<u32>() {
                Ok(n) if n > 0 => self.np = n,
                _ => warnings.push(format!(
                    "{src}: expected a positive integer, got `{val}` (ignored)"
                )),
            },
            "launcher" => self.launcher = (!val.is_empty()).then(|| val.to_string()),
            "launcher_args" => {
                self.launcher_args = val.split_whitespace().map(str::to_string).collect()
            }
            "min_entries" | "local_min_entries" => match val.replace('_', "").parse::<u64>() {
                Ok(n) if key == "min_entries" => self.min_entries = n,
                Ok(n) => self.local_min_entries = n,
                Err(_) => warnings.push(format!(
                    "{src}: expected a non-negative integer, got `{val}` (ignored)"
                )),
            },
            _ => warnings.push(format!("{src}: unknown key (ignored)")),
        }
    }

    /// Process-wide config (read once).
    pub fn get() -> &'static Config {
        static CFG: OnceLock<Config> = OnceLock::new();
        CFG.get_or_init(|| {
            let text = config_path().and_then(|p| fs::read_to_string(p).ok());
            let (cfg, warnings) = Config::from_sources(text.as_deref(), |k| std::env::var(k).ok());
            for w in warnings {
                eprintln!("rtrash: drm: {w}");
            }
            cfg
        })
    }
}

/// `$XDG_CONFIG_HOME/rtrash/config.toml` or `~/.config/rtrash/config.toml`.
pub fn config_path() -> Option<PathBuf> {
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return Some(PathBuf::from(x).join("rtrash/config.toml"));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/rtrash/config.toml"))
}

/// Minimal reader for the `[drm]` table: `key = value` or `key = "value"`
/// lines, `#` comments. Other tables are skipped.
fn parse_drm_section(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut in_drm = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_drm = line.split('#').next().map(str::trim) == Some("[drm]");
            continue;
        }
        if !in_drm {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        let v = if let Some(rest) = v.strip_prefix('"') {
            rest.split('"').next().unwrap_or("").to_string()
        } else {
            v.split('#').next().unwrap_or("").trim().to_string()
        };
        out.push((k.trim().to_string(), v));
    }
    out
}

/// Filesystem family of a payload, from `statfs.f_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsKind {
    Lustre,
    Gpfs,
    BeeGfs,
    Nfs,
    Smb,
    Ceph,
    PanFs,
    OrangeFs,
    Btrfs,
    Local,
}

impl FsKind {
    pub fn from_magic(magic: u64) -> Self {
        match magic & 0xFFFF_FFFF {
            0x0BD0_0BD0 => FsKind::Lustre,
            0x4750_4653 => FsKind::Gpfs,
            0x1983_0326 => FsKind::BeeGfs,
            0x6969 => FsKind::Nfs,
            0xFF53_4D42 | 0xFE53_4D42 | 0x517B => FsKind::Smb,
            0x00C3_6400 => FsKind::Ceph,
            0xAAD7_AAEA => FsKind::PanFs,
            0x2003_0528 => FsKind::OrangeFs,
            0x9123_683E => FsKind::Btrfs,
            _ => FsKind::Local,
        }
    }

    /// Parallel or network filesystem where metadata latency dominates.
    pub fn is_parallel_or_network(self) -> bool {
        !matches!(self, FsKind::Btrfs | FsKind::Local)
    }
}

/// Filesystem kind of `path`. Callers pass verified non-symlink directories,
/// so statfs following a final symlink does not arise.
pub fn fs_kind(path: &Path) -> FsKind {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return FsKind::Local;
        };
        let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut buf) } != 0 {
            return FsKind::Local;
        }
        FsKind::from_magic(buf.f_type as u64)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        FsKind::Local
    }
}

/// Which deleter handles a payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Native,
    Drm,
}

/// Pure decision: mode x tool availability x filesystem x subvolume x size.
/// `count(cap)` returns the payload's entry count, allowed to stop at `cap`;
/// it is only called in `auto` mode once every cheaper check has passed.
pub fn decide(
    cfg: &Config,
    tools_present: bool,
    fs: FsKind,
    btrfs_subvol_root: bool,
    count: impl FnOnce(u64) -> u64,
) -> Backend {
    if cfg.mode == Mode::Never || !tools_present || btrfs_subvol_root {
        return Backend::Native;
    }
    if cfg.mode == Mode::Always {
        return Backend::Drm;
    }
    let need = if fs.is_parallel_or_network() {
        cfg.min_entries
    } else {
        cfg.local_min_entries.max(cfg.min_entries)
    };
    if need == 0 || count(need) >= need {
        Backend::Drm
    } else {
        Backend::Native
    }
}

/// Count entries under `root` (not following symlinks), stopping at `cap`.
pub fn count_entries_capped(root: &Path, cap: u64) -> u64 {
    let mut n = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        for ent in rd.flatten() {
            n += 1;
            if n >= cap {
                return n;
            }
            if ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(ent.path());
            }
        }
    }
    n
}

/// Rank flag style of the launcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LauncherKind {
    /// `mpirun` / `mpiexec`: `-np N`.
    Mpi,
    /// Slurm `srun`: `-n N`.
    Srun,
}

/// Resolved executables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tools {
    pub launcher: PathBuf,
    pub kind: LauncherKind,
    pub drm: PathBuf,
}

fn is_executable_file(p: &Path) -> bool {
    fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Find `name` on `path_var`, absolute entries only (a relative PATH entry
/// would make the resolved binary depend on the working directory).
pub fn which_in(name: &str, path_var: Option<&OsStr>) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return (p.is_absolute() && is_executable_file(&p)).then_some(p);
    }
    for dir in std::env::split_paths(path_var?) {
        if !dir.is_absolute() {
            continue;
        }
        let cand = dir.join(name);
        if is_executable_file(&cand) {
            return Some(cand);
        }
    }
    None
}

fn launcher_kind(p: &Path) -> LauncherKind {
    match p.file_name().and_then(|n| n.to_str()) {
        Some(n) if n.starts_with("srun") => LauncherKind::Srun,
        _ => LauncherKind::Mpi,
    }
}

/// Resolve `drm` and a launcher. `srun` is auto-probed only inside a Slurm
/// allocation so a login node never queues a new job behind the user's back.
pub fn resolve_tools(cfg: &Config, path_var: Option<&OsStr>, in_slurm: bool) -> Option<Tools> {
    let drm = which_in("drm", path_var)?;
    let launcher = match &cfg.launcher {
        Some(l) => which_in(l, path_var)?,
        None => {
            let mut names = vec!["mpirun", "mpiexec"];
            if in_slurm {
                names.push("srun");
            }
            names.into_iter().find_map(|n| which_in(n, path_var))?
        }
    };
    Some(Tools {
        kind: launcher_kind(&launcher),
        launcher,
        drm,
    })
}

fn tools() -> Option<&'static Tools> {
    static TOOLS: OnceLock<Option<Tools>> = OnceLock::new();
    TOOLS
        .get_or_init(|| {
            let cfg = Config::get();
            if cfg.mode == Mode::Never {
                return None;
            }
            let path = std::env::var_os("PATH");
            let in_slurm = std::env::var_os("SLURM_JOB_ID").is_some();
            resolve_tools(cfg, path.as_deref(), in_slurm)
        })
        .as_ref()
}

/// Whether drm could be used at all in this process (mode not `never` and
/// tools found). Cheap after the first call.
pub fn enabled() -> bool {
    Config::get().mode != Mode::Never && tools().is_some()
}

/// Launcher argv (without the launcher itself): extra args, rank flag,
/// drm, `--`, target.
pub fn build_args(cfg: &Config, tools: &Tools, target: &Path) -> Vec<OsString> {
    let mut v: Vec<OsString> = cfg.launcher_args.iter().map(OsString::from).collect();
    v.push(match tools.kind {
        LauncherKind::Mpi => "-np".into(),
        LauncherKind::Srun => "-n".into(),
    });
    v.push(cfg.np.to_string().into());
    v.push(tools.drm.clone().into_os_string());
    v.push("--".into());
    v.push(target.as_os_str().to_os_string());
    v
}

/// Verify `payload` is a real (non-symlink) directory directly inside the trash
/// `files_dir`, and return its canonical absolute path for drm. `None` means
/// the payload must not be given to drm.
pub fn trash_target(files_dir: &Path, payload: &Path) -> Option<PathBuf> {
    if files_dir.file_name() != Some(OsStr::new("files")) {
        return None;
    }
    if payload.parent() != Some(files_dir) {
        return None;
    }
    let name = payload.file_name()?;
    if name == "." || name == ".." {
        return None;
    }
    let fmeta = fs::symlink_metadata(files_dir).ok()?;
    if !fmeta.file_type().is_dir() {
        return None;
    }
    let pmeta = fs::symlink_metadata(payload).ok()?;
    if !pmeta.file_type().is_dir() {
        return None;
    }
    let canon_files = fs::canonicalize(files_dir).ok()?;
    if !canon_files.is_absolute() {
        return None;
    }
    let target = canon_files.join(name);
    // The canonical child must be the same inode we just inspected.
    let tmeta = fs::symlink_metadata(&target).ok()?;
    if !tmeta.file_type().is_dir() || tmeta.ino() != pmeta.ino() || tmeta.dev() != pmeta.dev() {
        return None;
    }
    Some(target)
}

fn describe_status(st: &ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (st.code(), st.signal()) {
        (Some(c), _) => format!("exit status {c}"),
        (None, Some(s)) => format!("signal {s}"),
        _ => "unknown status".into(),
    }
}

/// Run drm on a verified target. `Ok` only if drm exited 0 and the path is gone.
pub fn run_drm(cfg: &Config, tools: &Tools, target: &Path) -> io::Result<()> {
    // One MPI job at a time: callers may iterate victims in parallel, and each
    // drm run already fans out over its ranks.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
    let _guard = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
    let status = Command::new(&tools.launcher)
        .args(build_args(cfg, tools, target))
        .stdin(Stdio::null())
        // drm progress goes to stdout; keep rtrash's stdout (incl. --json) clean.
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("cannot spawn '{}': {e}", tools.launcher.display()),
            )
        })?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "drm failed with {}",
            describe_status(&status)
        )));
    }
    match fs::symlink_metadata(target) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(_) => Err(io::Error::other(
            "drm exited with status 0 but the path still exists",
        )),
    }
}

/// Outcome of offering a payload to drm.
#[derive(Debug)]
pub enum Attempt {
    /// drm not selected for this payload; nothing was done.
    NotUsed,
    /// drm removed the payload.
    Removed,
    /// drm was selected and failed; the payload may be partly removed.
    Failed(io::Error),
}

/// Offer one trash payload to drm according to the process config.
pub fn try_drm(files_dir: &Path, payload: &Path) -> Attempt {
    let cfg = Config::get();
    if cfg.mode == Mode::Never {
        return Attempt::NotUsed;
    }
    let Some(tools) = tools() else {
        return Attempt::NotUsed;
    };
    let Some(target) = trash_target(files_dir, payload) else {
        return Attempt::NotUsed;
    };
    let fs = fs_kind(&target);
    let subvol = fs == FsKind::Btrfs
        && fs::symlink_metadata(&target)
            .map(|m| m.ino() == BTRFS_SUBVOL_ROOT_INO)
            .unwrap_or(false);
    match decide(cfg, true, fs, subvol, |cap| {
        count_entries_capped(&target, cap)
    }) {
        Backend::Native => Attempt::NotUsed,
        Backend::Drm => match run_drm(cfg, tools, &target) {
            Ok(()) => Attempt::Removed,
            Err(e) => Attempt::Failed(e),
        },
    }
}

/// Permanently remove a payload inside the trash `files_dir`: drm when
/// selected, else (or after a drm failure) the native walker.
pub fn remove_payload(files_dir: &Path, payload: &Path) -> io::Result<()> {
    match try_drm(files_dir, payload) {
        Attempt::Removed => Ok(()),
        Attempt::NotUsed => crate::fastdelete::remove_path(payload),
        Attempt::Failed(drm_err) => {
            eprintln!(
                "rtrash: drm on '{}': {drm_err}; falling back to native delete",
                payload.display()
            );
            crate::fastdelete::remove_path(payload).map_err(|e| {
                io::Error::new(e.kind(), format!("{e} (after drm fallback: {drm_err})"))
            })
        }
    }
}

/// Full-empty pre-pass: offer each top-level directory of `files_dir` to drm.
/// Returns how many children drm removed; failures are reported and left for
/// the native wipe that follows.
pub fn purge_children(files_dir: &Path) -> u64 {
    if !enabled() {
        return 0;
    }
    let Ok(rd) = fs::read_dir(files_dir) else {
        return 0;
    };
    let mut n = 0u64;
    for ent in rd.flatten() {
        if !ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let payload = files_dir.join(ent.file_name());
        match try_drm(files_dir, &payload) {
            Attempt::Removed => n += 1,
            Attempt::NotUsed => {}
            Attempt::Failed(e) => eprintln!(
                "rtrash: drm on '{}': {e}; falling back to native delete",
                payload.display()
            ),
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(mode: Mode) -> Config {
        Config {
            mode,
            np: 4,
            launcher: None,
            launcher_args: Vec::new(),
            min_entries: 100,
            local_min_entries: 1000,
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "rtrash-drm-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    const ALL_FS: [FsKind; 10] = [
        FsKind::Lustre,
        FsKind::Gpfs,
        FsKind::BeeGfs,
        FsKind::Nfs,
        FsKind::Smb,
        FsKind::Ceph,
        FsKind::PanFs,
        FsKind::OrangeFs,
        FsKind::Btrfs,
        FsKind::Local,
    ];

    #[test]
    fn magic_classification() {
        assert_eq!(FsKind::from_magic(0x0BD00BD0), FsKind::Lustre);
        assert_eq!(FsKind::from_magic(0x47504653), FsKind::Gpfs);
        assert_eq!(FsKind::from_magic(0x19830326), FsKind::BeeGfs);
        assert_eq!(FsKind::from_magic(0x6969), FsKind::Nfs);
        assert_eq!(FsKind::from_magic(0xFE534D42), FsKind::Smb);
        // Sign-extended 32-bit f_type must still classify.
        assert_eq!(FsKind::from_magic(0xFFFF_FFFF_FF53_4D42), FsKind::Smb);
        assert_eq!(FsKind::from_magic(0x9123683E), FsKind::Btrfs);
        assert_eq!(FsKind::from_magic(0xEF53), FsKind::Local); // ext4
        assert_eq!(FsKind::from_magic(0x58465342), FsKind::Local); // xfs
        assert_eq!(FsKind::from_magic(0x01021994), FsKind::Local); // tmpfs
        for k in ALL_FS {
            assert_eq!(
                k.is_parallel_or_network(),
                !matches!(k, FsKind::Btrfs | FsKind::Local),
                "{k:?}"
            );
        }
    }

    #[test]
    fn decide_never_and_missing_tools_are_native() {
        for fs in ALL_FS {
            for n in [0u64, 99, 100, 10_000_000] {
                assert_eq!(
                    decide(&cfg(Mode::Never), true, fs, false, |_| n),
                    Backend::Native
                );
                for m in [Mode::Auto, Mode::Always] {
                    assert_eq!(decide(&cfg(m), false, fs, false, |_| n), Backend::Native);
                }
            }
        }
    }

    #[test]
    fn decide_subvolume_root_keeps_ioctl_path() {
        for m in [Mode::Auto, Mode::Always] {
            assert_eq!(
                decide(&cfg(m), true, FsKind::Btrfs, true, |_| u64::MAX),
                Backend::Native
            );
        }
    }

    #[test]
    fn decide_always_skips_count() {
        for fs in ALL_FS {
            let b = decide(&cfg(Mode::Always), true, fs, false, |_| {
                panic!("always must not walk the tree")
            });
            assert_eq!(b, Backend::Drm);
        }
    }

    #[test]
    fn decide_auto_matrix() {
        let c = cfg(Mode::Auto);
        // fs x size -> expected
        let table: &[(FsKind, u64, Backend)] = &[
            (FsKind::Lustre, 99, Backend::Native),
            (FsKind::Lustre, 100, Backend::Drm),
            (FsKind::Gpfs, 5_000, Backend::Drm),
            (FsKind::BeeGfs, 100, Backend::Drm),
            (FsKind::Nfs, 10, Backend::Native),
            (FsKind::Nfs, 100, Backend::Drm),
            (FsKind::Local, 100, Backend::Native),
            (FsKind::Local, 999, Backend::Native),
            (FsKind::Local, 1000, Backend::Drm),
            (FsKind::Btrfs, 999, Backend::Native),
            (FsKind::Btrfs, 1000, Backend::Drm),
        ];
        for &(fs, n, want) in table {
            let mut asked = None;
            let got = decide(&c, true, fs, false, |cap| {
                asked = Some(cap);
                n.min(cap)
            });
            assert_eq!(got, want, "{fs:?} n={n}");
            let cap = if fs.is_parallel_or_network() {
                100
            } else {
                1000
            };
            assert_eq!(asked, Some(cap), "count cap for {fs:?}");
        }
    }

    #[test]
    fn decide_auto_local_threshold_never_below_min() {
        let mut c = cfg(Mode::Auto);
        c.local_min_entries = 0;
        assert_eq!(
            decide(&c, true, FsKind::Local, false, |_| 99),
            Backend::Native
        );
        c.min_entries = 0;
        assert_eq!(
            decide(&c, true, FsKind::Local, false, |_| panic!("no walk at 0")),
            Backend::Drm
        );
    }

    #[test]
    fn config_env_overrides_file() {
        let file = "# rtrash\n[keys]\nmode = \"always\"\n\n[drm]\nmode = \"never\" # off\nnp = 8\nlauncher = \"mpiexec\"\nmin_entries = 50_000\nlauncher_args = \"--oversubscribe --bind-to none\"\n";
        let env: HashMap<&str, &str> = HashMap::new();
        let (c, w) = Config::from_sources(Some(file), |k| env.get(k).map(|s| s.to_string()));
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.mode, Mode::Never);
        assert_eq!(c.np, 8);
        assert_eq!(c.launcher.as_deref(), Some("mpiexec"));
        assert_eq!(c.min_entries, 50_000);
        assert_eq!(c.launcher_args, ["--oversubscribe", "--bind-to", "none"]);
        assert_eq!(c.local_min_entries, DEFAULT_LOCAL_MIN_ENTRIES);

        let env: HashMap<&str, &str> = [
            ("RTRASH_DRM", "always"),
            ("RTRASH_DRM_NP", "3"),
            ("RTRASH_DRM_LAUNCHER", "srun"),
            ("RTRASH_DRM_MIN_ENTRIES", "7"),
            ("RTRASH_DRM_LOCAL_MIN_ENTRIES", "9"),
        ]
        .into_iter()
        .collect();
        let (c, w) = Config::from_sources(Some(file), |k| env.get(k).map(|s| s.to_string()));
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.mode, Mode::Always);
        assert_eq!(c.np, 3);
        assert_eq!(c.launcher.as_deref(), Some("srun"));
        assert_eq!((c.min_entries, c.local_min_entries), (7, 9));
    }

    #[test]
    fn config_bad_values_warn_and_keep_defaults() {
        let env: HashMap<&str, &str> = [("RTRASH_DRM", "sometimes"), ("RTRASH_DRM_NP", "0")]
            .into_iter()
            .collect();
        let (c, w) = Config::from_sources(None, |k| env.get(k).map(|s| s.to_string()));
        assert_eq!(c.mode, Mode::Auto);
        assert_eq!(c.np, Config::default().np);
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[0].contains("RTRASH_DRM"));
    }

    #[test]
    fn argv_mpirun_and_srun() {
        let mut c = cfg(Mode::Always);
        c.launcher_args = vec!["--oversubscribe".into()];
        let t = Tools {
            launcher: "/opt/mpi/bin/mpirun".into(),
            kind: LauncherKind::Mpi,
            drm: "/opt/mfu/bin/drm".into(),
        };
        let target = Path::new("/scratch/.Trash-u/files/-rf weird name");
        let a = build_args(&c, &t, target);
        let want: Vec<OsString> = [
            "--oversubscribe",
            "-np",
            "4",
            "/opt/mfu/bin/drm",
            "--",
            "/scratch/.Trash-u/files/-rf weird name",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        assert_eq!(a, want);
        assert_eq!(
            launcher_kind(Path::new("/usr/bin/srun")),
            LauncherKind::Srun
        );
        let t = Tools {
            kind: LauncherKind::Srun,
            ..t
        };
        let a = build_args(&cfg(Mode::Always), &t, target);
        assert_eq!(a[0], "-n");
        assert_eq!(a[3], "--");
    }

    #[test]
    fn resolve_tools_probe_order_and_slurm_gate() {
        let bin = tmp("which");
        for n in ["drm", "srun", "mpiexec"] {
            let p = bin.join(n);
            fs::write(&p, "#!/bin/sh\n").unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::write(bin.join("mpirun"), "not executable").unwrap();
        let path = std::env::join_paths([Path::new("relative/dir"), bin.as_path()]).unwrap();
        let c = cfg(Mode::Auto);
        let t = resolve_tools(&c, Some(&path), false).unwrap();
        assert_eq!(t.launcher, bin.join("mpiexec"));
        assert_eq!(t.kind, LauncherKind::Mpi);
        assert_eq!(t.drm, bin.join("drm"));
        fs::remove_file(bin.join("mpiexec")).unwrap();
        assert!(
            resolve_tools(&c, Some(&path), false).is_none(),
            "no srun off-allocation"
        );
        let t = resolve_tools(&c, Some(&path), true).unwrap();
        assert_eq!(t.kind, LauncherKind::Srun);
        fs::remove_file(bin.join("drm")).unwrap();
        assert!(
            resolve_tools(&c, Some(&path), true).is_none(),
            "drm required"
        );
        let _ = fs::remove_dir_all(&bin);
    }

    #[test]
    fn trash_target_only_real_dirs_in_files() {
        let root = tmp("target");
        let files = root.join("files");
        fs::create_dir_all(files.join("payload/sub")).unwrap();
        fs::write(files.join("plain"), b"x").unwrap();
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, files.join("link")).unwrap();

        let canon = fs::canonicalize(&files).unwrap();
        assert_eq!(
            trash_target(&files, &files.join("payload")),
            Some(canon.join("payload"))
        );
        // Not directly in files/.
        assert_eq!(trash_target(&files, &files.join("payload/sub")), None);
        assert_eq!(trash_target(&files, &outside), None);
        // Symlink to a directory: never handed over.
        assert_eq!(trash_target(&files, &files.join("link")), None);
        // Regular files stay on the native path.
        assert_eq!(trash_target(&files, &files.join("plain")), None);
        // Parent not named files/.
        assert_eq!(trash_target(&root, &outside), None);
        // files/ itself a symlink.
        let alias = root.join("alias");
        fs::create_dir_all(&alias).unwrap();
        std::os::unix::fs::symlink(&files, alias.join("files")).unwrap();
        assert_eq!(
            trash_target(&alias.join("files"), &alias.join("files/payload")),
            None
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn count_stops_at_cap() {
        let root = tmp("count");
        for i in 0..5 {
            fs::create_dir_all(root.join(format!("d{i}"))).unwrap();
            for j in 0..4 {
                fs::write(root.join(format!("d{i}/f{j}")), b"").unwrap();
            }
        }
        assert_eq!(count_entries_capped(&root, u64::MAX), 25);
        assert_eq!(count_entries_capped(&root, 7), 7);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_section_handles_comments_and_quotes() {
        let kv = parse_drm_section(
            "[drm] # backend\nmode = auto # c\nlauncher = \"/a b/mpirun\"\n[other]\nnp = 9\n",
        );
        assert_eq!(
            kv,
            vec![
                ("mode".to_string(), "auto".to_string()),
                ("launcher".to_string(), "/a b/mpirun".to_string())
            ]
        );
    }
}
