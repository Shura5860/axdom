use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use axdom_core::provider::RawWindowTree;
use axdom_core::{
    AccessibilityProvider, ActionRequest, AppPolicy, AxdomError, Bounds, CheckStatus, KeyCombo,
    NodeRole, NodeState, OutputFormat, Pruner, PrunerConfig, RawNode, SnapshotStore, TreeQuery,
    WindowInfo, find_node_ref, format_snapshot, format_windows, scope_snapshot,
};
use axdom_daemon::{DaemonClient, DaemonState, default_socket_path, serve_unix};
#[cfg(target_os = "linux")]
use axdom_linux::LinuxProvider as PlatformProvider;
#[cfg(target_os = "linux")]
use axdom_linux::bus::enable_a11y_if_needed;
#[cfg(target_os = "linux")]
use axdom_linux::doctor::run_doctor;
#[cfg(target_os = "linux")]
use axdom_linux::events::watch_events;
#[cfg(target_os = "linux")]
use axdom_linux::setup::{SetupMode, run_full_setup, run_setup};
#[cfg(target_os = "macos")]
use axdom_macos::MacOsProvider as PlatformProvider;
use axdom_mcp::{McpServer, serve_stdio};
#[cfg(target_os = "windows")]
use axdom_windows::WindowsProvider as PlatformProvider;
use clap::{Parser, Subcommand};

#[cfg(not(target_os = "linux"))]
async fn enable_a11y_if_needed(_enable_screen_reader: bool) -> axdom_core::Result<Vec<String>> {
    Ok(Vec::new())
}

#[cfg(not(target_os = "linux"))]
async fn run_full_setup() -> axdom_core::Result<Vec<String>> {
    Ok(vec![
        "no extra setup required on this OS (accessibility uses native OS APIs)".to_string(),
    ])
}

#[cfg(not(target_os = "linux"))]
async fn run_doctor() -> axdom_core::DoctorReport {
    match PlatformProvider::connect().await {
        Ok(p) => p.doctor().await,
        Err(e) => axdom_core::DoctorReport {
            checks: vec![axdom_core::DoctorCheck {
                name: "platform_a11y".to_string(),
                status: CheckStatus::Fail,
                detail: e.to_string(),
                hint: None,
            }],
        },
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "axdom",
    version,
    about = "Zero-latency desktop accessibility DOM engine for AI agents"
)]
struct Cli {
    /// Emit machine-readable JSON output instead of compact YAML-like format.
    #[arg(long, global = true)]
    json: bool,

    /// Bypass a running `axdom daemon` and talk directly to AT-SPI2.
    #[arg(long, global = true)]
    direct: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// One-time Linux setup: configure environment.d, Chromium/Electron/Flatpak flags, and .desktop overrides.
    Setup {
        /// Preview changes without modifying any files or Flatpak overrides.
        #[arg(long)]
        dry_run: bool,

        /// Revert changes previously made by `axdom setup` (restoring backups when present).
        #[arg(long)]
        undo: bool,
    },

    /// Check Linux AT-SPI2 bus, display session, and accessibility flags.
    Doctor {
        /// Attempt to enable org.a11y.Status IsEnabled & ScreenReaderEnabled on the session bus.
        #[arg(long)]
        enable_a11y: bool,

        /// One-time setup: enable AT-SPI2 and add --force-renderer-accessibility to ~/.config/{brave,chromium,chrome,code,electron}-flags.conf.
        #[arg(long)]
        fix: bool,
    },

    /// List open desktop windows registered on AT-SPI2.
    Windows,

    /// Dump the pruned accessibility tree of the active (or filtered) window and cache its IDs.
    Tree {
        /// Target window by title substring, app name, or window ID (defaults to active window).
        #[arg(short, long)]
        window: Option<String>,

        /// Maximum tree traversal depth.
        #[arg(long, default_value_t = 12)]
        depth: usize,

        /// Maximum raw nodes to inspect before truncating.
        #[arg(long, default_value_t = 1500)]
        max_nodes: usize,

        /// Traversal timeout budget in milliseconds.
        #[arg(long, default_value_t = 2000)]
        timeout_ms: u64,

        /// Force a fresh AT-SPI2 read even if the daemon holds a warm cached snapshot.
        #[arg(long)]
        refresh: bool,

        /// Scope the printed output to a subtree rooted at a specific node ID (e.g. `#b4f`).
        #[arg(long)]
        root: Option<String>,
    },

