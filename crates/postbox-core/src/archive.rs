use crate::{ChatKey, Provider};
use serde::{Deserialize, Serialize};

pub const MAX_ARCHIVE_ENTRIES: usize = 2048;
pub const MAX_JID_LEN: usize = 191;
pub const MAX_ACCOUNT_LEN: usize = 32;
pub const MAX_TS_LEN: usize = 47;
pub const LEGACY_LOCAL_BASELINE: &str = "2026-11-03T00:00:00Z";

/// Local WhatsApp archive resurface check (`IGSH-902`):
/// Only real incoming messages from another participant resurface an archived chat.
/// Own messages (`from_me == true`) and reaction-only events must NEVER resurface.
pub fn should_resurface_on_event(
    archived_local: bool,
    from_me: bool,
    is_reaction_only: bool,
) -> bool {
    archived_local && !from_me && !is_reaction_only
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Howard Hinnant civil-from-days / days-from-civil algorithm (`media_age.zig:3-46`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m = u64::from(month);
    let d = u64::from(day);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(m <= 2);
    (year, m as u32, d as u32)
}

/// Parses an RFC3339-like UTC timestamp prefix (`YYYY-MM-DD[T ]HH:MM:SS...`) into Unix seconds.
pub fn unix_seconds(ts: &str) -> Option<i64> {
    let bytes = ts.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't' | b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year: i64 = ts.get(0..4)?.parse().ok()?;
    let month: u32 = ts.get(5..7)?.parse().ok()?;
    let day: u32 = ts.get(8..10)?.parse().ok()?;
    let hour: u32 = ts.get(11..13)?.parse().ok()?;
    let minute: u32 = ts.get(14..16)?.parse().ok()?;
    let second: u32 = ts.get(17..19)?.parse().ok()?;

    if !(1..=12).contains(&month) {
        return None;
    }
    let max_day = days_in_month(year, month);
    if day == 0 || day > max_day || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second))
}

/// Formats Unix seconds as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn format_utc_seconds(unix_secs: i64) -> String {
    let secs = unix_secs.max(0);
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Exact port of `archive_rules.zig:437-444`: compares two timestamps normalizing `' '` at byte 10 to `'T'`.
pub fn later_stamp(left: &str, right: &str) -> bool {
    let lb = left.as_bytes();
    let rb = right.as_bytes();
    let n = lb.len().min(rb.len());
    for i in 0..n {
        let l = if i == 10 && lb[i] == b' ' {
            b'T'
        } else {
            lb[i]
        };
        let r = if i == 10 && rb[i] == b' ' {
            b'T'
        } else {
            rb[i]
        };
        if l != r {
            return l > r;
        }
    }
    lb.len() > rb.len()
}

/// Exact port of `archive_rules.zig:421-435`: returns `max(row_ts, format_utc(now_seconds))`.
pub fn archive_baseline(row_ts: &str, now_seconds: i64) -> String {
    let now_ts = format_utc_seconds(now_seconds.max(0));
    if !row_ts.is_empty() && later_stamp(row_ts, &now_ts) {
        truncate_ts(row_ts)
    } else {
        now_ts
    }
}

