use crate::{Store, StoreError};
use postbox_core::outbox::{
    MAX_OUTBOX_ENTRIES, MAX_PENDING_SENDS, OutboxKind, OutboxPhase, OutboxRecord, SendOutcome,
    apply_outcome, is_valid_client_id, validate_record,
};
use postbox_core::{ChatKey, Provider};
use rusqlite::{OptionalExtension, Row, TransactionBehavior, params};

const OUTBOX_SELECT_COLS: &str = r#"
    client_id, provider, account_id, chat_id, kind, phase,
    attempt_serial, attempts, ambiguous, text_body, reply_to_id, reply_broadcast,
    staged_file_path, wire_filename, file_sha256, file_bytes,
    ptt, voice_seconds, attachment_ready, failure_reason,
    not_before_ms, created_at_ms
"#;

pub(crate) fn map_outbox_row(row: &Row<'_>) -> rusqlite::Result<OutboxRecord> {
    let provider = Provider::parse(&row.get::<_, String>(1)?).unwrap_or(Provider::Whatsapp);
    let sha_blob: Option<Vec<u8>> = row.get(14)?;
    let file_sha256 = sha_blob.and_then(|v| <[u8; 32]>::try_from(v).ok());
    Ok(OutboxRecord {
        client_id: row.get(0)?,
        chat: ChatKey::new(provider, row.get::<_, String>(2)?, row.get::<_, String>(3)?),
        kind: OutboxKind::parse(&row.get::<_, String>(4)?),
        phase: OutboxPhase::parse(&row.get::<_, String>(5)?),
        attempt_serial: row.get::<_, i64>(6)?.max(0) as u64,
        attempts: row.get::<_, i64>(7)?.clamp(0, u8::MAX as i64) as u8,
        ambiguous: row.get::<_, i64>(8)? != 0,
        text_body: row.get(9)?,
        reply_to_id: row.get(10)?,
        reply_broadcast: row.get::<_, i64>(11)? != 0,
        staged_file_path: row.get(12)?,
        wire_filename: row.get(13)?,
        file_sha256,
        file_bytes: row.get::<_, i64>(15)?.max(0) as u64,
        ptt: row.get::<_, i64>(16)? != 0,
        voice_seconds: row.get::<_, i64>(17)?.max(0) as u32,
        attachment_ready: row.get::<_, i64>(18)? != 0,
        failure_reason: row.get(19)?,
        not_before_ms: row.get(20)?,
        created_at_ms: row.get(21)?,
    })
}

impl Store {
    /// Durably enqueues a validated `OutboxRecord` inside an `IMMEDIATE` transaction.
    pub fn enqueue_outbox(&mut self, rec: &OutboxRecord) -> Result<(), StoreError> {
        validate_record(rec).map_err(|_| StoreError::InvalidOutboxRecord)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let total: i64 = tx.query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get(0))?;
        if total >= MAX_OUTBOX_ENTRIES as i64 {
            return Err(StoreError::OutboxFull);
        }
        if rec.phase == OutboxPhase::Ready {
            let ready: i64 = tx.query_row(
                "SELECT COUNT(*) FROM outbox WHERE phase = 'ready'",
                [],
                |r| r.get(0),
            )?;
            if ready >= MAX_PENDING_SENDS as i64 {
                return Err(StoreError::OutboxFull);
            }
        }

