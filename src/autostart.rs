//! Autostart management:
//! - Windows: "Start with Windows" via the HKCU `Run` key.
//! - Linux: systemd user service in `~/.config/systemd/user/t10s-keep-awake.service`.

use anyhow::{Context, Result};

#[cfg(windows)]
mod imp {
    use super::*;
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "T10sKeepAwake";

    fn run_key(access: u32) -> Result<RegKey> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        hkcu.open_subkey_with_flags(RUN_KEY, access)
            .context("opening HKCU Run key")
    }

    /// Is the app registered to start at login?
    #[allow(dead_code)] // kept as the read side of the Run-key pair
    pub fn is_enabled() -> bool {
        run_key(KEY_READ)
            .and_then(|k| k.get_value::<String, _>(VALUE_NAME).map_err(Into::into))
            .is_ok()
    }

    /// Register (true) or unregister (false) autostart, pointing at this exe.
    pub fn set_enabled(enable: bool) -> Result<()> {
        if enable {
            let exe = std::env::current_exe().context("current exe path")?;
            let command = format!("\"{}\"", exe.display());
            let key = RegKey::predef(HKEY_CURRENT_USER)
                .create_subkey(RUN_KEY)
                .context("creating HKCU Run key")?
                .0;
            key.set_value(VALUE_NAME, &command)
                .context("writing autostart value")?;
        } else if let Ok(key) = run_key(KEY_WRITE) {
            // ignore "not found" on delete
            let _ = key.delete_value(VALUE_NAME);
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    use std::path::PathBuf;

    const SERVICE_NAME: &str = "t10s-keep-awake.service";

    pub fn service_path() -> Result<PathBuf> {
        let base = dirs::config_dir().context("no ~/.config dir")?;
        Ok(base.join("systemd").join("user").join(SERVICE_NAME))
    }

    #[allow(dead_code)]
    pub fn is_enabled() -> bool {
        service_path().map(|p| p.exists()).unwrap_or(false)
    }

    pub fn set_enabled(enable: bool) -> Result<()> {
        let path = service_path()?;
        if enable {
            let exe = std::env::current_exe().context("current exe path")?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            let unit = format!(
                "[Unit]\n\
                 Description=Adam Audio T10S Subwoofer Keep-Awake Daemon\n\
                 After=sound.target pipewire.service pulseaudio.service\n\
                 \n\
                 [Service]\n\
                 Type=simple\n\
                 ExecStart=\"{}\" run\n\
                 Restart=on-failure\n\
                 RestartSec=5\n\
                 \n\
                 [Install]\n\
                 WantedBy=default.target\n",
                exe.display()
            );
            std::fs::write(&path, unit)
                .with_context(|| format!("writing {}", path.display()))?;
            // Best-effort: reload and enable via systemctl --user if systemd is running
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .status();
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "enable", SERVICE_NAME])
                .status();
        } else if path.exists() {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "disable", SERVICE_NAME])
                .status();
            let _ = std::fs::remove_file(&path);
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .status();
        }
        Ok(())
    }
}

#[allow(unused_imports)]
pub use imp::{is_enabled, set_enabled};

