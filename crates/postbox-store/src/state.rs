use crate::{SETTING_VIEW_ORDER, Store, StoreError};
use postbox_core::archive::{ArchiveEntry, ArchiveTable, PendingArchiveIntent};
use postbox_core::read::{
    MAX_PENDING_READS, PendingReadEntry, PendingReadQueue, ReadState, ReadWatermark,
};
use postbox_core::{ChatKey, Provider};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use std::collections::HashMap;

pub const MAX_PINS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingArchiveRow {
    pub chat: ChatKey,
    pub intent: PendingArchiveIntent,
    pub not_before_ms: i64,
    pub retries: u8,
}

impl Store {
    // ── Pins (cap 16, synced with `chats.pinned`) ────────────────────────

    pub fn pin_chat(&mut self, key: &ChatKey, now_unix: i64) -> Result<bool, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let already: i64 = tx.query_row(
            "SELECT COUNT(*) FROM pins WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3",
            params![key.provider.as_str(), key.account, key.id],
            |r| r.get(0),
        )?;
        if already > 0 {
            tx.execute(
                "UPDATE chats SET pinned = 1 WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3",
                params![key.provider.as_str(), key.account, key.id],
            )?;
            tx.commit()?;
            return Ok(false);
        }
        let total: i64 = tx.query_row("SELECT COUNT(*) FROM pins", [], |r| r.get(0))?;
        if total >= MAX_PINS as i64 {
            return Err(StoreError::PinLimitReached);
        }
        let next_pos: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM pins",
            [],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO pins (provider, account_id, chat_id, position, pinned_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![key.provider.as_str(), key.account, key.id, next_pos, now_unix],
        )?;
        tx.execute(
            "UPDATE chats SET pinned = 1 WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3",
            params![key.provider.as_str(), key.account, key.id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn unpin_chat(&mut self, key: &ChatKey) -> Result<bool, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let removed = tx.execute(
            "DELETE FROM pins WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3",
            params![key.provider.as_str(), key.account, key.id],
        )?;
        tx.execute(
            "UPDATE chats SET pinned = 0 WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3",
            params![key.provider.as_str(), key.account, key.id],
        )?;
        tx.commit()?;
        Ok(removed > 0)
    }

    pub fn list_pins(&self) -> Result<Vec<ChatKey>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT provider, account_id, chat_id FROM pins ORDER BY position ASC")?;
        let rows = stmt.query_map([], |row| {
            let provider = Provider::parse(&row.get::<_, String>(0)?).unwrap_or(Provider::Whatsapp);
            Ok(ChatKey::new(
                provider,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn is_pinned(&self, key: &ChatKey) -> Result<bool, StoreError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM pins WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3",
            params![key.provider.as_str(), key.account, key.id],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn count_pinned_chats(&self) -> Result<usize, StoreError> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM pins", [], |r| r.get(0))?;
        Ok(count.max(0) as usize)
    }

    // ── View Order & Per-View Prefs ──────────────────────────────────────

    pub fn load_view_order(&self) -> Result<Vec<String>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT view_key FROM view_order ORDER BY position ASC")?;
        let keys: Vec<String> = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if !keys.is_empty() {
            return Ok(keys);
        }
        // Fallback to M1 settings JSON key for seamless migration.
        let legacy: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![SETTING_VIEW_ORDER],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(raw) = legacy
            && let Ok(parsed) = serde_json::from_str::<Vec<String>>(&raw)
        {
            return Ok(parsed);
        }
        Ok(Vec::new())
    }

    pub fn save_view_order(&mut self, keys: &[String]) -> Result<(), StoreError> {
        let json = serde_json::to_string(keys)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM view_order", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO view_order (position, view_key) VALUES (?1, ?2)",
            )?;
            for (pos, key) in keys.iter().enumerate() {
                stmt.execute(params![pos as i64, key])?;
            }
        }
        tx.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![SETTING_VIEW_ORDER, json],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_view_show_archived(&self, view_key: &str) -> Result<bool, StoreError> {
        let val: Option<i64> = self
            .conn
            .query_row(
                "SELECT show_archived FROM view_prefs WHERE view_key = ?1",
                params![view_key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(val.unwrap_or(0) != 0)
    }

    pub fn set_view_show_archived(&self, view_key: &str, enabled: bool) -> Result<(), StoreError> {
        self.conn.execute(
            r#"
            INSERT INTO view_prefs (view_key, show_archived)
            VALUES (?1, ?2)
            ON CONFLICT(view_key) DO UPDATE SET show_archived = excluded.show_archived
            "#,
            params![view_key, i64::from(enabled)],
        )?;
        Ok(())
    }

    pub fn get_view_selected_chat(&self, view_key: &str) -> Result<Option<ChatKey>, StoreError> {
        let row: Option<(Option<String>, Option<String>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT selected_provider, selected_account_id, selected_chat_id FROM view_prefs WHERE view_key = ?1",
                params![view_key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        Ok(row.and_then(|(p, a, c)| {
            let provider = Provider::parse(&p?)?;
            Some(ChatKey::new(provider, a?, c?))
        }))
    }

    pub fn set_view_selected_chat(
        &self,
        view_key: &str,
        chat: Option<&ChatKey>,
    ) -> Result<(), StoreError> {
        let (p, a, c) = match chat {
            Some(k) => (
                Some(k.provider.as_str()),
                Some(k.account.as_str()),
                Some(k.id.as_str()),
            ),
            None => (None, None, None),
        };
        self.conn.execute(
            r#"
            INSERT INTO view_prefs (view_key, selected_provider, selected_account_id, selected_chat_id)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(view_key) DO UPDATE SET
                selected_provider = excluded.selected_provider,
                selected_account_id = excluded.selected_account_id,
                selected_chat_id = excluded.selected_chat_id
            "#,
            params![view_key, p, a, c],
        )?;
        Ok(())
    }

    // ── Read Watermarks & Pending Reads (cap 8) ──────────────────────────

    pub fn load_read_state(&self) -> Result<ReadState, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT provider, account_id, chat_id, read_ts, incoming_ts, checked_ts FROM read_watermarks",
        )?;
        let rows = stmt.query_map([], |row| {
            let provider = Provider::parse(&row.get::<_, String>(0)?).unwrap_or(Provider::Whatsapp);
            let key = ChatKey::new(provider, row.get::<_, String>(1)?, row.get::<_, String>(2)?);
            let wm = ReadWatermark {
                read_ts: row.get(3)?,
                incoming_ts: row.get(4)?,
                checked_ts: row.get(5)?,
            };
            Ok((key, wm))
        })?;
        let mut map = HashMap::new();
        for pair in rows {
            let (k, wm) = pair?;
            map.insert(k, wm);
        }
        Ok(ReadState::from_map(map))
    }

    pub fn upsert_read_watermark(
        &self,
        key: &ChatKey,
        wm: &ReadWatermark,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            r#"
            INSERT INTO read_watermarks (provider, account_id, chat_id, read_ts, incoming_ts, checked_ts)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(provider, account_id, chat_id) DO UPDATE SET
                read_ts = MAX(read_watermarks.read_ts, excluded.read_ts),
                incoming_ts = MAX(read_watermarks.incoming_ts, excluded.incoming_ts),
                checked_ts = MAX(read_watermarks.checked_ts, excluded.checked_ts)
            "#,
            params![
                key.provider.as_str(),
                key.account,
                key.id,
                wm.read_ts,
                wm.incoming_ts,
                wm.checked_ts,
            ],
        )?;
        Ok(())
    }

    /// Persists all watermarks in `ReadState` in a single atomic transaction while preserving
    /// monotonic `MAX(...)` invariants against concurrent writers.
    pub fn save_read_state(&mut self, state: &ReadState) -> Result<(), StoreError> {
        if state.entries().is_empty() {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut stmt = tx.prepare(
                r#"
                INSERT INTO read_watermarks (provider, account_id, chat_id, read_ts, incoming_ts, checked_ts)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(provider, account_id, chat_id) DO UPDATE SET
                    read_ts = MAX(read_watermarks.read_ts, excluded.read_ts),
                    incoming_ts = MAX(read_watermarks.incoming_ts, excluded.incoming_ts),
                    checked_ts = MAX(read_watermarks.checked_ts, excluded.checked_ts)
                "#,
            )?;
            for (key, wm) in state.entries() {
                stmt.execute(params![
                    key.provider.as_str(),
                    key.account,
                    key.id,
                    wm.read_ts,
                    wm.incoming_ts,
                    wm.checked_ts,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn next_pending_read_ready_at_ms(&self, now_ms: i64) -> Result<Option<i64>, StoreError> {
        let row: Option<(i64, i64)> = self
            .conn
            .query_row(
                "SELECT retries, backoff_until_ms FROM pending_reads ORDER BY position ASC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.map(|(retries, backoff_until_ms)| {
            if backoff_until_ms > 0 {
                backoff_until_ms
            } else if retries > 0 {
                now_ms.saturating_add(3_000)
            } else {
                0
            }
        }))
    }

    pub fn load_pending_reads(&self) -> Result<PendingReadQueue, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT provider, account_id, chat_id, retries, backoff_until_ms FROM pending_reads ORDER BY position ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let provider = Provider::parse(&row.get::<_, String>(0)?).unwrap_or(Provider::Whatsapp);
            Ok(PendingReadEntry {
                chat: ChatKey::new(provider, row.get::<_, String>(1)?, row.get::<_, String>(2)?),
                retries: row.get::<_, i64>(3)?.clamp(0, u8::MAX as i64) as u8,
                backoff_until_ms: row.get::<_, i64>(4)?.max(0) as u64,
            })
        })?;
        let entries = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(PendingReadQueue::from_entries(entries))
    }

    pub fn save_pending_reads(&mut self, queue: &PendingReadQueue) -> Result<(), StoreError> {
        if queue.len() > MAX_PENDING_READS {
            return Err(StoreError::PendingReadsFull);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM pending_reads", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO pending_reads (position, provider, account_id, chat_id, retries, backoff_until_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for (pos, entry) in queue.entries().iter().enumerate() {
                stmt.execute(params![
                    pos as i64,
                    entry.chat.provider.as_str(),
                    entry.chat.account,
                    entry.chat.id,
                    i64::from(entry.retries),
                    entry.backoff_until_ms as i64,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ── Archive Rules & Pending Remote Archive Queue ─────────────────────

    pub fn load_archive_table(&self) -> Result<ArchiveTable, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT provider, account_id, chat_id, baseline_ts, keep, checked_unix, returned, phone FROM archive_rules",
        )?;
        let rows = stmt.query_map([], |row| {
            let provider = Provider::parse(&row.get::<_, String>(0)?).unwrap_or(Provider::Whatsapp);
            Ok(ArchiveEntry {
                key: ChatKey::new(provider, row.get::<_, String>(1)?, row.get::<_, String>(2)?),
                baseline_ts: row.get(3)?,
                keep: row.get::<_, i64>(4)? != 0,
                checked_unix: row.get(5)?,
                returned: row.get::<_, i64>(6)? != 0,
                phone: row.get::<_, i64>(7)? != 0,
                held: false,
                held_ts: String::new(),
            })
        })?;
        let entries = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(ArchiveTable::from_entries(entries))
    }

    pub fn save_archive_table(&mut self, table: &ArchiveTable) -> Result<(), StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM archive_rules", [])?;
        {
            let mut stmt = tx.prepare(
                r#"
                INSERT INTO archive_rules (
                    provider, account_id, chat_id, baseline_ts, keep, checked_unix, returned, phone
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                "#,
            )?;
            for entry in table.entries() {
                stmt.execute(params![
                    entry.key.provider.as_str(),
                    entry.key.account,
                    entry.key.id,
                    entry.baseline_ts,
                    i64::from(entry.keep),
                    entry.checked_unix,
                    i64::from(entry.returned),
                    i64::from(entry.phone),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_archive_entry(&self, entry: &ArchiveEntry) -> Result<(), StoreError> {
        self.conn.execute(
            r#"
            INSERT INTO archive_rules (
                provider, account_id, chat_id, baseline_ts, keep, checked_unix, returned, phone
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT(provider, account_id, chat_id) DO UPDATE SET
                baseline_ts = excluded.baseline_ts,
                keep = excluded.keep,
                checked_unix = excluded.checked_unix,
                returned = excluded.returned,
                phone = excluded.phone
            "#,
            params![
                entry.key.provider.as_str(),
                entry.key.account,
                entry.key.id,
                entry.baseline_ts,
                i64::from(entry.keep),
                entry.checked_unix,
                i64::from(entry.returned),
                i64::from(entry.phone),
            ],
        )?;
        Ok(())
    }

    pub fn delete_archive_entry(&self, key: &ChatKey) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM archive_rules WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3",
            params![key.provider.as_str(), key.account, key.id],
        )?;
        Ok(())
    }

    pub fn enqueue_pending_archive(
        &self,
        key: &ChatKey,
        intent: PendingArchiveIntent,
        not_before_ms: i64,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            r#"
            INSERT INTO pending_archives (provider, account_id, chat_id, intent, not_before_ms, retries)
            VALUES (?1, ?2, ?3, ?4, ?5, 0)
            ON CONFLICT(provider, account_id, chat_id) DO UPDATE SET
                intent = excluded.intent,
                not_before_ms = excluded.not_before_ms,
                retries = 0
            "#,
            params![
                key.provider.as_str(),
                key.account,
                key.id,
                intent.as_str(),
                not_before_ms,
            ],
        )?;
        Ok(())
    }

    pub fn list_ready_pending_archives(
        &self,
        now_ms: i64,
    ) -> Result<Vec<PendingArchiveRow>, StoreError> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT provider, account_id, chat_id, intent, not_before_ms, retries
            FROM pending_archives
            WHERE not_before_ms <= ?1
            ORDER BY not_before_ms ASC, rowid ASC
            "#,
        )?;
        let rows = stmt.query_map(params![now_ms], |row| {
            let provider = Provider::parse(&row.get::<_, String>(0)?).unwrap_or(Provider::Whatsapp);
            let intent = PendingArchiveIntent::parse(&row.get::<_, String>(3)?)
                .unwrap_or(PendingArchiveIntent::ArchivePending);
            Ok(PendingArchiveRow {
                chat: ChatKey::new(provider, row.get::<_, String>(1)?, row.get::<_, String>(2)?),
                intent,
                not_before_ms: row.get(4)?,
                retries: row.get::<_, i64>(5)?.clamp(0, u8::MAX as i64) as u8,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn delete_pending_archive(&self, key: &ChatKey) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM pending_archives WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3",
            params![key.provider.as_str(), key.account, key.id],
        )?;
        Ok(())
    }

    pub fn delay_pending_archive(
        &self,
        key: &ChatKey,
        not_before_ms: i64,
        retries: u8,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE pending_archives SET not_before_ms = ?4, retries = ?5 WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3",
            params![
                key.provider.as_str(),
                key.account,
                key.id,
                not_before_ms,
                i64::from(retries),
            ],
        )?;
        Ok(())
    }

    pub fn next_pending_archive_ready_at_ms(&self) -> Result<Option<i64>, StoreError> {
        let min_ts: Option<Option<i64>> = self
            .conn
            .query_row("SELECT MIN(not_before_ms) FROM pending_archives", [], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(min_ts.flatten())
    }

    // ── Chat & Account Prefs (Slack Threads & Workspace Names) ───────────

    pub fn get_chat_replies_fold(&self, key: &ChatKey) -> Result<Option<bool>, StoreError> {
        let val: Option<Option<i64>> = self
            .conn
            .query_row(
                "SELECT replies_fold FROM chat_prefs WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3",
                params![key.provider.as_str(), key.account, key.id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(val.flatten().map(|v| v != 0))
    }

    pub fn set_chat_replies_fold(
        &self,
        key: &ChatKey,
        fold: Option<bool>,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            r#"
            INSERT INTO chat_prefs (provider, account_id, chat_id, replies_fold)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(provider, account_id, chat_id) DO UPDATE SET replies_fold = excluded.replies_fold
            "#,
            params![key.provider.as_str(), key.account, key.id, fold.map(i64::from)],
        )?;
        Ok(())
    }

    pub fn get_account_slack_threads(
        &self,
        provider: Provider,
        account: &str,
    ) -> Result<bool, StoreError> {
        let val: Option<i64> = self
            .conn
            .query_row(
                "SELECT slack_threads_enabled FROM account_prefs WHERE provider = ?1 AND account_id = ?2",
                params![provider.as_str(), account],
                |r| r.get(0),
            )
            .optional()?;
        Ok(val.unwrap_or(1) != 0)
    }

    pub fn set_account_slack_threads(
        &self,
        provider: Provider,
        account: &str,
        enabled: bool,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            r#"
            INSERT INTO account_prefs (provider, account_id, slack_threads_enabled)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(provider, account_id) DO UPDATE SET slack_threads_enabled = excluded.slack_threads_enabled
            "#,
            params![provider.as_str(), account, i64::from(enabled)],
        )?;
        Ok(())
    }

    pub fn get_account_workspace_name(
        &self,
        provider: Provider,
        account: &str,
    ) -> Result<Option<String>, StoreError> {
        let val: Option<String> = self
            .conn
            .query_row(
                "SELECT workspace_name FROM account_prefs WHERE provider = ?1 AND account_id = ?2",
                params![provider.as_str(), account],
                |r| r.get(0),
            )
            .optional()?;
        Ok(val.filter(|s| !s.is_empty()))
    }

    pub fn set_account_workspace_name(
        &self,
        provider: Provider,
        account: &str,
        name: &str,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            r#"
            INSERT INTO account_prefs (provider, account_id, workspace_name)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(provider, account_id) DO UPDATE SET workspace_name = excluded.workspace_name
            "#,
            params![provider.as_str(), account, name],
        )?;
        Ok(())
    }
}
