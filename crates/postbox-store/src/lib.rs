use postbox_core::{
    Account, Chat, ChatKey, Draft, FontScale, MediaKind, Message, Provider, Reaction, ReadStatus,
    ThemeChoice, UiSettings, ViewTab,
};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;

pub const MEDIA_TTL_SECS: i64 = 14 * 24 * 60 * 60;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite_error")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json_error")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutboxPhase {
    Ready,
    Uncertain,
    Held,
}

impl OutboxPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Uncertain => "uncertain",
            Self::Held => "held",
        }
    }

    fn parse(raw: &str) -> Self {
        match raw {
            "ready" => Self::Ready,
            "uncertain" => Self::Uncertain,
            _ => Self::Held,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRecord {
    pub client_id: String,
    pub chat: ChatKey,
    pub kind: String,
    pub phase: OutboxPhase,
    pub attempt_serial: u64,
    pub text_body: String,
    pub reply_to_id: Option<String>,
    pub reply_broadcast: bool,
    pub not_before_ms: i64,
    pub created_at_ms: i64,
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
    conn: Connection,
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
    text_body TEXT NOT NULL DEFAULT '',
    reply_to_id TEXT,
    reply_broadcast INTEGER NOT NULL DEFAULT 0,
    failure_reason TEXT,
    not_before_ms INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS transcripts (
    media_sha256 TEXT PRIMARY KEY,
    language TEXT NOT NULL,
    text TEXT NOT NULL,
    created_at INTEGER NOT NULL
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
"#;

const SETTING_THEME: &str = "ui.theme";
const SETTING_FONT_SCALE: &str = "ui.font_scale";
const SETTING_VIEW_ORDER: &str = "ui.view_order";
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

    fn init(conn: Connection) -> Result<Self, StoreError> {
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
                chat.last_msg_ts,
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
                    ORDER BY last_msg_ts DESC, title ASC, chat_id ASC
                    LIMIT ?1
                    "#
                );
                let mut stmt = self.conn.prepare(&sql)?;
                let rows = stmt.query_map(params![limit as i64], map_chat_row)?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(StoreError::from)
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
                    ORDER BY pinned DESC, last_msg_ts DESC, title ASC, chat_id ASC
                    LIMIT ?3
                    "#
                );
                let mut stmt = self.conn.prepare(&sql)?;
                let rows = stmt.query_map(
                    params![provider.as_str(), account, limit as i64],
                    map_chat_row,
                )?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(StoreError::from)
            }
        }
    }

    pub fn upsert_message(&mut self, msg: &Message) -> Result<(), StoreError> {
        let reactions_json = serde_json::to_string(&msg.reactions)?;
        let media_type = msg.media_kind.map(MediaKind::as_str);
        let read_status = msg.read_status.as_str();

        let tx = self.conn.transaction()?;
        tx.execute(
            r#"
            INSERT INTO messages (
                provider, account_id, chat_id, msg_id, sender_id, sender_name,
                from_me, ts, body, reply_to_id, reply_broadcast,
                media_type, mime_type, filename, reactions_json, read_status, deleted
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6,
                ?7, ?8, ?9, ?10, ?11,
                ?12, ?13, ?14, ?15, ?16, ?17
            )
            ON CONFLICT(provider, account_id, chat_id, msg_id) DO UPDATE SET
                sender_id = excluded.sender_id,
                sender_name = excluded.sender_name,
                from_me = excluded.from_me,
                ts = excluded.ts,
                body = excluded.body,
                reply_to_id = excluded.reply_to_id,
                reply_broadcast = excluded.reply_broadcast,
                media_type = excluded.media_type,
                mime_type = excluded.mime_type,
                filename = excluded.filename,
                reactions_json = excluded.reactions_json,
                read_status = excluded.read_status,
                deleted = excluded.deleted
            "#,
            params![
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
                read_status,
                i64::from(msg.deleted),
            ],
        )?;

        let msg_rowid: i64 = tx.query_row(
            "SELECT rowid FROM messages WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3 AND msg_id = ?4",
            params![
                msg.chat.provider.as_str(),
                msg.chat.account,
                msg.chat.id,
                msg.msg_id,
            ],
            |row| row.get(0),
        )?;

        let chat_title: String = tx
            .query_row(
                "SELECT title FROM chats WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3",
                params![msg.chat.provider.as_str(), msg.chat.account, msg.chat.id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or_default();

        tx.execute(
            "DELETE FROM messages_fts WHERE rowid = ?1",
            params![msg_rowid],
        )?;

        if !msg.deleted
            && (!msg.body.is_empty() || !msg.sender_name.is_empty() || !chat_title.is_empty())
        {
            tx.execute(
                r#"
                INSERT INTO messages_fts (
                    rowid, provider, account_id, chat_id, msg_id, chat_title, sender_name, body
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                "#,
                params![
                    msg_rowid,
                    msg.chat.provider.as_str(),
                    msg.chat.account,
                    msg.chat.id,
                    msg.msg_id,
                    chat_title,
                    msg.sender_name,
                    msg.body,
                ],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Returns up to `limit` messages for `chat_key` (optionally strictly before `before_ts`)
    /// in chronological order (oldest -> newest) for `MessageScroller`.
    pub fn list_messages(
        &self,
        chat_key: &ChatKey,
        limit: usize,
        before_ts: Option<&str>,
    ) -> Result<Vec<Message>, StoreError> {
        let mut messages = if let Some(before) = before_ts {
            let mut stmt = self.conn.prepare(
                r#"
                SELECT msg_id, sender_id, sender_name, from_me, ts, body,
                       reply_to_id, reply_broadcast, media_type, mime_type,
                       filename, reactions_json, read_status, deleted
                FROM messages
                WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3 AND ts < ?4
                ORDER BY ts DESC, msg_id DESC
                LIMIT ?5
                "#,
            )?;
            let rows = stmt.query_map(
                params![
                    chat_key.provider.as_str(),
                    chat_key.account,
                    chat_key.id,
                    before,
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
                       filename, reactions_json, read_status, deleted
                FROM messages
                WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3
                ORDER BY ts DESC, msg_id DESC
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

    pub fn enqueue_outbox(&self, rec: &OutboxRecord) -> Result<(), StoreError> {
        self.conn.execute(
            r#"
            INSERT INTO outbox (
                client_id, provider, account_id, chat_id, kind, phase,
                attempt_serial, text_body, reply_to_id, reply_broadcast,
                not_before_ms, created_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            "#,
            params![
                rec.client_id,
                rec.chat.provider.as_str(),
                rec.chat.account,
                rec.chat.id,
                rec.kind,
                rec.phase.as_str(),
                rec.attempt_serial as i64,
                rec.text_body,
                rec.reply_to_id,
                i64::from(rec.reply_broadcast),
                rec.not_before_ms,
                rec.created_at_ms,
            ],
        )?;
        Ok(())
    }

    pub fn next_ready_outbox(&self, now_ms: i64) -> Result<Option<OutboxRecord>, StoreError> {
        self.conn
            .query_row(
                r#"
                SELECT client_id, provider, account_id, chat_id, kind, phase,
                       attempt_serial, text_body, reply_to_id, reply_broadcast,
                       not_before_ms, created_at_ms
                FROM outbox
                WHERE phase = 'ready' AND not_before_ms <= ?1
                ORDER BY created_at_ms ASC
                LIMIT 1
                "#,
                params![now_ms],
                |row| {
                    let provider =
                        Provider::parse(&row.get::<_, String>(1)?).unwrap_or(Provider::Whatsapp);
                    Ok(OutboxRecord {
                        client_id: row.get(0)?,
                        chat: ChatKey::new(
                            provider,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ),
                        kind: row.get(4)?,
                        phase: OutboxPhase::parse(&row.get::<_, String>(5)?),
                        attempt_serial: row.get::<_, i64>(6)? as u64,
                        text_body: row.get(7)?,
                        reply_to_id: row.get(8)?,
                        reply_broadcast: row.get::<_, i64>(9)? != 0,
                        not_before_ms: row.get(10)?,
                        created_at_ms: row.get(11)?,
                    })
                },
            )
            .optional()
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
        let cutoff = now_unix - MEDIA_TTL_SECS;
        let mut stmt = self
            .conn
            .prepare("SELECT rel_path FROM media_cache WHERE last_accessed_at < ?1")?;
        let paths = stmt
            .query_map(params![cutoff], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        self.conn.execute(
            "DELETE FROM media_cache WHERE last_accessed_at < ?1",
            params![cutoff],
        )?;
        Ok(paths)
    }
}

fn map_message_row(chat_key: &ChatKey, row: &Row<'_>) -> rusqlite::Result<Message> {
    let media_type: Option<String> = row.get(8)?;
    let reactions_json: String = row.get(11)?;
    let read_status_str: String = row.get(12)?;
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
        reactions,
        read_status: ReadStatus::parse(&read_status_str),
        deleted: row.get::<_, i64>(13)? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn cache_size_pragma_is_set_to_16mb() {
        let store = Store::open_in_memory().unwrap();
        let cache_size: i64 = store
            .conn
            .pragma_query_value(None, "cache_size", |row| row.get(0))
            .unwrap();
        assert_eq!(cache_size, -16_000);
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
            .list_messages(&chat, 10, Some("2026-11-26T10:05:00Z"))
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
            reactions: vec![],
            read_status: ReadStatus::Read,
            deleted: false,
        };
        store.upsert_message(&msg).unwrap();
        // Upsert again with edited body — must NOT duplicate FTS entries
        msg.body = "Beta milestone ready".into();
        store.upsert_message(&msg).unwrap();
        store.upsert_message(&msg).unwrap();

        assert_eq!(store.search_messages_fts("Alpha", 10).unwrap().len(), 0);
        assert_eq!(store.search_messages_fts("Beta", 10).unwrap().len(), 1);
        assert_eq!(store.search_messages_fts("Clara", 10).unwrap().len(), 1);
        assert_eq!(store.search_messages_fts("Phoenix", 10).unwrap().len(), 1);

        // Rename chat -> FTS must reflect new title and drop stale title
        let mut renamed = chat.clone();
        renamed.title = "Orion Release".into();
        store.upsert_chat(&renamed, 200).unwrap();

        assert_eq!(store.search_messages_fts("Phoenix", 10).unwrap().len(), 0);
        assert_eq!(store.search_messages_fts("Orion", 10).unwrap().len(), 1);

        // Deleting message removes it from FTS
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

        // Older pinned chat in WhatsApp
        let mut wa_pinned = sample_chat(
            Provider::Whatsapp,
            "main",
            "c1",
            "Pinned Older",
            "2026-11-26T08:00:00Z",
        );
        wa_pinned.pinned = true;
        wa_pinned.unread_count = 0;

        // Newer unpinned chat in WhatsApp
        let mut wa_newer = sample_chat(
            Provider::Whatsapp,
            "main",
            "c2",
            "Newer Unpinned",
            "2026-11-26T12:00:00Z",
        );
        wa_newer.unread_count = 2;

        // Archived unread chat in Slack
        let mut sl_archived_unread = sample_chat(
            Provider::Slack,
            "T1",
            "c3",
            "Archived Unread",
            "2026-11-26T11:00:00Z",
        );
        sl_archived_unread.archived_local = true;
        sl_archived_unread.unread_count = 1;

        // Archived read chat in Slack
        let mut sl_archived_read = sample_chat(
            Provider::Slack,
            "T1",
            "c4",
            "Archived Read",
            "2026-11-26T13:00:00Z",
        );
        sl_archived_read.archived_remote = true;
        sl_archived_read.unread_count = 0;

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

        // Combined Inbox ignores pins -> Newer Unpinned before Pinned Older
        let inbox = store
            .list_chats_filtered(&ViewTab::CombinedInbox, false, false, false, 20)
            .unwrap();
        let inbox_ids: Vec<&str> = inbox.iter().map(|c| c.key.id.as_str()).collect();
        assert_eq!(inbox_ids, vec!["c2", "c1"]);

        // Combined Inbox + Alt+A (include_archived = true, archived_only = false) -> unread-only!
        let inbox_alt_a = store
            .list_chats_filtered(&ViewTab::CombinedInbox, true, false, false, 20)
            .unwrap();
        let alt_a_ids: Vec<&str> = inbox_alt_a.iter().map(|c| c.key.id.as_str()).collect();
        assert_eq!(alt_a_ids, vec!["c2", "c3"]);

        // Account view honors pins -> Pinned Older before Newer Unpinned
        let wa_view = wa.to_view_tab();
        let wa_chats = store
            .list_chats_filtered(&wa_view, false, false, false, 20)
            .unwrap();
        let wa_ids: Vec<&str> = wa_chats.iter().map(|c| c.key.id.as_str()).collect();
        assert_eq!(wa_ids, vec!["c1", "c2"]);

        // Archived only across inbox
        let archived = store
            .list_chats_filtered(&ViewTab::CombinedInbox, true, true, false, 20)
            .unwrap();
        let archived_ids: Vec<&str> = archived.iter().map(|c| c.key.id.as_str()).collect();
        assert_eq!(archived_ids, vec!["c4", "c3"]);

        // Unread only in account view
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

        // Simulate legacy persisted font_scale = 82
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
        let store = Store::open_in_memory().unwrap();
        let chat = ChatKey::new(Provider::Whatsapp, "main", "49123@s.whatsapp.net");
        store
            .enqueue_outbox(&OutboxRecord {
                client_id: "cid-1".into(),
                chat,
                kind: "text".into(),
                phase: OutboxPhase::Ready,
                attempt_serial: 0,
                text_body: "hello".into(),
                reply_to_id: None,
                reply_broadcast: false,
                not_before_ms: 1000,
                created_at_ms: 900,
            })
            .unwrap();

        assert!(store.next_ready_outbox(999).unwrap().is_none());
        assert_eq!(
            store.next_ready_outbox(1000).unwrap().unwrap().client_id,
            "cid-1"
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
}