    /// Click an interactive node from the latest snapshot (e.g. `axdom click #b4f`).
    Click {
        /// Node ID from `axdom tree` (with or without leading `#`).
        id: String,

        /// Skip live role/name staleness verification before dispatching.
        #[arg(long)]
        no_verify: bool,

        /// Fall back to clicking the node's center screen coordinates if AT-SPI2 exposes no actions.
        #[arg(long)]
        fallback_input: bool,
    },

    /// Set text contents of an editable node from the latest snapshot.
    Type {
        /// Node ID from `axdom tree` (with or without leading `#`).
        id: String,

        /// Text to write into the editable node.
        text: String,

        /// Press Enter immediately after writing text.
        #[arg(long)]
        submit: bool,

        /// Skip live role/name staleness verification before dispatching.
        #[arg(long)]
        no_verify: bool,
    },

    /// Scroll vertically and/or horizontally on a node.
    Scroll {
        /// Node ID from `axdom tree` (with or without leading `#`).
        id: String,

        /// Vertical scroll steps (e.g. `5` for down, `-5` for up).
        #[arg(allow_hyphen_values = true, default_value_t = 0)]
        delta_y: i32,

        /// Horizontal scroll steps (positive = right, negative = left).
        #[arg(long, allow_hyphen_values = true, default_value_t = 0)]
        delta_x: i32,

        /// Skip live role/name staleness verification before dispatching.
        #[arg(long)]
        no_verify: bool,
    },

    /// Right-click a node (context menu) using its screen bounds.
    RightClick {
        id: String,
        #[arg(long)]
        no_verify: bool,
    },

    /// Double-click a node using its screen bounds.
    DoubleClick {
        id: String,
        #[arg(long)]
        no_verify: bool,
    },

    /// Hover the pointer over a node using its screen bounds.
    Hover {
        id: String,
        #[arg(long)]
        no_verify: bool,
    },

    /// Drag a node to screen coordinates `(to_x, to_y)`.
    Drag {
        id: String,
        to_x: i32,
        to_y: i32,
        #[arg(long)]
        no_verify: bool,
    },

    /// Grab keyboard focus on a node from the latest snapshot.
    Focus {
        /// Node ID from `axdom tree` (with or without leading `#`).
        id: String,

        /// Skip live role/name staleness verification before dispatching.
        #[arg(long)]
        no_verify: bool,
    },

    /// Set the numeric value of a slider or range node from the latest snapshot.
    SetValue {
        /// Node ID from `axdom tree` (with or without leading `#`).
        id: String,

        /// Target numeric value.
        value: f64,

        /// Skip live role/name staleness verification before dispatching.
        #[arg(long)]
        no_verify: bool,
    },

    /// Send a keyboard shortcut (e.g. `axdom key "Ctrl+S"`, `axdom key Enter`).
    Key {
        /// Key or modifier+key combination.
        combo: String,
    },

    /// Capture a PNG screenshot (blind-spot visual fallback).
    Screenshot {
        /// Output file path (prints base64 summary if omitted).
        #[arg(short, long)]
        out: Option<PathBuf>,
    },

    /// Stream live AT-SPI2 focus, window, and object-tree events.
    Watch,

    /// Manage the local Unix-socket daemon with hot event-invalidated cache.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },

    /// Start the Model Context Protocol (MCP) server over stdio.
    Mcp {
        /// Restrict access exclusively to matching app/window substrings (repeatable).
        #[arg(long = "allow-app")]
        allow_apps: Vec<String>,

        /// Add extra app/window substrings to the default security denylist (repeatable).
        #[arg(long = "deny-app")]
        deny_apps: Vec<String>,

        /// Run in read-only mode (disable click/type/focus tools).
        #[arg(long)]
        read_only: bool,

        /// Enable org.a11y.Status IsEnabled & ScreenReaderEnabled on startup.
        #[arg(long)]
        enable_a11y: bool,
    },

