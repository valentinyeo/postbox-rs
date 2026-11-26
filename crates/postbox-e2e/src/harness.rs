#![forbid(unsafe_code)]

use crate::fixtures::{minimal_ogg_opus_packet, minimal_png_1x1};
use crate::mock_hub::{HubChatRow, HubMessageRow, MediaBehavior, MockHubServer};
use postbox_core::{Chat, ChatKey, MediaKind, Message, Provider, ReadStatus};
use postbox_keymap::Keymap;
use postbox_platform::MemoryPlatform;
use postbox_store::{Store, StoreError};
use rusqlite::{Connection, OptionalExtension, params};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static WORKSPACE_SEQ: AtomicU64 = AtomicU64::new(1);
static POSTBOXCTL_BIN: OnceLock<PathBuf> = OnceLock::new();
static POSTBOX_BIN: OnceLock<PathBuf> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrationMode {
    M0LoopbackBaseline,
    M8FullWorkspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct M8HookRegistry {
    pub mode: IntegrationMode,
    pub m1_ui: bool,
    pub m2_hub_sync: bool,
    pub m3_media: bool,
    pub m4_audio: bool,
    pub m5_dictation: bool,
    pub m6_video: bool,
    pub m7_os: bool,
}

impl Default for M8HookRegistry {
    fn default() -> Self {
        Self::current()
    }
}

impl M8HookRegistry {
    pub const fn current() -> Self {
        let mode = if cfg!(feature = "m8-full") {
            IntegrationMode::M8FullWorkspace
        } else {
            IntegrationMode::M0LoopbackBaseline
        };
        Self {
            mode,
            m1_ui: cfg!(feature = "m1_ui"),
            m2_hub_sync: cfg!(feature = "m2_hub_sync"),
            m3_media: cfg!(feature = "m3_media"),
            m4_audio: cfg!(feature = "m4_audio"),
            m5_dictation: cfg!(feature = "m5_dictation"),
            m6_video: cfg!(feature = "m6_video"),
            m7_os: cfg!(feature = "m7_os"),
        }
    }

    pub const fn is_m8_full(&self) -> bool {
        matches!(self.mode, IntegrationMode::M8FullWorkspace)
    }
}

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub struct E2eWorkspace {
    db_path: PathBuf,
    pub store: Store,
    pub platform: MemoryPlatform,
    pub keymap: Keymap,
    // MUST remain the final field: Rust drops fields in declaration order,
    // guaranteeing `store` closes all SQLite WAL/SHM handles and unmaps
    // `mmap_size` before `TempDirGuard::drop` deletes the temp directory on Windows.
    cleanup: TempDirGuard,
}

impl E2eWorkspace {
    pub fn new(label: &str) -> Self {
        let seq = WORKSPACE_SEQ.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let root = std::env::temp_dir().join(format!("postbox-e2e-{label}-{pid}-{seq}"));
        fs::create_dir_all(&root).expect("create E2eWorkspace tempdir");

        for sub in [
            "hub-media",
            "avatars",
            "paste",
            "link-previews",
            "msg-cache-wa",
            "voice-format-wa",
            "install",
            "update/stage",
        ] {
            fs::create_dir_all(root.join(sub)).expect("create Postbox subdir");
        }

        let db_path = root.join("postbox.db");
        let store = Store::open(&db_path).expect("open on-disk WAL Store");

        Self {
            db_path,
            store,
            platform: MemoryPlatform::default(),
            keymap: Keymap::postbox_defaults(),
            cleanup: TempDirGuard(root),
        }
    }

    pub fn root(&self) -> &Path {
        &self.cleanup.0
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn reopen_store(&mut self) -> Result<(), StoreError> {
        let placeholder = Store::open_in_memory()?;
        let old = std::mem::replace(&mut self.store, placeholder);
        drop(old);
        self.store = Store::open(&self.db_path)?;
        Ok(())
    }

    pub fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("resolve workspace root from CARGO_MANIFEST_DIR")
            .to_path_buf()
    }

    pub fn assets_dir() -> PathBuf {
        Self::repo_root().join("assets")
    }

    pub fn open_aux_conn(&self) -> Connection {
        Connection::open(&self.db_path).expect("open aux sqlite connection")
    }

    pub fn pragma_journal_mode(&self) -> String {
        let conn = self.open_aux_conn();
        conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .expect("query journal_mode")
    }

    pub fn pragma_foreign_keys(&self) -> i64 {
        let conn = self.open_aux_conn();
        conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .expect("query foreign_keys")
    }

    pub fn seed_media_cache(
        &self,
        cache_key: &str,
        sha256: &str,
        rel_path: &str,
        mime_type: &str,
        byte_size: i64,
        last_accessed_at: i64,
    ) {
        let conn = self.open_aux_conn();
        conn.execute(
            "INSERT OR REPLACE INTO media_cache (cache_key, sha256, rel_path, mime_type, byte_size, last_accessed_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![cache_key, sha256, rel_path, mime_type, byte_size, last_accessed_at],
        )
        .expect("insert media_cache row");
    }

    pub fn seed_media_cache_batch(&self, rows: &[(&str, &str, &str, &str, i64, i64)]) {
        let mut conn = self.open_aux_conn();
        let tx = conn
            .transaction()
            .expect("begin media_cache batch transaction");
        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO media_cache (cache_key, sha256, rel_path, mime_type, byte_size, last_accessed_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )
                .expect("prepare media_cache batch insert");
            for &(cache_key, sha256, rel_path, mime_type, byte_size, last_accessed_at) in rows {
                stmt.execute(params![
                    cache_key,
                    sha256,
                    rel_path,
                    mime_type,
                    byte_size,
                    last_accessed_at
                ])
                .expect("insert media_cache row");
            }
        }
        tx.commit().expect("commit media_cache batch transaction");
    }

    pub fn count_media_cache(&self) -> usize {
        let conn = self.open_aux_conn();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM media_cache", [], |r| r.get(0))
            .expect("count media_cache");
        n as usize
    }

    pub fn media_cache_exists(&self, cache_key: &str) -> bool {
        let conn = self.open_aux_conn();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM media_cache WHERE cache_key = ?1",
                params![cache_key],
                |r| r.get(0),
            )
            .expect("check media_cache key");
        n > 0
    }

    pub fn save_draft_row(
        &self,
        chat: &ChatKey,
        text_body: &str,
        reply_to_id: Option<&str>,
        updated_at: i64,
    ) {
        let conn = self.open_aux_conn();
        conn.execute(
            "INSERT INTO drafts (provider, account_id, chat_id, text_body, reply_to_id, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(provider, account_id, chat_id) DO UPDATE SET
               text_body = excluded.text_body,
               reply_to_id = excluded.reply_to_id,
               updated_at = excluded.updated_at",
            params![
                chat.provider.as_str(),
                chat.account,
                chat.id,
                text_body,
                reply_to_id,
                updated_at
            ],
        )
        .expect("upsert draft row");
    }

    pub fn load_draft_row(&self, chat: &ChatKey) -> Option<(String, Option<String>)> {
        let conn = self.open_aux_conn();
        conn.query_row(
            "SELECT text_body, reply_to_id FROM drafts WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3",
            params![chat.provider.as_str(), chat.account, chat.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .expect("load draft row")
    }

    pub fn delete_draft_row(&self, chat: &ChatKey) {
        let conn = self.open_aux_conn();
        conn.execute(
            "DELETE FROM drafts WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3",
            params![chat.provider.as_str(), chat.account, chat.id],
        )
        .expect("delete draft row");
    }

    pub fn upsert_account_row(
        &self,
        provider: Provider,
        account_id: &str,
        label: &str,
        state: &str,
        sort_order: i64,
    ) {
        let conn = self.open_aux_conn();
        conn.execute(
            "INSERT INTO accounts (provider, account_id, label, state, sort_order)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(provider, account_id) DO UPDATE SET
               label = excluded.label,
               state = excluded.state,
               sort_order = excluded.sort_order",
            params![provider.as_str(), account_id, label, state, sort_order],
        )
        .expect("upsert account row");
    }

    pub fn count_accounts(&self) -> usize {
        let conn = self.open_aux_conn();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))
            .expect("count accounts");
        n as usize
    }

    pub fn count_chats(&self) -> usize {
        let conn = self.open_aux_conn();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM chats", [], |r| r.get(0))
            .expect("count chats");
        n as usize
    }

    pub fn count_messages(&self) -> usize {
        let conn = self.open_aux_conn();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .expect("count messages");
        n as usize
    }
}

