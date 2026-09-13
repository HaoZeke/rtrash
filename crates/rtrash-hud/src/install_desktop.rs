//! `rtrash desktop --install-desktop` — user-local icons, launcher, Sway rules.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Overlay app_id (Sway float/sticky).
pub const OVERLAY_APP_ID: &str = "me.rgoswami.rtrash-hud.overlay";
/// Decorated / pop-out app_id.
pub const APP_ID: &str = "me.rgoswami.rtrash-hud";
/// Human name.
pub const APP_NAME: &str = "rtrash";

/// Sway fragment: float/sticky overlay + bindsym toggle.
pub fn sway_overlay_rules() -> String {
    format!(
        "# rtrash overlay ({OVERLAY_APP_ID}). Include from sway config:\n\
         #   include ~/.config/rtrash/sway-hud.conf\n\
         for_window [app_id=\"{OVERLAY_APP_ID}\"] floating enable\n\
         for_window [app_id=\"{OVERLAY_APP_ID}\"] border pixel 0\n\
         for_window [app_id=\"{OVERLAY_APP_ID}\"] sticky enable\n\
         bindsym $mod+Shift+t exec rtrash desktop --toggle\n"
    )
}

/// Paths written by a successful install.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// Files created or replaced.
    pub wrote: Vec<PathBuf>,
}

impl Report {
    /// Lines for stdout.
    pub fn lines(&self) -> Vec<String> {
        self.wrote
            .iter()
            .map(|p| format!("wrote {}", p.display()))
            .collect()
    }
}

/// Install under `xdg_data` / `xdg_config` using `exe` as Exec=.
pub fn install(home: &Path, exe: &Path, xdg_data: Option<&Path>) -> io::Result<Report> {
    let data = xdg_data
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(PathBuf::from))
        .unwrap_or_else(|| home.join(".local/share"));
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let mut report = Report::default();

    let apps = data.join("applications");
    fs::create_dir_all(&apps)?;
    let desktop = apps.join("rtrash-hud.desktop");
    let exec = exe.display();
    fs::write(
        &desktop,
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name={APP_NAME}\n\
             Comment=FreeDesktop trash HUD\n\
             Exec={exec}\n\
             TryExec={exec}\n\
             Icon=rtrash-hud\n\
             Terminal=false\n\
             Categories=Utility;FileTools;\n\
             StartupWMClass={APP_ID}\n\
             MimeType=inode/directory;\n\
             Keywords=trash;recycle;\n"
        ),
    )?;
    report.wrote.push(desktop);

    let rtrash_cfg = config.join("rtrash");
    fs::create_dir_all(&rtrash_cfg)?;
    let sway = rtrash_cfg.join("sway-hud.conf");
    fs::write(&sway, sway_overlay_rules())?;
    report.wrote.push(sway);
    Ok(report)
}

/// CLI: install using this process executable.
pub fn run_cli() -> io::Result<Report> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME unset"))?;
    let exe = std::env::current_exe()?;
    let report = install(&home, &exe, None)?;
    for line in report.lines() {
        println!("{line}");
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sway_rules_name_overlay_and_toggle() {
        let s = sway_overlay_rules();
        assert!(s.contains(OVERLAY_APP_ID));
        assert!(s.contains("rtrash desktop --toggle"));
        assert!(s.contains("floating enable"));
    }

    #[test]
    fn install_writes_desktop_and_sway() {
        let root = std::env::temp_dir().join(format!(
            "rtrash-hud-install-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let exe = root.join("bin/rtrash-hud");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"x").unwrap();
        let data = root.join("share");
        #[allow(unused_unsafe)]
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
        }
        let report = install(&root, &exe, Some(&data)).unwrap();
        let desktop = data.join("applications/rtrash-hud.desktop");
        let body = fs::read_to_string(&desktop).unwrap();
        assert!(body.contains("Exec="));
        assert!(body.contains("rtrash-hud"));
        assert!(body.contains("MimeType=inode/directory"));
        let sway = root.join("config/rtrash/sway-hud.conf");
        assert!(sway.is_file());
        assert!(report.wrote.iter().any(|p| p == &desktop));
        let _ = fs::remove_dir_all(&root);
    }
}
