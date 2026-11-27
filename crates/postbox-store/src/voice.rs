use crate::{Store, StoreError, gone_media_pair_key};
use postbox_core::ChatKey;
use postbox_core::dictation::DictationLanguage;
use postbox_core::voice::{
    PLAYED_VOICE_CAP, PlaybackSpeed, TranscriptRecord, VOICE_POS_CAP, VOICE_POS_END_CLEAR_MS,
};
use rusqlite::{OptionalExtension, params};
use std::collections::{HashMap, HashSet};

pub const SETTING_PLAYBACK_SPEED: &str = "audio.playback_speed";
pub const SETTING_MICROPHONE: &str = "audio.microphone";
pub const SETTING_DICTATION_LANGUAGE: &str = "audio.dictation_language";

impl Store {
    /// Marks a voice message as played, maintaining a 2048-entry FIFO ring cap.
    pub fn mark_voice_played(
        &mut self,
        chat: &ChatKey,
        msg_id: &str,
        now_unix: i64,
    ) -> Result<(), StoreError> {
        if msg_id.is_empty() {
            return Ok(());
        }
        let pair_key = gone_media_pair_key(chat, msg_id);
        let tx = self.conn.transaction()?;
        let next_seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM played_voice",
            [],
            |r| r.get(0),
        )?;
        tx.execute(
            r#"
            INSERT INTO played_voice (pair_key, played_at, seq)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(pair_key) DO UPDATE SET
                played_at = excluded.played_at,
                seq = excluded.seq
            "#,
            params![pair_key, now_unix, next_seq],
        )?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM played_voice", [], |r| r.get(0))?;
        if count as usize > PLAYED_VOICE_CAP {
            let excess = count - PLAYED_VOICE_CAP as i64;
            tx.execute(
                r#"
                DELETE FROM played_voice
                WHERE pair_key IN (
                    SELECT pair_key FROM played_voice ORDER BY seq ASC LIMIT ?1
                )
                "#,
                params![excess],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn is_voice_played(&self, chat: &ChatKey, msg_id: &str) -> Result<bool, StoreError> {
        if msg_id.is_empty() {
            return Ok(false);
        }
        let pair_key = gone_media_pair_key(chat, msg_id);
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM played_voice WHERE pair_key = ?1",
                params![pair_key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    pub fn list_played_voice_keys(&self) -> Result<HashSet<String>, StoreError> {
        let mut stmt = self.conn.prepare("SELECT pair_key FROM played_voice")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<HashSet<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Saves a resume position for a voice message, enforcing a 64-entry FIFO cap.
    /// Automatically deletes the entry when `pos_ms == 0` OR within `500 ms` of `dur_ms`.
    pub fn save_voice_position(
        &mut self,
        chat: &ChatKey,
        msg_id: &str,
        pos_ms: u64,
        dur_ms: u64,
        now_unix: i64,
    ) -> Result<(), StoreError> {
        if msg_id.is_empty() {
            return Ok(());
        }
        if pos_ms == 0 || (dur_ms > 0 && dur_ms.saturating_sub(pos_ms) <= VOICE_POS_END_CLEAR_MS) {
            return self.clear_voice_position(chat, msg_id);
        }
        let pair_key = gone_media_pair_key(chat, msg_id);
        let tx = self.conn.transaction()?;
        let next_seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM voice_positions",
            [],
            |r| r.get(0),
        )?;
        tx.execute(
            r#"
            INSERT INTO voice_positions (pair_key, pos_ms, dur_ms, updated_at, seq)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(pair_key) DO UPDATE SET
                pos_ms = excluded.pos_ms,
                dur_ms = excluded.dur_ms,
                updated_at = excluded.updated_at,
                seq = excluded.seq
            "#,
            params![pair_key, pos_ms as i64, dur_ms as i64, now_unix, next_seq],
        )?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM voice_positions", [], |r| r.get(0))?;
        if count as usize > VOICE_POS_CAP {
            let excess = count - VOICE_POS_CAP as i64;
            tx.execute(
                r#"
                DELETE FROM voice_positions
                WHERE pair_key IN (
                    SELECT pair_key FROM voice_positions ORDER BY seq ASC LIMIT ?1
                )
                "#,
                params![excess],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_voice_position(
        &self,
        chat: &ChatKey,
        msg_id: &str,
    ) -> Result<Option<(u64, u64)>, StoreError> {
        if msg_id.is_empty() {
            return Ok(None);
        }
        let pair_key = gone_media_pair_key(chat, msg_id);
        self.conn
            .query_row(
                "SELECT pos_ms, dur_ms FROM voice_positions WHERE pair_key = ?1",
                params![pair_key],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?.max(0) as u64,
                        r.get::<_, i64>(1)?.max(0) as u64,
                    ))
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn clear_voice_position(&self, chat: &ChatKey, msg_id: &str) -> Result<(), StoreError> {
        if msg_id.is_empty() {
            return Ok(());
        }
        let pair_key = gone_media_pair_key(chat, msg_id);
        self.conn.execute(
            "DELETE FROM voice_positions WHERE pair_key = ?1",
            params![pair_key],
        )?;
        Ok(())
    }

    pub fn load_all_voice_positions(&self) -> Result<HashMap<String, (u64, u64)>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT pair_key, pos_ms, dur_ms FROM voice_positions")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (
                    r.get::<_, i64>(1)?.max(0) as u64,
                    r.get::<_, i64>(2)?.max(0) as u64,
                ),
            ))
        })?;
        rows.collect::<Result<HashMap<_, _>, _>>()
            .map_err(StoreError::from)
    }

    /// Inserts an immutable raw transcript keyed by `media_sha256`.
    /// Returns `true` if inserted, `false` if already cached (existing raw text is NEVER modified).
    pub fn cache_raw_transcript(
        &self,
        media_sha256: &str,
        language: &str,
        raw_text: &str,
        now_unix: i64,
    ) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            r#"
            INSERT INTO transcripts (media_sha256, language, text, formatted_text, created_at, updated_at)
            VALUES (?1, ?2, ?3, NULL, ?4, ?4)
            ON CONFLICT(media_sha256) DO NOTHING
            "#,
            params![media_sha256, language, raw_text, now_unix],
        )?;
        Ok(changed > 0)
    }

    /// Updates ONLY `formatted_text` and `updated_at` on an existing transcript row.
    /// Immutable raw `text` is NEVER touched.
    pub fn save_formatted_transcript(
        &self,
        media_sha256: &str,
        formatted_text: &str,
        now_unix: i64,
    ) -> Result<bool, StoreError> {
        let changed = self.conn.execute(
            r#"
            UPDATE transcripts
            SET formatted_text = ?2, updated_at = ?3
            WHERE media_sha256 = ?1
            "#,
            params![media_sha256, formatted_text, now_unix],
        )?;
        Ok(changed > 0)
    }

    pub fn get_transcript_record(
        &self,
        media_sha256: &str,
    ) -> Result<Option<TranscriptRecord>, StoreError> {
        self.conn
            .query_row(
                r#"
                SELECT media_sha256, language, text, formatted_text,
                       CASE WHEN updated_at > 0 THEN updated_at ELSE created_at END
                FROM transcripts
                WHERE media_sha256 = ?1
                "#,
                params![media_sha256],
                |r| {
                    Ok(TranscriptRecord {
                        media_sha256: r.get(0)?,
                        language: r.get(1)?,
                        raw_text: r.get(2)?,
                        formatted_text: r.get(3)?,
                        updated_at: r.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn load_playback_speed(&self) -> Result<PlaybackSpeed, StoreError> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![SETTING_PLAYBACK_SPEED],
                |r| r.get(0),
            )
            .optional()?;
        Ok(raw
            .and_then(|s| s.parse::<u16>().ok())
            .map(PlaybackSpeed::from_percent)
            .unwrap_or(PlaybackSpeed::X1))
    }

    pub fn save_playback_speed(&self, speed: PlaybackSpeed) -> Result<(), StoreError> {
        self.conn.execute(
            r#"
            INSERT INTO settings (key, value) VALUES (?1, ?2)
            ON CONFLICT(key) DO UPDATE SET value = excluded.value
            "#,
            params![SETTING_PLAYBACK_SPEED, speed.percent().to_string()],
        )?;
        Ok(())
    }

    pub fn load_microphone_preference(&self) -> Result<Option<String>, StoreError> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![SETTING_MICROPHONE],
                |r| r.get(0),
            )
            .optional()?;
        Ok(raw.and_then(|s| {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        }))
    }

    pub fn save_microphone_preference(&self, name: Option<&str>) -> Result<(), StoreError> {
        let value = name.map(str::trim).unwrap_or("");
        self.conn.execute(
            r#"
            INSERT INTO settings (key, value) VALUES (?1, ?2)
            ON CONFLICT(key) DO UPDATE SET value = excluded.value
            "#,
            params![SETTING_MICROPHONE, value],
        )?;
        Ok(())
    }

    pub fn load_dictation_language(&self) -> Result<DictationLanguage, StoreError> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![SETTING_DICTATION_LANGUAGE],
                |r| r.get(0),
            )
            .optional()?;
        Ok(raw
            .as_deref()
            .and_then(DictationLanguage::parse)
            .unwrap_or(DictationLanguage::Automatic))
    }

    pub fn save_dictation_language(&self, lang: DictationLanguage) -> Result<(), StoreError> {
        self.conn.execute(
            r#"
            INSERT INTO settings (key, value) VALUES (?1, ?2)
            ON CONFLICT(key) DO UPDATE SET value = excluded.value
            "#,
            params![SETTING_DICTATION_LANGUAGE, lang.as_str()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::Provider;

    #[test]
    fn played_voice_enforces_2048_fifo_cap() {
        let mut store = Store::open_in_memory().unwrap();
        let chat = ChatKey::new(Provider::Whatsapp, "main", "1@s.whatsapp.net");
        for i in 0..2060 {
            store
                .mark_voice_played(&chat, &format!("msg-{i}"), 1_000 + i as i64)
                .unwrap();
        }
        let keys = store.list_played_voice_keys().unwrap();
        assert_eq!(keys.len(), PLAYED_VOICE_CAP);
        assert!(!store.is_voice_played(&chat, "msg-0").unwrap());
        assert!(!store.is_voice_played(&chat, "msg-11").unwrap());
        assert!(store.is_voice_played(&chat, "msg-12").unwrap());
        assert!(store.is_voice_played(&chat, "msg-2059").unwrap());
    }

    #[test]
    fn voice_positions_enforces_64_cap_and_500ms_end_clear() {
        let mut store = Store::open_in_memory().unwrap();
        let chat = ChatKey::new(Provider::Whatsapp, "main", "1@s.whatsapp.net");
        for i in 0..70 {
            store
                .save_voice_position(&chat, &format!("v-{i}"), 2_000, 10_000, 1_000 + i as i64)
                .unwrap();
        }
        let all = store.load_all_voice_positions().unwrap();
        assert_eq!(all.len(), VOICE_POS_CAP);
        assert!(store.get_voice_position(&chat, "v-0").unwrap().is_none());
        assert!(store.get_voice_position(&chat, "v-5").unwrap().is_none());
        assert_eq!(
            store.get_voice_position(&chat, "v-6").unwrap(),
            Some((2_000, 10_000))
        );

        // Within 500 ms of end -> auto-clears entry.
        store
            .save_voice_position(&chat, "v-69", 9_500, 10_000, 2_000)
            .unwrap();
        assert!(store.get_voice_position(&chat, "v-69").unwrap().is_none());

        // Zero position -> auto-clears entry.
        store
            .save_voice_position(&chat, "v-68", 0, 10_000, 2_001)
            .unwrap();
        assert!(store.get_voice_position(&chat, "v-68").unwrap().is_none());
    }

    #[test]
    fn transcripts_raw_is_immutable_and_formatted_updates_independently() {
        let store = Store::open_in_memory().unwrap();
        assert!(
            store
                .cache_raw_transcript("sha-abc", "en", "Original raw text.", 100)
                .unwrap()
        );
        // Second insert is a no-op and never overwrites raw text.
        assert!(
            !store
                .cache_raw_transcript("sha-abc", "de", "Overwritten attempt.", 200)
                .unwrap()
        );

        assert!(
            store
                .save_formatted_transcript("sha-abc", "1. Key point\n- Original raw text.", 300)
                .unwrap()
        );

        let rec = store.get_transcript_record("sha-abc").unwrap().unwrap();
        assert_eq!(rec.language, "en");
        assert_eq!(rec.raw_text, "Original raw text.");
        assert_eq!(
            rec.formatted_text.as_deref(),
            Some("1. Key point\n- Original raw text.")
        );
        assert_eq!(rec.updated_at, 300);
        // Backward-compatible get_transcript still returns raw text.
        assert_eq!(
            store.get_transcript("sha-abc").unwrap().as_deref(),
            Some("Original raw text.")
        );
    }

    #[test]
    fn transcripts_forward_migration_preserves_legacy_rows() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE transcripts (
                media_sha256 TEXT PRIMARY KEY,
                language TEXT NOT NULL,
                text TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            INSERT INTO transcripts (media_sha256, language, text, created_at)
            VALUES ('legacy-sha', 'de', 'Hallo Welt', 42);
            "#,
        )
        .unwrap();
        let store = Store::init(conn).unwrap();
        let rec = store.get_transcript_record("legacy-sha").unwrap().unwrap();
        assert_eq!(rec.raw_text, "Hallo Welt");
        assert_eq!(rec.formatted_text, None);
        assert_eq!(rec.updated_at, 42);

        store
            .save_formatted_transcript("legacy-sha", "1. Begrüßung", 99)
            .unwrap();
        let updated = store.get_transcript_record("legacy-sha").unwrap().unwrap();
        assert_eq!(updated.raw_text, "Hallo Welt");
        assert_eq!(updated.formatted_text.as_deref(), Some("1. Begrüßung"));
        assert_eq!(updated.updated_at, 99);
    }

    #[test]
    fn audio_settings_round_trip() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.load_playback_speed().unwrap(), PlaybackSpeed::X1);
        store.save_playback_speed(PlaybackSpeed::X1_5).unwrap();
        assert_eq!(store.load_playback_speed().unwrap(), PlaybackSpeed::X1_5);

        assert_eq!(store.load_microphone_preference().unwrap(), None);
        store
            .save_microphone_preference(Some("USB Studio Mic"))
            .unwrap();
        assert_eq!(
            store.load_microphone_preference().unwrap().as_deref(),
            Some("USB Studio Mic")
        );
        store.save_microphone_preference(None).unwrap();
        assert_eq!(store.load_microphone_preference().unwrap(), None);

        assert_eq!(
            store.load_dictation_language().unwrap(),
            DictationLanguage::Automatic
        );
        store
            .save_dictation_language(DictationLanguage::German)
            .unwrap();
        assert_eq!(
            store.load_dictation_language().unwrap(),
            DictationLanguage::German
        );
        store
            .save_dictation_language(DictationLanguage::English)
            .unwrap();
        assert_eq!(
            store.load_dictation_language().unwrap(),
            DictationLanguage::English
        );
    }
}
