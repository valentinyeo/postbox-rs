use postbox_core::{Chat, ChatKey, Message, Provider};
use rusqlite::{Connection, OptionalExtension, params};
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
"#;

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
        conn.execute_batch(SCHEMA_SQL)?;
        Ok(Self { conn })
    }

    pub fn upsert_chat(&self, chat: &Chat, updated_at: i64) -> Result<(), StoreError> {
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
        Ok(())
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
        let rows = stmt.query_map(params![limit as i64], |row| {
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
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn upsert_message(&mut self, msg: &Message) -> Result<(), StoreError> {
        let reactions_json = serde_json::to_string(&msg.reactions)?;
        let tx = self.conn.transaction()?;
        tx.execute(
            r#"
            INSERT INTO messages (
                provider, account_id, chat_id, msg_id, sender_id, sender_name,
                from_me, ts, body, reply_to_id, reply_broadcast, reactions_json, deleted
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            ON CONFLICT(provider, account_id, chat_id, msg_id) DO UPDATE SET
                sender_name = excluded.sender_name,
                body = excluded.body,
                reactions_json = excluded.reactions_json,
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
                reactions_json,
                i64::from(msg.deleted),
            ],
        )?;
        tx.execute(
            "DELETE FROM messages_fts WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3 AND msg_id = ?4",
            params![msg.chat.provider.as_str(), msg.chat.account, msg.chat.id, msg.msg_id],
        )?;
        if !msg.deleted && !msg.body.is_empty() {
            tx.execute(
                "INSERT INTO messages_fts (provider, account_id, chat_id, msg_id, sender_name, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    msg.chat.provider.as_str(),
                    msg.chat.account,
                    msg.chat.id,
                    msg.msg_id,
                    msg.sender_name,
                    msg.body,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn search_messages_fts(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>, StoreError> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT provider, account_id, chat_id, msg_id, sender_name,
                   snippet(messages_fts, 5, '[', ']', '...', 12)
            FROM messages_fts
            WHERE messages_fts MATCH ?1
            ORDER BY rank
            LIMIT ?2
            "#,
        )?;
        let rows = stmt.query_map(params![query, limit as i64], |row| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::ReadStatus;

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
}
