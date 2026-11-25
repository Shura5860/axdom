use axdom_core::{CheckStatus, DoctorCheck, DoctorReport};
use zbus::Connection;

use crate::proxy::{A11yBusProxy, a11y_status_proxy, accessible_proxy};

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
        let sr_enabled = status_proxy.screen_reader_enabled().await.unwrap_or(false);

        checks.push(DoctorCheck {
            name: "a11y-status".to_string(),
            status: if is_enabled {
                CheckStatus::Ok
            } else {
                CheckStatus::Warn
            },
            detail: format!("IsEnabled={is_enabled}, ScreenReaderEnabled={sr_enabled}"),
            hint: (!is_enabled || !sr_enabled).then(|| {
                "Chromium/Electron apps only expose their accessibility tree when ScreenReaderEnabled=true or launched with --force-renderer-accessibility."
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
