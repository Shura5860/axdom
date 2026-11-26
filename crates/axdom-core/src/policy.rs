use crate::error::{AxdomError, Result};
use crate::model::WindowInfo;

/// Default sensitive applications blocked from agent inspection and control.
/// Covers password managers, authentication agents, private browser windows, and encrypted messengers.
pub const DEFAULT_DENY_APPS: &[&str] = &[
    "keepassxc",
    "org.keepassxc.keepassxc",
    "1password",
    "bitwarden",
    "com.bitwarden.desktop",
    "polkit",
    "gnome-keyring",
    "kwallet",
    "gpg-agent",
    "pinentry",
    "signal-desktop",
    "org.signal.signal",
    "telegram-desktop",
    "org.telegram.desktop",
    "incognito",
    "inprivate",
    "private browsing",
];

/// Security policy governing which desktop applications an AI agent may read or control.
#[derive(Debug, Clone)]
pub struct AppPolicy {
    pub deny_apps: Vec<String>,
    pub allow_apps: Vec<String>,
    pub read_only: bool,
    /// When false (default), `launch_app` only permits known desktop GUI apps or `.desktop` launchers
    /// and blocks arbitrary shells/binaries (e.g. `sh`, `bash`, `python`).
    pub allow_launch_any: bool,
}

impl Default for AppPolicy {
    fn default() -> Self {
        Self {
            deny_apps: DEFAULT_DENY_APPS.iter().map(|s| (*s).to_string()).collect(),
            allow_apps: Vec::new(),
            read_only: false,
            allow_launch_any: false,
        }
    }
}

impl AppPolicy {
    pub fn check_read(&self, window: &WindowInfo) -> Result<()> {
        let app = window.app_name.to_lowercase();
        let title = window.title.to_lowercase();

        for denied in &self.deny_apps {
            let needle = denied.to_lowercase();
            if !needle.is_empty() && (app.contains(&needle) || title.contains(&needle)) {
                return Err(AxdomError::PolicyDenied(format!(
                    "window {:?} (app: {:?}) is blocked by denylist rule {:?}",
                    window.title, window.app_name, denied
                )));
            }
        }

        if !self.allow_apps.is_empty() {
            let allowed = self.allow_apps.iter().any(|a| {
                let needle = a.to_lowercase();
                !needle.is_empty() && (app.contains(&needle) || title.contains(&needle))
            });
            if !allowed {
                return Err(AxdomError::PolicyDenied(format!(
                    "window {:?} (app: {:?}) is not in allowlist",
                    window.title, window.app_name
                )));
            }
        }

        Ok(())
    }

    pub fn check_write(&self, window: &WindowInfo) -> Result<()> {
        self.check_read(window)?;
        if self.read_only {
            return Err(AxdomError::PolicyDenied(
                "axdom is running in read-only mode; write actions are disabled".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(app: &str, title: &str) -> WindowInfo {
        WindowInfo {
            id: "w1".into(),
            title: title.into(),
            app_name: app.into(),
            pid: Some(1),
            active: true,
        }
    }

    #[test]
    fn blocks_default_password_managers() {
        let policy = AppPolicy::default();
        assert!(policy.check_read(&win("keepassxc", "Vault.kdbx")).is_err());
        assert!(
            policy
                .check_read(&win("polkit-gnome", "Authenticate"))
                .is_err()
        );
        assert!(policy.check_read(&win("firefox", "Rust Docs")).is_ok());
        assert!(policy.check_read(&win("nvim", "signal.rs — nvim")).is_ok());
        assert!(policy.check_read(&win("signal-desktop", "Signal")).is_err());
        assert!(
            policy
                .check_read(&win("org.signal.signal", "Messages"))
                .is_err()
        );
    }

    #[test]
    fn enforces_allowlist_and_read_only() {
        let policy = AppPolicy {
            deny_apps: vec![],
            allow_apps: vec!["firefox".into()],
            read_only: true,
            allow_launch_any: false,
        };
        assert!(policy.check_read(&win("firefox", "Home")).is_ok());
        assert!(policy.check_write(&win("firefox", "Home")).is_err());
        assert!(policy.check_read(&win("alacritty", "Terminal")).is_err());
    }
}
