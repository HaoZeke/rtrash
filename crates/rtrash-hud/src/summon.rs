//! Unix summon socket for compositor binds (`rtrash desktop --toggle`).
//!
//! Commands are one line: `show`, `hide`, `toggle`, plus an optional
//! xdg-activation token. `--hide` with no listener exits 0.

#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

/// Env override for the summon socket path.
pub const SOCKET_ENV: &str = "RTRASH_HUD_SOCKET";

/// Operator action for the iced loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummonAction {
    /// Map the HUD window.
    Show,
    /// Unmap the HUD window.
    Hide,
    /// Invert mapped state.
    Toggle,
}

/// One summon request: verb plus optional xdg-activation token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummonRequest {
    /// Verb the iced loop applies.
    pub action: SummonAction,
    /// Optional xdg-activation token; never set on [`SummonAction::Hide`].
    pub token: Option<String>,
}

impl SummonRequest {
    /// Request `action` with no activation token.
    pub fn new(action: SummonAction) -> Self {
        Self {
            action,
            token: None,
        }
    }
}

/// Why a summon send or bind failed.
#[derive(Debug)]
pub enum SummonError {
    /// Summon sockets are Unix-only.
    Unsupported,
    /// Socket path resolved empty.
    NoPath,
    /// No HUD is accepting on this path.
    NotRunning(String),
    /// Another HUD already owns this path.
    AlreadyRunning(String),
    /// Filesystem or stream I/O failed.
    Io(std::io::Error),
    /// Thread spawn or other bind-side failure.
    Other(String),
}

impl std::fmt::Display for SummonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => write!(f, "summon socket not available on this platform"),
            Self::NoPath => write!(f, "summon socket path could not be resolved"),
            Self::NotRunning(path) => write!(f, "HUD summon socket not accepting ({path})"),
            Self::AlreadyRunning(path) => write!(f, "HUD summon socket already in use ({path})"),
            Self::Io(err) => write!(f, "{err}"),
            Self::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for SummonError {}

impl From<std::io::Error> for SummonError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Holds the listener thread and bound path for process lifetime.
#[derive(Debug)]
pub struct SummonServer {
    path: PathBuf,
    #[allow(dead_code)]
    join: Option<thread::JoinHandle<()>>,
}

impl Drop for SummonServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Parse a single command line (trimmed, case-insensitive).
pub fn parse_command(raw: &str) -> Option<SummonAction> {
    parse_request(raw).map(|r| r.action)
}

/// Parse `show` / `hide` / `toggle` and an optional same-line token.
pub fn parse_request(raw: &str) -> Option<SummonRequest> {
    let line = raw.trim();
    if line.is_empty() {
        return None;
    }
    let mut parts = line.splitn(2, char::is_whitespace);
    let verb = parts.next()?.to_ascii_lowercase();
    let action = match verb.as_str() {
        "show" => SummonAction::Show,
        "hide" => SummonAction::Hide,
        "toggle" => SummonAction::Toggle,
        _ => return None,
    };
    let token = parts
        .next()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .and_then(sanitize_token);
    let token = match action {
        SummonAction::Hide => None,
        _ => token,
    };
    Some(SummonRequest { action, token })
}

/// Token after trim: 1..=512 bytes, no CR/LF.
pub fn sanitize_token(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() || t.len() > 512 || t.contains('\n') || t.contains('\r') {
        return None;
    }
    Some(t.to_string())
}

/// Read `XDG_ACTIVATION_TOKEN` and unset it and `DESKTOP_STARTUP_ID`.
pub fn take_env_token() -> Option<String> {
    let raw = std::env::var("XDG_ACTIVATION_TOKEN").ok();
    #[allow(unused_unsafe)]
    unsafe {
        std::env::remove_var("XDG_ACTIVATION_TOKEN");
        std::env::remove_var("DESKTOP_STARTUP_ID");
    }
    raw.as_deref().and_then(sanitize_token)
}

/// Wire form for `action` (one word, no newline).
pub fn command_word(action: SummonAction) -> &'static str {
    match action {
        SummonAction::Show => "show",
        SummonAction::Hide => "hide",
        SummonAction::Toggle => "toggle",
    }
}

/// Default path: `$XDG_RUNTIME_DIR/rtrash/hud.sock`, or the env override.
pub fn default_socket_path() -> PathBuf {
    if let Ok(raw) = std::env::var(SOCKET_ENV) {
        let t = raw.trim();
        if !t.is_empty() {
            return PathBuf::from(t);
        }
    }
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        let t = runtime.trim();
        if !t.is_empty() {
            return Path::new(t).join("rtrash").join("hud.sock");
        }
    }
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".rtrash/run/hud.sock"))
        .unwrap_or_else(|| PathBuf::from("/tmp/rtrash-hud.sock"))
}

/// True when a listener is bound (connect succeeds).
pub fn socket_accepts(path: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(path).is_ok()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// True when the socket is absent so a compositor bind cannot talk to a HUD.
pub fn is_summon_miss(err: &SummonError) -> bool {
    matches!(
        err,
        SummonError::NotRunning(_) | SummonError::NoPath | SummonError::Unsupported
    )
}

/// What `--show` / `--hide` / `--toggle` should do after talking to the socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummonCli {
    /// Command delivered, or hide with nothing running.
    Done,
    /// Start a new HUD and show the overlay.
    StartShown,
}

/// Plan the binary's next step after [`send_command`].
pub fn plan_summon_cli(
    action: SummonAction,
    result: Result<(), SummonError>,
) -> Result<SummonCli, SummonError> {
    match result {
        Ok(()) => Ok(SummonCli::Done),
        Err(err) if is_summon_miss(&err) && matches!(action, SummonAction::Hide) => {
            Ok(SummonCli::Done)
        }
        Err(err) if is_summon_miss(&err) => Ok(SummonCli::StartShown),
        Err(err) => Err(err),
    }
}

