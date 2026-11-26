use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitMode {
    Apply,
    DryRun,
    Undo,
}

struct McpClientTarget {
    name: &'static str,
    path: PathBuf,
    only_if_parent_exists: bool,
    /// When true, only cleaned up during `--undo` (never modified on apply/dry-run).
    undo_only: bool,
}

/// Discovers installed MCP clients (Claude Desktop, Cursor, Windsurf, Antigravity)
/// and registers or removes `mcpServers.axdom` pointing to the current `axdom` executable.
pub fn configure_mcp_clients(exe_path: &Path, mode: InitMode) -> Result<Vec<String>> {
    let targets = discover_mcp_targets();
    let mut actions = Vec::new();
    let exe_str = exe_path.to_string_lossy().to_string();

    for target in targets {
        if target.undo_only && mode != InitMode::Undo {
            continue;
        }
        let parent_exists = target.path.parent().is_some_and(Path::is_dir);
        let file_exists = target.path.is_file();
        if mode != InitMode::Undo && target.only_if_parent_exists && !parent_exists && !file_exists
        {
            continue;
        }
        if mode == InitMode::Undo && !file_exists {
            continue;
        }

        match patch_mcp_json(&target.path, target.name, &exe_str, mode) {
            Ok(Some(msg)) => actions.push(msg),
            Ok(None) => {}
            Err(e) => actions.push(format!(
                "skipped ({}): {} ({e})",
                target.name,
                target.path.display()
            )),
        }
    }

    if actions.is_empty() {
        actions.push(
            "no MCP client configs detected (Claude Desktop, Cursor, Windsurf, Antigravity)"
                .to_string(),
        );
    }

    Ok(actions)
}

fn discover_mcp_targets() -> Vec<McpClientTarget> {
    let mut out = Vec::new();
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let Some(home) = home else {
        return out;
    };

    #[cfg(target_os = "macos")]
    let claude_path = home
        .join("Library")
        .join("Application Support")
        .join("Claude")
        .join("claude_desktop_config.json");
    #[cfg(target_os = "windows")]
    let claude_path = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("AppData").join("Roaming"))
        .join("Claude")
        .join("claude_desktop_config.json");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let claude_path = home
        .join(".config")
        .join("Claude")
        .join("claude_desktop_config.json");

    out.push(McpClientTarget {
        name: "Claude Desktop",
        path: claude_path,
        only_if_parent_exists: true,
        undo_only: false,
    });
    out.push(McpClientTarget {
        name: "Cursor",
        path: home.join(".cursor").join("mcp.json"),
        only_if_parent_exists: true,
        undo_only: false,
    });
    out.push(McpClientTarget {
        name: "Windsurf",
        path: home
            .join(".codeium")
            .join("windsurf")
            .join("mcp_config.json"),
        only_if_parent_exists: true,
        undo_only: false,
    });
    out.push(McpClientTarget {
        name: "Antigravity",
        path: home.join(".gemini").join("config").join("mcp_config.json"),
        only_if_parent_exists: true,
        undo_only: false,
    });
    // Legacy cleanup only: never write to ~/.claude.json on apply, but clean up older entries on --undo.
    out.push(McpClientTarget {
        name: "Claude Code (legacy)",
        path: home.join(".claude.json"),
        only_if_parent_exists: true,
        undo_only: true,
    });

    out
}

pub fn patch_mcp_json(
    path: &Path,
    client_name: &str,
    exe_path: &str,
    mode: InitMode,
) -> Result<Option<String>> {
    let mut root: Value = if path.is_file() {
        let raw = std::fs::read_to_string(path)?;
        if raw.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&raw)?
        }
    } else {
        json!({})
    };

    let Some(obj) = root.as_object_mut() else {
        return Ok(None);
    };

    if mode == InitMode::Undo {
        let removed = obj
            .get_mut("mcpServers")
            .and_then(Value::as_object_mut)
            .and_then(|s| s.remove("axdom"))
            .is_some();
        if !removed {
            return Ok(None);
        }
        let rendered = format!("{}\n", serde_json::to_string_pretty(&root)?);
        atomic_write(path, rendered.as_bytes())?;
        return Ok(Some(format!(
            "removed MCP entry ({client_name}): {}",
            path.display()
        )));
    }

    let desired = json!({
        "command": exe_path,
        "args": ["mcp", "--enable-a11y"]
    });

    let already_matches = obj
        .get("mcpServers")
        .and_then(|s| s.get("axdom"))
        .is_some_and(|cur| cur == &desired);
    if already_matches {
        return Ok(Some(format!(
            "already configured ({client_name}): {}",
            path.display()
        )));
    }

    if mode == InitMode::DryRun {
        return Ok(Some(format!(
            "would configure ({client_name}): {}",
            path.display()
        )));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.is_file() {
        let mut bak = path.as_os_str().to_os_string();
        bak.push(".axdom.bak");
        let bak_path = PathBuf::from(bak);
        if !bak_path.exists() {
            let _ = std::fs::copy(path, bak_path);
        }
    }

    let servers = obj
        .entry("mcpServers".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(map) = servers.as_object_mut() {
        map.insert("axdom".to_string(), desired);
    }

    let rendered = format!("{}\n", serde_json::to_string_pretty(&root)?);
    atomic_write(path, rendered.as_bytes())?;
    Ok(Some(format!(
        "configured ({client_name}): {}",
        path.display()
    )))
}

fn atomic_write(path: &Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let existing_perms = std::fs::metadata(path).ok().map(|m| m.permissions());

    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(format!(".axdom.{}.tmp", std::process::id()));
    let tmp_path = PathBuf::from(tmp);

    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mode = existing_perms
            .as_ref()
            .map(|p| p.mode() & 0o777)
            .unwrap_or(0o600);
        opts.mode(mode);
    }
    let mut file = opts.open(&tmp_path)?;
    file.write_all(content)?;
    file.sync_all()?;
    drop(file);

    if let Some(perms) = existing_perms {
        let _ = std::fs::set_permissions(&tmp_path, perms);
    }
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_and_undoes_mcp_config_preserving_existing_servers() {
        let tmp = std::env::temp_dir().join(format!("axdom-init-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let cfg = tmp.join("mcp_config.json");
        std::fs::write(
            &cfg,
            r#"{"mcpServers":{"other":{"command":"other-bin","args":[]}}}"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let dry = patch_mcp_json(&cfg, "Test", "/usr/bin/axdom", InitMode::DryRun)
            .unwrap()
            .unwrap();
        assert!(dry.starts_with("would configure"));

        let applied = patch_mcp_json(&cfg, "Test", "/usr/bin/axdom", InitMode::Apply)
            .unwrap()
            .unwrap();
        assert!(applied.starts_with("configured"));

        let second = patch_mcp_json(&cfg, "Test", "/usr/bin/axdom", InitMode::Apply)
            .unwrap()
            .unwrap();
        assert!(second.starts_with("already configured"));

        let parsed: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(parsed["mcpServers"]["other"]["command"], "other-bin");
        assert_eq!(parsed["mcpServers"]["axdom"]["command"], "/usr/bin/axdom");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let undone = patch_mcp_json(&cfg, "Test", "/usr/bin/axdom", InitMode::Undo)
            .unwrap()
            .unwrap();
        assert!(undone.starts_with("removed"));
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert!(after["mcpServers"].get("axdom").is_none());
        assert_eq!(after["mcpServers"]["other"]["command"], "other-bin");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
