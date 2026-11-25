use axdom_core::{AxdomError, Result};
use zbus::Connection;

use crate::proxy::{A11yBusProxy, a11y_status_proxy};

/// Connects to the Linux AT-SPI2 accessibility D-Bus via the user session bus.
pub async fn connect_a11y_bus() -> Result<Connection> {
    let session = Connection::session()
        .await
        .map_err(|e| AxdomError::BusUnavailable(format!("session D-Bus: {e}")))?;

    let bus_proxy = A11yBusProxy::new(&session)
        .await
        .map_err(|e| AxdomError::BusUnavailable(format!("org.a11y.Bus proxy: {e}")))?;

    let addr = bus_proxy
        .get_address()
        .await
        .map_err(|e| AxdomError::BusUnavailable(format!("org.a11y.Bus.GetAddress: {e}")))?;

    if addr.trim().is_empty() {
        return Err(AxdomError::BusUnavailable(
            "org.a11y.Bus returned an empty address".to_string(),
        ));
    }

    zbus::connection::Builder::address(addr.as_str())
        .map_err(|e| AxdomError::BusUnavailable(format!("invalid a11y bus address '{addr}': {e}")))?
        .build()
        .await
        .map_err(|e| AxdomError::BusUnavailable(format!("connect to a11y bus '{addr}': {e}")))
}

/// Ensures `org.a11y.Status.IsEnabled` is turned on so toolkits (GTK/Qt/Electron) publish trees.
/// Returns a list of non-fatal warnings if a property could not be updated (e.g. read-only on KDE).
pub async fn enable_a11y_if_needed(enable_screen_reader: bool) -> Result<Vec<String>> {
    let session = Connection::session()
        .await
        .map_err(|e| AxdomError::BusUnavailable(format!("session D-Bus: {e}")))?;

    let status = a11y_status_proxy(&session)
        .await
        .map_err(|e| AxdomError::BusUnavailable(format!("org.a11y.Status proxy: {e}")))?;

    let mut warnings = Vec::new();

    if !status.is_enabled().await.unwrap_or(false)
        && let Err(e) = status.set_is_enabled(true).await
    {
        warnings.push(format!("Could not set org.a11y.Status.IsEnabled=true: {e}"));
    }
    if enable_screen_reader {
        match status.screen_reader_enabled().await {
            Ok(true) => {}
            Ok(false) => match status.set_screen_reader_enabled(true).await {
                Ok(()) => {
                    if !status.screen_reader_enabled().await.unwrap_or(false) {
                        warnings.push(
                            "org.a11y.Status.ScreenReaderEnabled stayed false; run `axdom doctor --fix` once for Chromium/Electron/Brave apps."
                                .to_string(),
                        );
                    }
                }
                Err(e) => {
                    warnings.push(format!(
                        "Could not set org.a11y.Status.ScreenReaderEnabled=true: {e}"
                    ));
                }
            },
            Err(_) => {
                warnings.push(
                    "org.a11y.Status.ScreenReaderEnabled is not exposed by this desktop; run `axdom doctor --fix` once for Chromium/Electron/Brave apps."
                        .to_string(),
                );
            }
        }
    }
    Ok(warnings)
}