        tx.execute(
            r#"
            INSERT INTO outbox (
                client_id, provider, account_id, chat_id, kind, phase,
                attempt_serial, attempts, ambiguous, text_body, reply_to_id, reply_broadcast,
                staged_file_path, wire_filename, file_sha256, file_bytes,
                ptt, voice_seconds, attachment_ready, failure_reason,
                not_before_ms, created_at_ms
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6,
                ?7, ?8, ?9, ?10, ?11, ?12,
                ?13, ?14, ?15, ?16,
                ?17, ?18, ?19, ?20,
                ?21, ?22
            )
            "#,
            params![
                rec.client_id,
                rec.chat.provider.as_str(),
                rec.chat.account,
                rec.chat.id,
                rec.kind.as_str(),
                rec.phase.as_str(),
                rec.attempt_serial as i64,
                i64::from(rec.attempts),
                i64::from(rec.ambiguous),
                rec.text_body,
                rec.reply_to_id,
                i64::from(rec.reply_broadcast),
                rec.staged_file_path,
                rec.wire_filename,
                rec.file_sha256.as_ref().map(|h| h.as_slice()),
                rec.file_bytes as i64,
                i64::from(rec.ptt),
                i64::from(rec.voice_seconds),
                i64::from(rec.attachment_ready),
                rec.failure_reason,
                rec.not_before_ms,
                rec.created_at_ms,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Atomically claims the oldest ready outbox row (`not_before_ms <= now_ms`) and commits
    /// `phase = 'uncertain', attempt_serial = attempt_serial + 1` BEFORE returning.
    ///
    /// Enforces `SendQueue.tla` invariants:
    /// - `SingleInflight`: returns `Ok(None)` if any row currently has `phase = 'uncertain'`.
    /// - `InflightIsUncertain`: durable checkpoint committed before HTTP dispatch.
    /// - `ReadyResolves`: skips `held` and backoff-waiting rows across all chats.
    pub fn claim_next_ready_outbox(
        &mut self,
        now_ms: i64,
    ) -> Result<Option<OutboxRecord>, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let inflight: i64 = tx.query_row(
            "SELECT COUNT(*) FROM outbox WHERE phase = 'uncertain'",
            [],
            |r| r.get(0),
        )?;
        if inflight > 0 {
            return Ok(None);
        }

        let sql = format!(
            "SELECT {OUTBOX_SELECT_COLS} FROM outbox \
             WHERE phase = 'ready' AND not_before_ms <= ?1 \
             ORDER BY created_at_ms ASC, rowid ASC LIMIT 1"
        );
        let Some(mut rec) = tx
            .query_row(&sql, params![now_ms], map_outbox_row)
            .optional()?
        else {
            return Ok(None);
        };

        if rec.attempt_serial >= i64::MAX as u64 {
            return Err(StoreError::InvalidOutboxRecord);
        }
        rec.phase = OutboxPhase::Uncertain;
        rec.attempt_serial += 1;

        let changed = tx.execute(
            "UPDATE outbox SET phase = 'uncertain', attempt_serial = ?2 WHERE client_id = ?1 AND phase = 'ready'",
            params![rec.client_id, rec.attempt_serial as i64],
        )?;
        if changed != 1 {
            return Ok(None);
        }

        tx.commit()?;
        Ok(Some(rec))
    }

    /// Deletes a confirmed delivered send (`NoLoss`: only called after Hub confirms `status == "sent"`).
    pub fn acknowledge_outbox(
        &mut self,
        client_id: &str,
        attempt_serial: u64,
    ) -> Result<bool, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rows = tx.execute(
            "DELETE FROM outbox WHERE client_id = ?1 AND phase = 'uncertain' AND attempt_serial = ?2",
            params![client_id, attempt_serial as i64],
        )?;
        tx.commit()?;
        Ok(rows == 1)
    }

    /// Transitions an in-flight `uncertain` row to retry backoff (`3s/10s/30s`, keeping identical
    /// `client_id` for `NoDouble`) or `held` after attempt 4 (`AutoSendsBounded`).
    pub fn mark_outbox_retryable(
        &mut self,
        client_id: &str,
        attempt_serial: u64,
        ambiguous: bool,
        reason: &str,
        now_ms: i64,
    ) -> Result<Option<OutboxRecord>, StoreError> {
        self.apply_inflight_outcome(
            client_id,
            attempt_serial,
            SendOutcome::Retryable,
            ambiguous,
            reason,
            now_ms,
        )
    }

    /// Transitions an in-flight `uncertain` row to terminal `held` (`401`/`403`/`409`/Signal-TG `429`).
    pub fn mark_outbox_held(
        &mut self,
        client_id: &str,
        attempt_serial: u64,
        ambiguous: bool,
        reason: &str,
    ) -> Result<Option<OutboxRecord>, StoreError> {
        self.apply_inflight_outcome(
            client_id,
            attempt_serial,
            SendOutcome::Held,
            ambiguous,
            reason,
            0,
        )
    }

