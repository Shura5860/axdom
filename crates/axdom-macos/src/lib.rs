pub mod mapping;

#[cfg(target_os = "macos")]
mod native;

#[cfg(target_os = "macos")]
pub use native::{MacOsProvider, watch_macos_events};

#[cfg(not(target_os = "macos"))]
mod stub {
    use async_trait::async_trait;
    use axdom_core::provider::RawWindowTree;
    use axdom_core::{
        AccessibilityProvider, ActionRequest, ActionResult, AxdomError, CheckStatus, DoctorCheck,
        DoctorReport, NodeRef, Result, TreeQuery, WindowInfo,
    };

    /// Placeholder stub when compiling `axdom-macos` on non-macOS hosts.
    pub struct MacOsProvider;

    impl MacOsProvider {
        pub async fn connect() -> Result<Self> {
            Err(AxdomError::BusUnavailable(
                "macOS Accessibility (AXUIElement) backend is only available on macOS".to_string(),
            ))
        }
    }

    #[async_trait]
    impl AccessibilityProvider for MacOsProvider {
        async fn doctor(&self) -> DoctorReport {
            DoctorReport {
                checks: vec![DoctorCheck {
                    name: "macos_accessibility".to_string(),
                    status: CheckStatus::Fail,
                    detail: "not running on macOS".to_string(),
                    hint: None,
                }],
            }
        }

        async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
            Err(AxdomError::BusUnavailable("requires macOS".to_string()))
        }

        async fn get_window_tree(&self, _query: &TreeQuery) -> Result<RawWindowTree> {
            Err(AxdomError::BusUnavailable("requires macOS".to_string()))
        }

        async fn verify_node(&self, _node: &NodeRef) -> Result<()> {
            Err(AxdomError::BusUnavailable("requires macOS".to_string()))
        }

        async fn perform_action(
            &self,
            _node: &NodeRef,
            _action: &ActionRequest,
            _verify: bool,
        ) -> Result<ActionResult> {
            Err(AxdomError::BusUnavailable("requires macOS".to_string()))
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub use stub::MacOsProvider;