    /// Launch an application with accessibility environment variables and Electron/Chromium flags.
    Launch {
        /// Force adding `--force-renderer-accessibility` even for unrecognized binary names.
        #[arg(long)]
        chromium: bool,

        /// Command and arguments to launch (e.g. `axdom launch code .`).
        #[arg(required = true, trailing_var_arg = true)]
        command: Vec<String>,
    },

    /// Benchmark tree traversal, pruning latency, and token compression ratio.
    Bench {
        /// Run an offline synthetic 2,000-node pruner/serializer benchmark without AT-SPI2.
        #[arg(long)]
        synthetic: bool,

        /// Target window filter for live AT-SPI2 benchmarking.
        #[arg(short, long)]
        window: Option<String>,

        /// Number of iterations to average.
        #[arg(long, default_value_t = 5)]
        runs: usize,
    },
}

#[derive(Debug, Subcommand)]
enum DaemonAction {
    /// Start the foreground Unix-socket daemon and live AT-SPI2 cache invalidator.
    Start {
        /// Custom Unix socket path (defaults to $XDG_RUNTIME_DIR/axdom/axdom.sock).
        #[arg(long)]
        socket: Option<PathBuf>,

        /// Restrict access exclusively to matching app/window substrings (repeatable).
        #[arg(long = "allow-app")]
        allow_apps: Vec<String>,

        /// Add extra app/window substrings to the default security denylist (repeatable).
        #[arg(long = "deny-app")]
        deny_apps: Vec<String>,

        /// Run in read-only mode (disable click/type/focus).
        #[arg(long)]
        read_only: bool,

        /// Enable org.a11y.Status IsEnabled & ScreenReaderEnabled on startup.
        #[arg(long)]
        enable_a11y: bool,
    },

    /// Query metrics and cache status from a running daemon.
    Status {
        /// Custom Unix socket path.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Always log to stderr so stdout remains clean for CLI piping and stdio MCP JSON-RPC.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let format = if cli.json {
        OutputFormat::Json
    } else {
        OutputFormat::Compact
    };

