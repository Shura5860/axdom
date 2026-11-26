pub mod mapping;

#[cfg(target_os = "windows")]
mod native;

#[cfg(target_os = "windows")]
pub use native::{WindowsProvider, watch_windows_events};

#[cfg(not(target_os = "windows"))]
mod stub {
    use async_trait::async_trait;
    use axdom_core::provider::RawWindowTree;
    use axdom_core::{
        AccessibilityProvider, ActionRequest, ActionResult, AxdomError, CheckStatus, DoctorCheck,
        DoctorReport, NodeRef, Result, TreeQuery, WindowInfo,
    };

    /// Placeholder stub when compiling `axdom-windows` on non-Windows hosts.
    pub struct WindowsProvider;

    impl WindowsProvider {
        pub async fn connect() -> Result<Self> {
            Err(AxdomError::BusUnavailable(
                "Windows UI Automation backend is only available on Windows".to_string(),
            ))
        }
    }

    #[async_trait]
    impl AccessibilityProvider for WindowsProvider {
        async fn doctor(&self) -> DoctorReport {
            DoctorReport {
                checks: vec![DoctorCheck {
                    name: "windows_uia".to_string(),
                    status: CheckStatus::Fail,
                    detail: "not running on Windows".to_string(),
                    hint: None,
                }],
            }
        }

        async fn list_windows(&self) -> Result<Vec<WindowInfo>> {
            Err(AxdomError::BusUnavailable("requires Windows".to_string()))
        }

        async fn get_window_tree(&self, _query: &TreeQuery) -> Result<RawWindowTree> {
            Err(AxdomError::BusUnavailable("requires Windows".to_string()))
        }

        async fn verify_node(&self, _node: &NodeRef) -> Result<()> {
            Err(AxdomError::BusUnavailable("requires Windows".to_string()))
        }

        async fn perform_action(
            &self,
            _node: &NodeRef,
            _action: &ActionRequest,
            _verify: bool,
        ) -> Result<ActionResult> {
            Err(AxdomError::BusUnavailable("requires Windows".to_string()))
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub use stub::WindowsProvider;
