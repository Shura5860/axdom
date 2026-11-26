use std::path::{Path, PathBuf};

use crate::doctor::apply_chromium_a11y_flags;

const CHROMIUM_FLAG: &str = "--force-renderer-accessibility";

/// Known system `.desktop` launchers for Debian/Ubuntu/Fedora Chromium/Electron apps that
/// do not read Arch-style `*-flags.conf` files.
const DESKTOP_OVERRIDES: &[&str] = &[
    "brave-browser.desktop",
    "google-chrome.desktop",
    "chromium.desktop",
    "chromium-browser.desktop",
    "code.desktop",
    "com.microsoft.VSCode.desktop",
    "discord.desktop",
    "slack.desktop",
    "opera.desktop",
];

const FLATPAK_APP_IDS: &[&str] = &[
    "com.brave.Browser",
    "org.chromium.Chromium",
    "com.google.Chrome",
    "com.visualstudio.code",
    "com.discordapp.Discord",
    "com.slack.Slack",
    "md.obsidian.Obsidian",
    "com.opera.Opera",
    "com.vivaldi.Vivaldi",
];

/// Execution mode for `axdom setup`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupMode {
    Apply,
    DryRun,
    Undo,
}

/// Runs the full one-time Linux accessibility setup (`axdom setup`).
pub async fn run_full_setup() -> std::io::Result<Vec<String>> {
    run_setup(SetupMode::Apply).await
}

/// Runs Linux accessibility setup in `Apply`, `DryRun`, or `Undo` mode:
/// 1. `~/.config/environment.d/axdom.conf`
/// 2. `~/.config/*-flags.conf` and installed Flatpak `~/.var/app/*/config/*-flags.conf`
/// 3. Flatpak `org.a11y.Bus` D-Bus permission
/// 4. User `.desktop` overrides under `~/.local/share/applications/` (with `.axdom.bak` backups)
pub async fn run_setup(mode: SetupMode) -> std::io::Result<Vec<String>> {
    let mut report = Vec::new();

    // 1. systemd user environment.d
    if let Some(config_dir) = xdg_config_dir() {
        let env_file = config_dir.join("environment.d").join("axdom.conf");
        report.push(handle_environment_d_file(&env_file, mode)?);
    }

    // 2. Native & Flatpak *-flags.conf
    match mode {
        SetupMode::Apply => report.extend(apply_chromium_a11y_flags()?),
        SetupMode::DryRun => {
            report.push(
                "would update: ~/.config/*-flags.conf and installed Flatpak *-flags.conf"
                    .to_string(),
            );
        }
        SetupMode::Undo => {
            if let Some(config_dir) = xdg_config_dir() {
                report.extend(undo_flags_in_dir(&config_dir)?);
            }
            if let Some(home) = std::env::var_os("HOME") {
                let fp_root = PathBuf::from(&home).join(".var").join("app");
                for app_id in FLATPAK_APP_IDS {
                    let cfg = fp_root.join(app_id).join("config");
                    if cfg.is_dir() {
                        report.extend(undo_flags_in_dir(&cfg)?);
                    }
                }
            }
        }
    }

    // 3. Flatpak D-Bus a11y override
    if let Some(home) = std::env::var_os("HOME") {
        let fp_root = PathBuf::from(&home).join(".var").join("app");
        for app_id in FLATPAK_APP_IDS {
            if fp_root.join(app_id).is_dir() {
                match mode {
                    SetupMode::DryRun => {
                        report.push(format!(
                            "would run:   flatpak override --user --talk-name=org.a11y.Bus {app_id}"
                        ));
                    }
                    SetupMode::Apply => {
                        let status = tokio::process::Command::new("flatpak")
                            .args(["override", "--user", "--talk-name=org.a11y.Bus", app_id])
                            .output()
                            .await;
                        if let Ok(out) = status
                            && out.status.success()
                        {
                            report.push(format!("flatpak a11y override: {app_id}"));
                        }
                    }
                    SetupMode::Undo => {
                        let status = tokio::process::Command::new("flatpak")
                            .args(["override", "--user", "--no-talk-name=org.a11y.Bus", app_id])
                            .output()
                            .await;
                        if let Ok(out) = status
                            && out.status.success()
                        {
                            report.push(format!("removed flatpak override: {app_id}"));
                        }
                    }
                }
            }
        }

        // 4. User .desktop overrides for Debian/Ubuntu/Fedora native launchers
        let sys_apps = Path::new("/usr/share/applications");
        let user_apps = PathBuf::from(&home)
            .join(".local")
            .join("share")
            .join("applications");
        report.extend(handle_desktop_overrides(sys_apps, &user_apps, mode)?);
    }

    Ok(report)
}