#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
}

pub struct ProcessRunner;

impl ProcessRunner {
    pub fn resolve_workspace_binary(bin_name: &str) -> PathBuf {
        let lock = match bin_name {
            "postboxctl" => &POSTBOXCTL_BIN,
            "postbox" => &POSTBOX_BIN,
            other => panic!("unsupported workspace binary: {other}"),
        };
        lock.get_or_init(|| Self::locate_or_build(bin_name)).clone()
    }

    fn locate_or_build(bin_name: &str) -> PathBuf {
        let exe_name = format!("{bin_name}{}", std::env::consts::EXE_SUFFIX);

        // 1. Honor explicit Cargo binary env var if exported.
        if let Some(env_path) = std::env::var_os(format!("CARGO_BIN_EXE_{bin_name}")) {
            let candidate = PathBuf::from(env_path);
            if candidate.is_file() {
                return candidate;
            }
        }

        // 2. Fast path: walk ancestors of the running test executable to find a
        //    pre-built sibling binary in the active profile dir (handles both
        //    `<target>/<profile>/deps/<test>` and Cargo 1.100.0's
        //    `<target>/<profile>/build/<pkg>/<hash>/out/<test>`, as well as
        //    `CARGO_TARGET_DIR`, `--release`, and `--target <triple>`).
        if let Ok(current) = std::env::current_exe()
            && let Some(found) = find_sibling_in_ancestors(&current, &exe_name)
        {
            return found;
        }

        // 3. Fallback: invoke `cargo build --message-format=json` in the active
        //    profile from `repo_root` so relative/absolute `CARGO_TARGET_DIR` and
        //    `--release` match the parent test run, then parse the exact executable path.
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let repo_root = E2eWorkspace::repo_root();
        let manifest = repo_root.join("Cargo.toml");
        let is_release = !cfg!(debug_assertions);
        let profile = if is_release { "release" } else { "debug" };

        let mut cmd = Command::new(cargo);
        cmd.current_dir(&repo_root)
            .args([
                "build",
                "--quiet",
                "--message-format=json",
                "--bin",
                bin_name,
                "--manifest-path",
            ])
            .arg(&manifest);
        if is_release {
            cmd.arg("--release");
        }
        let output = cmd.output().expect("invoke cargo build for sibling binary");
        assert!(
            output.status.success(),
            "cargo build --bin {bin_name} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );

        if let Some(exe) = parse_cargo_artifact_executable(&output.stdout, bin_name)
            && exe.is_file()
        {
            return exe;
        }

        // 4. Belt-and-suspenders post-build fallback (ancestors -> CARGO_TARGET_DIR -> default).
        if let Ok(current) = std::env::current_exe()
            && let Some(found) = find_sibling_in_ancestors(&current, &exe_name)
        {
            return found;
        }
        let target_dir = match std::env::var_os("CARGO_TARGET_DIR") {
            Some(dir) => {
                let p = PathBuf::from(dir);
                if p.is_absolute() {
                    p
                } else {
                    repo_root.join(p)
                }
            }
            None => repo_root.join("target"),
        };
        target_dir.join(profile).join(exe_name)
    }

    pub fn run(bin_name: &str, args: &[&str], timeout: Duration) -> CommandOutput {
        let path = Self::resolve_workspace_binary(bin_name);
        let started = Instant::now();
        let mut child = Command::new(&path)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn {}: {e}", path.display()));

        let mut stdout_pipe = child.stdout.take().expect("stdout piped");
        let mut stderr_pipe = child.stderr.take().expect("stderr piped");
        let stdout_thread = thread::spawn(move || {
            let mut s = String::new();
            let _ = stdout_pipe.read_to_string(&mut s);
            s
        });
        let stderr_thread = thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr_pipe.read_to_string(&mut s);
            s
        });

