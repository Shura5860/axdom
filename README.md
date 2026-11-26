# axdom

**Playwright's DOM model, for your entire desktop.**

`axdom` turns native desktop applications into a compact, ID-addressable DOM tree and exposes it to AI agents over **MCP**, **CLI**, and a **hot-cache local daemon** (Unix socket on Linux/macOS, Named Pipe on Windows). Written in pure Rust.

[![CI](https://github.com/Shura5860/axdom/actions/workflows/ci.yml/badge.svg)](https://github.com/Shura5860/axdom/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/rust-2024-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT%20%2F%20Apache--2.0-blue.svg)](#license)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows-informational.svg)](#platform-status)
[![MCP](https://img.shields.io/badge/MCP-2024--11--05-green.svg)](#mcp-integration)

[English](#axdom) · [Türkçe](#türkçe)

---

## Why not screenshots?

Most desktop AI agents take a full-screen PNG, send it to a vision model, wait 2–5 seconds, and guess `(x, y)` coordinates — which burns thousands of tokens per step and breaks under Wayland's coordinate isolation.

Your OS already maintains a live structured UI tree for screen readers. `axdom` reads that tree directly, strips layout noise, redacts passwords, and lets agents act on controls by short hash ID without ever moving the mouse cursor.

| | Screenshot + VLM Agent | `axdom` |
|---|---|---|
| **Input per step** | 1080p/1440p image (~1,500–4,000 tokens) | Compact DOM (~100–300 tokens) |
| **Read latency** | ~1,500–4,000 ms (capture + VLM) | ~44 ms cold / **~4 ms** warm daemon |
| **Wayland actions** | Blocked or needs virtual pointer | Coordinate-free AT-SPI2 calls |
| **Password fields** | Visible if unmasked on screen | Redacted to `[REDACTED]` in core |
| **UI shift safety** | Clicks wrong `(x, y)` silently | Live role/name check (`StaleNode`) |

*(Live latency measured on KDE Plasma 6 / Wayland, Konsole window. Release-build synthetic benchmark on a 4,201-node noisy tree: **1.18 ms avg** (`1.08 ms` min), pruning 1.41 MB raw JSON down to 6.7 KB compact DOM — **209× smaller**. Large live Electron/browser trees are slower on cold reads.)*

---

## 30-Second Walkthrough

A real session on KDE Plasma (Wayland) — opening Konsole's Find bar, typing a query, and toggling **Case sensitive** without touching the mouse:

```bash
# 1. Read the active window as a pruned DOM
$ axdom tree
window: "Axdom : bash — Konsole" (pid: 189309) [app: konsole, snapshot: #1]
elements:
  - [#0da] input value="[shura@ShuraLinux Axdom]$ axdom tree" [focused]
  - toolbar "Main Toolbar"
    - [#f5a] button "New Tab"
    - [#9ab] button "Split View"
  - toolbar "Session Toolbar"
    - [#e84] button "Copy" [disabled]
    - [#20b] button "Paste"
    - [#790] button "Find"
    - [#f21] button "Open Menu"

# 2. Click "Find" by its short ID (coordinate-free AT-SPI2)
$ axdom click 790
click #790 -> atspi:Action.DoAction(Press) (ok) | available actions: [Press, SetFocus]

# 3. Inspect the newly opened Find bar, type into it, and toggle the checkbox
$ axdom tree
...
  - [#f40] input "Enter the text to search for here" [editable]
  - [#645] checkbox "Case sensitive"

$ axdom type f40 "axdom"
set_text #f40 -> atspi:EditableText.SetTextContents (ok) | wrote 5 chars

$ axdom click 645
click #645 -> atspi:Action.DoAction(Press) (ok) | available actions: [Toggle, Press, SetFocus]
```

Over **MCP**, `click_node` and `type_text` automatically wait briefly for UI settle and return only the **compact diff** (`+`, `~`, `-`) instead of dumping the whole tree again.

---

## Install & Run

Requires Rust (edition 2024) and `at-spi2-core` on Linux.

```bash
cargo build --release -p axdom-cli
install -Dm755 target/release/axdom ~/.local/bin/axdom

# One-time setup: environment.d, Chromium/Electron/Flatpak flags, and .desktop overrides
axdom setup
axdom doctor
```

### Connect to Claude Desktop, Cursor, or any MCP Client

`axdom mcp` speaks JSON-RPC 2.0 over stdio (`2024-11-05`) and logs strictly to `stderr`.

```json
{
  "mcpServers": {
    "axdom": {
      "command": "/home/USER/.local/bin/axdom",
      "args": ["mcp", "--enable-a11y"]
    }
  }
}
```

**Exposed MCP tools (14):**
- **Inspect:** `list_windows`, `get_desktop_tree`
- **Semantic actions:** `click_node`, `type_text` (`submit: true` supported), `set_value`, `focus_node`, `scroll_node` (`delta_x` & `delta_y`), `press_key`
- **Pointer & canvas actions:** `right_click_node`, `double_click_node`, `hover_node`, `drag_node` (`to_node_id` or `to_x`/`to_y`), `click_coords`
- **Visual / blind-spot fallback:** `capture_screenshot` (cropped to the active window or `node_id` by default for privacy & token savings; `som_grid: true` overlays a 4×4 `A1..D4` Set-of-Mark coordinate grid)

Optional safety flags: `--allow-app firefox --allow-app libreoffice`, `--deny-app <name>`, `--read-only`.

---

## Hot-Cache Daemon (~4 ms Reads)

Start the local Unix-socket daemon (`$XDG_RUNTIME_DIR/axdom/axdom.sock`) to keep a bounded multi-window cache invalidated in real time by AT-SPI2 focus/window/tree events:

```bash
axdom daemon start --enable-a11y
axdom daemon status
```

When the daemon is running, both CLI commands and `axdom mcp` use it automatically (`--direct` bypasses it).

<details>
<summary>Run as a systemd user service</summary>

```bash
install -Dm755 target/release/axdom ~/.local/bin/axdom
mkdir -p ~/.config/systemd/user
cp contrib/systemd/axdomd.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now axdomd.service
```

</details>

<details>
<summary>Full CLI reference</summary>

```bash
axdom setup                                   # One-time Linux setup (environment.d, Chromium/Flatpak flags, .desktop)
axdom doctor [--enable-a11y] [--fix]          # Session, D-Bus, and OS accessibility health check
axdom windows                                 # List open windows (filtered by AppPolicy)
axdom tree [--window X] [--root #id] [--depth 12] [--max-nodes 1500] [--refresh]
axdom click <id> [--fallback-input] [--no-verify]
axdom right-click <id> [--no-verify]
axdom double-click <id> [--no-verify]
axdom hover <id> [--no-verify]
axdom drag <id> <to_x> <to_y> [--no-verify]
axdom type <id> <text> [--submit] [--no-verify]
axdom scroll <id> [delta_y] [--delta-x N] [--no-verify]
axdom set-value <id> <number> [--no-verify]
axdom focus <id> [--no-verify]
axdom key "Ctrl+S"                            # Native XDG Portal / wtype / xdotool / Win32 / CGEvent
axdom screenshot [--out shot.png]             # Cropped/full PNG fallback (grim, XDG Portal, GDI, screencapture)
axdom launch code .                           # Sets a11y env vars + --force-renderer-accessibility
axdom watch                                   # Stream live AT-SPI2 events (Linux)
axdom bench [--runs 5 | --synthetic]          # Measure latency and token compression
axdom mcp [--allow-app X] [--deny-app X] [--read-only] [--enable-a11y]
```

Every command supports `--json` and `--direct`.

</details>

---

## Security & Guardrails

- **Automatic secret redaction:** `PasswordInput` nodes and input fields with sensitive labels (`password`, `parola`, `cvv`, `api key`, `secret key`, `access token`, etc.) are replaced with `[REDACTED]` in `axdom-core` before serialization. *(Note: apps that misreport both role and label cannot be guaranteed 100% masked — use `--allow-app` for strict scoping.)*
- **Hardened local daemon socket:** Unix sockets are bound inside a `0700` user-private directory with mode `0600`, symlink/live-daemon collision checks, and per-connection `peer_cred` UID verification.
- **Default denylist (`AppPolicy`):** Password managers (`keepassxc`, `1password`, `bitwarden`, Flatpak IDs), auth agents (`polkit`, `gnome-keyring`, `kwallet`, `gpg-agent`, `pinentry`), private browser windows (`incognito`, `inprivate`, `private browsing`), and encrypted messengers (`signal`, `telegram`) are blocked by default.
- **Strict `#id` targeting (anti-hallucination):** Actions require the exact short `#xxx` ID from a snapshot. Passing a button label like `axdom click Find` is rejected (with a `#id` hint) so LLM agents cannot blindly guess UI controls without reading the tree first.
- **Live stale-node check:** Before clicking or typing, `axdom` verifies the live node's role and label still match the snapshot.
- **Reversible Linux setup:** `axdom setup` supports `--dry-run`, creates `.axdom.bak` backups before modifying existing files, and can be reverted with `axdom setup --undo`.

See [SECURITY.md](SECURITY.md) for the full threat model.

---

## Platform Status

The core engine (`axdom-core`, `axdom-mcp`) is already **OS-agnostic** — tree pruning, noise flattening, duplicate child-text removal, deterministic `#xxx` IDs, `[REDACTED]` password masking, `AppPolicy`, snapshot diffing, subtree scoping, and the MCP server compile and run on Linux, macOS, and Windows unchanged behind the `AccessibilityProvider` trait.

| Platform | Accessibility Backend | Daemon Transport | Status |
|---|---|---|---|
| **Linux** (Wayland & X11) | `axdom-linux` (AT-SPI2 via `zbus` + XDG Portals) | Unix domain socket (`0600` + `peer_cred`) | Tested on KDE Plasma 6 Wayland |
| **Windows** | `axdom-windows` (UI Automation + Win32 `SendInput`/GDI) | Windows Named Pipe (`\\.\pipe\axdom-daemon`) | Experimental (CI-verified) |
| **macOS** | `axdom-macos` (`AXUIElement` + `CGWindowList` + `CGEvent`) | Unix domain socket (`0600` + `peer_cred`) | Experimental (CI-verified) |

### Known Limitations

- **Electron / Chromium / Brave apps on Linux** often keep accessibility off until `ScreenReaderEnabled=true` or `--force-renderer-accessibility` is passed. Run `axdom setup` once (configures `environment.d`, `~/.config/*-flags.conf`, Flatpak `~/.var/app/*/config/`, and `.desktop` overrides) and restart the app (`Ctrl+Q`), or use `axdom launch <app>`.
- **Canvas, games, and custom-drawn UIs** (e.g., 3D viewports, Figma canvas) do not expose accessibility nodes; `axdom` flags a blind-spot warning and supports cropped `capture_screenshot` with `som_grid: true` (`A1..D4` Set-of-Mark grid) + `click_coords`.
- **Wayland portals & pointer fallback:** Standard AT-SPI2 actions need no external tools. Keyboard, scroll, and pointer actions use native XDG Desktop Portal `RemoteDesktop` (built into KDE/GNOME/Hyprland, with `persist_mode=2` remembering permission after the first prompt), falling back to `wtype`/`ydotool`/`xdotool` when installed.
- **Windows & macOS live testing:** Native backends compile and pass unit tests in GitHub Actions, but have not yet been stress-tested across as many real desktop apps as Linux.

---

## Workspace Layout

- `crates/axdom-core` — models, `AccessibilityProvider` trait, pruner, `AppPolicy`, snapshot store, diffing, PNG cropping & Set-of-Mark grid, formatters
- `crates/axdom-linux` — pure-Rust async AT-SPI2 backend on `zbus` (`Cache.GetItems` fast path, semantic depth, overlay merge, XDG `RemoteDesktop`/`Screenshot` portals, `axdom setup`)
- `crates/axdom-windows` — native Windows UI Automation (`IUIAutomation`), Win32 `SendInput`, GDI screenshot, focus event watcher
- `crates/axdom-macos` — native macOS `AXUIElement`, `CGWindowListCopyWindowInfo`, `CGEvent`, `screencapture` (zero extra FFI crates)
- `crates/axdom-daemon` — hot-cache local daemon (Unix domain socket on Linux/macOS, Named Pipe on Windows) with live event invalidation
- `crates/axdom-mcp` — stdio JSON-RPC 2.0 MCP server (14 tools)
- `crates/axdom-cli` — single cross-platform `axdom` binary

## License

Dual-licensed under MIT or Apache-2.0.

---

## Türkçe

**Playwright'ın DOM modeli, tüm masaüstünüz için.**

`axdom`, masaüstü uygulamalarını kompakt ve kısa ID'lerle (`#790`) hedeflenebilir bir DOM ağacına dönüştürüp yapay zeka ajanlarına **MCP**, **CLI** ve **yerel Unix soket daemon'ı** üzerinden sunan saf Rust motorudur.

### Neden ekran görüntüsü değil?

Klasik masaüstü ajanları her adımda tam ekran PNG alıp vision modeline yollar, 2–5 saniye bekler ve `(x, y)` koordinatı tahmin eder. Bu hem adım başına binlerce token yakar hem Wayland'ın koordinat izolasyonunda kırılır.

İşletim sistemi zaten ekran okuyucular için canlı ve yapısal bir arayüz ağacı tutar. `axdom` bu ağacı doğrudan okur, boş kapsayıcı gürültüsünü budar, şifre alanlarını maskeler ve fare imlecini hiç oynatmadan elemanları kısa hash ID ile tetikler:

- **Token maliyeti:** Adım başına ~100–300 token (ekran görüntüsü yerine kompakt DOM)
- **Okuma hızı:** Soğuk AT-SPI2 okumada ~44 ms (`26 ms` min), sıcak `axdom daemon` önbelleğinde **~4 ms** *(KDE Plasma 6 / Wayland, debug derleme, Konsole penceresi ölçümü)*
- **Wayland uyumu:** Doğrudan AT-SPI2 aksiyonları (`Action.DoAction`, `EditableText`, `Component.GrabFocus`) koordinat gerektirmez

### Hızlı Kurulum ve Kullanım

```bash
cargo build --release -p axdom-cli
install -Dm755 target/release/axdom ~/.local/bin/axdom

axdom setup                  # Tek seferlik Linux kurulumu (environment.d, Chromium/Flatpak bayrakları, .desktop)
axdom doctor                 # Oturum, D-Bus ve erişilebilirlik kontrolü
axdom windows                # Açık pencereleri listele
axdom tree                   # Aktif pencerenin budanmış DOM ağacı
axdom click 790              # Koordinatsız tıklama (sadece kesin #id kabul eder)
axdom type f40 "axdom" --submit  # Yaz ve Enter gönder
axdom scroll 790 5           # Dikey / yatay (--delta-x) kaydırma
axdom right-click 790        # Sağ tık (hover, double-click, drag de desteklenir)
axdom daemon start --enable-a11y  # ~4 ms sıcak önbellekli yerel daemon
```

### Güvenlik ve Koruma Katmanları

- **Otomatik şifre maskeleme:** `PasswordInput` alanları çekirdekte `[REDACTED]` yapılır.
- **Varsayılan engelli listesi (`AppPolicy`):** `keepassxc`, `1password`, `bitwarden`, `polkit`, `gnome-keyring`, `kwallet` ve `gpg-agent` varsayılan olarak engellidir (`--allow-app`, `--deny-app`, `--read-only` desteklenir).
- **Gizlilik korumalı kırpılmış ekran görüntüsü:** `capture_screenshot` varsayılan olarak yalnızca aktif pencereyi (veya belirtilen `node_id`'yi) kırpar; canvas/oyun gibi kör noktalarda `som_grid: true` ile `A1..D4` koordinat ızgarası + `click_coords` desteği sunar.
- **Sıkı `#id` zorunluluğu (anti-halüsinasyon):** Aksiyonlar yalnızca snapshot'taki `#xxx` ID'sini kabul eder; buton ismiyle kör tahmin tıklaması reddedilir.
- **Canlı `StaleNode` doğrulaması:** Tıklama/yazma öncesi düğümün canlı rol ve etiketi yeniden doğrulanır.

### Platform Durumu

- **Linux (`axdom-linux`):** AT-SPI2 (`zbus`) + XDG Desktop Portal (`RemoteDesktop` / `Screenshot`) + `0600`/`peer_cred` korumalı Unix soket daemon. KDE Plasma 6 Wayland üzerinde canlı test edildi.
- **Windows (`axdom-windows`):** Windows UI Automation + Win32 `SendInput`/GDI + Named Pipe daemon (`\\.\pipe\axdom-daemon`). **Deneysel** (GitHub Actions CI'da derlenip test ediliyor).
- **macOS (`axdom-macos`):** `AXUIElement` + `CGWindowListCopyWindowInfo` + `CGEvent` + Unix soket daemon. **Deneysel** (GitHub Actions CI'da derlenip test ediliyor).

### Lisans

MIT veya Apache-2.0 altında çift lisanslıdır.

