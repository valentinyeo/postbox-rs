use crate::{ChatKey, Draft};
use thiserror::Error;

pub const MAX_DRAFTS: usize = 500;
pub const MAX_DRAFT_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_DRAFT_ACCOUNT_LEN: usize = 32;
pub const MAX_DRAFT_JID_LEN: usize = 191;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DraftError {
    #[error("invalid_identity")]
    InvalidIdentity,
    #[error("draft_too_long")]
    DraftTooLong,
    #[error("contains_nul")]
    ContainsNul,
    #[error("too_many_drafts")]
    TooManyDrafts,
}

pub fn validate_draft_key(key: &ChatKey) -> Result<(), DraftError> {
    if key.account.is_empty()
        || key.account.len() > MAX_DRAFT_ACCOUNT_LEN
        || key.id.is_empty()
        || key.id.len() > MAX_DRAFT_JID_LEN
        || key.account.as_bytes().contains(&0)
        || key.id.as_bytes().contains(&0)
    {
        return Err(DraftError::InvalidIdentity);
    }
    Ok(())
}

pub fn validate_draft_text(text: &str) -> Result<(), DraftError> {
    if text.len() > MAX_DRAFT_TEXT_BYTES {
        return Err(DraftError::DraftTooLong);
    }
    if text.as_bytes().contains(&0) {
        return Err(DraftError::ContainsNul);
    }
    Ok(())
}

/// Origin-scoped draft store (`drafts.zig:5-145`):
/// - Cap 500 entries, 64 KiB text max, no NUL bytes
/// - Preserves whitespace-only drafts (`IGSH-497`)
/// - Origin-scoped `append_to_origin` so async dictation/transcripts never leak across chats
#[derive(Debug, Default, Clone)]
pub struct DraftStore {
    entries: Vec<Draft>,
    open: Option<ChatKey>,
    pub dirty: bool,
}

