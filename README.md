# postbox-rs

Minimal, GPU-accelerated Rust messaging client for the Postbox / Triage Hub (`/v1/wazig/*`), built with **Zed GPUI + `gpui-kit` (shadcn/ui)** and **embedded SQLite (WAL + FTS5)** — no Electron, no WebView, no Tauri.

## Design Goals

- **GPU-accelerated UI (Intel Arc)**: Instanced DirectX rendering via GPUI + Windows Media Foundation DXVA hardware video decode, targeting `< 8 ms` keystroke-to-photon latency at 120/144Hz and `0%` idle CPU/GPU.
- **Minimal footprint**: `< 65 MB` idle RAM, `< 110 MB` active RAM on a 320-chat / 6,700-message inbox, `< 28 MB` stripped release binary.
- **Keyboard-first**: Preserves Postbox's full shortcut map (`Ctrl+K` unified command + search palette, `Ctrl+1..9` with hold-`Ctrl` badges, `Alt+I`/`Alt+1..9`/`G then I` views, `Alt+J/K` message walk, `Ctrl+D` live dictation, `Ctrl+Shift+M` voice notes, `E` archive, `U` unread) with optional `keymap.json` overrides.
- **shadcn aesthetic + Postbox invariants**: No toolbar (all commands in `Ctrl+K`), bundled IBM Plex Sans & Mono (`assets/IBM-Plex-LICENSE.txt`), saved `60–160%` font scaling, shadcn Dark Zinc / AMOLED / Light themes, auto-downloaded media (14-day LRU cache), in-app Opus voice playback with cached transcripts, and atomic self-update.
- **Hub-only transport**: WhatsApp, Slack, Telegram, and Signal all flow through `/v1/wazig/*` HTTP + SSE with zero local provider daemons.

## Workspace Layout

- `crates/postbox-core` — Domain types (`ChatKey`, `Provider`, `Chat`, `Message`), `postbox://` deep links, Deepgram EN/DE policy, local archive re-surface rules, control command schema.
- `crates/postbox-hub` — Typed `/v1/wazig/*` REST & SSE request builders and payload validators.
- `crates/postbox-store` — Embedded SQLite (WAL + FTS5) store, ACID outbox state machine, cached transcripts, drafts, and 14-day media cache eviction.
- `crates/postbox-keymap` — Declarative Postbox default keymap, chord state machines (`G then I`, `Alt+G, Alt+G`), and `keymap.json` override merger.
- `crates/postbox-platform` — Thin OS trait boundary (Windows DPAPI, WASAPI, DXVA Media Foundation, Toasts, Named Pipes, Registry; portable mocks for fast headless tests).
- `crates/postbox-ui` — Shadcn Zinc/AMOLED/Light themes, window title contract, and GPUI + `gpui-kit` desktop shell (`--features desktop`).
- `crates/postbox-app` — Desktop executable (`postbox`).
- `crates/postbox-ctl` — Companion local automation CLI (`postboxctl`).

See [PLAN.md](PLAN.md) for the complete architecture, performance budgets, SQLite schema, and 8-milestone implementation roadmap.

## Live Dictation (Windows)

Set `DEEPGRAM_API_KEY` in your Windows user environment (`HKCU\Environment`), then restart Postbox. The app trims the key and saves it with Windows DPAPI encryption; you can then remove the environment variable. A changed seed replaces the saved key. Failed legacy plaintext migration disables dictation rather than using the unencrypted key.

- `Ctrl+D` starts dictation at the composer selection. Press it again to stop and receive the final text.
- `Esc` cancels and restores the original draft, unless you have edited it yourself.
- `Ctrl+Enter` stops dictation and sends after the final text arrives, provided the draft, cursor, and chat context have not changed.
- Choose **Automatic**, **English**, **German**, or a microphone in `Ctrl+K`. Automatic accepts only English and German and rejects results without language information.

Audio streams to Deepgram Nova-3 over an encrypted WebSocket. The eight-bar microphone meter updates every 120 ms only while dictation and the window are active.

## Quick Commands

```bash
# Fast headless check & unit tests (runs on Linux or Windows in seconds)
cargo test --workspace
cargo clippy --workspace --all-targets

# Build the GPU desktop app with GPUI + gpui-kit (Windows / desktop environment)
cargo build --release -p postbox-app --features desktop
```
