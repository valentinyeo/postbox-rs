pub mod media;
pub mod outbox;
pub mod state;
pub mod voice;

use postbox_core::archive::unix_seconds;
use postbox_core::drafts::{MAX_DRAFTS, validate_draft_key, validate_draft_text};
use postbox_core::views::{
    canonical_ts_sort_key, compare_account_view, compare_combined_inbox, normalize_timestamp,
};
use postbox_core::{
    Account, Chat, ChatKey, Draft, FontScale, MediaKind, Message, Provider, Reaction, ReadStatus,
    ThemeChoice, UiSettings, ViewTab,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use thiserror::Error;

pub use media::{FetchMarker, MediaCacheEntry, SETTING_EMOJI_RECENTS, gone_media_pair_key};
pub use postbox_core::outbox::{OutboxKind, OutboxPhase, OutboxRecord};
pub use state::{MAX_PINS, PendingArchiveRow};
pub use voice::{SETTING_DICTATION_LANGUAGE, SETTING_MICROPHONE, SETTING_PLAYBACK_SPEED};

pub const MEDIA_TTL_SECS: i64 = 14 * 24 * 60 * 60;
pub const BUSY_TIMEOUT_MS: u64 = 5_000;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite_error")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json_error")]
    Json(#[from] serde_json::Error),
    #[error("invalid_outbox_record")]
    InvalidOutboxRecord,
    #[error("outbox_full")]
    OutboxFull,
    #[error("outbox_inflight")]
    OutboxInflight,
    #[error("outbox_not_found")]
    OutboxNotFound,
    #[error("pin_limit_reached")]
    PinLimitReached,
    #[error("draft_limit_reached")]
    DraftLimitReached,
    #[error("invalid_draft")]
    InvalidDraft,
    #[error("pending_reads_full")]
    PendingReadsFull,
}

/// Composite `(ts, msg_id)` cursor matching Hub's `<MsgID>:<Timestamp>` `before` cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageCursor {
    pub ts: String,
    pub msg_id: String,
}

impl MessageCursor {
    pub fn new(ts: impl Into<String>, msg_id: impl Into<String>) -> Self {
        Self {
            ts: ts.into(),
            msg_id: msg_id.into(),
        }
    }

    pub fn from_message(msg: &Message) -> Self {
        Self {
            ts: msg.ts.clone(),
            msg_id: msg.msg_id.clone(),
        }
    }

    /// Formats Hub wire `<MsgID>:<Timestamp>` cursor (`hub.zig:56-60`).
    pub fn to_hub_before_cursor(&self) -> String {
        format!("{}:{}", self.msg_id, self.ts)
    }

    /// Parses Hub wire `<MsgID>:<Timestamp>` safely even when `MsgID` contains colons
    /// (e.g. `"personal:ABC:2026-10-01T11:58:41Z"`).
    pub fn parse_hub_before_cursor(raw: &str) -> Option<Self> {
        for (idx, _) in raw.rmatch_indices(':') {
            let candidate_ts = &raw[idx + 1..];
            if unix_seconds(candidate_ts).is_some() {
                let msg_id = &raw[..idx];
                if !msg_id.is_empty() {
                    return Some(Self {
                        ts: candidate_ts.to_owned(),
                        msg_id: msg_id.to_owned(),
                    });
                }
            }
        }
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub chat: ChatKey,
    pub msg_id: String,
    pub sender_name: String,
    pub snippet: String,
}

fn is_combining_diacritical_mark(ch: char) -> bool {
    matches!(
        ch,
        '\u{0300}'..='\u{036F}'
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE20}'..='\u{FE2F}'
    )
}

/// Converts arbitrary user input into a safe FTS5 MATCH expression with prefix matching.
/// Keeps Unicode combining diacritical marks inside active tokens so NFD queries
/// (such as `"u\u{0308}ber"`) remain a single prefix term for SQLite `unicode61 remove_diacritics 2`.
/// Returns `None` if no searchable alphanumeric tokens remain.
pub fn sanitize_fts_query(raw: &str) -> Option<String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    for ch in raw.chars() {
        if ch.is_alphanumeric() {
            current.push(ch);
        } else if is_combining_diacritical_mark(ch) {
            if !current.is_empty() {
                current.push(ch);
            }
        } else if !current.is_empty() {
            terms.push(format!("\"{current}\"*"));
            current.clear();
            if terms.len() >= 16 {
                break;
            }
        }
    }
    if !current.is_empty() && terms.len() < 16 {
        terms.push(format!("\"{current}\"*"));
    }
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

pub struct Store {
    pub(crate) conn: Connection,
}

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (
    provider TEXT NOT NULL,
    account_id TEXT NOT NULL,
    label TEXT NOT NULL,
    state TEXT NOT NULL,
    sort_order INTEGER NOT NULL,
    last_event_at TEXT,
    PRIMARY KEY (provider, account_id)
);

CREATE TABLE IF NOT EXISTS chats (
    provider TEXT NOT NULL,
    account_id TEXT NOT NULL,
    chat_id TEXT NOT NULL,
    title TEXT NOT NULL,
    is_group INTEGER NOT NULL DEFAULT 0,
    archived_local INTEGER NOT NULL DEFAULT 0,
    archived_remote INTEGER NOT NULL DEFAULT 0,
    pinned INTEGER NOT NULL DEFAULT 0,
    unread_count INTEGER NOT NULL DEFAULT 0,
    last_msg_ts TEXT NOT NULL DEFAULT '',
    last_msg_preview TEXT NOT NULL DEFAULT '',
    avatar_hash TEXT,
    updated_at INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (provider, account_id, chat_id)
);

CREATE TABLE IF NOT EXISTS messages (
    provider TEXT NOT NULL,
    account_id TEXT NOT NULL,
    chat_id TEXT NOT NULL,
    msg_id TEXT NOT NULL,
    sender_id TEXT NOT NULL,
    sender_name TEXT NOT NULL,
    from_me INTEGER NOT NULL DEFAULT 0,
    ts TEXT NOT NULL,
    ts_key TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL DEFAULT '',
    reply_to_id TEXT,
    reply_broadcast INTEGER NOT NULL DEFAULT 0,
    media_type TEXT,
    mime_type TEXT,
    filename TEXT,
    file_size INTEGER,
    reactions_json TEXT NOT NULL DEFAULT '[]',
    read_status TEXT NOT NULL DEFAULT 'none',
    deleted INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (provider, account_id, chat_id, msg_id)
);

CREATE INDEX IF NOT EXISTS idx_messages_timeline
    ON messages (provider, account_id, chat_id, ts DESC, msg_id DESC);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    provider UNINDEXED,
    account_id UNINDEXED,
    chat_id UNINDEXED,
    msg_id UNINDEXED,
    chat_title,
    sender_name,
    body,
    tokenize='unicode61 remove_diacritics 2'
);

