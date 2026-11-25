use std::path::{Path, PathBuf};

use axdom_core::{CheckStatus, DoctorCheck, DoctorReport};
use zbus::Connection;

use crate::proxy::{A11yBusProxy, a11y_status_proxy, accessible_proxy};

const CHROMIUM_FLAG: &str = "--force-renderer-accessibility";
const FLAG_CONFIG_FILES: &[&str] = &[
    "brave-flags.conf",
    "chromium-flags.conf",
    "chrome-flags.conf",
    "code-flags.conf",
    "electron-flags.conf",
];

/// Idempotently writes `--force-renderer-accessibility` into standard Linux Chromium/Electron
/// flag files under `$XDG_CONFIG_HOME` (or `$HOME/.config`) so Brave, Chrome, VS Code, and
/// Electron apps expose their accessibility tree automatically on launch.
pub fn apply_chromium_a11y_flags() -> std::io::Result<Vec<String>> {
    let config_dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or_else(|| std::io::Error::other("neither XDG_CONFIG_HOME nor HOME is set"))?;
    apply_chromium_a11y_flags_in(&config_dir)
}

pub fn apply_chromium_a11y_flags_in(config_dir: &Path) -> std::io::Result<Vec<String>> {
    std::fs::create_dir_all(config_dir)?;
    let mut actions = Vec::new();
    for filename in FLAG_CONFIG_FILES {
        let path = config_dir.join(filename);
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        if existing.lines().any(|line| line.trim() == CHROMIUM_FLAG) {
            actions.push(format!("already set: {}", path.display()));
            continue;
        }
        let mut updated = existing;
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(CHROMIUM_FLAG);
        updated.push('\n');
        std::fs::write(&path, updated)?;
        actions.push(format!("updated:     {}", path.display()));
    }
    Ok(actions)
}

