use crate::{Store, StoreError, map_chat_row};
use postbox_core::{Chat, ChatKey, Reaction};
use rusqlite::{OptionalExtension, params};

pub const SETTING_EMOJI_RECENTS: &str = "ui.emoji_recents";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaCacheEntry {
    pub cache_key: String,
    pub sha256: String,
    pub rel_path: String,
    pub mime_type: String,
    pub byte_size: u64,
    pub last_accessed_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchMarker {
    pub status: String,
    pub checked_at: i64,
    pub rel_path: Option<String>,
}

/// Canonical composite key for `gone_media` guaranteeing identical `msg_id` across different
/// chats/accounts/providers never collides.
pub fn gone_media_pair_key(chat: &ChatKey, msg_id: &str) -> String {
    format!(
        "{}\0{}\0{}\0{msg_id}",
        chat.provider.as_str(),
        chat.account,
        chat.id
    )
}

impl Store {
    pub fn get_media_cache(&self, cache_key: &str) -> Result<Option<MediaCacheEntry>, StoreError> {
        self.conn
            .query_row(
                r#"
                SELECT cache_key, sha256, rel_path, mime_type, byte_size, last_accessed_at
                FROM media_cache
                WHERE cache_key = ?1
                "#,
                params![cache_key],
                |row| {
                    Ok(MediaCacheEntry {
                        cache_key: row.get(0)?,
                        sha256: row.get(1)?,
                        rel_path: row.get(2)?,
                        mime_type: row.get(3)?,
                        byte_size: row.get::<_, i64>(4)?.max(0) as u64,
                        last_accessed_at: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn upsert_media_cache(&self, entry: &MediaCacheEntry) -> Result<(), StoreError> {
        self.conn.execute(
            r#"
            INSERT INTO media_cache (cache_key, sha256, rel_path, mime_type, byte_size, last_accessed_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(cache_key) DO UPDATE SET
                sha256 = excluded.sha256,
                rel_path = excluded.rel_path,
                mime_type = excluded.mime_type,
                byte_size = excluded.byte_size,
                last_accessed_at = excluded.last_accessed_at
            "#,
            params![
                entry.cache_key,
                entry.sha256,
                entry.rel_path,
                entry.mime_type,
                entry.byte_size as i64,
                entry.last_accessed_at,
            ],
        )?;
        Ok(())
    }

    pub fn touch_media_cache(&self, cache_key: &str, now_unix: i64) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "UPDATE media_cache SET last_accessed_at = ?2 WHERE cache_key = ?1",
            params![cache_key, now_unix],
        )?;
        Ok(changed > 0)
    }

    pub fn delete_media_cache(&self, cache_key: &str) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            "DELETE FROM media_cache WHERE cache_key = ?1",
            params![cache_key],
        )?;
        Ok(changed > 0)
    }

    pub fn evict_expired_media_older_than(
        &self,
        cutoff_unix: i64,
    ) -> Result<Vec<String>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT rel_path FROM media_cache WHERE last_accessed_at < ?1")?;
        let paths = stmt
            .query_map(params![cutoff_unix], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        self.conn.execute(
            "DELETE FROM media_cache WHERE last_accessed_at < ?1",
            params![cutoff_unix],
        )?;
        Ok(paths)
    }

    pub fn list_live_media_rel_paths(
        &self,
    ) -> Result<std::collections::HashSet<String>, StoreError> {
        let mut stmt = self.conn.prepare("SELECT rel_path FROM media_cache")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<std::collections::HashSet<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn is_media_gone(&self, chat: &ChatKey, msg_id: &str) -> Result<bool, StoreError> {
        let pair_key = gone_media_pair_key(chat, msg_id);
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM gone_media WHERE pair_key = ?1",
                params![pair_key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    pub fn mark_media_gone(
        &self,
        chat: &ChatKey,
        msg_id: &str,
        now_unix: i64,
    ) -> Result<(), StoreError> {
        let pair_key = gone_media_pair_key(chat, msg_id);
        self.conn.execute(
            r#"
            INSERT INTO gone_media (pair_key, provider, account_id, chat_id, msg_id, recorded_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(pair_key) DO NOTHING
            "#,
            params![
                pair_key,
                chat.provider.as_str(),
                chat.account,
                chat.id,
                msg_id,
                now_unix,
            ],
        )?;
        Ok(())
    }

    pub fn list_gone_media_for_chat(&self, chat: &ChatKey) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT msg_id
            FROM gone_media
            WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3
            ORDER BY recorded_at ASC, msg_id ASC
            "#,
        )?;
        let rows = stmt.query_map(
            params![chat.provider.as_str(), chat.account, chat.id],
            |row| row.get::<_, String>(0),
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn get_fetch_marker(&self, key: &str) -> Result<Option<FetchMarker>, StoreError> {
        self.conn
            .query_row(
                "SELECT status, checked_at, rel_path FROM media_fetch_markers WHERE marker_key = ?1",
                params![key],
                |row| {
                    Ok(FetchMarker {
                        status: row.get(0)?,
                        checked_at: row.get(1)?,
                        rel_path: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn upsert_fetch_marker(
        &self,
        key: &str,
        status: &str,
        checked_at: i64,
        rel_path: Option<&str>,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            r#"
            INSERT INTO media_fetch_markers (marker_key, status, checked_at, rel_path)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(marker_key) DO UPDATE SET
                status = excluded.status,
                checked_at = excluded.checked_at,
                rel_path = excluded.rel_path
            "#,
            params![key, status, checked_at, rel_path],
        )?;
        Ok(())
    }

    pub fn load_emoji_recents(&self) -> Result<Vec<String>, StoreError> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![SETTING_EMOJI_RECENTS],
                |row| row.get(0),
            )
            .optional()?;
        let Some(text) = raw else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for part in text.split(',') {
            let trimmed = part.trim();
            if !trimmed.is_empty() && !out.iter().any(|existing| existing == trimmed) {
                out.push(trimmed.to_owned());
                if out.len() >= 8 {
                    break;
                }
            }
        }
        Ok(out)
    }

    pub fn save_emoji_recents(&self, recents: &[String]) -> Result<(), StoreError> {
        let mut dedup: Vec<&str> = Vec::with_capacity(8);
        for item in recents {
            let trimmed = item.trim();
            if !trimmed.is_empty() && !dedup.contains(&trimmed) {
                dedup.push(trimmed);
                if dedup.len() >= 8 {
                    break;
                }
            }
        }
        let encoded = dedup.join(",");
        self.conn.execute(
            r#"
            INSERT INTO settings (key, value) VALUES (?1, ?2)
            ON CONFLICT(key) DO UPDATE SET value = excluded.value
            "#,
            params![SETTING_EMOJI_RECENTS, encoded],
        )?;
        Ok(())
    }

    pub fn get_message_reactions(
        &self,
        chat: &ChatKey,
        msg_id: &str,
    ) -> Result<Option<Vec<Reaction>>, StoreError> {
        let json: Option<String> = self
            .conn
            .query_row(
                r#"
                SELECT reactions_json
                FROM messages
                WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3 AND msg_id = ?4
                "#,
                params![chat.provider.as_str(), chat.account, chat.id, msg_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(json.map(|raw| serde_json::from_str(&raw).unwrap_or_default()))
    }

    pub fn update_message_reactions(
        &mut self,
        chat: &ChatKey,
        msg_id: &str,
        reactions: &[Reaction],
    ) -> Result<bool, StoreError> {
        let reactions_json = serde_json::to_string(reactions)?;
        let changed = self.conn.execute(
            r#"
            UPDATE messages
            SET reactions_json = ?5
            WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3 AND msg_id = ?4
            "#,
            params![
                chat.provider.as_str(),
                chat.account,
                chat.id,
                msg_id,
                reactions_json,
            ],
        )?;
        Ok(changed > 0)
    }

    /// Lists non-empty recent chats ordered newest-first whose `last_msg_ts` is at or after
    /// `cutoff_ts` (RFC3339 prefix/lexicographical filter for 14-day background media drain).
    pub fn list_recent_chats_within_days(
        &self,
        cutoff_ts: &str,
        limit: usize,
    ) -> Result<Vec<Chat>, StoreError> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT provider, account_id, chat_id, title, is_group, archived_local, archived_remote,
                   pinned, unread_count, last_msg_ts, last_msg_preview, avatar_hash
            FROM chats
            WHERE last_msg_ts != ''
              AND REPLACE(RTRIM(last_msg_ts, 'Z'), ' ', 'T') >= REPLACE(RTRIM(?1, 'Z'), ' ', 'T')
            ORDER BY REPLACE(RTRIM(last_msg_ts, 'Z'), ' ', 'T') DESC, chat_id ASC
            LIMIT ?2
            "#,
        )?;
        let rows = stmt.query_map(params![cutoff_ts, limit as i64], map_chat_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }
}