/// Send one summon command to a running HUD (takes env token).
pub fn send_command(action: SummonAction) -> Result<(), SummonError> {
    let token = match action {
        SummonAction::Hide => {
            let _ = take_env_token();
            None
        }
        _ => take_env_token(),
    };
    send_request(SummonRequest { action, token })
}

/// Send a parsed request to the default socket.
pub fn send_request(req: SummonRequest) -> Result<(), SummonError> {
    #[cfg(unix)]
    {
        send_request_to(&default_socket_path(), &req)
    }
    #[cfg(not(unix))]
    {
        let _ = req;
        Err(SummonError::Unsupported)
    }
}

/// Write one wire line to `path`.
pub fn send_request_to(path: &Path, req: &SummonRequest) -> Result<(), SummonError> {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        let mut stream = UnixStream::connect(path)
            .map_err(|err| SummonError::NotRunning(format!("{}: {err}", path.display())))?;
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let line = encode_request(req);
        stream.write_all(line.as_bytes())?;
        stream.flush()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (path, req);
        Err(SummonError::Unsupported)
    }
}

/// Canonical wire: `verb` or `verb token`, always one LF.
pub fn encode_request(req: &SummonRequest) -> String {
    match req.action {
        SummonAction::Hide => format!("{}\n", command_word(req.action)),
        _ => match req.token.as_deref().and_then(sanitize_token) {
            Some(tok) => format!("{} {tok}\n", command_word(req.action)),
            None => format!("{}\n", command_word(req.action)),
        },
    }
}

/// Bind the summon socket and start the accept thread.
pub fn install() -> Result<SummonServer, SummonError> {
    #[cfg(unix)]
    {
        install_unix()
    }
    #[cfg(not(unix))]
    {
        Err(SummonError::Unsupported)
    }
}

/// Non-blocking read of the next queued summon request.
pub fn try_recv() -> Option<SummonRequest> {
    let guard = action_pair().1.lock().ok()?;
    guard.try_recv().ok()
}

fn action_pair() -> &'static (SyncSender<SummonRequest>, Mutex<Receiver<SummonRequest>>) {
    static PAIR: OnceLock<(SyncSender<SummonRequest>, Mutex<Receiver<SummonRequest>>)> =
        OnceLock::new();
    PAIR.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel(16);
        (tx, Mutex::new(rx))
    })
}

#[cfg(unix)]
fn action_sender() -> SyncSender<SummonRequest> {
    action_pair().0.clone()
}

#[cfg(unix)]
fn prepare_bind_path(path: &Path) -> Result<(), SummonError> {
    if socket_accepts(path) {
        return Err(SummonError::AlreadyRunning(path.display().to_string()));
    }
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

#[cfg(unix)]
fn install_unix() -> Result<SummonServer, SummonError> {
    use std::os::unix::net::UnixListener;

    let path = default_socket_path();
    if path.as_os_str().is_empty() {
        return Err(SummonError::NoPath);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    prepare_bind_path(&path)?;
    let listener = UnixListener::bind(&path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::AddrInUse {
            SummonError::AlreadyRunning(path.display().to_string())
        } else {
            SummonError::Io(err)
        }
    })?;
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    let _ = action_sender();
    let join = thread::Builder::new()
        .name("rtrash-hud-summon".into())
        .spawn(move || accept_loop(listener))
        .map_err(|err| SummonError::Other(format!("spawn summon thread: {err}")))?;
    Ok(SummonServer {
        path,
        join: Some(join),
    })
}

#[cfg(unix)]
fn accept_loop(listener: std::os::unix::net::UnixListener) {
    let tx = action_sender();
    loop {
        let Ok((stream, _)) = listener.accept() else {
            thread::sleep(Duration::from_millis(50));
            continue;
        };
        if let Some(req) = read_action(stream) {
            if tx.send(req).is_err() {
                break;
            }
        }
    }
}

#[cfg(unix)]
fn read_action(stream: std::os::unix::net::UnixStream) -> Option<SummonRequest> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    parse_request(&line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_command_words() {
        assert_eq!(parse_command("show"), Some(SummonAction::Show));
        assert_eq!(parse_command(" HIDE\n"), Some(SummonAction::Hide));
        assert_eq!(parse_command("Toggle"), Some(SummonAction::Toggle));
        assert_eq!(parse_command("quit"), None);
    }

    #[test]
    fn parse_request_keeps_token_on_show_and_toggle() {
        assert_eq!(
            parse_request("toggle abc.def"),
            Some(SummonRequest {
                action: SummonAction::Toggle,
                token: Some("abc.def".into()),
            })
        );
        assert_eq!(
            parse_request("hide leftover"),
            Some(SummonRequest::new(SummonAction::Hide))
        );
    }

    #[test]
    fn hide_miss_is_done() {
        let err = SummonError::NotRunning("/tmp/missing.sock".into());
        assert_eq!(
            plan_summon_cli(SummonAction::Hide, Err(err)).unwrap(),
            SummonCli::Done
        );
    }

    #[test]
    fn take_env_token_unsets_activation_vars() {
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var("XDG_ACTIVATION_TOKEN", "tok-xyz");
            std::env::set_var("DESKTOP_STARTUP_ID", "startup");
        }
        let tok = take_env_token();
        assert_eq!(tok.as_deref(), Some("tok-xyz"));
        assert!(std::env::var("XDG_ACTIVATION_TOKEN").is_err());
        assert!(std::env::var("DESKTOP_STARTUP_ID").is_err());
    }
}