        let exit_code = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status.code().unwrap_or(-1),
                Ok(None) => {
                    if started.elapsed() >= timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!(
                            "binary {bin_name} timed out after {} ms",
                            timeout.as_millis()
                        );
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("try_wait failed for {bin_name}: {e}"),
            }
        };

        let stdout = stdout_thread.join().unwrap_or_default();
        let stderr = stderr_thread.join().unwrap_or_default();
        CommandOutput {
            exit_code,
            stdout,
            stderr,
            elapsed: started.elapsed(),
        }
    }
}

fn find_sibling_in_ancestors(current: &Path, exe_name: &str) -> Option<PathBuf> {
    // Bounded ancestor walk covering both classic Cargo (`<target>/<profile>/deps/<test>`)
    // and Cargo 1.100.0 (`<target>/<profile>/build/<pkg>/<hash>/out/<test>`).
    // Stops immediately after checking the active profile directory (parent of `deps` or `build`)
    // so it never wanders above `<target>/<profile>` into unrelated directories.
    let mut prev_name: Option<&std::ffi::OsStr> = None;
    for ancestor in current.ancestors().skip(1).take(6) {
        let candidate = ancestor.join(exe_name);
        if candidate.is_file() {
            return Some(candidate);
        }
        if matches!(prev_name.and_then(|n| n.to_str()), Some("deps" | "build"))
            && ancestor.file_name().and_then(|n| n.to_str()) != Some("build")
        {
            break;
        }
        prev_name = ancestor.file_name();
    }
    None
}