    match cli.command {
        Commands::Setup { dry_run, undo } => {
            #[cfg(target_os = "linux")]
            let actions = {
                let mode = if undo {
                    SetupMode::Undo
                } else if dry_run {
                    SetupMode::DryRun
                } else {
                    let _ = enable_a11y_if_needed(true).await;
                    SetupMode::Apply
                };
                run_setup(mode).await?
            };
            #[cfg(not(target_os = "linux"))]
            let actions = {
                let _ = (dry_run, undo);
                run_full_setup().await?
            };
            if format == OutputFormat::Json {
                println!("{}", serde_json::to_string_pretty(&actions)?);
            } else {
                for act in &actions {
                    println!("[SETUP] {act}");
                }
                if !dry_run && !undo {
                    println!(
                        "Done. Fully restart any open Chromium/Electron/Brave apps (Ctrl+Q) for flags to take effect."
                    );
                }
            }
        }

        Commands::Doctor { enable_a11y, fix } => {
            let mut enable_warnings = Vec::new();
            let mut fix_actions = Vec::new();
            if enable_a11y || fix {
                match enable_a11y_if_needed(true).await {
                    Ok(warns) => enable_warnings = warns,
                    Err(e) => enable_warnings.push(e.to_string()),
                }
            }
            if fix {
                fix_actions = run_full_setup().await?;
            }
            let report = run_doctor().await;
            if format == OutputFormat::Json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "checks": report.checks,
                        "fix_actions": fix_actions,
                        "warnings": enable_warnings,
                    }))?
                );
            } else {
                for check in &report.checks {
                    let badge = match check.status {
                        CheckStatus::Ok => "[OK]  ",
                        CheckStatus::Warn => "[WARN]",
                        CheckStatus::Fail => "[FAIL]",
                    };
                    println!("{badge} {}: {}", check.name, check.detail);
                    if let Some(hint) = &check.hint {
                        println!("       hint: {hint}");
                    }
                }
                for act in &fix_actions {
                    println!("[FIX]  {act}");
                }
                if !fix {
                    for warn in &enable_warnings {
                        println!("[WARN] enable-a11y: {warn}");
                    }
                }
            }
            if !report.is_healthy() {
                std::process::exit(1);
            }
        }

        Commands::Windows => {
            let windows = if !cli.direct
                && let Ok(mut client) = DaemonClient::connect_default().await
            {
                client.list_windows().await?
            } else {
                let policy = AppPolicy::default();
                let provider = PlatformProvider::connect().await?;
                provider
                    .list_windows()
                    .await?
                    .into_iter()
                    .filter(|w| policy.check_read(w).is_ok())
                    .collect()
            };
            print!("{}", format_windows(&windows, format));
        }

        Commands::Tree {
            window,
            depth,
            max_nodes,
            timeout_ms,
            refresh,
            root,
        } => {
            let query = TreeQuery {
                window_filter: window,
                max_depth: depth,
                max_nodes,
                timeout_ms,
            };

            let snapshot = if !cli.direct
                && let Ok(mut client) = DaemonClient::connect_default().await
            {
                let (snap, cache_hit, elapsed_ms) = client.get_tree(&query, refresh).await?;
                tracing::debug!("daemon tree: cache_hit={cache_hit}, elapsed={elapsed_ms}ms");
                snap
            } else {
                let policy = AppPolicy::default();
                let provider = PlatformProvider::connect().await?;
                let raw = provider.get_window_tree(&query).await?;
                policy.check_read(&raw.window)?;
                Pruner::new(PrunerConfig::default()).prune(raw)
            };

            let store = SnapshotStore::default_location();
            if let Err(e) = store.save(&snapshot) {
                tracing::warn!("failed to save snapshot cache: {e}");
            }
            let display = match root {
                Some(ref root_id) => scope_snapshot(&snapshot, root_id)?,
                None => snapshot,
            };
            print!("{}", format_snapshot(&display, format));
        }

        Commands::Click {
            id,
            no_verify,
            fallback_input,
        } => {
            run_action(
                &id,
                ActionRequest::Click,
                !no_verify,
                fallback_input,
                cli.direct,
                format,
            )
            .await?;
        }

        Commands::Type {
            id,
            text,
            submit,
            no_verify,
        } => {
            run_action(
                &id,
                ActionRequest::SetText { text },
                !no_verify,
                false,
                cli.direct,
                format,
            )
            .await?;
            if submit {
                let enter = KeyCombo::parse("Enter")?;
                let res = if !cli.direct
                    && let Ok(mut client) = DaemonClient::connect_default().await
                {
                    client.press_key(enter).await?
                } else {
                    let provider = PlatformProvider::connect().await?;
                    provider.press_key(&enter).await?
                };
                if format != OutputFormat::Json {
                    println!("submit(Enter) -> {} (ok)", res.method_used);
                }
            }
        }

        Commands::Scroll {
            id,
            delta_y,
            delta_x,
            no_verify,
        } => {
            run_action(
                &id,
                ActionRequest::Scroll { delta_x, delta_y },
                !no_verify,
                false,
                cli.direct,
                format,
            )
            .await?;
        }

        Commands::RightClick { id, no_verify } => {
            run_action(
                &id,
                ActionRequest::RightClick,
                !no_verify,
                false,
                cli.direct,
                format,
            )
            .await?;
        }

        Commands::DoubleClick { id, no_verify } => {
            run_action(
                &id,
                ActionRequest::DoubleClick,
                !no_verify,
                false,
                cli.direct,
                format,
            )
            .await?;
        }

        Commands::Hover { id, no_verify } => {
            run_action(
                &id,
                ActionRequest::Hover,
                !no_verify,
                false,
                cli.direct,
                format,
            )
            .await?;
        }

        Commands::Drag {
            id,
            to_x,
            to_y,
            no_verify,
        } => {
            run_action(
                &id,
                ActionRequest::DragTo { to_x, to_y },
                !no_verify,
                false,
                cli.direct,
                format,
            )
            .await?;
        }

        Commands::Focus { id, no_verify } => {
            run_action(
                &id,
                ActionRequest::Focus,
                !no_verify,
                false,
                cli.direct,
                format,
            )
            .await?;
        }

        Commands::SetValue {
            id,
            value,
            no_verify,
        } => {
            run_action(
                &id,
                ActionRequest::SetValue { value },
                !no_verify,
                false,
                cli.direct,
                format,
            )
            .await?;
        }

        Commands::Key { combo } => {
            let parsed = KeyCombo::parse(&combo)?;
            let result = if !cli.direct
                && let Ok(mut client) = DaemonClient::connect_default().await
            {
                client.press_key(parsed).await?
            } else {
                let provider = PlatformProvider::connect().await?;
                provider.press_key(&parsed).await?
            };
            if format == OutputFormat::Json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("key {:?} -> {} (ok)", combo, result.method_used);
            }
        }

        Commands::Screenshot { out } => {
            if let Some(path) = out {
                #[cfg(target_os = "linux")]
                let (bytes, method) = axdom_linux::input::capture_screen_bytes(None).await?;
                #[cfg(not(target_os = "linux"))]
                let (bytes, method) = {
                    let provider = PlatformProvider::connect().await?;
                    let shot = provider.capture_screenshot(None).await?;
                    (
                        axdom_core::decode_base64(&shot.base64_data)?,
                        shot.method_used,
                    )
                };
                tokio::fs::write(&path, &bytes).await?;
                println!(
                    "saved {} bytes to {} via {}",
                    bytes.len(),
                    path.display(),
                    method
                );
            } else {
                let shot = if !cli.direct
                    && let Ok(mut client) = DaemonClient::connect_default().await
                {
                    client.capture_screenshot(None).await?
                } else {
                    let provider = PlatformProvider::connect().await?;
                    provider.capture_screenshot(None).await?
                };
                if format == OutputFormat::Json {
                    println!("{}", serde_json::to_string_pretty(&shot)?);
                } else {
                    println!(
                        "captured {} bytes via {} ({})",
                        shot.bytes_len, shot.method_used, shot.mime_type
                    );
                }
            }
        }

        Commands::Watch => {
            #[cfg(target_os = "linux")]
            {
                let provider = PlatformProvider::connect().await?;
                let conn = provider.connection();
                let (tx, mut rx) = tokio::sync::mpsc::channel(128);

                let watcher = tokio::spawn(async move { watch_events(conn, tx).await });

                while let Some(ev) = rx.recv().await {
                    if format == OutputFormat::Json {
                        println!("{}", serde_json::to_string(&ev)?);
                    } else {
                        println!(
                            "[{}] {} {} ({})",
                            ev.category, ev.kind, ev.detail, ev.source_ref
                        );
                    }
                }

                watcher.await??;
            }
            #[cfg(not(target_os = "linux"))]
            {
                anyhow::bail!("`axdom watch` live event stream is currently Linux AT-SPI2 only");
            }
        }

        Commands::Daemon { action } => match action {
            DaemonAction::Start {
                socket,
                allow_apps,
                deny_apps,
                read_only,
                enable_a11y,
            } => {
                if enable_a11y {
                    let _ = enable_a11y_if_needed(true).await;
                }
                let mut policy = AppPolicy {
                    allow_apps,
                    read_only,
                    ..Default::default()
                };
                policy.deny_apps.extend(deny_apps);

                let provider = Arc::new(PlatformProvider::connect().await?);
                #[cfg(target_os = "linux")]
                let conn = provider.connection();
                let state = Arc::new(DaemonState::new(provider, policy));

                // Background event listener invalidates the hot cache on UI mutations.
                #[cfg(target_os = "linux")]
                {
                    let (tx, mut rx) = tokio::sync::mpsc::channel(128);
                    tokio::spawn(async move {
                        if let Err(e) = watch_events(conn, tx).await {
                            tracing::warn!("AT-SPI2 event watcher exited: {e}");
                        }
                    });
                    let invalidator_state = Arc::clone(&state);
                    tokio::spawn(async move {
                        while let Some(_ev) = rx.recv().await {
                            invalidator_state.invalidate().await;
                        }
                    });
                }
                #[cfg(target_os = "windows")]
                {
                    let (tx, mut rx) = tokio::sync::mpsc::channel(128);
                    tokio::spawn(axdom_windows::watch_windows_events(tx));
                    let invalidator_state = Arc::clone(&state);
                    tokio::spawn(async move {
                        while let Some(_ev) = rx.recv().await {
                            invalidator_state.invalidate().await;
                        }
                    });
                }
                #[cfg(target_os = "macos")]
                {
                    let (tx, mut rx) = tokio::sync::mpsc::channel(128);
                    tokio::spawn(axdom_macos::watch_macos_events(tx));
                    let invalidator_state = Arc::clone(&state);
                    tokio::spawn(async move {
                        while let Some(_ev) = rx.recv().await {
                            invalidator_state.invalidate().await;
                        }
                    });
                }

                let sock_path = socket.unwrap_or_else(default_socket_path);
                eprintln!("axdom daemon listening on {}", sock_path.display());
                serve_unix(&sock_path, state).await?;
            }

            DaemonAction::Status { socket } => {
                let sock_path = socket.unwrap_or_else(default_socket_path);
                let mut client = DaemonClient::connect(&sock_path).await?;
                let status = client.status().await?;
                if format == OutputFormat::Json {
                    println!("{}", serde_json::to_string_pretty(&status)?);
                } else {
                    println!(
                        "daemon pid={} | hits={} misses={} invalidations={}",
                        status.pid, status.cache_hits, status.cache_misses, status.invalidations
                    );
                    if let Some(win) = status.cached_window {
                        println!(
                            "cached: {:?} (snapshot #{})",
                            win,
                            status.cached_snapshot_id.unwrap_or(0)
                        );
                    } else {
                        println!("cached: <empty>");
                    }
                }
            }
        },

        Commands::Mcp {
            allow_apps,
            deny_apps,
            read_only,
            enable_a11y,
        } => {
            if enable_a11y {
                let _ = enable_a11y_if_needed(true).await;
            }
            let mut policy = AppPolicy {
                allow_apps,
                read_only,
                ..Default::default()
            };
            policy.deny_apps.extend(deny_apps);

            let provider = Arc::new(PlatformProvider::connect().await?);
            let mut server = McpServer::new(provider, policy);
            if !cli.direct {
                server = server.with_daemon_socket(default_socket_path());
            }
            serve_stdio(Arc::new(server)).await?;
        }

        Commands::Launch { chromium, command } => {
            let _ = enable_a11y_if_needed(true).await;
            let bin = &command[0];
            let mut args: Vec<String> = command[1..].to_vec();

            let is_chromium_like = chromium || is_known_chromium_app(bin);
            if is_chromium_like && !args.iter().any(|a| a == "--force-renderer-accessibility") {
                args.insert(0, "--force-renderer-accessibility".to_string());
            }

            let mut cmd = tokio::process::Command::new(bin);
            cmd.args(&args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .env("ACCESSIBILITY_ENABLED", "1")
                .env("QT_LINUX_ACCESSIBILITY_ALWAYS_ON", "1")
                .env("GTK_MODULES", "gail:atk-bridge");
            #[cfg(unix)]
            cmd.process_group(0);
            let child = cmd.spawn()?;

            let pid = child.id().unwrap_or(0);
            if format == OutputFormat::Json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "pid": pid,
                        "binary": bin,
                        "args": args,
                        "chromium_flag_injected": is_chromium_like,
                    }))?
                );
            } else {
                println!(
                    "launched {bin} (pid: {pid}) | chromium_a11y_flag={is_chromium_like} | args={args:?}"
                );
            }
        }

        Commands::Bench {
            synthetic,
            window,
            runs,
        } => {
            let iterations = runs.max(1);
            let pruner = Pruner::new(PrunerConfig::default());

            if synthetic {
                let raw_template = build_synthetic_noisy_tree(200, 10);
                let raw_json_bytes = serde_json::to_vec(&raw_template.root)?.len();
                let mut total_us = 0u128;
                let mut min_us = u128::MAX;
                let mut max_us = 0u128;
                let mut last_compact_bytes = 0usize;
                let mut last_interactive = 0usize;

                for _ in 0..iterations {
                    let t0 = Instant::now();
                    let snap = pruner.prune(raw_template.clone());
                    let compact = format_snapshot(&snap, OutputFormat::Compact);
                    let elapsed = t0.elapsed().as_micros();
                    total_us += elapsed;
                    min_us = min_us.min(elapsed);
                    max_us = max_us.max(elapsed);
                    last_compact_bytes = compact.len();
                    last_interactive = snap.interactive_count;
                }

                let avg_ms = (total_us as f64 / iterations as f64) / 1000.0;
                let ratio = raw_json_bytes as f64 / last_compact_bytes.max(1) as f64;
                if format == OutputFormat::Json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "mode": "synthetic",
                            "runs": iterations,
                            "raw_nodes": raw_template.total_raw_nodes,
                            "interactive_nodes": last_interactive,
                            "raw_json_bytes": raw_json_bytes,
                            "compact_bytes": last_compact_bytes,
                            "compression_ratio": format!("{ratio:.1}x"),
                            "est_raw_tokens": raw_json_bytes / 4,
                            "est_compact_tokens": last_compact_bytes / 4,
                            "avg_ms": avg_ms,
                            "min_ms": min_us as f64 / 1000.0,
                            "max_ms": max_us as f64 / 1000.0,
                        }))?
                    );
                } else {
                    println!(
                        "bench (synthetic, {iterations} runs): {} raw nodes -> {} interactive nodes",
                        raw_template.total_raw_nodes, last_interactive
                    );
                    println!(
                        "latency: avg={avg_ms:.2}ms min={:.2}ms max={:.2}ms",
                        min_us as f64 / 1000.0,
                        max_us as f64 / 1000.0
                    );
                    println!(
                        "size:    {} B raw (~{} tok) -> {} B compact (~{} tok) [{ratio:.1}x smaller]",
                        raw_json_bytes,
                        raw_json_bytes / 4,
                        last_compact_bytes,
                        last_compact_bytes / 4
                    );
                }
            } else {
                let provider = PlatformProvider::connect().await?;
                let query = TreeQuery {
                    window_filter: window,
                    ..Default::default()
                };
                let mut total_us = 0u128;
                let mut min_us = u128::MAX;
                let mut max_us = 0u128;
                let mut last_raw_nodes = 0usize;
                let mut last_interactive = 0usize;
                let mut last_compact_bytes = 0usize;

                for _ in 0..iterations {
                    let t0 = Instant::now();
                    let raw = provider.get_window_tree(&query).await?;
                    last_raw_nodes = raw.total_raw_nodes;
                    let snap = pruner.prune(raw);
                    let compact = format_snapshot(&snap, OutputFormat::Compact);
                    let elapsed = t0.elapsed().as_micros();
                    total_us += elapsed;
                    min_us = min_us.min(elapsed);
                    max_us = max_us.max(elapsed);
                    last_interactive = snap.interactive_count;
                    last_compact_bytes = compact.len();
                }

                let avg_ms = (total_us as f64 / iterations as f64) / 1000.0;
                if format == OutputFormat::Json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "mode": "live",
                            "runs": iterations,
                            "raw_nodes": last_raw_nodes,
                            "interactive_nodes": last_interactive,
                            "compact_bytes": last_compact_bytes,
                            "est_compact_tokens": last_compact_bytes / 4,
                            "avg_ms": avg_ms,
                            "min_ms": min_us as f64 / 1000.0,
                            "max_ms": max_us as f64 / 1000.0,
                        }))?
                    );
                } else {
                    println!(
                        "bench (live AT-SPI2, {iterations} runs): {last_raw_nodes} raw nodes -> {last_interactive} interactive nodes"
                    );
                    println!(
                        "latency: avg={avg_ms:.2}ms min={:.2}ms max={:.2}ms | compact={last_compact_bytes} B (~{} tok)",
                        min_us as f64 / 1000.0,
                        max_us as f64 / 1000.0,
                        last_compact_bytes / 4
                    );
                }
            }
        }
    }

    Ok(())
}