fn truncate_ts(ts: &str) -> String {
    if ts.len() <= MAX_TS_LEN {
        ts.to_owned()
    } else {
        let mut end = MAX_TS_LEN;
        while end > 0 && !ts.is_char_boundary(end) {
            end -= 1;
        }
        ts[..end].to_owned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArchiveStatus {
    Incoming,
    Archived,
    KeptArchived,
}

impl ArchiveStatus {
    pub fn from_flags(archived: bool, kept: bool) -> Self {
        if !archived {
            Self::Incoming
        } else if kept {
            Self::KeptArchived
        } else {
            Self::Archived
        }
    }

    pub fn label(self) -> Option<&'static str> {
        match self {
            Self::Incoming => None,
            Self::Archived => Some("Archived"),
            Self::KeptArchived => Some("Kept archived"),
        }
    }

    pub fn is_archived(self) -> bool {
        self != Self::Incoming
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveAction {
    Archive,
    Keep,
    Unarchive,
}

impl ArchiveAction {
    pub fn status_message(self) -> &'static str {
        match self {
            Self::Archive => "Archived",
            Self::Keep => "Kept archived. Won't come back",
            Self::Unarchive => "Chat unarchived",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveSource {
    Shortcut,
    Menu,
    Control,
    Send,
    Kept,
    Incoming,
    Migration,
}

/// Exact port of `messenger_view.zig:337-341`.
/// Combined Inbox + `Alt+A` (`inbox && include_archived && !archived_only`) restricts to UNREAD chats only!
pub fn archive_matches(
    archived_only: bool,
    include_archived: bool,
    inbox: bool,
    unread_only: bool,
    status: ArchiveStatus,
    unread: bool,
) -> bool {
    let archived = status != ArchiveStatus::Incoming;
    let needs_unread = unread_only || (inbox && include_archived && !archived_only);
    (!needs_unread || unread)
        && if archived_only {
            archived
        } else {
            include_archived || !archived
        }
}

/// Exact port of `archive_rules.zig:38-41`.
pub fn pending_hides_chat(show_archived: bool, should_unarchive: bool) -> bool {
    let pending_status = ArchiveStatus::from_flags(!should_unarchive, false);
    (pending_status != ArchiveStatus::Incoming) != show_archived
}

/// Exact port of `archive_rules.zig:43-50`.
pub fn progress_status(should_unarchive: bool, total: usize, remaining: usize) -> Option<String> {
    if total <= 1 || remaining == 0 || remaining > total {
        return None;
    }
    let verb = if should_unarchive {
        "Unarchiving"
    } else {
        "Archiving"
    };
    Some(format!("{verb} {} of {total}", total - remaining + 1))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveEntry {
    pub key: ChatKey,
    pub baseline_ts: String,
    pub keep: bool,
    pub checked_unix: i64,
    pub returned: bool,
    pub phone: bool,
    #[serde(skip)]
    pub held: bool,
    #[serde(skip)]
    pub held_ts: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncomingProbeMessage<'a> {
    pub from_me: bool,
    pub is_reaction: bool,
    pub ts: &'a str,
}

#[derive(Debug, Clone, Default)]
pub struct ArchiveTable {
    entries: Vec<ArchiveEntry>,
}

impl ArchiveTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_entries(entries: Vec<ArchiveEntry>) -> Self {
        Self {
            entries: entries.into_iter().take(MAX_ARCHIVE_ENTRIES).collect(),
        }
    }

    pub fn entries(&self) -> &[ArchiveEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn find_idx(&self, key: &ChatKey) -> Option<usize> {
        self.entries.iter().position(|e| &e.key == key)
    }

    pub fn get(&self, key: &ChatKey) -> Option<&ArchiveEntry> {
        self.find_idx(key).map(|i| &self.entries[i])
    }

    pub fn is_kept(&self, key: &ChatKey) -> bool {
        self.get(key).is_some_and(|e| e.keep)
    }

    pub fn is_returned(&self, key: &ChatKey) -> bool {
        self.get(key).is_some_and(|e| e.returned)
    }

    pub fn is_phone(&self, key: &ChatKey) -> bool {
        self.get(key).is_some_and(|e| e.phone)
    }

    pub fn baseline(&self, key: &ChatKey) -> Option<&str> {
        self.get(key)
            .filter(|e| !e.baseline_ts.is_empty())
            .map(|e| e.baseline_ts.as_str())
    }

    /// Exact port of `archive_rules.zig:107-109`.
    pub fn local_archived(&self, key: &ChatKey) -> bool {
        let Some(entry) = self.get(key) else {
            return false;
        };
        if key.provider == Provider::Whatsapp {
            !entry.returned
        } else {
            entry.keep || !entry.baseline_ts.is_empty()
        }
    }

    /// Exact port of `archive_rules.zig:111-120`.
    pub fn is_archived(&self, key: &ChatKey, provider_archived: bool) -> bool {
        let Some(entry) = self.get(key) else {
            return provider_archived;
        };
        if key.provider == Provider::Whatsapp {
            entry.keep || (!entry.returned && (provider_archived || !entry.phone))
        } else {
            entry.keep || !entry.baseline_ts.is_empty()
        }
    }

    pub fn status(&self, key: &ChatKey, provider_archived: bool) -> ArchiveStatus {
        ArchiveStatus::from_flags(self.is_archived(key, provider_archived), self.is_kept(key))
    }

    /// `IGSH-920` (`archive_rules.zig:253-261`): reclaims ONLY the first WhatsApp phone-observed
    /// non-kept non-returned entry. Never reclaims user-archived, kept, returned, or local entries.
    pub fn reclaim_phone(&mut self) -> bool {
        if let Some(idx) = self
            .entries
            .iter()
            .position(|e| e.key.provider == Provider::Whatsapp && e.phone && !e.keep && !e.returned)
        {
            self.entries.remove(idx);
            true
        } else {
            false
        }
    }

    fn valid_key(key: &ChatKey) -> bool {
        !key.id.is_empty() && key.id.len() <= MAX_JID_LEN && key.account.len() <= MAX_ACCOUNT_LEN
    }

    fn slot_reclaiming(&mut self, key: &ChatKey) -> Option<usize> {
        if !Self::valid_key(key) {
            return None;
        }
        if let Some(idx) = self.find_idx(key) {
            return Some(idx);
        }
        if self.entries.len() >= MAX_ARCHIVE_ENTRIES && !self.reclaim_phone() {
            return None;
        }
        self.entries.push(ArchiveEntry {
            key: key.clone(),
            baseline_ts: String::new(),
            keep: false,
            checked_unix: 0,
            returned: false,
            phone: false,
            held: false,
            held_ts: String::new(),
        });
        Some(self.entries.len() - 1)
    }

    /// Archives a chat (`archive_rules.zig:326-333`).
    pub fn archive(&mut self, key: &ChatKey, ts: &str, keep: bool) -> bool {
        if key.provider != Provider::Whatsapp {
            return self.set_local_archived(key, true, ts, keep);
        }
        let Some(idx) = self.slot_reclaiming(key) else {
            return false;
        };
        let entry = &mut self.entries[idx];
        entry.phone = false;
        entry.keep = keep;
        entry.returned = false;
        entry.held = false;
        entry.held_ts.clear();
        if !ts.is_empty() {
            entry.baseline_ts = truncate_ts(ts);
        }
        true
    }

    /// Local archive management for Slack/Telegram/Signal (`archive_rules.zig:137-164`).
    pub fn set_local_archived(
        &mut self,
        key: &ChatKey,
        archived: bool,
        ts: &str,
        keep: bool,
    ) -> bool {
        if !Self::valid_key(key) {
            return false;
        }
        if !archived {
            return self.remove(key);
        }
        if let Some(idx) = self.find_idx(key) {
            let entry = &mut self.entries[idx];
            entry.keep = keep;
            entry.returned = false;
            entry.phone = false;
            entry.baseline_ts = truncate_ts(ts);
            return true;
        }
        if self.entries.len() >= MAX_ARCHIVE_ENTRIES && !self.reclaim_phone() {
            return false;
        }
        self.entries.push(ArchiveEntry {
            key: key.clone(),
            baseline_ts: truncate_ts(ts),
            keep,
            checked_unix: 0,
            returned: false,
            phone: false,
            held: false,
            held_ts: String::new(),
        });
        true
    }

    /// Explicit local unarchive marker (`keep = false, baseline_ts = ""`) overriding `provider_archived == true`.
    pub fn set_local_unarchived(&mut self, key: &ChatKey) -> bool {
        self.set_local_archived(key, true, "", false)
    }

    /// `archive_rules.zig:269-283`: observes a phone/provider-archived WhatsApp chat (never reclaims!).
    pub fn observe_phone(&mut self, key: &ChatKey, ts: &str) -> bool {
        if ts.is_empty() || !Self::valid_key(key) {
            return false;
        }
        if let Some(idx) = self.find_idx(key) {
            let entry = &mut self.entries[idx];
            if entry.keep || entry.returned || entry.phone {
                return false;
            }
            entry.phone = true;
            if entry.baseline_ts.is_empty() {
                entry.baseline_ts = truncate_ts(ts);
            }
            return true;
        }
        if self.entries.len() >= MAX_ARCHIVE_ENTRIES {
            return false;
        }
        self.entries.push(ArchiveEntry {
            key: key.clone(),
            baseline_ts: truncate_ts(ts),
            keep: false,
            checked_unix: 0,
            returned: false,
            phone: true,
            held: false,
            held_ts: String::new(),
        });
        true
    }

    /// `IGSH-1650` (`archive_rules.zig:285-296`): records provider-archived baseline for resurface checks.
    pub fn observe_provider_archived(&mut self, key: &ChatKey, ts: &str) -> bool {
        if ts.is_empty() || !Self::valid_key(key) {
            return false;
        }
        if let Some(idx) = self.find_idx(key) {
            let entry = &mut self.entries[idx];
            if !entry.keep && !entry.returned && entry.baseline_ts.is_empty() {
                entry.baseline_ts = truncate_ts(ts);
                return true;
            }
            return false;
        }
        self.observe_phone(key, ts)
    }

    /// `archive_rules.zig:171-177`: resurfaces local Slack/TG/Signal archived chat if `ts > baseline`.
    pub fn observe_local(&mut self, key: &ChatKey, ts: &str) -> bool {
        let Some(idx) = self.find_idx(key) else {
            return false;
        };
        let entry = &self.entries[idx];
        if !entry.keep && !entry.baseline_ts.is_empty() && later_stamp(ts, &entry.baseline_ts) {
            self.entries.remove(idx);
            true
        } else {
            false
        }
    }

    /// `archive_rules.zig:179-184`: true if `ts` is strictly later than an active non-kept baseline.
    pub fn returns_on(&self, key: &ChatKey, ts: &str) -> bool {
        self.get(key).is_some_and(|e| {
            !e.keep && !e.returned && !e.baseline_ts.is_empty() && later_stamp(ts, &e.baseline_ts)
        })
    }

    /// `archive_rules.zig:186-190`: true if chat needs an `IGSH-902` message probe.
    pub fn needs_return_probe(&self, key: &ChatKey, row_ts: &str) -> bool {
        let Some(entry) = self.get(key) else {
            return false;
        };
        if !self.returns_on(key, row_ts) {
            return false;
        }
        unix_seconds(row_ts).is_some_and(|s| s > entry.checked_unix)
    }

    pub fn checked_return_probe(&mut self, key: &ChatKey, row_ts: &str) {
        if let Some(secs) = unix_seconds(row_ts)
            && let Some(idx) = self.find_idx(key)
        {
            self.entries[idx].checked_unix = self.entries[idx].checked_unix.max(secs);
        }
    }

    /// `IGSH-902` (`main.zig:14422-14444`): probes recent messages for an archived WhatsApp chat;
    /// resurfaces ONLY when a non-`from_me`, non-reaction message has `ts > baseline`.
    pub fn apply_incoming_probe(
        &mut self,
        key: &ChatKey,
        row_ts: &str,
        messages: &[IncomingProbeMessage<'_>],
    ) -> bool {
        if !self.needs_return_probe(key, row_ts) {
            return false;
        }
        self.checked_return_probe(key, row_ts);
        for msg in messages {
            if msg.from_me || msg.is_reaction {
                continue;
            }
            if self.returns_on(key, msg.ts) {
                return self.return_to_inbox(key);
            }
        }
        false
    }

    /// `archive_rules.zig:231-239`: marks chat returned locally to inbox despite `provider_archived`.
    pub fn return_to_inbox(&mut self, key: &ChatKey) -> bool {
        let Some(idx) = self.slot_reclaiming(key) else {
            return false;
        };
        let entry = &mut self.entries[idx];
        entry.phone = false;
        entry.keep = false;
        entry.baseline_ts.clear();
        entry.checked_unix = 0;
        entry.returned = true;
        entry.held = false;
        entry.held_ts.clear();
        true
    }

    /// `archive_rules.zig:209-224`: deduplicates re-archive requests when a kept WhatsApp chat
    /// reappears unarchived on the provider.
    pub fn hold_kept(&mut self, key: &ChatKey, provider_archived: bool, ts: &str) -> bool {
        if provider_archived {
            return false;
        }
        let Some(idx) = self.find_idx(key) else {
            return false;
        };
        let entry = &mut self.entries[idx];
        if !entry.keep {
            return false;
        }
        let trunc = truncate_ts(ts);
        if entry.held && entry.held_ts == trunc {
            return false;
        }
        entry.held = true;
        entry.held_ts = trunc;
        true
    }

    pub fn release_hold(&mut self, key: &ChatKey) {
        if let Some(idx) = self.find_idx(key) {
            self.entries[idx].held = false;
        }
    }

    /// `archive_rules.zig:306-312`: converts local WA archive entry into phone-observed after Hub ack.
    pub fn confirm_archived(&mut self, key: &ChatKey) -> bool {
        if key.provider != Provider::Whatsapp {
            return false;
        }
        let Some(idx) = self.find_idx(key) else {
            return false;
        };
        let entry = &mut self.entries[idx];
        if !entry.keep && !entry.returned && !entry.phone {
            entry.phone = true;
            return true;
        }
        false
    }

    /// `archive_rules.zig:298-304`: cleans up phone/returned entries once provider unarchived.
    pub fn forget_provider_unarchived(&mut self, key: &ChatKey) -> bool {
        let Some(idx) = self.find_idx(key) else {
            return false;
        };
        let entry = &self.entries[idx];
        if !entry.keep && (entry.phone || entry.returned) {
            self.entries.remove(idx);
            true
        } else {
            false
        }
    }

    pub fn remove(&mut self, key: &ChatKey) -> bool {
        if let Some(idx) = self.find_idx(key) {
            self.entries.remove(idx);
            true
        } else {
            false
        }
    }

    pub fn set_keep(&mut self, key: &ChatKey, keep: bool) {
        if let Some(idx) = self.find_idx(key) {
            self.entries[idx].keep = keep;
        }
    }

    pub fn set_baseline(&mut self, key: &ChatKey, ts: &str) {
        if let Some(idx) = self.find_idx(key) {
            self.entries[idx].baseline_ts = truncate_ts(ts);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PendingArchiveIntent {
    ArchivePending,
    UnarchivePending,
}

impl PendingArchiveIntent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ArchivePending => "archive",
            Self::UnarchivePending => "unarchive",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "archive" => Some(Self::ArchivePending),
            "unarchive" => Some(Self::UnarchivePending),
            _ => None,
        }
    }

    pub fn archived(self) -> bool {
        matches!(self, Self::ArchivePending)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveCompletion {
    ConfirmArchivedAndDrop,
    Drop,
    DropManagedLocally,
    Retry,
}

pub fn visible_archived(base_archived: bool, pending: Option<PendingArchiveIntent>) -> bool {
    match pending {
        Some(PendingArchiveIntent::ArchivePending) => true,
        Some(PendingArchiveIntent::UnarchivePending) => false,
        None => base_archived,
    }
}

pub fn archive_completion(
    pending: PendingArchiveIntent,
    ok: bool,
    managed_locally: bool,
) -> ArchiveCompletion {
    if ok {
        match pending {
            PendingArchiveIntent::ArchivePending => ArchiveCompletion::ConfirmArchivedAndDrop,
            PendingArchiveIntent::UnarchivePending => ArchiveCompletion::Drop,
        }
    } else if managed_locally {
        ArchiveCompletion::DropManagedLocally
    } else {
        ArchiveCompletion::Retry
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveIntent {
    Archive,
    Unarchive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveBase {
    Inbox,
    Archived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveOperation {
    None(Option<ArchiveIntent>),
    Archiving(Option<ArchiveIntent>),
    Unarchiving(Option<ArchiveIntent>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveEvent {
    UserArchive,
    UserUnarchive,
    RemoteOk,
    RemoteFailed,
    SyncSaysArchived,
    SyncSaysInbox,
    QueueFull,
    StartQueued,
}

/// Opposing-intent archive state machine (`archive_state.zig:3-141`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchivePendingState {
    pub base: ArchiveBase,
    pub operation: ArchiveOperation,
}

impl ArchivePendingState {
    pub fn new(archived: bool) -> Self {
        Self {
            base: if archived {
                ArchiveBase::Archived
            } else {
                ArchiveBase::Inbox
            },
            operation: ArchiveOperation::None(None),
        }
    }

    pub fn effective_archived(self) -> bool {
        match self.operation {
            ArchiveOperation::None(Some(ArchiveIntent::Archive)) => true,
            ArchiveOperation::None(Some(ArchiveIntent::Unarchive)) => false,
            ArchiveOperation::None(None) => self.base == ArchiveBase::Archived,
            ArchiveOperation::Archiving(Some(ArchiveIntent::Unarchive)) => false,
            ArchiveOperation::Archiving(_) => true,
            ArchiveOperation::Unarchiving(Some(ArchiveIntent::Archive)) => true,
            ArchiveOperation::Unarchiving(_) => false,
        }
    }

    pub fn pending_intent(self) -> Option<PendingArchiveIntent> {
        match self.operation {
            ArchiveOperation::None(None) => None,
            ArchiveOperation::None(Some(ArchiveIntent::Archive)) => {
                Some(PendingArchiveIntent::ArchivePending)
            }
            ArchiveOperation::None(Some(ArchiveIntent::Unarchive)) => {
                Some(PendingArchiveIntent::UnarchivePending)
            }
            ArchiveOperation::Archiving(Some(ArchiveIntent::Unarchive)) => {
                Some(PendingArchiveIntent::UnarchivePending)
            }
            ArchiveOperation::Archiving(_) => Some(PendingArchiveIntent::ArchivePending),
            ArchiveOperation::Unarchiving(Some(ArchiveIntent::Archive)) => {
                Some(PendingArchiveIntent::ArchivePending)
            }
            ArchiveOperation::Unarchiving(_) => Some(PendingArchiveIntent::UnarchivePending),
        }
    }

    pub fn step(self, event: ArchiveEvent) -> Self {
        let mut next = self;
        match event {
            ArchiveEvent::SyncSaysArchived => {
                next.base = ArchiveBase::Archived;
            }
            ArchiveEvent::SyncSaysInbox => {
                next.base = ArchiveBase::Inbox;
            }
            ArchiveEvent::UserArchive | ArchiveEvent::UserUnarchive => {
                let intent = if event == ArchiveEvent::UserArchive {
                    ArchiveIntent::Archive
                } else {
                    ArchiveIntent::Unarchive
                };
                next.operation = match self.operation {
                    ArchiveOperation::None(Some(_)) => ArchiveOperation::None(Some(intent)),
                    ArchiveOperation::None(None) => match intent {
                        ArchiveIntent::Archive => ArchiveOperation::Archiving(None),
                        ArchiveIntent::Unarchive => ArchiveOperation::Unarchiving(None),
                    },
                    ArchiveOperation::Archiving(_) => ArchiveOperation::Archiving(
                        (intent != ArchiveIntent::Archive).then_some(intent),
                    ),
                    ArchiveOperation::Unarchiving(_) => ArchiveOperation::Unarchiving(
                        (intent != ArchiveIntent::Unarchive).then_some(intent),
                    ),
                };
            }
            ArchiveEvent::RemoteOk | ArchiveEvent::RemoteFailed => {
                let ok = event == ArchiveEvent::RemoteOk;
                match self.operation {
                    ArchiveOperation::None(_) => {}
                    ArchiveOperation::Archiving(queued) => {
                        next.base = if ok {
                            ArchiveBase::Archived
                        } else {
                            ArchiveBase::Inbox
                        };
                        next.operation = match queued {
                            Some(ArchiveIntent::Archive) => ArchiveOperation::Archiving(None),
                            Some(ArchiveIntent::Unarchive) => ArchiveOperation::Unarchiving(None),
                            None => ArchiveOperation::None(None),
                        };
                    }
                    ArchiveOperation::Unarchiving(queued) => {
                        if ok {
                            next.base = ArchiveBase::Inbox;
                        }
                        next.operation = match queued {
                            Some(ArchiveIntent::Archive) => ArchiveOperation::Archiving(None),
                            Some(ArchiveIntent::Unarchive) => ArchiveOperation::Unarchiving(None),
                            None => ArchiveOperation::None(None),
                        };
                    }
                }
            }
            ArchiveEvent::QueueFull => {
                if let ArchiveOperation::Unarchiving(None) = self.operation {
                    next.operation = ArchiveOperation::None(Some(ArchiveIntent::Unarchive));
                }
            }
            ArchiveEvent::StartQueued => {
                if let ArchiveOperation::None(Some(intent)) = self.operation {
                    next.operation = match intent {
                        ArchiveIntent::Archive => ArchiveOperation::Archiving(None),
                        ArchiveIntent::Unarchive => ArchiveOperation::Unarchiving(None),
                    };
                }
            }
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn igsh_902_resurface_ignores_from_me_and_reactions_and_respects_keep() {
        let chat = ChatKey::new(Provider::Whatsapp, "personal", "123@s.whatsapp.net");
        let mut table = ArchiveTable::new();
        assert!(table.archive(&chat, "2026-11-26T10:00:00Z", false));
        assert_eq!(table.status(&chat, true), ArchiveStatus::Archived);

        // Own message or reaction after baseline MUST NOT resurface.
        let non_qualifying = [
            IncomingProbeMessage {
                from_me: true,
                is_reaction: false,
                ts: "2026-11-26T10:05:00Z",
            },
            IncomingProbeMessage {
                from_me: false,
                is_reaction: true,
                ts: "2026-11-26T10:06:00Z",
            },
        ];
        assert!(!table.apply_incoming_probe(&chat, "2026-11-26T10:06:00Z", &non_qualifying));
        assert!(table.is_archived(&chat, true));

        // Real incoming message after baseline DOES resurface!
        let qualifying = [IncomingProbeMessage {
            from_me: false,
            is_reaction: false,
            ts: "2026-11-26T10:10:00Z",
        }];
        assert!(table.apply_incoming_probe(&chat, "2026-11-26T10:10:00Z", &qualifying));
        assert!(table.is_returned(&chat));
        assert!(!table.is_archived(&chat, true));

        // Kept archived NEVER resurfaces and triggers hold_kept once per distinct ts.
        let kept_chat = ChatKey::new(Provider::Whatsapp, "personal", "456@s.whatsapp.net");
        assert!(table.archive(&kept_chat, "2026-11-26T10:00:00Z", true));
        assert_eq!(table.status(&kept_chat, true), ArchiveStatus::KeptArchived);
        assert!(!table.apply_incoming_probe(&kept_chat, "2026-11-26T11:00:00Z", &qualifying));
        assert!(table.is_archived(&kept_chat, false));
        assert!(table.hold_kept(&kept_chat, false, "2026-11-26T11:00:00Z"));
        assert!(!table.hold_kept(&kept_chat, false, "2026-11-26T11:00:00Z"));
        assert!(table.hold_kept(&kept_chat, false, "2026-11-26T11:05:00Z"));
    }

    #[test]
    fn igsh_920_reclaims_only_wa_phone_entries_at_2048_cap() {
        let mut table = ArchiveTable::new();
        // Fill 2047 user-kept entries + 1 phone-observed WA entry.
        let phone_chat = ChatKey::new(Provider::Whatsapp, "personal", "phone@s.whatsapp.net");
        assert!(table.observe_phone(&phone_chat, "2026-11-26T09:00:00Z"));
        for i in 0..(MAX_ARCHIVE_ENTRIES - 1) {
            let k = ChatKey::new(Provider::Slack, "T1", format!("C{i}"));
            assert!(table.archive(&k, "2026-11-26T09:00:00Z", true));
        }
        assert_eq!(table.len(), MAX_ARCHIVE_ENTRIES);

        // New user archive reclaims the phone entry.
        let user_wa = ChatKey::new(Provider::Whatsapp, "personal", "user@s.whatsapp.net");
        assert!(table.archive(&user_wa, "2026-11-26T10:00:00Z", false));
        assert_eq!(table.len(), MAX_ARCHIVE_ENTRIES);
        assert!(table.get(&phone_chat).is_none());
        assert!(table.get(&user_wa).is_some());

        // Now zero phone entries remain -> cannot reclaim user/kept entries!
        let overflow = ChatKey::new(Provider::Whatsapp, "personal", "overflow@s.whatsapp.net");
        assert!(!table.archive(&overflow, "2026-11-26T10:01:00Z", false));
    }

    #[test]
    fn archive_matches_enforces_combined_inbox_alt_a_unread_only_nuance() {
        // Combined Inbox + Alt+A (include_archived=true, archived_only=false, inbox=true):
        // ONLY unread chats (archived or incoming) match!
        assert!(archive_matches(
            false,
            true,
            true,
            false,
            ArchiveStatus::Archived,
            true
        ));
        assert!(!archive_matches(
            false,
            true,
            true,
            false,
            ArchiveStatus::Archived,
            false
        ));
        assert!(!archive_matches(
            false,
            true,
            true,
            false,
            ArchiveStatus::Incoming,
            false
        ));
        // Account view + Alt+A (inbox=false): shows read & unread archived chats.
        assert!(archive_matches(
            false,
            true,
            false,
            false,
            ArchiveStatus::Archived,
            false
        ));
    }

    #[test]
    fn archive_pending_state_queues_opposing_intent() {
        let s0 = ArchivePendingState::new(false);
        let s1 = s0.step(ArchiveEvent::UserArchive);
        assert!(s1.effective_archived());
        // User clicks unarchive while archive is still in-flight.
        let s2 = s1.step(ArchiveEvent::UserUnarchive);
        assert!(!s2.effective_archived());
        assert_eq!(
            s2.operation,
            ArchiveOperation::Archiving(Some(ArchiveIntent::Unarchive))
        );
        // Remote archive completes -> automatically starts queued unarchive.
        let s3 = s2.step(ArchiveEvent::RemoteOk);
        assert_eq!(s3.base, ArchiveBase::Archived);
        assert_eq!(s3.operation, ArchiveOperation::Unarchiving(None));
        assert!(!s3.effective_archived());
        // Remote unarchive completes -> settled in Inbox.
        let s4 = s3.step(ArchiveEvent::RemoteOk);
        assert_eq!(s4.base, ArchiveBase::Inbox);
        assert_eq!(s4.operation, ArchiveOperation::None(None));
        assert!(!s4.effective_archived());
    }

    #[test]
    fn archive_pending_state_exhaustive_144_transitions_match_archive_state_zig() {
        let bases = [ArchiveBase::Inbox, ArchiveBase::Archived];
        let ops = [
            ArchiveOperation::None(None),
            ArchiveOperation::None(Some(ArchiveIntent::Archive)),
            ArchiveOperation::None(Some(ArchiveIntent::Unarchive)),
            ArchiveOperation::Archiving(None),
            ArchiveOperation::Archiving(Some(ArchiveIntent::Archive)),
            ArchiveOperation::Archiving(Some(ArchiveIntent::Unarchive)),
            ArchiveOperation::Unarchiving(None),
            ArchiveOperation::Unarchiving(Some(ArchiveIntent::Archive)),
            ArchiveOperation::Unarchiving(Some(ArchiveIntent::Unarchive)),
        ];
        let events = [
            ArchiveEvent::UserArchive,
            ArchiveEvent::UserUnarchive,
            ArchiveEvent::RemoteOk,
            ArchiveEvent::RemoteFailed,
            ArchiveEvent::SyncSaysArchived,
            ArchiveEvent::SyncSaysInbox,
            ArchiveEvent::QueueFull,
            ArchiveEvent::StartQueued,
        ];

        let mut count = 0usize;
        for &base in &bases {
            for &op in &ops {
                for &event in &events {
                    count += 1;
                    let state = ArchivePendingState {
                        base,
                        operation: op,
                    };
                    let got = state.step(event);

                    // Reference oracle transcribed directly from `archive_state.zig:92-141`.
                    let mut exp_base = base;
                    let mut exp_op = op;
                    match event {
                        ArchiveEvent::SyncSaysArchived => exp_base = ArchiveBase::Archived,
                        ArchiveEvent::SyncSaysInbox => exp_base = ArchiveBase::Inbox,
                        ArchiveEvent::UserArchive => {
                            exp_op = match op {
                                ArchiveOperation::None(Some(_)) => {
                                    ArchiveOperation::None(Some(ArchiveIntent::Archive))
                                }
                                ArchiveOperation::None(None) => ArchiveOperation::Archiving(None),
                                ArchiveOperation::Archiving(_) => ArchiveOperation::Archiving(None),
                                ArchiveOperation::Unarchiving(_) => {
                                    ArchiveOperation::Unarchiving(Some(ArchiveIntent::Archive))
                                }
                            };
                        }
                        ArchiveEvent::UserUnarchive => {
                            exp_op = match op {
                                ArchiveOperation::None(Some(_)) => {
                                    ArchiveOperation::None(Some(ArchiveIntent::Unarchive))
                                }
                                ArchiveOperation::None(None) => ArchiveOperation::Unarchiving(None),
                                ArchiveOperation::Archiving(_) => {
                                    ArchiveOperation::Archiving(Some(ArchiveIntent::Unarchive))
                                }
                                ArchiveOperation::Unarchiving(_) => {
                                    ArchiveOperation::Unarchiving(None)
                                }
                            };
                        }
                        ArchiveEvent::RemoteOk => match op {
                            ArchiveOperation::None(_) => {}
                            ArchiveOperation::Archiving(q) => {
                                exp_base = ArchiveBase::Archived;
                                exp_op = match q {
                                    Some(ArchiveIntent::Archive) => {
                                        ArchiveOperation::Archiving(None)
                                    }
                                    Some(ArchiveIntent::Unarchive) => {
                                        ArchiveOperation::Unarchiving(None)
                                    }
                                    None => ArchiveOperation::None(None),
                                };
                            }
                            ArchiveOperation::Unarchiving(q) => {
                                exp_base = ArchiveBase::Inbox;
                                exp_op = match q {
                                    Some(ArchiveIntent::Archive) => {
                                        ArchiveOperation::Archiving(None)
                                    }
                                    Some(ArchiveIntent::Unarchive) => {
                                        ArchiveOperation::Unarchiving(None)
                                    }
                                    None => ArchiveOperation::None(None),
                                };
                            }
                        },
                        ArchiveEvent::RemoteFailed => match op {
                            ArchiveOperation::None(_) => {}
                            ArchiveOperation::Archiving(q) => {
                                exp_base = ArchiveBase::Inbox;
                                exp_op = match q {
                                    Some(ArchiveIntent::Archive) => {
                                        ArchiveOperation::Archiving(None)
                                    }
                                    Some(ArchiveIntent::Unarchive) => {
                                        ArchiveOperation::Unarchiving(None)
                                    }
                                    None => ArchiveOperation::None(None),
                                };
                            }
                            ArchiveOperation::Unarchiving(q) => {
                                // Preserves `base` (including `SyncSaysInbox`) on failed unarchive!
                                exp_op = match q {
                                    Some(ArchiveIntent::Archive) => {
                                        ArchiveOperation::Archiving(None)
                                    }
                                    Some(ArchiveIntent::Unarchive) => {
                                        ArchiveOperation::Unarchiving(None)
                                    }
                                    None => ArchiveOperation::None(None),
                                };
                            }
                        },
                        ArchiveEvent::QueueFull => {
                            if op == ArchiveOperation::Unarchiving(None) {
                                exp_op = ArchiveOperation::None(Some(ArchiveIntent::Unarchive));
                            }
                        }
                        ArchiveEvent::StartQueued => match op {
                            ArchiveOperation::None(Some(ArchiveIntent::Archive)) => {
                                exp_op = ArchiveOperation::Archiving(None);
                            }
                            ArchiveOperation::None(Some(ArchiveIntent::Unarchive)) => {
                                exp_op = ArchiveOperation::Unarchiving(None);
                            }
                            _ => {}
                        },
                    }
                    assert_eq!(
                        got,
                        ArchivePendingState {
                            base: exp_base,
                            operation: exp_op,
                        },
                        "mismatch for state={state:?}, event={event:?}"
                    );
                }
            }
        }
        assert_eq!(count, 144);
    }
}