fn xdg_config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

fn backup_file_once(path: &Path) -> std::io::Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let bak = backup_path_for(path);
    if !bak.exists() {
        std::fs::copy(path, bak)?;
    }
    Ok(())
}

fn backup_path_for(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".axdom.bak");
    PathBuf::from(os)
}

pub fn ensure_environment_d_file(path: &Path) -> std::io::Result<String> {
    handle_environment_d_file(path, SetupMode::Apply)
}

fn handle_environment_d_file(path: &Path, mode: SetupMode) -> std::io::Result<String> {
    match mode {
        SetupMode::Undo => {
            if path.is_file() {
                std::fs::remove_file(path)?;
                return Ok(format!("removed:     {}", path.display()));
            }
            return Ok(format!("not present: {}", path.display()));
        }
        SetupMode::DryRun => {
            if let Ok(existing) = std::fs::read_to_string(path)
                && existing.contains("ACCESSIBILITY_ENABLED=1")
                && existing.contains("QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1")
            {
                return Ok(format!("already set: {}", path.display()));
            }
            return Ok(format!("would write: {}", path.display()));
        }
        SetupMode::Apply => {}
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let desired = "\
ACCESSIBILITY_ENABLED=1
QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1
GTK_MODULES=gail:atk-bridge
";
    if let Ok(existing) = std::fs::read_to_string(path)
        && existing.contains("ACCESSIBILITY_ENABLED=1")
        && existing.contains("QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1")
    {
        return Ok(format!("already set: {}", path.display()));
    }
    backup_file_once(path)?;
    std::fs::write(path, desired)?;
    Ok(format!("updated:     {}", path.display()))
}

fn undo_flags_in_dir(dir: &Path) -> std::io::Result<Vec<String>> {
    let mut actions = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(actions);
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if !p
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with("-flags.conf"))
        {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&p) else {
            continue;
        };
        if !content.contains(CHROMIUM_FLAG) {
            continue;
        }
        let filtered: Vec<&str> = content
            .lines()
            .filter(|l| l.trim() != CHROMIUM_FLAG)
            .collect();
        if filtered.is_empty() {
            std::fs::remove_file(&p)?;
            actions.push(format!("removed:     {}", p.display()));
        } else {
            std::fs::write(&p, format!("{}\n", filtered.join("\n")))?;
            actions.push(format!("reverted:    {}", p.display()));
        }
    }
    Ok(actions)
}

pub fn apply_desktop_overrides(sys_dir: &Path, user_dir: &Path) -> std::io::Result<Vec<String>> {
    handle_desktop_overrides(sys_dir, user_dir, SetupMode::Apply)
}