fn parse_cargo_artifact_executable(stdout: &[u8], bin_name: &str) -> Option<PathBuf> {
    let text = std::str::from_utf8(stdout).ok()?;
    for line in text.lines().rev() {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if val.get("reason").and_then(|v| v.as_str()) != Some("compiler-artifact") {
            continue;
        }
        let target_name = val
            .get("target")
            .and_then(|t| t.get("name"))
            .and_then(|n| n.as_str());
        let is_bin = val
            .get("target")
            .and_then(|t| t.get("kind"))
            .and_then(|k| k.as_array())
            .is_some_and(|kinds| kinds.iter().any(|k| k.as_str() == Some("bin")));
        if target_name == Some(bin_name)
            && is_bin
            && let Some(exe) = val.get("executable").and_then(|e| e.as_str())
        {
            return Some(PathBuf::from(exe));
        }
    }
    None
}

fn media_kind_as_str(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "image",
        MediaKind::Gif => "gif",
        MediaKind::Video => "video",
        MediaKind::Voice => "voice",
        MediaKind::Sticker => "sticker",
        MediaKind::Document => "document",
    }
}

fn read_status_as_str(status: ReadStatus) -> &'static str {
    match status {
        ReadStatus::None => "none",
        ReadStatus::Sending => "sending",
        ReadStatus::Sent => "sent",
        ReadStatus::Delivered => "delivered",
        ReadStatus::Read => "read",
        ReadStatus::Failed => "failed",
    }
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

#[derive(Debug, Clone)]
pub struct RealSizeInboxStats {
    pub chats: usize,
    pub messages: usize,
    pub images: usize,
    pub voice_notes: usize,
    pub seed_duration: Duration,
}

pub struct RealSizeInbox {
    pub chats: Vec<Chat>,
    pub messages: Vec<Message>,
    pub image_count: usize,
    pub voice_count: usize,
}

impl RealSizeInbox {
    pub const EXPECTED_CHATS: usize = 320;
    pub const EXPECTED_MESSAGES: usize = 6_704;
    pub const EXPECTED_IMAGES: usize = 638;
    pub const EXPECTED_VOICE_NOTES: usize = 319;

