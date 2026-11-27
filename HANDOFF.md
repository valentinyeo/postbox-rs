# Handoff for Continuation (`valentinyeo/postbox-rs`)

## Current Repository State

- **`main` (`aa8abcd`) — 100% green on Linux & Windows CI (238/238 tests, downloadable `postbox-windows-x64` artifact)**:
  - **M0**: Workspace scaffold, CI, IBM Plex Sans/Mono assets + OFL license.
  - **M1 (PR #1)**: GPUI + `gpui-kit 0.11.0` shadcn shell (no toolbar, `ZincDark`/`Amoled`/`Light`, 60–160% scaling), keymap (`keymap.json` overrides), hold-`Ctrl` `1–9` badges, unified `Ctrl+K` fuzzy palette + SQLite FTS5 search, `F1` shortcuts sheet, Windows release CI artifact upload (`postbox-windows-x64`).
  - **M2 (PR #2)**: Live `/v1/wazig/*` Hub HTTP/2 + SSE sync, automatic Windows Hub discovery (`POSTBOX_HUB_URL`/`WAZIG_HUB_URL`, `POSTBOX_HUB_TOKEN`/`WAZIG_HUB_TOKEN`, DPAPI `HKCU\Software\Messages\HubToken`), ACID durable outbox, Combined Inbox (`Alt+I` / `G then I`) + `Alt+2..9` views, local WhatsApp archive resurface (`IGSH-902`), Slack threads + broadcast, drafts, pins, unread, read ticks.
  - **M3 (PR #3)**: `crates/postbox-media` — auto-download on chat open (14-day LRU disk cache), off-UI-thread PNG/JPEG/WebP/GIF decode, avatars/workspace icons/Slack custom emoji, paste-to-attach (`Ctrl+V`), image lightbox, `Ctrl+R` reactions, `@mentions`, SSRF-safe link unfurls, Giphy picker.
  - **M4 (PR #4)**: `crates/postbox-audio` (pure-Rust `opus-pure 0.2.5`) — in-app Ogg Opus playback (`1x`/`1.5x`/`2x` WSOLA via WASAPI), waveform scrubber & resume, `Ctrl+Shift+M` voice recording, cached Deepgram Nova-3 REST transcripts (`Ctrl+T`).

- **`e2e/suite` (`a74c84a`) — Complete & verified**:
  - 185-test opaque-box 4-tier E2E suite (`crates/postbox-e2e`) with loopback mock Hub, mock Deepgram, and mock external servers. Ready to rebase/merge onto `main` in Milestone 8.

- **`m5/deepgram-live-dictation` (Milestone 5, PR #5)**:
  - Implements Deepgram Nova-3 live WebSocket dictation (`Ctrl+D`), DPAPI `HKCU\Environment\DEEPGRAM_API_KEY` seeding, EN/DE fail-closed Automatic mode, anchored composer replacement, `CloseStream` final drain, and an eight-bar microphone meter.
  - Loopback tests cover fragmented Unicode interims, replayed finals, cursor preservation, and all three language modes. GPUI tests cover real composer selection, cursor-safe auto-send, and the 120 ms meter timer's active-window lifecycle.
  - Dictation results stay with the origin chat even when another draft is edited. Cursor or context changes disarm `Ctrl+Enter` auto-send.
  - Verification runs on GitHub-hosted Linux and Windows runners. See PR #5 for the final run and test counts; no Rust toolchain is installed on this host.

## Remaining Work (Priority Order)

1. **Review Milestone 5 (`m5/deepgram-live-dictation` -> PR #5)**:
   - Review the implementation and verification evidence in PR #5, then merge to `main` when authorized.
   - The implementation run stops after opening the PR; it does not merge or wait on the PR-triggered CI run.

2. **RAM-Diet Pass (M5.5 -> PR #6, HIGH PRIORITY — user measured ~130 MB vs Zig's ~30 MB on Windows + Intel Arc)**:
   - Target: **~65–85 MB active / ~40–55 MB minimized** while keeping GPUI + `gpui-kit`.
   - `crates/postbox-store/src/lib.rs`: Set `mmap_size = 0` (was `64 MiB`) and `cache_size = -2000` (2 MiB/conn, was `-16000`); run `PRAGMA shrink_memory` after bulk syncs and on minimize/idle.
   - `crates/postbox-media/src/lib.rs` & `crates/postbox-ui/src/desktop_m3.rs`: Lower `VRAM_TEXTURE_BUDGET_BYTES` from `64 MiB` to `16 MiB` (Intel Arc unified memory counts D3D11/RGBA textures in Task Manager RAM). Evict previous-chat decoded images/GIFs on chat switch and purge transient textures when minimized/inactive.
   - Lazy-load bundled emoji/shortcode tables (`slack_emoji.tsv`, `emoji_records.txt`, `shortcode_aliases.txt`) on first use; cap `reqwest` idle pool (`pool_max_idle_per_host(2)`) and media decode concurrency to 2 workers; retain only active chat's timeline in RAM (reload from SQLite in <5 ms).
   - `crates/postbox-platform/src/win/`: On minimize/extended idle, purge transient caches, run SQLite `PRAGMA shrink_memory`, and trim Windows working set (`SetProcessWorkingSetSizeEx(GetCurrentProcess(), usize::MAX, usize::MAX, 0)`).
   - Wire `GetProcessMemoryInfo` (`WorkingSetSize` / `PrivateUsage`) into `TelemetryCounters` for measurable RAM tracking.

3. **Milestone 6 (PR #7) — Intel Arc DXVA / Windows Media Foundation Video**:
   - Hardware-decoded video thumbnails (cached to disk) and inline video player (double-click to play, `F11` fullscreen, WASAPI audio sync) via `IMFSourceReader` + `IMFDXGIDeviceManager` (<5% CPU on Intel Arc).

4. **Milestone 7 (PR #8) — Windows OS Integration, `postboxctl` & Atomic Self-Updater**:
   - Windows Toast XML notifications with click-to-focus chat routing.
   - Per-user `postbox://` URL handler (`HKCU\Software\Classes\postbox`) + single-instance mutex + `\\.\pipe\postbox-link-<user>` forwarder + non-modal "Chat not found" card.
   - `postboxctl` named-pipe server (`\\.\pipe\postbox-control-<user>`) with live RAM/FPS/outbox `status` telemetry.
   - Atomic GitHub Releases self-updater (stage -> SHA-256 verify -> `MoveFileExW` atomic swap -> relaunch maximized on last monitor) + crash/exit records.

5. **Milestone 8 (PR #9) — Integrate `origin/e2e/suite` & Release Hardening**:
   - Rebase/wire `crates/postbox-e2e` from `origin/e2e/suite`, ensure all 185+ E2E tests and workspace tests pass on Linux & Windows CI, verify RAM/latency targets, and tag `v0.1.0`.

## Reference Repo & Invariants
- Behavioral reference: `valentinyeo/postbox` (Zig implementation).
- Strict invariants: Hub-only (`/v1/wazig/*`), no toolbar (all commands in `Ctrl+K`), IBM Plex Sans/Mono + saved 60–160% scale, auto-download all media on chat open, in-app voice playback with cached transcripts, atomic self-update, zero secrets or private data in Git, `unsafe_code = "deny"` everywhere except localized `postbox-platform/src/win/` FFI.