fn handle_desktop_overrides(
    sys_dir: &Path,
    user_dir: &Path,
    mode: SetupMode,
) -> std::io::Result<Vec<String>> {
    let mut actions = Vec::new();
    for name in DESKTOP_OVERRIDES {
        let dst = user_dir.join(name);
        if mode == SetupMode::Undo {
            let bak = backup_path_for(&dst);
            if bak.is_file() {
                std::fs::rename(&bak, &dst)?;
                actions.push(format!("restored:    {}", dst.display()));
            } else if dst.is_file()
                && std::fs::read_to_string(&dst)
                    .map(|c| c.contains(CHROMIUM_FLAG))
                    .unwrap_or(false)
            {
                std::fs::remove_file(&dst)?;
                actions.push(format!("removed:     {}", dst.display()));
            }
            continue;
        }

        let src = sys_dir.join(name);
        if !src.is_file() {
            continue;
        }
        let content = if dst.is_file() {
            std::fs::read_to_string(&dst)?
        } else {
            std::fs::read_to_string(&src)?
        };
        if content.contains(CHROMIUM_FLAG) {
            actions.push(format!("already set: {}", dst.display()));
            continue;
        }
        if mode == SetupMode::DryRun {
            actions.push(format!("would write: {}", dst.display()));
            continue;
        }
        std::fs::create_dir_all(user_dir)?;
        backup_file_once(&dst)?;
        let patched = inject_flag_into_desktop(&content, CHROMIUM_FLAG);
        std::fs::write(&dst, patched)?;
        actions.push(format!("updated:     {}", dst.display()));
    }
    Ok(actions)
}

fn inject_flag_into_desktop(content: &str, flag: &str) -> String {
    let mut out = String::with_capacity(content.len() + 64);
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("Exec=")
            && !rest.contains(flag)
        {
            // Insert flag right after the executable token (handling quoted paths with spaces).
            let (exe, args) = split_exec_token(rest);
            match args {
                Some(a) if !a.is_empty() => out.push_str(&format!("Exec={exe} {flag} {a}\n")),
                _ => out.push_str(&format!("Exec={exe} {flag}\n")),
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn split_exec_token(rest: &str) -> (&str, Option<&str>) {
    let trimmed = rest.trim_start();
    if let Some(quote) = trimmed.chars().next().filter(|&c| c == '"' || c == '\'') {
        let mut escaped = false;
        for (idx, ch) in trimmed.char_indices().skip(1) {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == quote {
                let end = idx + ch.len_utf8();
                let exe = &trimmed[..end];
                let remainder = trimmed[end..].trim_start();
                return (
                    exe,
                    if remainder.is_empty() {
                        None
                    } else {
                        Some(remainder)
                    },
                );
            }
        }
        return (trimmed, None);
    }

    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let exe = parts.next().unwrap_or("");
    let args = parts.next().map(str::trim_start).filter(|s| !s.is_empty());
    (exe, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_flag_into_desktop_exec_lines() {
        let desktop = "[Desktop Entry]\nName=Code\nExec=/usr/bin/code %F\n";
        let patched = inject_flag_into_desktop(desktop, CHROMIUM_FLAG);
        assert!(patched.contains("Exec=/usr/bin/code --force-renderer-accessibility %F"));
    }

    #[test]
    fn injects_flag_into_quoted_desktop_exec_with_spaces() {
        let desktop = "[Desktop Entry]\nName=Brave\nExec=\"/opt/Brave Browser/brave\" %U\n";
        let patched = inject_flag_into_desktop(desktop, CHROMIUM_FLAG);
        assert!(
            patched.contains("Exec=\"/opt/Brave Browser/brave\" --force-renderer-accessibility %U")
        );
    }

    #[test]
    fn creates_environment_d_file_idempotently() {
        let tmp = std::env::temp_dir().join(format!("axdom-envd-test-{}", std::process::id()));
        let file = tmp.join("axdom.conf");
        let _ = std::fs::remove_dir_all(&tmp);

        assert!(
            ensure_environment_d_file(&file)
                .unwrap()
                .starts_with("updated:")
        );
        assert!(
            ensure_environment_d_file(&file)
                .unwrap()
                .starts_with("already set:")
        );
        assert!(
            handle_environment_d_file(&file, SetupMode::Undo)
                .unwrap()
                .starts_with("removed:")
        );
        assert!(!file.exists());
        assert!(
            handle_environment_d_file(&file, SetupMode::DryRun)
                .unwrap()
                .starts_with("would write:")
        );
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