fn is_known_chromium_app(bin: &str) -> bool {
    let base = std::path::Path::new(bin)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(bin)
        .to_lowercase();
    matches!(
        base.as_str(),
        "code"
            | "code-insiders"
            | "chromium"
            | "chromium-browser"
            | "google-chrome"
            | "google-chrome-stable"
            | "brave"
            | "brave-browser"
            | "discord"
            | "slack"
            | "obsidian"
            | "electron"
            | "opera"
            | "vivaldi"
            | "microsoft-edge"
    )
}

fn build_synthetic_noisy_tree(groups: usize, depth_noise: usize) -> RawWindowTree {
    let visible = NodeState {
        enabled: true,
        visible: true,
        showing: true,
        sensitive: true,
        ..Default::default()
    };
    let hidden = NodeState::default();

    let mut top_children = Vec::with_capacity(groups);
    let mut total = 1usize;

    for g in 0..groups {
        let leaf = RawNode {
            backend_ref: format!(":1.1:/group/{g}/btn"),
            role: if g % 5 == 0 {
                NodeRole::Input
            } else {
                NodeRole::Button
            },
            name: format!("Action {g}"),
            description: None,
            value: (g % 5 == 0).then(|| format!("val-{g}")),
            state: visible.clone(),
            bounds: Some(Bounds {
                x: 0,
                y: g as i32 * 24,
                width: 120,
                height: 24,
            }),
            children: vec![],
        };
        total += 1;

        // Wrap each interactive leaf in noisy empty containers + hidden sibling nodes.
        let mut current = leaf;
        for d in 0..depth_noise {
            let hidden_junk = RawNode {
                backend_ref: format!(":1.1:/group/{g}/junk/{d}"),
                role: NodeRole::Container,
                name: String::new(),
                description: None,
                value: None,
                state: hidden.clone(),
                bounds: Some(Bounds::default()),
                children: vec![],
            };
            total += 1;
            current = RawNode {
                backend_ref: format!(":1.1:/group/{g}/wrap/{d}"),
                role: NodeRole::Container,
                name: String::new(),
                description: None,
                value: None,
                state: visible.clone(),
                bounds: Some(Bounds {
                    x: 0,
                    y: 0,
                    width: 800,
                    height: 600,
                }),
                children: vec![hidden_junk, current],
            };
            total += 1;
        }
        top_children.push(current);
    }

    RawWindowTree {
        window: WindowInfo {
            id: ":1.1:/root".into(),
            title: "Synthetic Benchmark Window".into(),
            app_name: "bench-app".into(),
            pid: Some(1),
            active: true,
        },
        root: RawNode {
            backend_ref: ":1.1:/root".into(),
            role: NodeRole::Window,
            name: "Synthetic Benchmark Window".into(),
            description: None,
            value: None,
            state: visible,
            bounds: Some(Bounds {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            }),
            children: top_children,
        },
        total_raw_nodes: total,
        truncated: false,
    }
}

