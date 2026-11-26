# Security Policy

## Threat Model

`axdom` reads desktop accessibility trees and can dispatch UI actions (clicks, typing, keyboard shortcuts, pointer events) on behalf of local AI agents. Because desktop control is high-privilege, `axdom` enforces defense-in-depth by default:

- **Local daemon isolation:** On Linux/macOS, the Unix socket is created inside a `0700` user-private directory (`$XDG_RUNTIME_DIR/axdom/axdom.sock` or `$HOME/.cache/axdom/axdom.sock`), bound with mode `0600`, protected against symlink replacement and live-socket hijacking, and verified per connection via `peer_cred` UID matching.
- **Default application denylist (`AppPolicy`):** Password managers (`keepassxc`, `1password`, `bitwarden`, Flatpak IDs), auth agents (`polkit`, `gnome-keyring`, `kwallet`, `gpg-agent`, `pinentry`), private browser windows (`incognito`, `inprivate`, `private browsing`), and encrypted messengers (`signal`, `telegram`) are blocked by default. Use `--allow-app` for strict allowlisting and `--read-only` to disable all write actions.
- **Automatic secret redaction:** `PasswordInput` nodes and input fields with sensitive labels (`password`, `parola`, `cvv`, `api key`, `secret key`, `access token`, etc.) are masked to `[REDACTED]` in `axdom-core` before serialization. Note that apps which misreport both role and label cannot be guaranteed 100% masked — use `--allow-app` or `--deny-app` for high-sensitivity workflows.
- **Privacy-preserving screenshots:** `capture_screenshot` crops to the active window (or target `node_id`) by default and checks `AppPolicy` before capturing.

## Reporting a Vulnerability

Please do not open a public GitHub issue for security vulnerabilities. Instead, use GitHub's private Security Advisory tab on `https://github.com/Shura5860/axdom/security/advisories/new` with reproduction steps and affected OS/compositor details.