impl DraftStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_entries(entries: Vec<Draft>) -> Self {
        Self {
            entries: entries.into_iter().take(MAX_DRAFTS).collect(),
            open: None,
            dirty: false,
        }
    }

    pub fn entries(&self) -> &[Draft] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn open_chat(&self) -> Option<&ChatKey> {
        self.open.as_ref()
    }

    pub fn is_open(&self, key: &ChatKey) -> bool {
        self.open.as_ref() == Some(key)
    }

    pub fn get(&self, key: &ChatKey) -> &str {
        self.entries
            .iter()
            .find(|d| &d.chat == key)
            .map(|d| d.text_body.as_str())
            .unwrap_or("")
    }

    pub fn get_draft(&self, key: &ChatKey) -> Option<&Draft> {
        self.entries.iter().find(|d| &d.chat == key)
    }

    pub fn save(
        &mut self,
        key: &ChatKey,
        text: &str,
        reply_to_id: Option<String>,
        staged_image_path: Option<String>,
        now_unix: i64,
    ) -> Result<(), DraftError> {
        validate_draft_key(key)?;
        validate_draft_text(text)?;

        // IGSH-497: preserve whitespace-only drafts; only delete when text is truly empty and no attachment.
        if text.is_empty() && staged_image_path.is_none() {
            if let Some(pos) = self.entries.iter().position(|d| &d.chat == key) {
                self.entries.remove(pos);
                self.dirty = true;
            }
            return Ok(());
        }

        if let Some(existing) = self.entries.iter_mut().find(|d| &d.chat == key) {
            if existing.text_body == text
                && existing.reply_to_id == reply_to_id
                && existing.staged_image_path == staged_image_path
            {
                return Ok(());
            }
            existing.text_body = text.to_owned();
            existing.reply_to_id = reply_to_id;
            existing.staged_image_path = staged_image_path;
            existing.updated_at = now_unix;
            self.dirty = true;
            return Ok(());
        }

        if self.entries.len() >= MAX_DRAFTS {
            return Err(DraftError::TooManyDrafts);
        }

        self.entries.push(Draft {
            chat: key.clone(),
            text_body: text.to_owned(),
            reply_to_id,
            staged_image_path,
            updated_at: now_unix,
        });
        self.dirty = true;
        Ok(())
    }

    pub fn switch_to(
        &mut self,
        next: Option<ChatKey>,
        current_composer: &str,
        now_unix: i64,
    ) -> Result<String, DraftError> {
        if let Some(ref key) = next {
            validate_draft_key(key)?;
        }
        if let Some(prev) = self.open.clone() {
            let reply_to = self.get_draft(&prev).and_then(|d| d.reply_to_id.clone());
            let staged = self
                .get_draft(&prev)
                .and_then(|d| d.staged_image_path.clone());
            self.save(&prev, current_composer, reply_to, staged, now_unix)?;
        }
        self.open = next.clone();
        Ok(next.map(|k| self.get(&k).to_owned()).unwrap_or_default())
    }

    /// Appends async text (e.g., dictation or transcript) strictly to `origin` (`drafts.zig:89-100`).
    /// Returns `Ok(Some(combined))` if `origin` is currently open (caller updates live composer),
    /// or `Ok(None)` if user switched away (saved to `origin` without leaking into open chat).
    pub fn append_to_origin(
        &mut self,
        origin: &ChatKey,
        active_composer: &str,
        text: &str,
        separate_with_space: bool,
        now_unix: i64,
    ) -> Result<Option<String>, DraftError> {
        validate_draft_key(origin)?;
        let is_open = self.is_open(origin);
        let previous = if is_open {
            active_composer
        } else {
            self.get(origin)
        };
        let combined = if separate_with_space && !previous.is_empty() && !text.is_empty() {
            format!("{previous} {text}")
        } else {
            format!("{previous}{text}")
        };
        let reply_to = self.get_draft(origin).and_then(|d| d.reply_to_id.clone());
        let staged = self
            .get_draft(origin)
            .and_then(|d| d.staged_image_path.clone());
        self.save(origin, &combined, reply_to, staged, now_unix)?;
        if is_open {
            Ok(Some(combined))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Provider;

    #[test]
    fn drafts_preserve_whitespace_and_enforce_origin_and_limits() {
        let mut store = DraftStore::new();
        let chat_a = ChatKey::new(Provider::Whatsapp, "p", "a@s.whatsapp.net");
        let chat_b = ChatKey::new(Provider::Whatsapp, "p", "b@s.whatsapp.net");

        // Preserves whitespace-only draft (IGSH-497).
        store.save(&chat_a, "  \t\n", None, None, 1).unwrap();
        assert_eq!(store.get(&chat_a), "  \t\n");

        // Origin-scoped append while switched away from chat_a to chat_b.
        store.switch_to(Some(chat_a.clone()), "", 2).unwrap();
        let loaded_b = store.switch_to(Some(chat_b.clone()), "hello", 3).unwrap();
        assert_eq!(loaded_b, "");
        assert_eq!(store.get(&chat_a), "hello");

        let live_update = store
            .append_to_origin(&chat_a, "typing in B", "world", true, 4)
            .unwrap();
        assert_eq!(live_update, None);
        assert_eq!(store.get(&chat_a), "hello world");
        assert_eq!(store.get(&chat_b), "");

        // NUL and >64KiB rejected.
        assert_eq!(
            store.save(&chat_b, "bad\0nul", None, None, 5),
            Err(DraftError::ContainsNul)
        );
        let huge = "a".repeat(MAX_DRAFT_TEXT_BYTES + 1);
        assert_eq!(
            store.save(&chat_b, &huge, None, None, 6),
            Err(DraftError::DraftTooLong)
        );

        // 500 draft cap enforced.
        let mut full = DraftStore::new();
        for i in 0..MAX_DRAFTS {
            let k = ChatKey::new(Provider::Slack, "T1", format!("C{i}"));
            full.save(&k, "x", None, None, 1).unwrap();
        }
        let overflow = ChatKey::new(Provider::Slack, "T1", "C500");
        assert_eq!(
            full.save(&overflow, "x", None, None, 1),
            Err(DraftError::TooManyDrafts)
        );
    }
}