    pub fn generate() -> Self {
        let mut prng = SplitMix64::new(0x504F_5354_424F_5831);
        let mut chats = Vec::with_capacity(Self::EXPECTED_CHATS);

        let account_specs: &[(Provider, &str, usize)] = &[
            (Provider::Whatsapp, "personal", 100),
            (Provider::Whatsapp, "business", 60),
            (Provider::Slack, "T_ACME", 70),
            (Provider::Slack, "T_OPS", 30),
            (Provider::Telegram, "telegram", 35),
            (Provider::Signal, "signal", 25),
        ];

        for &(provider, account, count) in account_specs {
            for local_idx in 0..count {
                let global_chat_idx = chats.len();
                let (id, is_group, title) = match provider {
                    Provider::Whatsapp => {
                        // First 10 business chats intentionally reuse personal phone JIDs
                        if account == "business" && local_idx < 10 {
                            (
                                format!("1555000{local_idx:04}@s.whatsapp.net"),
                                false,
                                format!("Business Shared Contact {local_idx:02}"),
                            )
                        } else if local_idx % 5 == 4 {
                            (
                                format!("120363{account}{local_idx:04}@g.us"),
                                true,
                                format!("WA Group {account} {local_idx:02}"),
                            )
                        } else {
                            (
                                format!("1555000{local_idx:04}@s.whatsapp.net"),
                                false,
                                format!("WA Contact {account} {local_idx:02}"),
                            )
                        }
                    }
                    Provider::Slack => {
                        let is_chan = local_idx % 3 != 0;
                        let prefix = if is_chan { "C" } else { "D" };
                        (
                            format!("{prefix}{local_idx:06}"),
                            is_chan,
                            format!("{account} #channel-{local_idx:02}"),
                        )
                    }
                    Provider::Telegram => {
                        let is_grp = local_idx % 4 == 3;
                        (
                            format!("tg-{local_idx:05}"),
                            is_grp,
                            if local_idx == 0 {
                                "Saved Messages".to_string()
                            } else {
                                format!("Telegram Chat {local_idx:02}")
                            },
                        )
                    }
                    Provider::Signal => {
                        let is_grp = local_idx % 4 == 3;
                        (
                            format!("sig-{local_idx:05}"),
                            is_grp,
                            format!("Signal Peer {local_idx:02}"),
                        )
                    }
                };

                let pinned = global_chat_idx < 12 && global_chat_idx % 2 == 0;
                let archived_local = (260..300).contains(&global_chat_idx);
                let archived_remote = global_chat_idx >= 300;
                let unread_count =
                    if !archived_local && !archived_remote && global_chat_idx % 5 == 0 {
                        ((prng.next_u64() % 5) + 1) as u32
                    } else {
                        0
                    };
                let day = 1 + (global_chat_idx % 26);
                let hour = (global_chat_idx / 26) % 24;
                let minute = global_chat_idx % 60;
                let last_msg_ts = format!("2026-11-{day:02}T{hour:02}:{minute:02}:00Z");

                chats.push(Chat {
                    key: ChatKey::new(provider, account, id),
                    title,
                    is_group,
                    archived_local,
                    archived_remote,
                    pinned,
                    unread_count,
                    last_msg_ts,
                    last_msg_preview: format!("Latest preview for chat {global_chat_idx}"),
                    avatar_hash: None,
                });
            }
        }

        debug_assert_eq!(chats.len(), Self::EXPECTED_CHATS);

        let filler_openers = [
            "Hey, just checking in about",
            "Quick update regarding",
            "Can we sync later today on",
            "Thanks for sending over",
            "Following up from yesterday on",
            "Sounds good, I will look at",
            "Let me know once you see",
            "Just pushed the latest notes for",
            "Are you still free tomorrow for",
            "Great catch earlier during",
            "Short reminder before lunch about",
            "Happy to pair this afternoon on",
        ];
        let filler_topics = [
            "the weekend train tickets",
            "dinner reservations downtown",
            "the sprint retrospective agenda",
            "shipping the parcel before five",
            "picking up coffee on the way",
            "confirming the dentist appointment",
            "the balcony plants while away",
            "updating the shared grocery list",
            "reviewing the slide deck draft",
            "booking flights for next month",
            "the new office keycard pickup",
            "comparing cycling routes along the river",
            "coordinating the team offsite schedule",
            "checking the weather forecast for Sunday",
            "organizing the photo album links",
        ];
        let target_phrases = [
            "Intel Arc DXVA hardware video decoding benchmark passed smoothly",
            "Quartalsbericht Überblick und Rechnung für November bereit",
            "Quarterly budget review scheduled for Thursday morning",
            "Besprechung im Café München wegen Architektur und Performance",
            "Please review the atomic self-updater SHA-256 verification",
            "Voice note transcript cached in SQLite WAL storage",
            "Slack thread broadcast update sent to engineering channel",
            "Combined inbox triage completed with zero toolbar clutter",
            "Deepgram Nova-3 live dictation supports English and German",
            "Media auto-download 14-day eviction keeps disk cache tidy",
        ];
        let senders = ["Ada", "Alan", "Grace", "Linus", "Clara", "Lukas"];

        let mut messages = Vec::with_capacity(Self::EXPECTED_MESSAGES);
        let mut image_count = 0usize;
        let mut voice_count = 0usize;

        for (chat_idx, chat) in chats.iter().enumerate() {
            let msg_count = match chat_idx {
                0 => 160,
                1 => 44,
                2..=141 => 21,
                _ => 20,
            };

            for m_idx in 0..msg_count {
                let global_idx = messages.len();
                let (media_kind, mime_type, filename) =
                    if global_idx % 10 == 2 && image_count < Self::EXPECTED_IMAGES {
                        image_count += 1;
                        let ext = match image_count % 3 {
                            0 => ("image/png", format!("photo-{image_count:04}.png")),
                            1 => ("image/jpeg", format!("photo-{image_count:04}.jpg")),
                            _ => ("image/webp", format!("photo-{image_count:04}.webp")),
                        };
                        (Some(MediaKind::Image), Some(ext.0.to_string()), Some(ext.1))
                    } else if global_idx % 20 == 7 && voice_count < Self::EXPECTED_VOICE_NOTES {
                        voice_count += 1;
                        (
                            Some(MediaKind::Voice),
                            Some("audio/ogg".to_string()),
                            Some(format!("voice-{voice_count:04}.ogg")),
                        )
                    } else {
                        (None, None, None)
                    };

                let r = prng.next_u64() as usize;
                let body = if global_idx < 10 {
                    format!(
                        "{} canary-needle-{:03}",
                        target_phrases[global_idx],
                        global_idx + 1
                    )
                } else if global_idx % 53 == 0 {
                    // Realistic selectivity (~12-13 hits per domain keyword across 6,704 messages)
                    format!(
                        "{} (ref #{global_idx})",
                        target_phrases[(global_idx / 53) % target_phrases.len()]
                    )
                } else {
                    let opener = filler_openers[(global_idx + r) % filler_openers.len()];
                    let topic = filler_topics[(global_idx / 3 + r) % filler_topics.len()];
                    format!("{opener} {topic} #{global_idx}")
                };

                let sender = senders[(chat_idx + m_idx) % senders.len()];
                let day = 1 + (m_idx % 26);
                let hour = (m_idx / 26) % 24;
                let sec = m_idx % 60;
                let ts = format!("2026-11-{day:02}T{hour:02}:{:02}:{sec:02}Z", chat_idx % 60);

                let reply_to_id = if chat_idx == 1 && m_idx > 0 && m_idx <= 20 {
                    Some("M-0001-0000".to_string())
                } else {
                    None
                };

                messages.push(Message {
                    chat: chat.key.clone(),
                    msg_id: format!("M-{chat_idx:04}-{m_idx:04}"),
                    sender_id: format!("u-{}", sender.to_ascii_lowercase()),
                    sender_name: sender.to_string(),
                    from_me: m_idx % 4 == 0,
                    ts,
                    body,
                    reply_to_id,
                    reply_broadcast: false,
                    media_kind,
                    mime_type,
                    filename,
                    reactions: Vec::new(),
                    read_status: if m_idx % 4 == 0 {
                        ReadStatus::Read
                    } else {
                        ReadStatus::None
                    },
                    deleted: false,
                });
            }
        }

        assert_eq!(chats.len(), Self::EXPECTED_CHATS);
        assert_eq!(messages.len(), Self::EXPECTED_MESSAGES);
        assert_eq!(image_count, Self::EXPECTED_IMAGES);
        assert_eq!(voice_count, Self::EXPECTED_VOICE_NOTES);

        Self {
            chats,
            messages,
            image_count,
            voice_count,
        }
    }

