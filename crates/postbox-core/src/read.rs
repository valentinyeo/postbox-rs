use crate::{ChatKey, Message, Provider, ReadStatus};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const MAX_PENDING_READS: usize = 8;
pub const MAX_READ_RETRIES: u8 = 3;
pub const READ_RETRY_BACKOFF_MS: u64 = 10 * 60 * 1000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadWatermark {
    pub read_ts: i64,
    pub incoming_ts: i64,
    pub checked_ts: i64,
}

/// Monotonic read watermarks per `ChatKey` (`read_state.zig:4-137`).
#[derive(Debug, Default, Clone)]
pub struct ReadState {
    entries: HashMap<ChatKey, ReadWatermark>,
    pub dirty: bool,
}

impl ReadState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_map(entries: HashMap<ChatKey, ReadWatermark>) -> Self {
        Self {
            entries,
            dirty: false,
        }
    }

    pub fn entries(&self) -> &HashMap<ChatKey, ReadWatermark> {
        &self.entries
    }

    pub fn get(&self, key: &ChatKey) -> ReadWatermark {
        self.entries.get(key).copied().unwrap_or_default()
    }

    pub fn mark_read(&mut self, key: &ChatKey, ts_unix: i64) -> bool {
        if ts_unix <= 0 {
            return false;
        }
        let entry = self.entries.entry(key.clone()).or_default();
        if ts_unix <= entry.read_ts {
            return false;
        }
        entry.read_ts = ts_unix;
        self.dirty = true;
        true
    }

    pub fn observe_chat(
        &mut self,
        key: &ChatKey,
        ts_unix: i64,
        reported_unread: bool,
        fresh: bool,
    ) {
        if fresh && !reported_unread {
            self.mark_read(key, ts_unix);
        }
    }

    pub fn is_unread(&self, key: &ChatKey) -> bool {
        self.entries
            .get(key)
            .is_some_and(|e| e.incoming_ts > e.read_ts)
    }

    pub fn needs_probe(&self, key: &ChatKey, row_ts_unix: i64, reported_unread: bool) -> bool {
        if !reported_unread || row_ts_unix <= 0 {
            return false;
        }
        let entry = self.get(key);
        row_ts_unix > entry.read_ts && row_ts_unix > entry.checked_ts
    }

    pub fn accept_probe(
        &mut self,
        key: &ChatKey,
        row_ts_unix: i64,
        incoming_timestamps: impl IntoIterator<Item = (bool, i64)>,
    ) -> bool {
        let mut max_incoming = 0i64;
        for (from_me, ts_unix) in incoming_timestamps {
            if !from_me && ts_unix > max_incoming {
                max_incoming = ts_unix;
            }
        }
        let entry = self.entries.entry(key.clone()).or_default();
        let prev = *entry;
        if max_incoming > entry.incoming_ts {
            entry.incoming_ts = max_incoming;
        }
        if row_ts_unix > entry.checked_ts {
            entry.checked_ts = row_ts_unix;
        }
        let changed = *entry != prev;
        if changed {
            self.dirty = true;
        }
        changed
    }
}