pub async fn run_doctor() -> DoctorReport {
    let mut checks = Vec::new();

    // 1. Display server detection (Wayland vs X11)
    let session_type = std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unknown".to_string());
    let wayland_display = std::env::var("WAYLAND_DISPLAY").ok();
    let display_detail = match wayland_display {
        Some(wd) => format!("XDG_SESSION_TYPE={session_type}, WAYLAND_DISPLAY={wd}"),
        None => format!("XDG_SESSION_TYPE={session_type}"),
    };
    checks.push(DoctorCheck {
        name: "display-session".to_string(),
        status: if session_type == "unknown" {
            CheckStatus::Warn
        } else {
            CheckStatus::Ok
        },
        detail: display_detail,
        hint: (session_type == "wayland").then(|| {
            "Wayland isolates global coordinates; axdom prioritizes coordinate-free AT-SPI2 actions."
                .to_string()
        }),
    });

    // 2. Session D-Bus
    let session = match Connection::session().await {
        Ok(conn) => {
            checks.push(DoctorCheck {
                name: "session-dbus".to_string(),
                status: CheckStatus::Ok,
                detail: "Connected to user session D-Bus".to_string(),
                hint: None,
            });
            conn
        }
        Err(e) => {
            checks.push(DoctorCheck {
                name: "session-dbus".to_string(),
                status: CheckStatus::Fail,
                detail: format!("Failed to connect: {e}"),
                hint: Some(
                    "Ensure DBUS_SESSION_BUS_ADDRESS is set and you are running inside a desktop session."
                        .to_string(),
                ),
            });
            return DoctorReport { checks };
        }
    };

    // 3. org.a11y.Status (IsEnabled & ScreenReaderEnabled)
    if let Ok(status_proxy) = a11y_status_proxy(&session).await {
        let is_enabled = status_proxy.is_enabled().await.unwrap_or(false);
        let sr_res = status_proxy.screen_reader_enabled().await;
        let (sr_label, needs_hint) = match sr_res {
            Ok(true) => ("true", !is_enabled),
            Ok(false) => ("false", true),
            Err(_) => ("unsupported", true),
        };

        checks.push(DoctorCheck {
            name: "a11y-status".to_string(),
            status: if is_enabled {
                CheckStatus::Ok
            } else {
                CheckStatus::Warn
            },
            detail: format!("IsEnabled={is_enabled}, ScreenReaderEnabled={sr_label}"),
            hint: needs_hint.then(|| {
                "Chromium/Electron/Brave apps need ScreenReaderEnabled=true or --force-renderer-accessibility. Run `axdom doctor --fix` once to configure flags.conf."
                    .to_string()
            }),
        });
    } else {
        checks.push(DoctorCheck {
            name: "a11y-status".to_string(),
            status: CheckStatus::Warn,
            detail: "Could not query org.a11y.Status".to_string(),
            hint: None,
        });
    }

    // 4. org.a11y.Bus & AT-SPI2 Registry
    let a11y_addr = match A11yBusProxy::new(&session).await {
        Ok(bus_proxy) => match bus_proxy.get_address().await {
            Ok(addr) if !addr.trim().is_empty() => {
                checks.push(DoctorCheck {
                    name: "a11y-bus".to_string(),
                    status: CheckStatus::Ok,
                    detail: format!("AT-SPI2 bus address: {addr}"),
                    hint: None,
                });
                Some(addr)
            }
            Ok(_) => {
                checks.push(DoctorCheck {
                    name: "a11y-bus".to_string(),
                    status: CheckStatus::Fail,
                    detail: "org.a11y.Bus returned an empty address".to_string(),
                    hint: Some(
                        "Check that at-spi-bus-launcher is installed and running.".to_string(),
                    ),
                });
                None
            }
            Err(e) => {
                checks.push(DoctorCheck {
                    name: "a11y-bus".to_string(),
                    status: CheckStatus::Fail,
                    detail: format!("org.a11y.Bus.GetAddress failed: {e}"),
                    hint: Some("Install/start at-spi2-core (at-spi-bus-launcher).".to_string()),
                });
                None
            }
        },
        Err(e) => {
            checks.push(DoctorCheck {
                name: "a11y-bus".to_string(),
                status: CheckStatus::Fail,
                detail: format!("Failed to create org.a11y.Bus proxy: {e}"),
                hint: None,
            });
            None
        }
    };

    if let Some(addr) = a11y_addr {
        match zbus::connection::Builder::address(addr.as_str()) {
            Ok(builder) => match builder.build().await {
                Ok(a11y_conn) => {
                    match accessible_proxy(
                        &a11y_conn,
                        "org.a11y.atspi.Registry",
                        "/org/a11y/atspi/accessible/root",
                    )
                    .await
                    {
                        Ok(root) => match root.child_count().await {
                            Ok(count) => checks.push(DoctorCheck {
                                name: "atspi-registry".to_string(),
                                status: CheckStatus::Ok,
                                detail: format!(
                                    "AT-SPI2 Registry reachable ({count} registered applications)"
                                ),
                                hint: None,
                            }),
                            Err(e) => checks.push(DoctorCheck {
                                name: "atspi-registry".to_string(),
                                status: CheckStatus::Fail,
                                detail: format!("Failed to read registry root: {e}"),
                                hint: None,
                            }),
                        },
                        Err(e) => checks.push(DoctorCheck {
                            name: "atspi-registry".to_string(),
                            status: CheckStatus::Fail,
                            detail: format!("Failed to build root proxy: {e}"),
                            hint: None,
                        }),
                    }
                }
                Err(e) => checks.push(DoctorCheck {
                    name: "atspi-registry".to_string(),
                    status: CheckStatus::Fail,
                    detail: format!("Could not connect to AT-SPI2 bus: {e}"),
                    hint: None,
                }),
            },
            Err(e) => checks.push(DoctorCheck {
                name: "atspi-registry".to_string(),
                status: CheckStatus::Fail,
                detail: format!("Invalid AT-SPI2 bus address: {e}"),
                hint: None,
            }),
        }
    }

    DoctorReport { checks }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_chromium_flags_idempotently() {
        let tmp = std::env::temp_dir().join(format!("axdom-flags-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        let first = apply_chromium_a11y_flags_in(&tmp).unwrap();
        assert_eq!(first.len(), FLAG_CONFIG_FILES.len());
        assert!(first.iter().all(|s| s.starts_with("updated:")));

        let second = apply_chromium_a11y_flags_in(&tmp).unwrap();
        assert!(second.iter().all(|s| s.starts_with("already set:")));

        let brave = std::fs::read_to_string(tmp.join("brave-flags.conf")).unwrap();
        assert_eq!(brave.trim(), CHROMIUM_FLAG);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
