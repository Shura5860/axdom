use std::io;
use std::sync::Arc;

use axdom_core::AccessibilityProvider;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::protocol::{JsonRpcRequest, JsonRpcResponse};
use crate::server::McpServer;

/// Runs the MCP server over newline-delimited JSON-RPC on stdin/stdout.
pub async fn serve_stdio<P: AccessibilityProvider>(server: Arc<McpServer<P>>) -> io::Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<JsonRpcRequest>(trimmed) {
            Ok(req) => server.handle_request(req).await,
            Err(e) => Some(JsonRpcResponse::err(
                serde_json::Value::Null,
                -32700,
                format!("parse error: {e}"),
            )),
        };

        if let Some(resp) = response {
            let mut payload = serde_json::to_string(&resp)
                .map_err(|e| io::Error::other(format!("serialize response: {e}")))?;
            payload.push('\n');
            stdout.write_all(payload.as_bytes()).await?;
            stdout.flush().await?;
        }
    }

    Ok(())
}