    fn apply_inflight_outcome(
        &mut self,
        client_id: &str,
        attempt_serial: u64,
        outcome: SendOutcome,
        ambiguous: bool,
        reason: &str,
        now_ms: i64,
    ) -> Result<Option<OutboxRecord>, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let sql = format!(
            "SELECT {OUTBOX_SELECT_COLS} FROM outbox \
             WHERE client_id = ?1 AND phase = 'uncertain' AND attempt_serial = ?2"
        );
        let Some(current) = tx
            .query_row(
                &sql,
                params![client_id, attempt_serial as i64],
                map_outbox_row,
            )
            .optional()?
        else {
            return Ok(None);
        };

        let Some(updated) = apply_outcome(&current, outcome, ambiguous, Some(reason), now_ms)
        else {
            tx.execute(
                "DELETE FROM outbox WHERE client_id = ?1",
                params![client_id],
            )?;
            tx.commit()?;
            return Ok(None);
        };

        tx.execute(
            r#"
            UPDATE outbox
            SET phase = ?2, attempts = ?3, ambiguous = ?4, failure_reason = ?5, not_before_ms = ?6
            WHERE client_id = ?1
            "#,
            params![
                updated.client_id,
                updated.phase.as_str(),
                i64::from(updated.attempts),
                i64::from(updated.ambiguous),
                updated.failure_reason,
                updated.not_before_ms,
            ],
        )?;
        tx.commit()?;
        Ok(Some(updated))
    }

    /// Holds a `ready` or `uncertain` attachment send as `"attachment unavailable"` (SHA-256/size guard).
    pub fn mark_outbox_attachment_unavailable(
        &mut self,
        client_id: &str,
    ) -> Result<bool, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            r#"
            UPDATE outbox
            SET phase = 'held', attachment_ready = 0, failure_reason = 'attachment unavailable', not_before_ms = 0
            WHERE client_id = ?1 AND phase IN ('ready', 'uncertain')
            "#,
            params![client_id],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }

    /// Startup crash recovery (`SendQueue.tla` `NoDouble` + `ReadyResolves`):
    /// Transitions any left-over `phase = 'uncertain'` rows into `phase = 'held', ambiguous = 1`
    /// so crashed sends NEVER auto-resend and never block other chats.
    pub fn recover_uncertain_on_startup(&mut self) -> Result<Vec<OutboxRecord>, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let select_sql = format!(
            "SELECT {OUTBOX_SELECT_COLS} FROM outbox WHERE phase = 'uncertain' ORDER BY created_at_ms ASC"
        );
        let mut recovered = {
            let mut stmt = tx.prepare(&select_sql)?;
            let rows = stmt.query_map([], map_outbox_row)?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        if !recovered.is_empty() {
            tx.execute(
                r#"
                UPDATE outbox
                SET phase = 'held',
                    ambiguous = 1,
                    not_before_ms = 0,
                    failure_reason = COALESCE(NULLIF(failure_reason, ''), 'delivery unconfirmed')
                WHERE phase = 'uncertain'
                "#,
                [],
            )?;
            for rec in &mut recovered {
                rec.phase = OutboxPhase::Held;
                rec.ambiguous = true;
                rec.not_before_ms = 0;
                if rec.failure_reason.as_deref().is_none_or(str::is_empty) {
                    rec.failure_reason = Some("delivery unconfirmed".to_owned());
                }
            }
        }
        tx.commit()?;
        Ok(recovered)
    }

    /// Explicit user retry (`SendQueue.tla` `UserRetry`): mints a fresh 32-hex `client_id`
    /// for a `held` row and resets attempts/ambiguity so it can be dispatched cleanly.
    pub fn user_retry_outbox_with_new_client_id(
        &mut self,
        old_client_id: &str,
        new_client_id: &str,
        now_ms: i64,
    ) -> Result<OutboxRecord, StoreError> {
        if !is_valid_client_id(new_client_id) || old_client_id == new_client_id {
            return Err(StoreError::InvalidOutboxRecord);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let select_sql = format!("SELECT {OUTBOX_SELECT_COLS} FROM outbox WHERE client_id = ?1");
        let Some(mut rec) = tx
            .query_row(&select_sql, params![old_client_id], map_outbox_row)
            .optional()?
        else {
            return Err(StoreError::OutboxNotFound);
        };

        match rec.phase {
            OutboxPhase::Uncertain => return Err(StoreError::OutboxInflight),
            OutboxPhase::Ready => return Err(StoreError::InvalidOutboxRecord),
            OutboxPhase::Held => {}
        }

        let ready_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM outbox WHERE phase = 'ready'",
            [],
            |r| r.get(0),
        )?;
        if ready_count >= MAX_PENDING_SENDS as i64 {
            return Err(StoreError::OutboxFull);
        }

        tx.execute(
            r#"
            UPDATE outbox
            SET client_id = ?2, phase = 'ready', attempts = 0, ambiguous = 0,
                failure_reason = NULL, not_before_ms = ?3
            WHERE client_id = ?1 AND phase = 'held'
            "#,
            params![old_client_id, new_client_id, now_ms],
        )?;
        tx.commit()?;

        rec.client_id = new_client_id.to_owned();
        rec.phase = OutboxPhase::Ready;
        rec.attempts = 0;
        rec.ambiguous = false;
        rec.failure_reason = None;
        rec.not_before_ms = now_ms;
        Ok(rec)
    }

    /// Discards a non-inflight (`ready` or `held`) outbox row. Refuses to delete `uncertain`.
    pub fn discard_outbox(&mut self, client_id: &str) -> Result<bool, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "DELETE FROM outbox WHERE client_id = ?1 AND phase != 'uncertain'",
            params![client_id],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }

    pub fn get_outbox(&self, client_id: &str) -> Result<Option<OutboxRecord>, StoreError> {
        let sql = format!("SELECT {OUTBOX_SELECT_COLS} FROM outbox WHERE client_id = ?1");
        self.conn
            .query_row(&sql, params![client_id], map_outbox_row)
            .optional()
            .map_err(StoreError::from)
    }

    pub fn list_outbox(&self) -> Result<Vec<OutboxRecord>, StoreError> {
        let sql = format!(
            "SELECT {OUTBOX_SELECT_COLS} FROM outbox ORDER BY created_at_ms ASC, rowid ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], map_outbox_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn list_outbox_for_chat(&self, chat: &ChatKey) -> Result<Vec<OutboxRecord>, StoreError> {
        let sql = format!(
            "SELECT {OUTBOX_SELECT_COLS} FROM outbox \
             WHERE provider = ?1 AND account_id = ?2 AND chat_id = ?3 \
             ORDER BY created_at_ms ASC, rowid ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![chat.provider.as_str(), chat.account, chat.id],
            map_outbox_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Read-only peek of the next ready outbox record (returns `None` while any `uncertain` row exists).
    pub fn next_ready_outbox(&self, now_ms: i64) -> Result<Option<OutboxRecord>, StoreError> {
        let inflight: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM outbox WHERE phase = 'uncertain'",
            [],
            |r| r.get(0),
        )?;
        if inflight > 0 {
            return Ok(None);
        }
        let sql = format!(
            "SELECT {OUTBOX_SELECT_COLS} FROM outbox \
             WHERE phase = 'ready' AND not_before_ms <= ?1 \
             ORDER BY created_at_ms ASC, rowid ASC LIMIT 1"
        );
        self.conn
            .query_row(&sql, params![now_ms], map_outbox_row)
            .optional()
            .map_err(StoreError::from)
    }

    /// Returns the earliest `not_before_ms` among `phase = 'ready'` rows for zero-polling sleep timers.
    pub fn next_outbox_ready_at_ms(&self) -> Result<Option<i64>, StoreError> {
        self.conn
            .query_row(
                "SELECT MIN(not_before_ms) FROM outbox WHERE phase = 'ready'",
                [],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(StoreError::from)
    }
}