async fn run_action(
    raw_id: &str,
    action: ActionRequest,
    verify: bool,
    allow_coord_fallback: bool,
    direct: bool,
    format: OutputFormat,
) -> Result<()> {
    let result = if !direct && let Ok(mut client) = DaemonClient::connect_default().await {
        client
            .perform_action(raw_id, action, verify, allow_coord_fallback)
            .await?
    } else {
        let store = SnapshotStore::default_location();
        let snapshot = store.load()?;
        AppPolicy::default().check_write(&snapshot.window)?;
        let node_ref = find_node_ref(&snapshot, raw_id)?;

        let provider = PlatformProvider::connect().await?;
        match provider.perform_action(&node_ref, &action, verify).await {
            Ok(res) => res,
            Err(AxdomError::ActionUnsupported(_))
                if allow_coord_fallback && matches!(action, ActionRequest::Click) =>
            {
                let (x, y) = node_ref
                    .bounds
                    .filter(|b| !b.is_zero_sized())
                    .map(|b| (b.x + b.width / 2, b.y + b.height / 2))
                    .unwrap_or((0, 0));
                let fallback = ActionRequest::ClickCoords { x, y };
                provider.perform_action(&node_ref, &fallback, false).await?
            }
            Err(e) => return Err(e.into()),
        }
    };

    if format == OutputFormat::Json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        let status = if result.success { "ok" } else { "false" };
        print!(
            "{} {} -> {} ({})",
            result.action, result.node_id, result.method_used, status
        );
        if let Some(detail) = &result.detail {
            print!(" | {detail}");
        }
        println!();
    }

    Ok(())
}