    pub fn seed_store(&self, store: &mut Store) -> Result<RealSizeInboxStats, StoreError> {
        let started = Instant::now();
        for (i, chat) in self.chats.iter().enumerate() {
            store.upsert_chat(chat, 1_764_150_000 + i as i64)?;
        }
        for msg in &self.messages {
            store.upsert_message(msg)?;
        }
        Ok(RealSizeInboxStats {
            chats: self.chats.len(),
            messages: self.messages.len(),
            images: self.image_count,
            voice_notes: self.voice_count,
            seed_duration: started.elapsed(),
        })
    }

    /// Seeds all 320 chats and 6,704 messages into the workspace's on-disk WAL SQLite DB in a
    /// single bulk transaction (modeling initial sync batching), then exercises `Store::upsert_message`
    /// incrementally on 10 non-media canary messages so all 638 image and 319 voice rows retain
    /// consistent SQL `media_type`/`mime_type`/`filename` metadata.
    pub fn seed_workspace_bulk(
        &self,
        ws: &mut E2eWorkspace,
    ) -> Result<RealSizeInboxStats, StoreError> {
        let started = Instant::now();
        let canary_indices: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, msg)| msg.media_kind.is_none())
            .map(|(idx, _)| idx)
            .take(10)
            .collect();

        {
            let mut conn = ws.open_aux_conn();
            let tx = conn.transaction()?;
            {
                let mut chat_stmt = tx.prepare(
                    "INSERT OR REPLACE INTO chats (
                        provider, account_id, chat_id, title, is_group,
                        archived_local, archived_remote, pinned, unread_count,
                        last_msg_ts, last_msg_preview, avatar_hash, updated_at
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                )?;
                for (i, chat) in self.chats.iter().enumerate() {
                    chat_stmt.execute(params![
                        chat.key.provider.as_str(),
                        chat.key.account,
                        chat.key.id,
                        chat.title,
                        i64::from(chat.is_group),
                        i64::from(chat.archived_local),
                        i64::from(chat.archived_remote),
                        i64::from(chat.pinned),
                        i64::from(chat.unread_count),
                        chat.last_msg_ts,
                        chat.last_msg_preview,
                        chat.avatar_hash,
                        1_764_150_000_i64 + i as i64,
                    ])?;
                }
            }
            {
                let mut msg_stmt = tx.prepare(
                    "INSERT OR REPLACE INTO messages (
                        provider, account_id, chat_id, msg_id, sender_id, sender_name,
                        from_me, ts, body, reply_to_id, reply_broadcast,
                        media_type, mime_type, filename, reactions_json, read_status, deleted
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
                )?;
                let mut fts_stmt = tx.prepare(
                    "INSERT INTO messages_fts (provider, account_id, chat_id, msg_id, sender_name, body)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )?;
                for (idx, msg) in self.messages.iter().enumerate() {
                    if canary_indices.contains(&idx) {
                        continue;
                    }
                    let media_type = msg.media_kind.map(media_kind_as_str);
                    let reactions_json = serde_json::to_string(&msg.reactions)?;
                    msg_stmt.execute(params![
                        msg.chat.provider.as_str(),
                        msg.chat.account,
                        msg.chat.id,
                        msg.msg_id,
                        msg.sender_id,
                        msg.sender_name,
                        i64::from(msg.from_me),
                        msg.ts,
                        msg.body,
                        msg.reply_to_id,
                        i64::from(msg.reply_broadcast),
                        media_type,
                        msg.mime_type,
                        msg.filename,
                        reactions_json,
                        read_status_as_str(msg.read_status),
                        i64::from(msg.deleted),
                    ])?;
                    if !msg.deleted && !msg.body.is_empty() {
                        fts_stmt.execute(params![
                            msg.chat.provider.as_str(),
                            msg.chat.account,
                            msg.chat.id,
                            msg.msg_id,
                            msg.sender_name,
                            msg.body,
                        ])?;
                    }
                }
            }
            tx.commit()?;
        }

        // Exercise live Store::upsert_message on 10 non-media canary messages
        // (indices 0, 1, 3, 4, 5, 6, 8, 9, 10, 11 — includes canary-needle-001/005/010).
        for idx in canary_indices {
            ws.store.upsert_message(&self.messages[idx])?;
        }

        Ok(RealSizeInboxStats {
            chats: self.chats.len(),
            messages: self.messages.len(),
            images: self.image_count,
            voice_notes: self.voice_count,
            seed_duration: started.elapsed(),
        })
    }

    pub fn seed_mock_hub(&self, hub: &MockHubServer) {
        let chat_rows: Vec<HubChatRow> = self
            .chats
            .iter()
            .map(|c| HubChatRow {
                jid: format!("{}:{}", c.key.account, c.key.id),
                provider: c.key.provider.as_str().to_string(),
                name: c.title.clone(),
                kind: if c.is_group {
                    "group".into()
                } else {
                    "dm".into()
                },
                last_message_ts: c.last_msg_ts.clone(),
                unread_count: c.unread_count,
                unread: c.unread_count > 0,
                archived: c.is_archived(),
                account: Some(c.key.account.clone()),
                team_name: if c.key.provider == Provider::Slack {
                    Some(format!("Workspace {}", c.key.account))
                } else {
                    None
                },
                member_count: if c.is_group { Some(8) } else { None },
                saved: None,
            })
            .collect();
        hub.set_chats(chat_rows);

        let mut grouped: std::collections::HashMap<String, Vec<HubMessageRow>> =
            std::collections::HashMap::new();
        let png = minimal_png_1x1();
        let ogg = minimal_ogg_opus_packet();

        for m in &self.messages {
            let handle = format!("{}:{}", m.chat.account, m.chat.id);
            let mut row = HubMessageRow::simple(
                &handle,
                &m.msg_id,
                &m.sender_name,
                &m.body,
                &m.ts,
                m.from_me,
            );
            row.quoted_msg_id = m.reply_to_id.clone();
            if let Some(kind) = m.media_kind {
                row.MediaType = Some(format!("{kind:?}").to_ascii_lowercase());
                row.MimeType = m.mime_type.clone();
                row.Filename = m.filename.clone();
                match kind {
                    MediaKind::Image => {
                        row.MediaSize = Some(png.len() as u64);
                        hub.set_media(
                            &handle,
                            &m.msg_id,
                            MediaBehavior::Ready {
                                bytes: png.clone(),
                                content_type: "image/png".into(),
                            },
                        );
                    }
                    MediaKind::Voice => {
                        row.MediaSize = Some(ogg.len() as u64);
                        hub.set_media(
                            &handle,
                            &m.msg_id,
                            MediaBehavior::Ready {
                                bytes: ogg.clone(),
                                content_type: "audio/ogg".into(),
                            },
                        );
                    }
                    _ => {}
                }
            }
            grouped.entry(handle).or_default().push(row);
        }
        for (handle, rows) in grouped {
            hub.set_messages(handle, rows);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_drop_removes_temp_dir_after_closing_store() {
        let ws = E2eWorkspace::new("unit_drop");
        let chat = Chat {
            key: ChatKey::new(Provider::Whatsapp, "personal", "drop@s.whatsapp.net"),
            title: "Drop Test".into(),
            is_group: false,
            archived_local: false,
            archived_remote: false,
            pinned: false,
            unread_count: 0,
            last_msg_ts: "2026-11-26T12:00:00Z".into(),
            last_msg_preview: "hi".into(),
            avatar_hash: None,
        };
        ws.store.upsert_chat(&chat, 100).unwrap();
        let root = ws.root().to_path_buf();
        assert!(root.exists());
        drop(ws);
        assert!(
            !root.exists(),
            "TempDirGuard must remove {} after Store drops",
            root.display()
        );
    }

    #[test]
    fn find_sibling_in_ancestors_handles_deps_and_cargo_1_100_layouts_without_escaping() {
        let base = std::env::temp_dir().join(format!(
            "postbox-e2e-ancestor-test-{}-{}",
            std::process::id(),
            WORKSPACE_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let _guard = TempDirGuard(base.clone());

        // Classic layout: t/debug/deps/test_bin + t/debug/postboxctl
        let classic_deps = base.join("t/debug/deps");
        fs::create_dir_all(&classic_deps).unwrap();
        let classic_test = classic_deps.join("test_bin");
        let classic_bin = base.join("t/debug/postboxctl");
        fs::write(&classic_test, b"t").unwrap();
        fs::write(&classic_bin, b"b").unwrap();
        assert_eq!(
            find_sibling_in_ancestors(&classic_test, "postboxctl"),
            Some(classic_bin)
        );

        // Cargo 1.100.0 layout: t2/release/build/postbox-e2e/abc123/out/test_bin + t2/release/postboxctl
        let modern_out = base.join("t2/release/build/postbox-e2e/abc123/out");
        fs::create_dir_all(&modern_out).unwrap();
        let modern_test = modern_out.join("test_bin");
        let modern_bin = base.join("t2/release/postboxctl");
        fs::write(&modern_test, b"t").unwrap();
        fs::write(&modern_bin, b"b").unwrap();
        assert_eq!(
            find_sibling_in_ancestors(&modern_test, "postboxctl"),
            Some(modern_bin)
        );

        // Escape guard: t3/postboxctl + t3/debug/deps/test_bin (no t3/debug/postboxctl) -> None
        let escape_deps = base.join("t3/debug/deps");
        fs::create_dir_all(&escape_deps).unwrap();
        let escape_test = escape_deps.join("test_bin");
        let escaped_bin = base.join("t3/postboxctl");
        fs::write(&escape_test, b"t").unwrap();
        fs::write(&escaped_bin, b"b").unwrap();
        assert_eq!(find_sibling_in_ancestors(&escape_test, "postboxctl"), None);
    }

    #[test]
    fn parse_cargo_artifact_executable_extracts_matching_bin() {
        let json_lines = concat!(
            "{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"lib\"],\"name\":\"postbox_core\"},\"executable\":null}\n",
            "{\"reason\":\"compiler-message\",\"message\":{\"level\":\"warning\"}}\n",
            "{\"reason\":\"compiler-artifact\",\"target\":{\"kind\":[\"bin\"],\"name\":\"postboxctl\"},\"executable\":\"/custom/target/release/postboxctl\"}\n",
            "{\"reason\":\"build-finished\",\"success\":true}\n"
        );
        assert_eq!(
            parse_cargo_artifact_executable(json_lines.as_bytes(), "postboxctl"),
            Some(PathBuf::from("/custom/target/release/postboxctl"))
        );
        assert_eq!(
            parse_cargo_artifact_executable(json_lines.as_bytes(), "postbox"),
            None
        );
    }

    #[test]
    fn seed_media_cache_batch_and_bulk_media_counts_and_neutral_senders() {
        let dataset = RealSizeInbox::generate();
        const ALLOWED_SENDERS: &[&str] = &["Ada", "Alan", "Grace", "Linus", "Clara", "Lukas"];
        assert!(
            dataset.messages.iter().all(|m| {
                ALLOWED_SENDERS.contains(&m.sender_name.as_str())
                    && m.sender_id == format!("u-{}", m.sender_name.to_ascii_lowercase())
            }),
            "fixture messages must use only allowlisted synthetic sender names"
        );

        let mut ws = E2eWorkspace::new("unit_bulk");
        let stats = dataset.seed_workspace_bulk(&mut ws).unwrap();
        assert_eq!(stats.chats, RealSizeInbox::EXPECTED_CHATS);
        assert_eq!(stats.messages, RealSizeInbox::EXPECTED_MESSAGES);

        let conn = ws.open_aux_conn();
        let img_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE media_type = 'image' AND mime_type IS NOT NULL AND filename IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let voice_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE media_type = 'voice' AND mime_type = 'audio/ogg' AND filename IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let read_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE read_status = 'read'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(img_rows as usize, RealSizeInbox::EXPECTED_IMAGES);
        assert_eq!(voice_rows as usize, RealSizeInbox::EXPECTED_VOICE_NOTES);
        assert!(read_rows > 0);

        let owned: Vec<(String, String, String, i64)> = (0..50usize)
            .map(|i| {
                (
                    format!("k-{i:03}"),
                    format!("s-{i:03}"),
                    format!("hub-media/k-{i:03}.png"),
                    1_764_150_000 + i as i64,
                )
            })
            .collect();
        let refs: Vec<(&str, &str, &str, &str, i64, i64)> = owned
            .iter()
            .map(|(k, s, r, ts)| (k.as_str(), s.as_str(), r.as_str(), "image/png", 512, *ts))
            .collect();
        ws.seed_media_cache_batch(&refs);
        assert_eq!(ws.count_media_cache(), 50);
    }
}