CREATE TABLE IF NOT EXISTS outbox (
    client_id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    account_id TEXT NOT NULL,
    chat_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    phase TEXT NOT NULL,
    attempt_serial INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    ambiguous INTEGER NOT NULL DEFAULT 0,
    text_body TEXT NOT NULL DEFAULT '',
    reply_to_id TEXT,
    reply_broadcast INTEGER NOT NULL DEFAULT 0,
    staged_file_path TEXT,
    wire_filename TEXT NOT NULL DEFAULT '',
    file_sha256 BLOB,
    file_bytes INTEGER NOT NULL DEFAULT 0,
    ptt INTEGER NOT NULL DEFAULT 0,
    voice_seconds INTEGER NOT NULL DEFAULT 0,
    attachment_ready INTEGER NOT NULL DEFAULT 0,
    failure_reason TEXT,
    not_before_ms INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_outbox_ready
    ON outbox (phase, not_before_ms ASC, created_at_ms ASC);

CREATE TABLE IF NOT EXISTS transcripts (
    media_sha256 TEXT PRIMARY KEY,
    language TEXT NOT NULL,
    text TEXT NOT NULL,
    formatted_text TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS drafts (
    provider TEXT NOT NULL,
    account_id TEXT NOT NULL,
    chat_id TEXT NOT NULL,
    text_body TEXT NOT NULL DEFAULT '',
    reply_to_id TEXT,
    staged_image_path TEXT,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (provider, account_id, chat_id)
);

CREATE TABLE IF NOT EXISTS media_cache (
    cache_key TEXT PRIMARY KEY,
    sha256 TEXT NOT NULL,
    rel_path TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    byte_size INTEGER NOT NULL,
    last_accessed_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS pins (
    provider TEXT NOT NULL,
    account_id TEXT NOT NULL,
    chat_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    pinned_at INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (provider, account_id, chat_id)
);

CREATE TABLE IF NOT EXISTS view_order (
    position INTEGER NOT NULL,
    view_key TEXT PRIMARY KEY
);

CREATE TABLE IF NOT EXISTS read_watermarks (
    provider TEXT NOT NULL,
    account_id TEXT NOT NULL,
    chat_id TEXT NOT NULL,
    read_ts INTEGER NOT NULL DEFAULT 0,
    incoming_ts INTEGER NOT NULL DEFAULT 0,
    checked_ts INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (provider, account_id, chat_id)
);

CREATE TABLE IF NOT EXISTS pending_reads (
    position INTEGER NOT NULL,
    provider TEXT NOT NULL,
    account_id TEXT NOT NULL,
    chat_id TEXT NOT NULL,
    retries INTEGER NOT NULL DEFAULT 0,
    backoff_until_ms INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (provider, account_id, chat_id)
);

CREATE TABLE IF NOT EXISTS archive_rules (
    provider TEXT NOT NULL,
    account_id TEXT NOT NULL,
    chat_id TEXT NOT NULL,
    baseline_ts TEXT NOT NULL DEFAULT '',
    keep INTEGER NOT NULL DEFAULT 0,
    checked_unix INTEGER NOT NULL DEFAULT 0,
    returned INTEGER NOT NULL DEFAULT 0,
    phone INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (provider, account_id, chat_id)
);

CREATE TABLE IF NOT EXISTS pending_archives (
    provider TEXT NOT NULL,
    account_id TEXT NOT NULL,
    chat_id TEXT NOT NULL,
    intent TEXT NOT NULL,
    not_before_ms INTEGER NOT NULL DEFAULT 0,
    retries INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (provider, account_id, chat_id)
);

CREATE TABLE IF NOT EXISTS chat_prefs (
    provider TEXT NOT NULL,
    account_id TEXT NOT NULL,
    chat_id TEXT NOT NULL,
    replies_fold INTEGER,
    PRIMARY KEY (provider, account_id, chat_id)
);

CREATE TABLE IF NOT EXISTS account_prefs (
    provider TEXT NOT NULL,
    account_id TEXT NOT NULL,
    slack_threads_enabled INTEGER NOT NULL DEFAULT 1,
    workspace_name TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (provider, account_id)
);

CREATE TABLE IF NOT EXISTS view_prefs (
    view_key TEXT PRIMARY KEY,
    show_archived INTEGER NOT NULL DEFAULT 0,
    selected_provider TEXT,
    selected_account_id TEXT,
    selected_chat_id TEXT
);

CREATE TABLE IF NOT EXISTS gone_media (
    pair_key TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    account_id TEXT NOT NULL,
    chat_id TEXT NOT NULL,
    msg_id TEXT NOT NULL,
    recorded_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_gone_media_chat
    ON gone_media (provider, account_id, chat_id);

CREATE TABLE IF NOT EXISTS media_fetch_markers (
    marker_key TEXT PRIMARY KEY,
    status TEXT NOT NULL,
    checked_at INTEGER NOT NULL,
    rel_path TEXT
);

CREATE TABLE IF NOT EXISTS played_voice (
    pair_key TEXT PRIMARY KEY,
    played_at INTEGER NOT NULL,
    seq INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_played_voice_seq
    ON played_voice (seq ASC);

CREATE TABLE IF NOT EXISTS voice_positions (
    pair_key TEXT PRIMARY KEY,
    pos_ms INTEGER NOT NULL,
    dur_ms INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    seq INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_voice_positions_seq
    ON voice_positions (seq ASC);
"#;

const SETTING_THEME: &str = "ui.theme";
const SETTING_FONT_SCALE: &str = "ui.font_scale";
pub(crate) const SETTING_VIEW_ORDER: &str = "ui.view_order";
const SETTING_SHOW_ARCHIVED: &str = "ui.show_archived";

fn map_chat_row(row: &Row<'_>) -> rusqlite::Result<Chat> {
    let provider_str: String = row.get(0)?;
    let provider = Provider::parse(&provider_str).unwrap_or(Provider::Whatsapp);
    Ok(Chat {
        key: ChatKey::new(provider, row.get::<_, String>(1)?, row.get::<_, String>(2)?),
        title: row.get(3)?,
        is_group: row.get::<_, i64>(4)? != 0,
        archived_local: row.get::<_, i64>(5)? != 0,
        archived_remote: row.get::<_, i64>(6)? != 0,
        pinned: row.get::<_, i64>(7)? != 0,
        unread_count: row.get::<_, i64>(8)? as u32,
        last_msg_ts: row.get(9)?,
        last_msg_preview: row.get(10)?,
        avatar_hash: row.get(11)?,
    })
}

fn table_has_column(conn: &Connection, table: &str, col_name: &str) -> Result<bool, StoreError> {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&sql)?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(names.iter().any(|name| name == col_name))
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    /// Opens a named shared-cache in-memory SQLite database so UI and background sync worker
    /// connections can share `--in-memory` state cleanly.
    pub fn open_shared_memory(name: &str) -> Result<Self, StoreError> {
        let uri = format!("file:{name}?mode=memory&cache=shared");
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI;
        let conn = Connection::open_with_flags(uri, flags)?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self, StoreError> {
        conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "mmap_size", 67_108_864_i64)?;
        conn.pragma_update(None, "cache_size", -16_000_i64)?;
        conn.execute_batch(SCHEMA_SQL)?;

        // Forward-migration for existing M0 databases missing `staged_image_path` on `drafts`.
        if !table_has_column(&conn, "drafts", "staged_image_path")? {
            conn.execute("ALTER TABLE drafts ADD COLUMN staged_image_path TEXT", [])?;
        }

        // Forward-migrations for existing M0/M1 databases missing M2 `outbox` columns.
        let outbox_migrations: &[(&str, &str)] = &[
            ("attempts", "INTEGER NOT NULL DEFAULT 0"),
            ("ambiguous", "INTEGER NOT NULL DEFAULT 0"),
            ("wire_filename", "TEXT NOT NULL DEFAULT ''"),
            ("file_bytes", "INTEGER NOT NULL DEFAULT 0"),
            ("ptt", "INTEGER NOT NULL DEFAULT 0"),
            ("voice_seconds", "INTEGER NOT NULL DEFAULT 0"),
            ("attachment_ready", "INTEGER NOT NULL DEFAULT 0"),
            ("staged_file_path", "TEXT"),
            ("file_sha256", "BLOB"),
        ];
        for (col, decl) in outbox_migrations {
            if !table_has_column(&conn, "outbox", col)? {
                let sql = format!("ALTER TABLE outbox ADD COLUMN {col} {decl}");
                conn.execute(&sql, [])?;
            }
        }
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_outbox_ready ON outbox (phase, not_before_ms ASC, created_at_ms ASC)",
            [],
        )?;

        // Forward-migration for existing M0/M1 databases missing `ts_key` on `messages`.
        if !table_has_column(&conn, "messages", "ts_key")? {
            conn.execute(
                "ALTER TABLE messages ADD COLUMN ts_key TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        // Forward-migration for existing M0/M1/M2 databases missing `file_size` on `messages`.
        if !table_has_column(&conn, "messages", "file_size")? {
            conn.execute("ALTER TABLE messages ADD COLUMN file_size INTEGER", [])?;
        }
        // Forward-migration for existing M0-M3 databases missing `formatted_text`/`updated_at` on `transcripts`.
        if !table_has_column(&conn, "transcripts", "formatted_text")? {
            conn.execute("ALTER TABLE transcripts ADD COLUMN formatted_text TEXT", [])?;
        }
        if !table_has_column(&conn, "transcripts", "updated_at")? {
            conn.execute(
                "ALTER TABLE transcripts ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        {
            let mut stmt =
                conn.prepare("SELECT rowid, ts FROM messages WHERE ts_key = '' AND ts != ''")?;
            let rows: Vec<(i64, String)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            if !rows.is_empty() {
                let mut upd = conn.prepare("UPDATE messages SET ts_key = ?1 WHERE rowid = ?2")?;
                for (rowid, ts) in rows {
                    let key = canonical_ts_sort_key(&ts);
                    upd.execute(params![key, rowid])?;
                }
            }
        }
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_messages_timeline_key ON messages (provider, account_id, chat_id, ts_key DESC, msg_id DESC)",
            [],
        )?;

        // Forward-migration for existing M0 databases whose `messages_fts` lacks `chat_title`.
        if !table_has_column(&conn, "messages_fts", "chat_title")? {
            conn.execute_batch(
                r#"
                DROP TABLE messages_fts;
                CREATE VIRTUAL TABLE messages_fts USING fts5(
                    provider UNINDEXED,
                    account_id UNINDEXED,
                    chat_id UNINDEXED,
                    msg_id UNINDEXED,
                    chat_title,
                    sender_name,
                    body,
                    tokenize='unicode61 remove_diacritics 2'
                );
                INSERT INTO messages_fts (
                    rowid, provider, account_id, chat_id, msg_id, chat_title, sender_name, body
                )
                SELECT m.rowid, m.provider, m.account_id, m.chat_id, m.msg_id,
                       COALESCE(c.title, ''), m.sender_name, m.body
                FROM messages m
                LEFT JOIN chats c
                    ON c.provider = m.provider
                   AND c.account_id = m.account_id
                   AND c.chat_id = m.chat_id
                WHERE m.deleted = 0
                  AND (m.body != '' OR m.sender_name != '' OR COALESCE(c.title, '') != '');
                "#,
            )?;
        }

        Ok(Self { conn })
    }

    pub fn upsert_account(&self, account: &Account) -> Result<(), StoreError> {
        self.conn.execute(
            r#"
            INSERT INTO accounts (
                provider, account_id, label, state, sort_order, last_event_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(provider, account_id) DO UPDATE SET
                label = excluded.label,
                state = excluded.state,
                sort_order = excluded.sort_order,
                last_event_at = excluded.last_event_at
            "#,
            params![
                account.provider.as_str(),
                account.account_id,
                account.label,
                account.state,
                account.sort_order,
                account.last_event_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_accounts(&self) -> Result<Vec<Account>, StoreError> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT provider, account_id, label, state, sort_order, last_event_at
            FROM accounts
            ORDER BY sort_order ASC, provider ASC, account_id ASC
            "#,
        )?;
        let rows = stmt.query_map([], |row| {
            let provider = Provider::parse(&row.get::<_, String>(0)?).unwrap_or(Provider::Whatsapp);
            Ok(Account {
                provider,
                account_id: row.get(1)?,
                label: row.get(2)?,
                state: row.get(3)?,
                sort_order: row.get(4)?,
                last_event_at: row.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn upsert_chat(&self, chat: &Chat, updated_at: i64) -> Result<(), StoreError> {
        let old_title: Option<String> = self
            .conn
            .query_row(
                "SELECT title FROM chats WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3",
                params![chat.key.provider.as_str(), chat.key.account, chat.key.id],
                |row| row.get(0),
            )
            .optional()?;

        let norm_ts = normalize_timestamp(&chat.last_msg_ts);

        self.conn.execute(
            r#"
            INSERT INTO chats (
                provider, account_id, chat_id, title, is_group,
                archived_local, archived_remote, pinned, unread_count,
                last_msg_ts, last_msg_preview, avatar_hash, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            ON CONFLICT(provider, account_id, chat_id) DO UPDATE SET
                title = excluded.title,
                is_group = excluded.is_group,
                archived_local = excluded.archived_local,
                archived_remote = excluded.archived_remote,
                pinned = excluded.pinned,
                unread_count = excluded.unread_count,
                last_msg_ts = excluded.last_msg_ts,
                last_msg_preview = excluded.last_msg_preview,
                avatar_hash = excluded.avatar_hash,
                updated_at = excluded.updated_at
            "#,
            params![
                chat.key.provider.as_str(),
                chat.key.account,
                chat.key.id,
                chat.title,
                i64::from(chat.is_group),
                i64::from(chat.archived_local),
                i64::from(chat.archived_remote),
                i64::from(chat.pinned),
                i64::from(chat.unread_count),
                norm_ts,
                chat.last_msg_preview,
                chat.avatar_hash,
                updated_at,
            ],
        )?;

        if old_title.as_deref() != Some(chat.title.as_str()) {
            self.sync_chat_messages_fts(&chat.key, &chat.title)?;
        }

        Ok(())
    }

    /// High-throughput transactional batch chat upsert (e.g., 320 chats in a single WAL commit).
    /// Preserves monotonic `last_msg_ts` and local `pins` table membership so Hub refreshes never
    /// regress timestamps or clear local pins.
    pub fn upsert_chats_batch(
        &mut self,
        chats: &[Chat],
        updated_at: i64,
    ) -> Result<usize, StoreError> {
        if chats.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.transaction()?;
        let mut renamed_chats: Vec<(ChatKey, String)> = Vec::new();
        {
            let mut existing_stmt = tx.prepare(
                "SELECT title, last_msg_ts FROM chats WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3",
            )?;
            let mut pin_stmt = tx.prepare(
                "SELECT 1 FROM pins WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3",
            )?;
            let mut upsert_stmt = tx.prepare(
                r#"
                INSERT INTO chats (
                    provider, account_id, chat_id, title, is_group,
                    archived_local, archived_remote, pinned, unread_count,
                    last_msg_ts, last_msg_preview, avatar_hash, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                ON CONFLICT(provider, account_id, chat_id) DO UPDATE SET
                    title = excluded.title,
                    is_group = excluded.is_group,
                    archived_local = excluded.archived_local,
                    archived_remote = excluded.archived_remote,
                    pinned = excluded.pinned,
                    unread_count = excluded.unread_count,
                    last_msg_ts = excluded.last_msg_ts,
                    last_msg_preview = excluded.last_msg_preview,
                    avatar_hash = excluded.avatar_hash,
                    updated_at = excluded.updated_at
                "#,
            )?;

            for chat in chats {
                let existing: Option<(String, String)> = existing_stmt
                    .query_row(
                        params![chat.key.provider.as_str(), chat.key.account, chat.key.id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;

                let is_locally_pinned = pin_stmt
                    .query_row(
                        params![chat.key.provider.as_str(), chat.key.account, chat.key.id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();

                let effective_pinned = chat.pinned || is_locally_pinned;
                let new_norm = normalize_timestamp(&chat.last_msg_ts);
                let effective_ts = match existing.as_ref() {
                    Some((_, prev_ts)) if !prev_ts.is_empty() => {
                        let prev_norm = normalize_timestamp(prev_ts);
                        if new_norm.trim_end_matches('Z') > prev_norm.trim_end_matches('Z') {
                            new_norm
                        } else {
                            prev_norm
                        }
                    }
                    _ => new_norm,
                };

                upsert_stmt.execute(params![
                    chat.key.provider.as_str(),
                    chat.key.account,
                    chat.key.id,
                    chat.title,
                    i64::from(chat.is_group),
                    i64::from(chat.archived_local),
                    i64::from(chat.archived_remote),
                    i64::from(effective_pinned),
                    i64::from(chat.unread_count),
                    effective_ts,
                    chat.last_msg_preview,
                    chat.avatar_hash,
                    updated_at,
                ])?;

                if let Some((old_title, _)) = existing
                    && old_title != chat.title
                {
                    renamed_chats.push((chat.key.clone(), chat.title.clone()));
                }
            }
        }

        if !renamed_chats.is_empty() {
            let mut msg_stmt = tx.prepare(
                "SELECT rowid, msg_id, sender_name, body FROM messages WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3 AND deleted = 0",
            )?;
            let mut del_stmt = tx.prepare("DELETE FROM messages_fts WHERE rowid = ?1")?;
            let mut ins_stmt = tx.prepare(
                "INSERT INTO messages_fts (rowid, provider, account_id, chat_id, msg_id, chat_title, sender_name, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for (chat_key, chat_title) in renamed_chats {
                let rows: Vec<(i64, String, String, String)> = msg_stmt
                    .query_map(
                        params![chat_key.provider.as_str(), chat_key.account, chat_key.id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )?
                    .collect::<Result<Vec<_>, _>>()?;
                for (rowid, msg_id, sender_name, body) in rows {
                    del_stmt.execute(params![rowid])?;
                    if !body.is_empty() || !sender_name.is_empty() || !chat_title.is_empty() {
                        ins_stmt.execute(params![
                            rowid,
                            chat_key.provider.as_str(),
                            chat_key.account,
                            chat_key.id,
                            msg_id,
                            chat_title,
                            sender_name,
                            body,
                        ])?;
                    }
                }
            }
        }

        tx.commit()?;
        Ok(chats.len())
    }

    fn sync_chat_messages_fts(
        &self,
        chat_key: &ChatKey,
        chat_title: &str,
    ) -> Result<(), StoreError> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT rowid, msg_id, sender_name, body
            FROM messages
            WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3 AND deleted = 0
            "#,
        )?;
        let rows: Vec<(i64, String, String, String)> = stmt
            .query_map(
                params![chat_key.provider.as_str(), chat_key.account, chat_key.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;

        if rows.is_empty() {
            return Ok(());
        }

        self.conn.execute_batch("SAVEPOINT sync_chat_fts")?;
        let res = (|| -> Result<(), StoreError> {
            let mut del_stmt = self
                .conn
                .prepare("DELETE FROM messages_fts WHERE rowid = ?1")?;
            let mut ins_stmt = self.conn.prepare(
                r#"
                INSERT INTO messages_fts (
                    rowid, provider, account_id, chat_id, msg_id, chat_title, sender_name, body
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                "#,
            )?;
            for (rowid, msg_id, sender_name, body) in rows {
                del_stmt.execute(params![rowid])?;
                if !body.is_empty() || !sender_name.is_empty() || !chat_title.is_empty() {
                    ins_stmt.execute(params![
                        rowid,
                        chat_key.provider.as_str(),
                        chat_key.account,
                        chat_key.id,
                        msg_id,
                        chat_title,
                        sender_name,
                        body,
                    ])?;
                }
            }
            Ok(())
        })();

        match res {
            Ok(()) => {
                self.conn.execute_batch("RELEASE SAVEPOINT sync_chat_fts")?;
                Ok(())
            }
            Err(err) => {
                let _ = self.conn.execute_batch(
                    "ROLLBACK TO SAVEPOINT sync_chat_fts; RELEASE SAVEPOINT sync_chat_fts",
                );
                Err(err)
            }
        }
    }

    pub fn get_chat(&self, chat_key: &ChatKey) -> Result<Option<Chat>, StoreError> {
        self.conn
            .query_row(
                r#"
                SELECT provider, account_id, chat_id, title, is_group,
                       archived_local, archived_remote, pinned, unread_count,
                       last_msg_ts, last_msg_preview, avatar_hash
                FROM chats
                WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3
                "#,
                params![chat_key.provider.as_str(), chat_key.account, chat_key.id],
                map_chat_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn list_inbox_chats(
        &self,
        include_archived: bool,
        limit: usize,
    ) -> Result<Vec<Chat>, StoreError> {
        let sql = if include_archived {
            r#"
            SELECT provider, account_id, chat_id, title, is_group,
                   archived_local, archived_remote, pinned, unread_count,
                   last_msg_ts, last_msg_preview, avatar_hash
            FROM chats
            ORDER BY pinned DESC, last_msg_ts DESC
            LIMIT ?1
            "#
        } else {
            r#"
            SELECT provider, account_id, chat_id, title, is_group,
                   archived_local, archived_remote, pinned, unread_count,
                   last_msg_ts, last_msg_preview, avatar_hash
            FROM chats
            WHERE archived_local = 0 AND archived_remote = 0
            ORDER BY pinned DESC, last_msg_ts DESC
            LIMIT ?1
            "#
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![limit as i64], map_chat_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Lists chats honoring exact Postbox view/archive/sort semantics:
    /// - Combined Inbox sorts strictly by recency (`last_msg_ts DESC`), ignoring pins.
    /// - Account views sort pinned-first (`pinned DESC, last_msg_ts DESC`).
    /// - Combined Inbox with `include_archived && !archived_only` (`Alt+A`) filters to unread chats only.
    pub fn list_chats_filtered(
        &self,
        view: &ViewTab,
        include_archived: bool,
        archived_only: bool,
        unread_only: bool,
        limit: usize,
    ) -> Result<Vec<Chat>, StoreError> {
        let is_inbox = matches!(view, ViewTab::CombinedInbox);
        let needs_unread = unread_only || (is_inbox && include_archived && !archived_only);

        let mut clauses: Vec<&'static str> = Vec::new();
        if archived_only {
            clauses.push("(archived_local != 0 OR archived_remote != 0)");
        } else if !include_archived {
            clauses.push("(archived_local = 0 AND archived_remote = 0)");
        }
        if needs_unread {
            clauses.push("unread_count > 0");
        }

        match view {
            ViewTab::CombinedInbox => {
                let where_sql = if clauses.is_empty() {
                    String::new()
                } else {
                    format!("WHERE {}", clauses.join(" AND "))
                };
                let sql = format!(
                    r#"
                    SELECT provider, account_id, chat_id, title, is_group,
                           archived_local, archived_remote, pinned, unread_count,
                           last_msg_ts, last_msg_preview, avatar_hash
                    FROM chats
                    {where_sql}
                    ORDER BY REPLACE(RTRIM(last_msg_ts, 'Z'), ' ', 'T') DESC, title ASC, chat_id ASC
                    LIMIT ?1
                    "#
                );
                let mut stmt = self.conn.prepare(&sql)?;
                let rows = stmt.query_map(params![limit as i64], map_chat_row)?;
                let mut chats = rows
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(StoreError::from)?;
                chats.sort_by(compare_combined_inbox);
                Ok(chats)
            }
            ViewTab::Account {
                provider, account, ..
            } => {
                clauses.push("provider = ?1 AND account_id = ?2");
                let where_sql = format!("WHERE {}", clauses.join(" AND "));
                let sql = format!(
                    r#"
                    SELECT provider, account_id, chat_id, title, is_group,
                           archived_local, archived_remote, pinned, unread_count,
                           last_msg_ts, last_msg_preview, avatar_hash
                    FROM chats
                    {where_sql}
                    ORDER BY pinned DESC, REPLACE(RTRIM(last_msg_ts, 'Z'), ' ', 'T') DESC, title ASC, chat_id ASC
                    LIMIT ?3
                    "#
                );
                let mut stmt = self.conn.prepare(&sql)?;
                let rows = stmt.query_map(
                    params![provider.as_str(), account, limit as i64],
                    map_chat_row,
                )?;
                let mut chats = rows
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(StoreError::from)?;
                chats.sort_by(compare_account_view);
                Ok(chats)
            }
        }
    }

    pub fn upsert_message(&mut self, msg: &Message) -> Result<(), StoreError> {
        self.upsert_messages_batch(std::slice::from_ref(msg))
            .map(|_| ())
    }

    /// Single-transaction batch upsert keeping `messages_fts` keyed by `messages.rowid`.
    /// Designed to ingest 6,704 messages in milliseconds with one WAL fsync.
    pub fn upsert_messages_batch(&mut self, messages: &[Message]) -> Result<usize, StoreError> {
        if messages.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.transaction()?;
        let mut title_cache: HashMap<&ChatKey, String> = HashMap::new();
        {
            let mut upsert_stmt = tx.prepare(
                r#"
                INSERT INTO messages (
                    provider, account_id, chat_id, msg_id, sender_id, sender_name,
                    from_me, ts, ts_key, body, reply_to_id, reply_broadcast,
                    media_type, mime_type, filename, file_size, reactions_json, read_status, deleted
                ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6,
                    ?7, ?8, ?9, ?10, ?11, ?12,
                    ?13, ?14, ?15, ?16, ?17, ?18, ?19
                )
                ON CONFLICT(provider, account_id, chat_id, msg_id) DO UPDATE SET
                    sender_id = excluded.sender_id,
                    sender_name = excluded.sender_name,
                    from_me = excluded.from_me,
                    ts = excluded.ts,
                    ts_key = excluded.ts_key,
                    body = excluded.body,
                    reply_to_id = excluded.reply_to_id,
                    reply_broadcast = excluded.reply_broadcast,
                    media_type = excluded.media_type,
                    mime_type = excluded.mime_type,
                    filename = excluded.filename,
                    file_size = COALESCE(excluded.file_size, messages.file_size),
                    reactions_json = excluded.reactions_json,
                    read_status = CASE
                        WHEN excluded.deleted != 0 THEN excluded.read_status
                        WHEN messages.read_status = 'read'
                             AND excluded.read_status IN ('none', 'sent', 'delivered') THEN 'read'
                        WHEN messages.read_status = 'delivered'
                             AND excluded.read_status IN ('none', 'sent') THEN 'delivered'
                        WHEN messages.read_status = 'sent'
                             AND excluded.read_status = 'none' THEN 'sent'
                        ELSE excluded.read_status
                    END,
                    deleted = excluded.deleted
                RETURNING rowid
                "#,
            )?;
            let mut title_stmt = tx.prepare(
                "SELECT title FROM chats WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3",
            )?;
            let mut fts_del_stmt = tx.prepare("DELETE FROM messages_fts WHERE rowid = ?1")?;
            let mut fts_ins_stmt = tx.prepare(
                r#"
                INSERT INTO messages_fts (
                    rowid, provider, account_id, chat_id, msg_id, chat_title, sender_name, body
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                "#,
            )?;

            for msg in messages {
                let reactions_json = serde_json::to_string(&msg.reactions)?;
                let media_type = msg.media_kind.map(MediaKind::as_str);
                let read_status = msg.read_status.as_str();
                let ts_key = canonical_ts_sort_key(&msg.ts);
                let file_size = msg.file_size.map(|sz| sz.min(i64::MAX as u64) as i64);

                let msg_rowid: i64 = upsert_stmt.query_row(
                    params![
                        msg.chat.provider.as_str(),
                        msg.chat.account,
                        msg.chat.id,
                        msg.msg_id,
                        msg.sender_id,
                        msg.sender_name,
                        i64::from(msg.from_me),
                        msg.ts,
                        ts_key,
                        msg.body,
                        msg.reply_to_id,
                        i64::from(msg.reply_broadcast),
                        media_type,
                        msg.mime_type,
                        msg.filename,
                        file_size,
                        reactions_json,
                        read_status,
                        i64::from(msg.deleted),
                    ],
                    |row| row.get(0),
                )?;

                let chat_title = if let Some(cached) = title_cache.get(&msg.chat) {
                    cached.as_str()
                } else {
                    let fetched: String = title_stmt
                        .query_row(
                            params![msg.chat.provider.as_str(), msg.chat.account, msg.chat.id],
                            |row| row.get(0),
                        )
                        .optional()?
                        .unwrap_or_default();
                    title_cache.insert(&msg.chat, fetched);
                    title_cache.get(&msg.chat).map(String::as_str).unwrap_or("")
                };

                fts_del_stmt.execute(params![msg_rowid])?;

                if !msg.deleted
                    && (!msg.body.is_empty()
                        || !msg.sender_name.is_empty()
                        || !chat_title.is_empty())
                {
                    fts_ins_stmt.execute(params![
                        msg_rowid,
                        msg.chat.provider.as_str(),
                        msg.chat.account,
                        msg.chat.id,
                        msg.msg_id,
                        chat_title,
                        msg.sender_name,
                        msg.body,
                    ])?;
                }
            }
        }
        tx.commit()?;
        Ok(messages.len())
    }

    /// Replaces a local optimistic message (`msg_id == client_id`) with the confirmed Hub message,
    /// cleanly removing the old FTS5 entry.
    pub fn replace_optimistic_message(
        &mut self,
        chat: &ChatKey,
        old_msg_id: &str,
        confirmed: &Message,
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        if old_msg_id != confirmed.msg_id
            && let Some(old_rowid) = tx
                .query_row(
                    "SELECT rowid FROM messages WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3 AND msg_id = ?4",
                    params![chat.provider.as_str(), chat.account, chat.id, old_msg_id],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
        {
            tx.execute(
                "DELETE FROM messages_fts WHERE rowid = ?1",
                params![old_rowid],
            )?;
            tx.execute("DELETE FROM messages WHERE rowid = ?1", params![old_rowid])?;
        }
        tx.commit()?;
        self.upsert_message(confirmed)
    }

    /// Returns up to `limit` messages for `chat_key` (optionally strictly before composite
    /// `(ts, msg_id)` cursor) in chronological order (oldest -> newest) for `MessageScroller`.
    pub fn list_messages(
        &self,
        chat_key: &ChatKey,
        limit: usize,
        before: Option<&MessageCursor>,
    ) -> Result<Vec<Message>, StoreError> {
        let mut messages = if let Some(cursor) = before {
            let cursor_key = canonical_ts_sort_key(&cursor.ts);
            let mut stmt = self.conn.prepare(
                r#"
                SELECT msg_id, sender_id, sender_name, from_me, ts, body,
                       reply_to_id, reply_broadcast, media_type, mime_type,
                       filename, file_size, reactions_json, read_status, deleted
                FROM messages
                WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3
                  AND (ts_key, msg_id) < (?4, ?5)
                ORDER BY ts_key DESC, msg_id DESC
                LIMIT ?6
                "#,
            )?;
            let rows = stmt.query_map(
                params![
                    chat_key.provider.as_str(),
                    chat_key.account,
                    chat_key.id,
                    cursor_key,
                    cursor.msg_id,
                    limit as i64,
                ],
                |row| map_message_row(chat_key, row),
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        } else {
            let mut stmt = self.conn.prepare(
                r#"
                SELECT msg_id, sender_id, sender_name, from_me, ts, body,
                       reply_to_id, reply_broadcast, media_type, mime_type,
                       filename, file_size, reactions_json, read_status, deleted
                FROM messages
                WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3
                ORDER BY ts_key DESC, msg_id DESC
                LIMIT ?4
                "#,
            )?;
            let rows = stmt.query_map(
                params![
                    chat_key.provider.as_str(),
                    chat_key.account,
                    chat_key.id,
                    limit as i64,
                ],
                |row| map_message_row(chat_key, row),
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        messages.reverse();
        Ok(messages)
    }

    pub fn search_messages_fts(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>, StoreError> {
        let Some(fts_expr) = sanitize_fts_query(query) else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(
            r#"
            SELECT provider, account_id, chat_id, msg_id, sender_name,
                   snippet(messages_fts, -1, '[', ']', '...', 12)
            FROM messages_fts
            WHERE messages_fts MATCH ?1
            ORDER BY rank
            LIMIT ?2
            "#,
        )?;
        let rows = stmt.query_map(params![fts_expr, limit as i64], |row| {
            let provider = Provider::parse(&row.get::<_, String>(0)?).unwrap_or(Provider::Whatsapp);
            Ok(SearchHit {
                chat: ChatKey::new(provider, row.get::<_, String>(1)?, row.get::<_, String>(2)?),
                msg_id: row.get(3)?,
                sender_name: row.get(4)?,
                snippet: row.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn load_ui_settings(&self) -> Result<UiSettings, StoreError> {
        let mut settings = UiSettings::default();
        let mut stmt = self
            .conn
            .prepare("SELECT key, value FROM settings WHERE key LIKE 'ui.%'")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for pair in rows {
            let (key, value) = pair?;
            match key.as_str() {
                SETTING_THEME => {
                    if let Some(theme) = ThemeChoice::parse(&value) {
                        settings.theme = theme;
                    }
                }
                SETTING_FONT_SCALE => {
                    if let Ok(raw) = value.trim().parse::<u16>() {
                        settings.font_scale = FontScale::from_persisted(raw);
                    }
                }
                SETTING_VIEW_ORDER => {
                    if let Ok(order) = serde_json::from_str::<Vec<String>>(&value) {
                        settings.view_order = order;
                    }
                }
                SETTING_SHOW_ARCHIVED => {
                    settings.show_archived = matches!(value.as_str(), "1" | "true");
                }
                _ => {}
            }
        }
        let table_order = self.load_view_order()?;
        if !table_order.is_empty() {
            settings.view_order = table_order;
        }
        Ok(settings)
    }

    pub fn save_ui_settings(&self, settings: &UiSettings) -> Result<(), StoreError> {
        let view_order_json = serde_json::to_string(&settings.view_order)?;
        let font_scale_str = settings.font_scale.percent().to_string();
        let show_archived_str = if settings.show_archived { "1" } else { "0" };

        self.conn.execute_batch("SAVEPOINT save_ui_settings")?;
        let res = (|| -> Result<(), StoreError> {
            let mut stmt = self.conn.prepare(
                "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )?;
            stmt.execute(params![SETTING_THEME, settings.theme.as_str()])?;
            stmt.execute(params![SETTING_FONT_SCALE, font_scale_str])?;
            stmt.execute(params![SETTING_VIEW_ORDER, view_order_json])?;
            stmt.execute(params![SETTING_SHOW_ARCHIVED, show_archived_str])?;
            self.conn.execute("DELETE FROM view_order", [])?;
            let mut vo = self.conn.prepare(
                "INSERT OR REPLACE INTO view_order (position, view_key) VALUES (?1, ?2)",
            )?;
            for (pos, key) in settings.view_order.iter().enumerate() {
                vo.execute(params![pos as i64, key])?;
            }
            Ok(())
        })();

        match res {
            Ok(()) => {
                self.conn
                    .execute_batch("RELEASE SAVEPOINT save_ui_settings")?;
                Ok(())
            }
            Err(err) => {
                let _ = self.conn.execute_batch(
                    "ROLLBACK TO SAVEPOINT save_ui_settings; RELEASE SAVEPOINT save_ui_settings",
                );
                Err(err)
            }
        }
    }

    pub fn save_draft(&self, draft: &Draft) -> Result<(), StoreError> {
        validate_draft_key(&draft.chat).map_err(|_| StoreError::InvalidDraft)?;
        validate_draft_text(&draft.text_body).map_err(|_| StoreError::InvalidDraft)?;
        let exists: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM drafts WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3",
            params![
                draft.chat.provider.as_str(),
                draft.chat.account,
                draft.chat.id
            ],
            |r| r.get(0),
        )?;
        if exists == 0 {
            let total: i64 = self
                .conn
                .query_row("SELECT COUNT(*) FROM drafts", [], |r| r.get(0))?;
            if total >= MAX_DRAFTS as i64 {
                return Err(StoreError::DraftLimitReached);
            }
        }

        self.conn.execute(
            r#"
            INSERT INTO drafts (
                provider, account_id, chat_id, text_body, reply_to_id, staged_image_path, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(provider, account_id, chat_id) DO UPDATE SET
                text_body = excluded.text_body,
                reply_to_id = excluded.reply_to_id,
                staged_image_path = excluded.staged_image_path,
                updated_at = excluded.updated_at
            "#,
            params![
                draft.chat.provider.as_str(),
                draft.chat.account,
                draft.chat.id,
                draft.text_body,
                draft.reply_to_id,
                draft.staged_image_path,
                draft.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn load_draft(&self, chat: &ChatKey) -> Result<Option<Draft>, StoreError> {
        self.conn
            .query_row(
                r#"
                SELECT text_body, reply_to_id, staged_image_path, updated_at
                FROM drafts
                WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3
                "#,
                params![chat.provider.as_str(), chat.account, chat.id],
                |row| {
                    Ok(Draft {
                        chat: chat.clone(),
                        text_body: row.get(0)?,
                        reply_to_id: row.get(1)?,
                        staged_image_path: row.get(2)?,
                        updated_at: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn delete_draft(&self, chat: &ChatKey) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM drafts WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3",
            params![chat.provider.as_str(), chat.account, chat.id],
        )?;
        Ok(())
    }

    pub fn list_drafts(&self) -> Result<Vec<Draft>, StoreError> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT provider, account_id, chat_id, text_body, reply_to_id, staged_image_path, updated_at
            FROM drafts
            ORDER BY updated_at DESC
            "#,
        )?;
        let rows = stmt.query_map([], |row| {
            let provider = Provider::parse(&row.get::<_, String>(0)?).unwrap_or(Provider::Whatsapp);
            Ok(Draft {
                chat: ChatKey::new(provider, row.get::<_, String>(1)?, row.get::<_, String>(2)?),
                text_body: row.get(3)?,
                reply_to_id: row.get(4)?,
                staged_image_path: row.get(5)?,
                updated_at: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn cache_transcript(
        &self,
        media_sha256: &str,
        language: &str,
        text: &str,
        created_at: i64,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            r#"
            INSERT INTO transcripts (media_sha256, language, text, created_at)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(media_sha256) DO NOTHING
            "#,
            params![media_sha256, language, text, created_at],
        )?;
        Ok(())
    }

    pub fn get_transcript(&self, media_sha256: &str) -> Result<Option<String>, StoreError> {
        self.conn
            .query_row(
                "SELECT text FROM transcripts WHERE media_sha256 = ?1",
                params![media_sha256],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn evict_expired_media(&self, now_unix: i64) -> Result<Vec<String>, StoreError> {
        self.evict_expired_media_older_than(now_unix - MEDIA_TTL_SECS)
    }
}

fn map_message_row(chat_key: &ChatKey, row: &Row<'_>) -> rusqlite::Result<Message> {
    let media_type: Option<String> = row.get(8)?;
    let file_size: Option<i64> = row.get(11)?;
    let reactions_json: String = row.get(12)?;
    let read_status_str: String = row.get(13)?;
    let reactions: Vec<Reaction> = serde_json::from_str(&reactions_json).unwrap_or_default();

    Ok(Message {
        chat: chat_key.clone(),
        msg_id: row.get(0)?,
        sender_id: row.get(1)?,
        sender_name: row.get(2)?,
        from_me: row.get::<_, i64>(3)? != 0,
        ts: row.get(4)?,
        body: row.get(5)?,
        reply_to_id: row.get(6)?,
        reply_broadcast: row.get::<_, i64>(7)? != 0,
        media_kind: media_type.as_deref().and_then(MediaKind::parse),
        mime_type: row.get(9)?,
        filename: row.get(10)?,
        file_size: file_size.and_then(|v| u64::try_from(v).ok()),
        reactions,
        read_status: ReadStatus::parse(&read_status_str),
        deleted: row.get::<_, i64>(14)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::archive::{ArchiveEntry, ArchiveTable, PendingArchiveIntent};
    use postbox_core::outbox::format_client_id;
    use postbox_core::read::{PendingReadQueue, ReadWatermark};

    fn sample_chat(provider: Provider, account: &str, id: &str, title: &str, ts: &str) -> Chat {
        Chat {
            key: ChatKey::new(provider, account, id),
            title: title.into(),
            is_group: false,
            archived_local: false,
            archived_remote: false,
            pinned: false,
            unread_count: 0,
            last_msg_ts: ts.into(),
            last_msg_preview: "preview".into(),
            avatar_hash: None,
        }
    }

    #[test]
    fn cache_size_and_busy_timeout_pragmas_are_set() {
        let store = Store::open_in_memory().unwrap();
        let cache_size: i64 = store
            .conn
            .pragma_query_value(None, "cache_size", |row| row.get(0))
            .unwrap();
        assert_eq!(cache_size, -16_000);
        let busy_timeout: i64 = store
            .conn
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .unwrap();
        assert_eq!(busy_timeout, BUSY_TIMEOUT_MS as i64);
    }

    #[test]
    fn forward_migration_upgrades_legacy_m0_outbox_table_without_data_loss() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE outbox (
                client_id TEXT PRIMARY KEY,
                provider TEXT NOT NULL,
                account_id TEXT NOT NULL,
                chat_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                phase TEXT NOT NULL,
                attempt_serial INTEGER NOT NULL DEFAULT 0,
                text_body TEXT NOT NULL DEFAULT '',
                reply_to_id TEXT,
                reply_broadcast INTEGER NOT NULL DEFAULT 0,
                failure_reason TEXT,
                not_before_ms INTEGER NOT NULL DEFAULT 0,
                created_at_ms INTEGER NOT NULL
            );
            INSERT INTO outbox VALUES (
                '0123456789abcdef0123456789abcdef', 'whatsapp', 'personal', '1@s.whatsapp.net',
                'text', 'ready', 0, 'legacy queued msg', NULL, 0, NULL, 100, 100
            );
            CREATE TABLE messages (
                provider TEXT NOT NULL,
                account_id TEXT NOT NULL,
                chat_id TEXT NOT NULL,
                msg_id TEXT NOT NULL,
                sender_id TEXT NOT NULL,
                sender_name TEXT NOT NULL,
                from_me INTEGER NOT NULL DEFAULT 0,
                ts TEXT NOT NULL,
                body TEXT NOT NULL DEFAULT '',
                reply_to_id TEXT,
                reply_broadcast INTEGER NOT NULL DEFAULT 0,
                media_type TEXT,
                mime_type TEXT,
                filename TEXT,
                reactions_json TEXT NOT NULL DEFAULT '[]',
                read_status TEXT NOT NULL DEFAULT 'none',
                deleted INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (provider, account_id, chat_id, msg_id)
            );
            INSERT INTO messages VALUES (
                'whatsapp', 'personal', '1@s.whatsapp.net', 'm_half', 'u1', 'Alice', 0,
                '2026-11-26T10:00:00.5Z', 'half', NULL, 0, NULL, NULL, NULL, '[]', 'none', 0
            );
            INSERT INTO messages VALUES (
                'whatsapp', 'personal', '1@s.whatsapp.net', 'm_zero', 'u1', 'Alice', 0,
                '2026-11-26T10:00:00Z', 'zero', NULL, 0, NULL, NULL, NULL, '[]', 'none', 0
            );
            "#,
        )
        .unwrap();

        let store = Store::init(conn).unwrap();
        let rows = store.list_outbox().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text_body, "legacy queued msg");
        assert_eq!(rows[0].attempts, 0);
        assert!(!rows[0].ambiguous);

        let key = ChatKey::new(Provider::Whatsapp, "personal", "1@s.whatsapp.net");
        let msgs = store.list_messages(&key, 10, None).unwrap();
        let ids: Vec<&str> = msgs.iter().map(|m| m.msg_id.as_str()).collect();
        assert_eq!(ids, vec!["m_zero", "m_half"]);
    }

    #[test]
    fn composite_cursor_handles_colons_and_same_timestamp_tiebreaking() {
        let parsed =
            MessageCursor::parse_hub_before_cursor("personal:ABC:2026-10-01T11:58:41Z").unwrap();
        assert_eq!(parsed.msg_id, "personal:ABC");
        assert_eq!(parsed.ts, "2026-10-01T11:58:41Z");
        assert_eq!(
            parsed.to_hub_before_cursor(),
            "personal:ABC:2026-10-01T11:58:41Z"
        );

        let mut store = Store::open_in_memory().unwrap();
        let chat = ChatKey::new(Provider::Whatsapp, "main", "tie@s.whatsapp.net");
        let msgs: Vec<Message> = ["m1", "m2", "m3", "m4"]
            .into_iter()
            .map(|id| Message {
                chat: chat.clone(),
                msg_id: id.into(),
                sender_id: "u1".into(),
                sender_name: "Alice".into(),
                from_me: false,
                ts: "2026-11-26T10:00:00Z".into(),
                body: format!("Body {id}"),
                reply_to_id: None,
                reply_broadcast: false,
                media_kind: None,
                mime_type: None,
                filename: None,
                file_size: None,
                reactions: vec![],
                read_status: ReadStatus::None,
                deleted: false,
            })
            .collect();
        store.upsert_messages_batch(&msgs).unwrap();

        let page1 = store.list_messages(&chat, 2, None).unwrap();
        assert_eq!(
            page1.iter().map(|m| m.msg_id.as_str()).collect::<Vec<_>>(),
            vec!["m3", "m4"]
        );
        let cursor = MessageCursor::from_message(&page1[0]);
        let page2 = store.list_messages(&chat, 2, Some(&cursor)).unwrap();
        assert_eq!(
            page2.iter().map(|m| m.msg_id.as_str()).collect::<Vec<_>>(),
            vec!["m1", "m2"]
        );
        let cursor2 = MessageCursor::from_message(&page2[0]);
        assert!(
            store
                .list_messages(&chat, 2, Some(&cursor2))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn acid_outbox_enforces_all_six_send_queue_tla_invariants() {
        let mut store = Store::open_in_memory().unwrap();
        let chat_a = ChatKey::new(Provider::Whatsapp, "main", "a@s.whatsapp.net");
        let chat_b = ChatKey::new(Provider::Whatsapp, "main", "b@s.whatsapp.net");
        let cid_a = format_client_id([0x01; 16]);
        let cid_b = format_client_id([0x02; 16]);

        store
            .enqueue_outbox(&OutboxRecord::new_text(
                cid_a.clone(),
                chat_a.clone(),
                "hello A",
                None,
                false,
                100,
            ))
            .unwrap();
        store
            .enqueue_outbox(&OutboxRecord::new_text(
                cid_b.clone(),
                chat_b.clone(),
                "hello B",
                None,
                false,
                200,
            ))
            .unwrap();

        // SingleInflight + InflightIsUncertain: claim A -> Uncertain, serial=1.
        let claimed_a = store.claim_next_ready_outbox(500).unwrap().unwrap();
        assert_eq!(claimed_a.client_id, cid_a);
        assert_eq!(claimed_a.phase, OutboxPhase::Uncertain);
        assert_eq!(claimed_a.attempt_serial, 1);

        // Cannot claim B while A is Uncertain (SingleInflight).
        assert!(store.claim_next_ready_outbox(500).unwrap().is_none());

        // Simulate crash recovery: A becomes Held + ambiguous (NEVER auto-resent -> NoDouble).
        let recovered = store.recover_uncertain_on_startup().unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].phase, OutboxPhase::Held);
        assert!(recovered[0].ambiguous);

        // ReadyResolves: held chat A never blocks chat B!
        let claimed_b = store.claim_next_ready_outbox(500).unwrap().unwrap();
        assert_eq!(claimed_b.client_id, cid_b);
        assert!(
            store
                .acknowledge_outbox(&cid_b, claimed_b.attempt_serial)
                .unwrap()
        );

        // Explicit UserRetry on A mints a fresh 32-hex client_id.
        let cid_a2 = format_client_id([0x03; 16]);
        let retried = store
            .user_retry_outbox_with_new_client_id(&cid_a, &cid_a2, 1000)
            .unwrap();
        assert_eq!(retried.client_id, cid_a2);
        assert_eq!(retried.phase, OutboxPhase::Ready);
        assert_eq!(retried.attempts, 0);
        assert!(!retried.ambiguous);

        // AutoSendsBounded: 3 retries (3s, 10s, 30s) reuse cid_a2; 4th transitions to Held (NoLoss).
        let mut t = 1_000i64;
        for expected_delay in [3_000i64, 10_000, 30_000] {
            let c = store.claim_next_ready_outbox(t).unwrap().unwrap();
            assert_eq!(c.client_id, cid_a2);
            let next = store
                .mark_outbox_retryable(&c.client_id, c.attempt_serial, true, "500", t)
                .unwrap()
                .unwrap();
            assert_eq!(next.phase, OutboxPhase::Ready);
            assert_eq!(next.not_before_ms, t + expected_delay);
            t += expected_delay;
        }
        let c4 = store.claim_next_ready_outbox(t).unwrap().unwrap();
        let held = store
            .mark_outbox_retryable(&c4.client_id, c4.attempt_serial, true, "500", t)
            .unwrap()
            .unwrap();
        assert_eq!(held.phase, OutboxPhase::Held);
        assert_eq!(held.attempts, 4);
    }

    #[test]
    fn pins_cap_16_view_order_read_watermarks_archive_rules_and_prefs_persist() {
        let mut store = Store::open_in_memory().unwrap();
        for i in 0..MAX_PINS {
            let k = ChatKey::new(Provider::Whatsapp, "p", format!("c{i}"));
            assert!(store.pin_chat(&k, i as i64).unwrap());
        }
        let overflow = ChatKey::new(Provider::Whatsapp, "p", "c16");
        assert!(matches!(
            store.pin_chat(&overflow, 99),
            Err(StoreError::PinLimitReached)
        ));
        assert_eq!(store.count_pinned_chats().unwrap(), MAX_PINS);
        let first = ChatKey::new(Provider::Whatsapp, "p", "c0");
        assert!(store.unpin_chat(&first).unwrap());
        assert!(store.pin_chat(&overflow, 100).unwrap());

        // View order + prefs.
        let order = vec!["slack:T1".to_owned(), "whatsapp:p".to_owned()];
        store.save_view_order(&order).unwrap();
        assert_eq!(store.load_view_order().unwrap(), order);
        store.set_view_show_archived("slack:T1", true).unwrap();
        assert!(store.get_view_show_archived("slack:T1").unwrap());
        store
            .set_view_selected_chat("slack:T1", Some(&overflow))
            .unwrap();
        assert_eq!(
            store.get_view_selected_chat("slack:T1").unwrap(),
            Some(overflow.clone())
        );

        // Read watermarks + pending reads.
        store
            .upsert_read_watermark(
                &overflow,
                &ReadWatermark {
                    read_ts: 50,
                    incoming_ts: 60,
                    checked_ts: 60,
                },
            )
            .unwrap();
        assert!(store.load_read_state().unwrap().is_unread(&overflow));
        let mut pr = PendingReadQueue::new();
        pr.enqueue(overflow.clone());
        store.save_pending_reads(&pr).unwrap();
        assert_eq!(store.load_pending_reads().unwrap().len(), 1);

        // Archive rules + pending archives.
        let mut tbl = ArchiveTable::new();
        tbl.archive(&overflow, "2026-11-26T10:00:00Z", true);
        store.save_archive_table(&tbl).unwrap();
        let loaded_tbl = store.load_archive_table().unwrap();
        assert!(loaded_tbl.is_kept(&overflow));

        store
            .enqueue_pending_archive(&overflow, PendingArchiveIntent::ArchivePending, 100)
            .unwrap();
        assert_eq!(store.list_ready_pending_archives(100).unwrap().len(), 1);
        store.delete_pending_archive(&overflow).unwrap();
        assert!(store.list_ready_pending_archives(100).unwrap().is_empty());

        // Chat & account prefs.
        store.set_chat_replies_fold(&overflow, Some(true)).unwrap();
        assert_eq!(store.get_chat_replies_fold(&overflow).unwrap(), Some(true));
        store
            .set_account_slack_threads(Provider::Slack, "T1", false)
            .unwrap();
        assert!(
            !store
                .get_account_slack_threads(Provider::Slack, "T1")
                .unwrap()
        );
        store
            .set_account_workspace_name(Provider::Slack, "T1", "Acme Corp")
            .unwrap();
        assert_eq!(
            store
                .get_account_workspace_name(Provider::Slack, "T1")
                .unwrap()
                .as_deref(),
            Some("Acme Corp")
        );

        let _ = ArchiveEntry {
            key: overflow,
            baseline_ts: String::new(),
            keep: false,
            checked_unix: 0,
            returned: false,
            phone: false,
            held: false,
            held_ts: String::new(),
        };
    }

    #[test]
    fn fts5_search_finds_inserted_messages_in_sub_millisecond_time() {
        let mut store = Store::open_in_memory().unwrap();
        let chat = ChatKey::new(Provider::Slack, "T01", "C01");
        store
            .upsert_message(&Message {
                chat: chat.clone(),
                msg_id: "1700.1".into(),
                sender_id: "U1".into(),
                sender_name: "Ada".into(),
                from_me: false,
                ts: "2026-11-26T12:00:00Z".into(),
                body: "Intel Arc DXVA hardware video decode looks crisp".into(),
                reply_to_id: None,
                reply_broadcast: false,
                media_kind: None,
                mime_type: None,
                filename: None,
                file_size: None,
                reactions: vec![],
                read_status: ReadStatus::Read,
                deleted: false,
            })
            .unwrap();

        let hits = store.search_messages_fts("DXVA*", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chat, chat);
        assert!(hits[0].snippet.contains("[DXVA]"));
    }

    #[test]
    fn upsert_message_round_trips_all_fields_and_paginates_chronologically() {
        let mut store = Store::open_in_memory().unwrap();
        let chat = ChatKey::new(Provider::Whatsapp, "main", "49123@s.whatsapp.net");

        let msg1 = Message {
            chat: chat.clone(),
            msg_id: "m1".into(),
            sender_id: "u1".into(),
            sender_name: "Alice".into(),
            from_me: false,
            ts: "2026-11-26T10:00:00Z".into(),
            body: "First".into(),
            reply_to_id: None,
            reply_broadcast: false,
            media_kind: Some(MediaKind::Voice),
            mime_type: Some("audio/ogg".into()),
            filename: Some("note.ogg".into()),
            file_size: None,
            reactions: vec![Reaction {
                emoji: "👍".into(),
                count: 2,
                from_me: true,
            }],
            read_status: ReadStatus::Delivered,
            deleted: false,
        };
        let msg2 = Message {
            chat: chat.clone(),
            msg_id: "m2".into(),
            sender_id: "me".into(),
            sender_name: "Me".into(),
            from_me: true,
            ts: "2026-11-26T10:05:00Z".into(),
            body: "Second".into(),
            reply_to_id: Some("m1".into()),
            reply_broadcast: true,
            media_kind: Some(MediaKind::Image),
            mime_type: Some("image/png".into()),
            filename: Some("pic.png".into()),
            file_size: None,
            reactions: vec![],
            read_status: ReadStatus::Read,
            deleted: false,
        };
        let msg3 = Message {
            chat: chat.clone(),
            msg_id: "m3".into(),
            sender_id: "u1".into(),
            sender_name: "Alice".into(),
            from_me: false,
            ts: "2026-11-26T10:10:00Z".into(),
            body: "Third".into(),
            reply_to_id: None,
            reply_broadcast: false,
            media_kind: None,
            mime_type: None,
            filename: None,
            file_size: None,
            reactions: vec![],
            read_status: ReadStatus::Sent,
            deleted: false,
        };

        store.upsert_message(&msg1).unwrap();
        store.upsert_message(&msg2).unwrap();
        store.upsert_message(&msg3).unwrap();

        let latest_two = store.list_messages(&chat, 2, None).unwrap();
        assert_eq!(latest_two.len(), 2);
        assert_eq!(latest_two[0], msg2);
        assert_eq!(latest_two[1], msg3);

        let older = store
            .list_messages(&chat, 10, Some(&MessageCursor::from_message(&msg2)))
            .unwrap();
        assert_eq!(older, vec![msg1]);
    }

    #[test]
    fn fts5_upsert_is_idempotent_indexes_chat_title_and_syncs_on_rename() {
        let mut store = Store::open_in_memory().unwrap();
        let chat = sample_chat(
            Provider::Slack,
            "T1",
            "C1",
            "Phoenix Launch",
            "2026-11-26T12:00:00Z",
        );
        store.upsert_chat(&chat, 100).unwrap();

        let mut msg = Message {
            chat: chat.key.clone(),
            msg_id: "m1".into(),
            sender_id: "U1".into(),
            sender_name: "Clara".into(),
            from_me: false,
            ts: "2026-11-26T12:00:00Z".into(),
            body: "Alpha milestone ready".into(),
            reply_to_id: None,
            reply_broadcast: false,
            media_kind: None,
            mime_type: None,
            filename: None,
            file_size: None,
            reactions: vec![],
            read_status: ReadStatus::Read,
            deleted: false,
        };
        store.upsert_message(&msg).unwrap();
        msg.body = "Beta milestone ready".into();
        store.upsert_message(&msg).unwrap();
        store.upsert_message(&msg).unwrap();

        assert_eq!(store.search_messages_fts("Alpha", 10).unwrap().len(), 0);
        assert_eq!(store.search_messages_fts("Beta", 10).unwrap().len(), 1);
        assert_eq!(store.search_messages_fts("Clara", 10).unwrap().len(), 1);
        assert_eq!(store.search_messages_fts("Phoenix", 10).unwrap().len(), 1);

        let mut renamed = chat.clone();
        renamed.title = "Orion Release".into();
        store.upsert_chat(&renamed, 200).unwrap();

        assert_eq!(store.search_messages_fts("Phoenix", 10).unwrap().len(), 0);
        assert_eq!(store.search_messages_fts("Orion", 10).unwrap().len(), 1);

        msg.deleted = true;
        store.upsert_message(&msg).unwrap();
        assert_eq!(store.search_messages_fts("Beta", 10).unwrap().len(), 0);
        assert_eq!(store.search_messages_fts("Orion", 10).unwrap().len(), 0);
    }

    #[test]
    fn sanitize_fts_query_handles_hostile_inputs_without_sqlite_errors() {
        let mut store = Store::open_in_memory().unwrap();
        let chat = ChatKey::new(Provider::Telegram, "main", "tg1");
        store
            .upsert_message(&Message {
                chat,
                msg_id: "1".into(),
                sender_id: "u1".into(),
                sender_name: "Müller".into(),
                from_me: false,
                ts: "2026-11-26T12:00:00Z".into(),
                body: "Checking AND OR NOT NEAR operators with café".into(),
                reply_to_id: None,
                reply_broadcast: false,
                media_kind: None,
                mime_type: None,
                filename: None,
                file_size: None,
                reactions: vec![],
                read_status: ReadStatus::None,
                deleted: false,
            })
            .unwrap();

        for hostile in [
            "",
            "   ",
            "\"unterminated",
            "foo:bar",
            "a-b+c*()",
            "AND OR NOT NEAR",
            "***---^^^",
            "Müller*",
            "cafe",
        ] {
            let res = store.search_messages_fts(hostile, 10);
            assert!(res.is_ok(), "query {hostile:?} failed: {:?}", res.err());
        }
        assert_eq!(store.search_messages_fts("Muller*", 10).unwrap().len(), 1);
        assert_eq!(store.search_messages_fts("cafe", 10).unwrap().len(), 1);
        assert_eq!(sanitize_fts_query("   !!!  "), None);
    }

    #[test]
    fn accounts_and_filtered_chats_honor_inbox_vs_account_sorting_and_archive_rules() {
        let store = Store::open_in_memory().unwrap();
        let wa = Account {
            provider: Provider::Whatsapp,
            account_id: "main".into(),
            label: "WhatsApp".into(),
            state: "connected".into(),
            sort_order: 1,
            last_event_at: None,
        };
        let sl = Account {
            provider: Provider::Slack,
            account_id: "T1".into(),
            label: "Acme Slack".into(),
            state: "connected".into(),
            sort_order: 2,
            last_event_at: None,
        };
        store.upsert_account(&sl).unwrap();
        store.upsert_account(&wa).unwrap();
        let accounts = store.list_accounts().unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].account_id, "main");
        assert_eq!(accounts[1].account_id, "T1");

        let mut wa_pinned = sample_chat(
            Provider::Whatsapp,
            "main",
            "c1",
            "Pinned Older",
            "2026-11-26T08:00:00Z",
        );
        wa_pinned.pinned = true;

        let mut wa_newer = sample_chat(
            Provider::Whatsapp,
            "main",
            "c2",
            "Newer Unpinned",
            "2026-11-26T12:00:00Z",
        );
        wa_newer.unread_count = 2;

        let mut sl_archived_unread = sample_chat(
            Provider::Slack,
            "T1",
            "c3",
            "Archived Unread",
            "2026-11-26T11:00:00Z",
        );
        sl_archived_unread.archived_local = true;
        sl_archived_unread.unread_count = 1;

        let mut sl_archived_read = sample_chat(
            Provider::Slack,
            "T1",
            "c4",
            "Archived Read",
            "2026-11-26T13:00:00Z",
        );
        sl_archived_read.archived_remote = true;

        for c in [
            &wa_pinned,
            &wa_newer,
            &sl_archived_unread,
            &sl_archived_read,
        ] {
            store.upsert_chat(c, 100).unwrap();
        }
        assert_eq!(
            store.get_chat(&wa_pinned.key).unwrap(),
            Some(wa_pinned.clone())
        );

        let inbox = store
            .list_chats_filtered(&ViewTab::CombinedInbox, false, false, false, 20)
            .unwrap();
        assert_eq!(
            inbox.iter().map(|c| c.key.id.as_str()).collect::<Vec<_>>(),
            vec!["c2", "c1"]
        );

        let inbox_alt_a = store
            .list_chats_filtered(&ViewTab::CombinedInbox, true, false, false, 20)
            .unwrap();
        assert_eq!(
            inbox_alt_a
                .iter()
                .map(|c| c.key.id.as_str())
                .collect::<Vec<_>>(),
            vec!["c2", "c3"]
        );

        let wa_view = wa.to_view_tab();
        let wa_chats = store
            .list_chats_filtered(&wa_view, false, false, false, 20)
            .unwrap();
        assert_eq!(
            wa_chats
                .iter()
                .map(|c| c.key.id.as_str())
                .collect::<Vec<_>>(),
            vec!["c1", "c2"]
        );

        let archived = store
            .list_chats_filtered(&ViewTab::CombinedInbox, true, true, false, 20)
            .unwrap();
        assert_eq!(
            archived
                .iter()
                .map(|c| c.key.id.as_str())
                .collect::<Vec<_>>(),
            vec!["c4", "c3"]
        );

        let unread_wa = store
            .list_chats_filtered(&wa_view, false, false, true, 20)
            .unwrap();
        assert_eq!(unread_wa.len(), 1);
        assert_eq!(unread_wa[0].key.id, "c2");
    }

    #[test]
    fn ui_settings_round_trip_and_migrate_legacy_font_scale_82() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.load_ui_settings().unwrap(), UiSettings::default());

        let saved = UiSettings {
            theme: ThemeChoice::Amoled,
            font_scale: FontScale::preset_large(),
            view_order: vec!["slack:T1".into(), "whatsapp:main".into()],
            show_archived: true,
        };
        store.save_ui_settings(&saved).unwrap();
        assert_eq!(store.load_ui_settings().unwrap(), saved);

        store
            .conn
            .execute(
                "UPDATE settings SET value = '82' WHERE key = ?1",
                params![SETTING_FONT_SCALE],
            )
            .unwrap();
        let migrated = store.load_ui_settings().unwrap();
        assert_eq!(migrated.font_scale, FontScale::default());
    }

    #[test]
    fn drafts_save_load_list_and_delete() {
        let store = Store::open_in_memory().unwrap();
        let key = ChatKey::new(Provider::Signal, "main", "sig1");
        let draft = Draft {
            chat: key.clone(),
            text_body: "Draft text".into(),
            reply_to_id: Some("m9".into()),
            staged_image_path: Some("img.png".into()),
            updated_at: 500,
        };
        store.save_draft(&draft).unwrap();
        assert_eq!(store.load_draft(&key).unwrap(), Some(draft.clone()));
        assert_eq!(store.list_drafts().unwrap(), vec![draft]);
        store.delete_draft(&key).unwrap();
        assert_eq!(store.load_draft(&key).unwrap(), None);
    }

    #[test]
    fn outbox_and_transcript_cache_preserve_invariants() {
        let mut store = Store::open_in_memory().unwrap();
        let chat = ChatKey::new(Provider::Whatsapp, "main", "49123@s.whatsapp.net");
        let cid = format_client_id([0xaa; 16]);
        let mut rec = OutboxRecord::new_text(cid.clone(), chat, "hello", None, false, 900);
        rec.not_before_ms = 1000;
        store.enqueue_outbox(&rec).unwrap();

        assert!(store.next_ready_outbox(999).unwrap().is_none());
        assert_eq!(
            store.next_ready_outbox(1000).unwrap().unwrap().client_id,
            cid
        );

        store
            .cache_transcript("sha-abc", "en", "First transcript", 100)
            .unwrap();
        store
            .cache_transcript("sha-abc", "en", "Should never overwrite", 200)
            .unwrap();
        assert_eq!(
            store.get_transcript("sha-abc").unwrap().as_deref(),
            Some("First transcript")
        );
    }

    #[test]
    fn batch_ingest_320_chats_6704_messages_is_fast_and_searchable() {
        let mut store = Store::open_in_memory().unwrap();
        let mut chats = Vec::with_capacity(320);
        let mut messages = Vec::with_capacity(6_704);
        for c in 0..320 {
            let key = ChatKey::new(
                Provider::Whatsapp,
                "personal",
                format!("chat_{c}@s.whatsapp.net"),
            );
            chats.push(sample_chat(
                Provider::Whatsapp,
                "personal",
                &key.id,
                &format!("Contact {c}"),
                "2026-11-26T12:00:00Z",
            ));
            let count = if c < 304 { 21 } else { 20 }; // 304*21 + 16*20 = 6384 + 320 = 6704
            for m in 0..count {
                messages.push(Message {
                    chat: key.clone(),
                    msg_id: format!("m_{c}_{m}"),
                    sender_id: "u1".into(),
                    sender_name: format!("Sender {c}"),
                    from_me: m % 2 == 0,
                    ts: format!("2026-11-26T10:{:02}:{:02}Z", m / 60, m % 60),
                    body: if c == 42 && m == 7 {
                        "needle_quantum_postbox_payload".into()
                    } else {
                        format!("Regular message {m} in chat {c}")
                    },
                    reply_to_id: None,
                    reply_broadcast: false,
                    media_kind: None,
                    mime_type: None,
                    filename: None,
                    file_size: None,
                    reactions: vec![],
                    read_status: ReadStatus::Read,
                    deleted: false,
                });
            }
        }
        assert_eq!(messages.len(), 6_704);
        store.upsert_chats_batch(&chats, 1_000).unwrap();
        store.upsert_messages_batch(&messages).unwrap();

        let hits = store
            .search_messages_fts("needle_quantum_postbox", 10)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].msg_id, "m_42_7");
    }

    #[test]
    fn sanitize_and_search_fts_handles_nfd_combining_diacritics() {
        assert_eq!(
            sanitize_fts_query("u\u{0308}ber").as_deref(),
            Some("\"u\u{0308}ber\"*")
        );
        assert_eq!(
            sanitize_fts_query("cafe\u{0301} u\u{0308}ber nai\u{0308}ve").as_deref(),
            Some("\"cafe\u{0301}\"* \"u\u{0308}ber\"* \"nai\u{0308}ve\"*")
        );
        assert_eq!(sanitize_fts_query("\u{0301}\u{0308}"), None);
        assert_eq!(
            sanitize_fts_query("\u{0308}ber").as_deref(),
            Some("\"ber\"*")
        );

        let mut store = Store::open_in_memory().unwrap();
        let chat = ChatKey::new(Provider::Whatsapp, "main", "diacritics@s.whatsapp.net");
        store
            .upsert_chat(
                &Chat {
                    key: chat.clone(),
                    title: "Café Über".into(),
                    is_group: false,
                    archived_local: false,
                    archived_remote: false,
                    pinned: false,
                    unread_count: 0,
                    last_msg_ts: "2026-11-26T12:00:00Z".into(),
                    last_msg_preview: "Testing café über naïve".into(),
                    avatar_hash: None,
                },
                100,
            )
            .unwrap();
        store
            .upsert_message(&Message {
                chat: chat.clone(),
                msg_id: "m-nfd".into(),
                sender_id: "s1".into(),
                sender_name: "Renée Müller".into(),
                from_me: false,
                ts: "2026-11-26T12:00:00Z".into(),
                body: "Testing café über naïve".into(),
                reply_to_id: None,
                reply_broadcast: false,
                media_kind: None,
                mime_type: None,
                filename: None,
                file_size: None,
                reactions: vec![],
                read_status: ReadStatus::Read,
                deleted: false,
            })
            .unwrap();

        let single = store.search_messages_fts("u\u{0308}ber", 10).unwrap();
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].msg_id, "m-nfd");

        let multi = store
            .search_messages_fts("cafe\u{0301} u\u{0308}ber nai\u{0308}ve", 10)
            .unwrap();
        assert_eq!(multi.len(), 1);
        assert_eq!(multi[0].msg_id, "m-nfd");

        let sender = store
            .search_messages_fts("Rene\u{0301}e Mu\u{0308}ller", 10)
            .unwrap();
        assert_eq!(sender.len(), 1);
        assert_eq!(sender[0].msg_id, "m-nfd");
    }

    #[test]
    fn list_messages_orders_fractional_rfc3339_timestamps_and_cursor_correctly() {
        let mut store = Store::open_in_memory().unwrap();
        let chat = ChatKey::new(Provider::Whatsapp, "main", "frac@s.whatsapp.net");
        let mk = |id: &str, ts: &str| Message {
            chat: chat.clone(),
            msg_id: id.into(),
            sender_id: "u1".into(),
            sender_name: "Alice".into(),
            from_me: false,
            ts: ts.into(),
            body: id.into(),
            reply_to_id: None,
            reply_broadcast: false,
            media_kind: None,
            mime_type: None,
            filename: None,
            file_size: None,
            reactions: vec![],
            read_status: ReadStatus::None,
            deleted: false,
        };
        store
            .upsert_messages_batch(&[
                mk("m_half", "2026-11-26T10:00:00.5Z"),
                mk("m_zero", "2026-11-26T10:00:00Z"),
                mk("m_zero_frac", "2026-11-26T10:00:00.000Z"),
                mk("m_quarter", "2026-11-26T10:00:00.250Z"),
            ])
            .unwrap();

        let all = store.list_messages(&chat, 10, None).unwrap();
        let ids: Vec<&str> = all.iter().map(|m| m.msg_id.as_str()).collect();
        // Chronological (oldest -> newest): 00Z == 00.000Z (tie-broken by msg_id), then .250Z, then .5Z.
        assert_eq!(ids, vec!["m_zero", "m_zero_frac", "m_quarter", "m_half"]);
        // Raw wire timestamps preserved for Hub before-cursors!
        assert_eq!(all[0].ts, "2026-11-26T10:00:00Z");
        assert_eq!(all[3].ts, "2026-11-26T10:00:00.5Z");

        // Composite cursor pagination strictly before m_half returns m_quarter, m_zero_frac, m_zero.
        let cursor = MessageCursor::from_message(&all[3]);
        let older = store.list_messages(&chat, 10, Some(&cursor)).unwrap();
        let older_ids: Vec<&str> = older.iter().map(|m| m.msg_id.as_str()).collect();
        assert_eq!(older_ids, vec!["m_zero", "m_zero_frac", "m_quarter"]);
    }

    #[test]
    fn upsert_messages_enforces_monotonic_read_status_igsh_1307() {
        let mut store = Store::open_in_memory().unwrap();
        let chat = ChatKey::new(Provider::Whatsapp, "main", "mono@s.whatsapp.net");
        let mut msg = Message {
            chat: chat.clone(),
            msg_id: "m1".into(),
            sender_id: "me".into(),
            sender_name: "You".into(),
            from_me: true,
            ts: "2026-11-26T10:00:00Z".into(),
            body: "Hi".into(),
            reply_to_id: None,
            reply_broadcast: false,
            media_kind: None,
            mime_type: None,
            filename: None,
            file_size: None,
            reactions: vec![],
            read_status: ReadStatus::Read,
            deleted: false,
        };
        store.upsert_message(&msg).unwrap();

        // Stale Hub refresh reporting Sent / Delivered / None MUST NOT regress Read!
        for stale in [ReadStatus::Sent, ReadStatus::Delivered, ReadStatus::None] {
            msg.read_status = stale;
            store.upsert_message(&msg).unwrap();
            let loaded = store.list_messages(&chat, 10, None).unwrap();
            assert_eq!(loaded[0].read_status, ReadStatus::Read);
        }

        // Revocation (deleted = true) resets status cleanly.
        msg.deleted = true;
        msg.read_status = ReadStatus::None;
        store.upsert_message(&msg).unwrap();
        let loaded = store.list_messages(&chat, 10, None).unwrap();
        assert_eq!(loaded[0].read_status, ReadStatus::None);
        assert!(loaded[0].deleted);

        // Sending -> Failed transition for optimistic sends remains allowed.
        let mut opt = msg.clone();
        opt.msg_id = "m_opt".into();
        opt.deleted = false;
        opt.read_status = ReadStatus::Sending;
        store.upsert_message(&opt).unwrap();
        opt.read_status = ReadStatus::Failed;
        store.upsert_message(&opt).unwrap();
        let loaded = store.list_messages(&chat, 10, None).unwrap();
        let opt_row = loaded.iter().find(|m| m.msg_id == "m_opt").unwrap();
        assert_eq!(opt_row.read_status, ReadStatus::Failed);
    }

    #[test]
    fn list_chats_filtered_normalizes_mixed_slack_space_and_iso_timestamps() {
        let mut store = Store::open_in_memory().unwrap();
        let wa_older = Chat {
            key: ChatKey::new(Provider::Whatsapp, "main", "wa@s.whatsapp.net"),
            title: "WA Older".into(),
            is_group: false,
            archived_local: false,
            archived_remote: false,
            pinned: false,
            unread_count: 0,
            last_msg_ts: "2026-11-26T11:00:00Z".into(),
            last_msg_preview: "older".into(),
            avatar_hash: None,
        };
        let slack_newer = Chat {
            key: ChatKey::new(Provider::Slack, "T1", "slack:T1:C1"),
            title: "Slack Newer".into(),
            is_group: true,
            archived_local: false,
            archived_remote: false,
            pinned: false,
            unread_count: 0,
            last_msg_ts: "2026-11-26 12:00:00".into(),
            last_msg_preview: "newer".into(),
            avatar_hash: None,
        };
        store
            .upsert_chats_batch(&[wa_older, slack_newer], 100)
            .unwrap();
        let chats = store
            .list_chats_filtered(&ViewTab::CombinedInbox, false, false, false, 50)
            .unwrap();
        assert_eq!(chats[0].title, "Slack Newer");
        assert_eq!(chats[1].title, "WA Older");
    }

    #[test]
    fn save_draft_validates_chat_key_and_save_read_state_is_monotonic() {
        let mut store = Store::open_in_memory().unwrap();
        let bad_draft = Draft {
            chat: ChatKey::new(Provider::Slack, "", "slack:T1:C1"),
            text_body: "should fail".into(),
            reply_to_id: None,
            staged_image_path: None,
            updated_at: 100,
        };
        assert!(matches!(
            store.save_draft(&bad_draft),
            Err(StoreError::InvalidDraft)
        ));

        let key = ChatKey::new(Provider::Whatsapp, "main", "1@s.whatsapp.net");
        let mut rs = postbox_core::read::ReadState::new();
        rs.mark_read(&key, 500);
        store.save_read_state(&rs).unwrap();
        let loaded = store.load_read_state().unwrap();
        assert_eq!(loaded.get(&key).read_ts, 500);

        // Lower watermark never regresses stored watermark.
        let mut older = postbox_core::read::ReadState::new();
        older.mark_read(&key, 200);
        store.save_read_state(&older).unwrap();
        assert_eq!(store.load_read_state().unwrap().get(&key).read_ts, 500);
    }

    #[test]
    fn m3_media_cache_gone_media_recents_and_reactions_round_trip() {
        let mut store = Store::open_in_memory().unwrap();
        let chat_a = ChatKey::new(Provider::Whatsapp, "main", "a@s.whatsapp.net");
        let chat_b = ChatKey::new(Provider::Whatsapp, "main", "b@s.whatsapp.net");
        store
            .upsert_chat(
                &sample_chat(
                    Provider::Whatsapp,
                    "main",
                    "a@s.whatsapp.net",
                    "Alice",
                    "2026-11-26T10:00:00Z",
                ),
                1000,
            )
            .unwrap();
        store
            .upsert_chat(
                &sample_chat(
                    Provider::Whatsapp,
                    "main",
                    "b@s.whatsapp.net",
                    "Bob",
                    "2026-10-01T10:00:00Z",
                ),
                1000,
            )
            .unwrap();

        // media_cache CRUD + eviction
        let entry = MediaCacheEntry {
            cache_key: "k1".into(),
            sha256: "deadbeef".into(),
            rel_path: "hub-media/a.bin.png".into(),
            mime_type: "image/png".into(),
            byte_size: 4096,
            last_accessed_at: 1000,
        };
        store.upsert_media_cache(&entry).unwrap();
        assert_eq!(store.get_media_cache("k1").unwrap(), Some(entry));
        assert!(store.touch_media_cache("k1", 5000).unwrap());
        assert_eq!(
            store
                .get_media_cache("k1")
                .unwrap()
                .unwrap()
                .last_accessed_at,
            5000
        );
        let evicted = store.evict_expired_media_older_than(5001).unwrap();
        assert_eq!(evicted, vec!["hub-media/a.bin.png".to_owned()]);
        assert!(store.get_media_cache("k1").unwrap().is_none());

        // gone_media never collides across chats even with identical msg_id
        store.mark_media_gone(&chat_a, "dup_id", 1000).unwrap();
        assert!(store.is_media_gone(&chat_a, "dup_id").unwrap());
        assert!(!store.is_media_gone(&chat_b, "dup_id").unwrap());
        assert_eq!(
            store.list_gone_media_for_chat(&chat_a).unwrap(),
            vec!["dup_id".to_owned()]
        );

        // media_fetch_markers
        store
            .upsert_fetch_marker("avatar:a", "none", 2000, None)
            .unwrap();
        let marker = store.get_fetch_marker("avatar:a").unwrap().unwrap();
        assert_eq!(marker.status, "none");
        assert_eq!(marker.checked_at, 2000);

        // emoji recents persistence + dedup + cap 8
        let recents: Vec<String> = ["🔥", "👍", "🔥", "😂", "❤️", "😮", "😢", "🙏", "🎉", "🚀"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        store.save_emoji_recents(&recents).unwrap();
        let loaded = store.load_emoji_recents().unwrap();
        assert_eq!(loaded.len(), 8);
        assert_eq!(loaded[0], "🔥");
        assert_eq!(loaded[1], "👍");

        // file_size + update_message_reactions
        let msg = Message {
            chat: chat_a.clone(),
            msg_id: "m1".into(),
            sender_id: "u1".into(),
            sender_name: "Alice".into(),
            from_me: false,
            ts: "2026-11-26T10:00:00Z".into(),
            body: "big file".into(),
            reply_to_id: None,
            reply_broadcast: false,
            media_kind: Some(MediaKind::Document),
            mime_type: Some("application/pdf".into()),
            filename: Some("big.pdf".into()),
            file_size: Some(104_857_601),
            reactions: vec![],
            read_status: ReadStatus::None,
            deleted: false,
        };
        store.upsert_message(&msg).unwrap();
        let reactions = vec![Reaction {
            emoji: "👍".into(),
            count: 1,
            from_me: true,
        }];
        assert!(
            store
                .update_message_reactions(&chat_a, "m1", &reactions)
                .unwrap()
        );
        let listed = store.list_messages(&chat_a, 10, None).unwrap();
        assert_eq!(listed[0].file_size, Some(104_857_601));
        assert_eq!(listed[0].reactions, reactions);

        // list_recent_chats_within_days (including Slack space-separated timestamps at exact cutoff)
        let chat_slack_space = ChatKey::new(Provider::Slack, "T1", "slack:T1:C999");
        store
            .upsert_chat(
                &Chat {
                    key: chat_slack_space.clone(),
                    title: "#general".into(),
                    is_group: true,
                    archived_local: false,
                    archived_remote: false,
                    pinned: false,
                    unread_count: 0,
                    last_msg_ts: "2026-11-12 00:00:00".into(),
                    last_msg_preview: "hi".into(),
                    avatar_hash: None,
                },
                1000,
            )
            .unwrap();
        let recent = store
            .list_recent_chats_within_days("2026-11-12T00:00:00Z", 10)
            .unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].key, chat_a);
        assert_eq!(recent[1].key, chat_slack_space);
    }
}