/// UX gate: auto mark-as-read ONLY runs when `user_viewed == true` (never on boot or background refresh).
pub fn should_auto_mark_read(user_viewed: bool, unread: bool) -> bool {
    user_viewed && unread
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingReadEntry {
    pub chat: ChatKey,
    pub retries: u8,
    pub backoff_until_ms: u64,
}

/// Bounded pending mark-read retry queue (`pending_reads.zig:3-28`, `main.zig:6444-6462`).
#[derive(Debug, Default, Clone)]
pub struct PendingReadQueue {
    entries: Vec<PendingReadEntry>,
}

impl PendingReadQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_entries(entries: Vec<PendingReadEntry>) -> Self {
        Self {
            entries: entries.into_iter().take(MAX_PENDING_READS).collect(),
        }
    }

    pub fn entries(&self) -> &[PendingReadEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn enqueue(&mut self, chat: ChatKey) -> bool {
        if self.entries.iter().any(|e| e.chat == chat) {
            return true;
        }
        if self.entries.len() >= MAX_PENDING_READS {
            return false;
        }
        self.entries.push(PendingReadEntry {
            chat,
            retries: 0,
            backoff_until_ms: 0,
        });
        true
    }

    pub fn peek_ready(&self, now_ms: u64) -> Option<&PendingReadEntry> {
        self.entries
            .first()
            .filter(|head| head.backoff_until_ms <= now_ms)
    }

    pub fn complete_head(&mut self) -> Option<PendingReadEntry> {
        if self.entries.is_empty() {
            None
        } else {
            Some(self.entries.remove(0))
        }
    }

    pub fn requeue_failed_head(&mut self, now_ms: u64) -> Option<PendingReadEntry> {
        if self.entries.is_empty() {
            return None;
        }
        let mut head = self.entries.remove(0);
        if head.retries + 1 >= MAX_READ_RETRIES {
            head.retries = 0;
            head.backoff_until_ms = now_ms.saturating_add(READ_RETRY_BACKOFF_MS);
        } else {
            head.retries += 1;
        }
        let cloned = head.clone();
        self.entries.push(head);
        Some(cloned)
    }
}

/// Returns `true` when a stored `Message` represents a reaction-only row with no text or media.
fn is_reaction_only(msg: &Message) -> bool {
    msg.body.is_empty()
        && msg.media_kind.is_none()
        && msg.filename.as_deref().is_none_or(str::is_empty)
        && !msg.reactions.is_empty()
}

/// Selects the newest incoming non-reaction message ID for Signal/Telegram `mark-read` (`hub.zig:382-399`).
/// Expects chronological (oldest -> newest) `messages`.
pub fn select_read_message_id(messages: &[Message]) -> Option<&str> {
    messages
        .iter()
        .rev()
        .find(|m| !m.from_me && !m.msg_id.is_empty() && !m.deleted && !is_reaction_only(m))
        .map(|m| m.msg_id.as_str())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptCapability {
    WhatsappDm,
    WhatsappGroup,
    Slack,
    Telegram,
    TelegramDm,
    Signal,
    SignalDm,
}

impl ReceiptCapability {
    pub fn for_chat(provider: Provider, is_group: bool) -> Self {
        match (provider, is_group) {
            (Provider::Whatsapp, false) => Self::WhatsappDm,
            (Provider::Whatsapp, true) => Self::WhatsappGroup,
            (Provider::Slack, _) => Self::Slack,
            (Provider::Telegram, false) => Self::TelegramDm,
            (Provider::Telegram, true) => Self::Telegram,
            (Provider::Signal, false) => Self::SignalDm,
            (Provider::Signal, true) => Self::Signal,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptTick {
    None,
    Sent,
    Delivered,
    Read,
}

impl ReceiptTick {
    pub fn ticks(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Sent => 1,
            Self::Delivered | Self::Read => 2,
        }
    }

    pub fn to_read_status(self) -> ReadStatus {
        match self {
            Self::None => ReadStatus::None,
            Self::Sent => ReadStatus::Sent,
            Self::Delivered => ReadStatus::Delivered,
            Self::Read => ReadStatus::Read,
        }
    }
}

/// Exact port of `read_receipt.zig:3-48`.
pub fn compute_tick(
    cap: ReceiptCapability,
    from_me: bool,
    sent: bool,
    revoked: bool,
    delivered: i64,
    read_by: i64,
) -> ReceiptTick {
    if !from_me || !sent || revoked {
        return ReceiptTick::None;
    }
    match cap {
        ReceiptCapability::WhatsappDm
        | ReceiptCapability::TelegramDm
        | ReceiptCapability::SignalDm => {
            if read_by >= 1 {
                ReceiptTick::Read
            } else if delivered >= 1 {
                ReceiptTick::Delivered
            } else {
                ReceiptTick::Sent
            }
        }
        ReceiptCapability::WhatsappGroup => {
            if read_by >= 1 && read_by >= delivered {
                ReceiptTick::Read
            } else if delivered >= 1 {
                ReceiptTick::Delivered
            } else {
                ReceiptTick::Sent
            }
        }
        ReceiptCapability::Slack | ReceiptCapability::Telegram | ReceiptCapability::Signal => {
            ReceiptTick::None
        }
    }
}

/// Monotonic upgrade (`IGSH-1307`): once read, never regresses to unread.
pub fn upgrade_read_by(previous_read: bool, read_by: i64) -> i64 {
    if previous_read && read_by < 1 {
        1
    } else {
        read_by
    }
}

pub fn newest_own_sent_index(messages: &[Message]) -> Option<usize> {
    messages.iter().rposition(|m| {
        m.from_me && !matches!(m.read_status, ReadStatus::Sending | ReadStatus::Failed)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_state_watermarks_and_pending_queue_backoff() {
        let key = ChatKey::new(Provider::Signal, "signal", "signal:u1");
        let mut state = ReadState::new();
        assert!(state.needs_probe(&key, 100, true));
        assert!(state.accept_probe(&key, 100, [(true, 100), (false, 90)]));
        assert!(state.is_unread(&key));
        assert!(state.mark_read(&key, 95));
        assert!(!state.is_unread(&key));
        // Watermark never moves backward.
        assert!(!state.mark_read(&key, 80));

        let mut q = PendingReadQueue::new();
        for i in 0..MAX_PENDING_READS {
            assert!(q.enqueue(ChatKey::new(Provider::Whatsapp, "p", format!("c{i}"))));
        }
        assert!(!q.enqueue(ChatKey::new(Provider::Whatsapp, "p", "overflow")));

        let mut q2 = PendingReadQueue::new();
        q2.enqueue(key.clone());
        let r1 = q2.requeue_failed_head(1_000).unwrap();
        assert_eq!(r1.retries, 1);
        assert_eq!(r1.backoff_until_ms, 0);
        let r2 = q2.requeue_failed_head(2_000).unwrap();
        assert_eq!(r2.retries, 2);
        let r3 = q2.requeue_failed_head(3_000).unwrap();
        assert_eq!(r3.retries, 0);
        assert_eq!(r3.backoff_until_ms, 3_000 + READ_RETRY_BACKOFF_MS);
        assert!(q2.peek_ready(4_000).is_none());
        assert!(q2.peek_ready(3_000 + READ_RETRY_BACKOFF_MS).is_some());
    }

    #[test]
    fn read_receipt_ticks_and_monotonic_upgrade() {
        assert_eq!(
            compute_tick(ReceiptCapability::WhatsappDm, true, true, false, 0, 0),
            ReceiptTick::Sent
        );
        assert_eq!(
            compute_tick(ReceiptCapability::WhatsappDm, true, true, false, 1, 0),
            ReceiptTick::Delivered
        );
        assert_eq!(
            compute_tick(ReceiptCapability::WhatsappDm, true, true, false, 1, 1),
            ReceiptTick::Read
        );
        assert_eq!(
            compute_tick(ReceiptCapability::WhatsappGroup, true, true, false, 3, 2),
            ReceiptTick::Delivered
        );
        assert_eq!(
            compute_tick(ReceiptCapability::WhatsappGroup, true, true, false, 3, 3),
            ReceiptTick::Read
        );
        assert_eq!(
            compute_tick(ReceiptCapability::Slack, true, true, false, 1, 1),
            ReceiptTick::None
        );
        assert_eq!(upgrade_read_by(true, 0), 1);
        assert_eq!(upgrade_read_by(false, 0), 0);
    }

    #[test]
    fn select_read_message_id_skips_reaction_only_and_own_messages() {
        let chat = ChatKey::new(Provider::Signal, "signal", "signal:u1");
        let base = |id: &str, from_me: bool, body: &str| Message {
            chat: chat.clone(),
            msg_id: id.into(),
            sender_id: "u1".into(),
            sender_name: "Alice".into(),
            from_me,
            ts: "2026-11-26T10:00:00Z".into(),
            body: body.into(),
            reply_to_id: None,
            reply_broadcast: false,
            media_kind: None,
            mime_type: None,
            filename: None,
            reactions: Vec::new(),
            read_status: ReadStatus::None,
            deleted: false,
        };
        let mut reaction_only = base("m_react", false, "");
        reaction_only.reactions.push(crate::Reaction {
            emoji: "👍".into(),
            count: 1,
            from_me: false,
        });
        let mut deleted = base("m_del", false, "gone");
        deleted.deleted = true;
        let msgs = vec![
            base("m_real", false, "hello"),
            base("m_own", true, "reply"),
            deleted,
            reaction_only,
        ];
        assert_eq!(select_read_message_id(&msgs), Some("m_real"));
        // If only reaction-only and own messages exist, returns None.
        assert_eq!(select_read_message_id(&msgs[1..]), None);
    }
}
