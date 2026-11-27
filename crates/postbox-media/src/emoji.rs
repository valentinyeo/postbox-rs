use postbox_core::{ChatKey, Provider, Reaction};
use std::collections::HashMap;
use std::sync::OnceLock;

pub const EMOJI_GRID_COLUMNS: usize = 8;
pub const EMOJI_GRID_ROWS: usize = 6;
pub const MAX_EMOJI_RECENTS: usize = 8;
pub const DEFAULT_EMOJI_RECENTS: [&str; 8] = ["👍", "❤️", "😂", "😮", "😢", "🙏", "🔥", "🎉"];
pub const QUICK_REACTIONS: [&str; 6] = ["👍", "❤️", "😂", "😮", "😢", "🙏"];

pub const SHORTCODE_MAX_MATCHES: usize = 6;
pub const SHORTCODE_MIN_QUERY: usize = 2;
pub const SHORTCODE_MAX_NAME: usize = 63;

pub const MAX_PENDING_REACTIONS: usize = 8;
pub const REACTION_RETRY_DELAYS_MS: [u64; 3] = [3_000, 10_000, 30_000];

const EMOJI_RECORDS_TXT: &str = include_str!("../data/emoji_records.txt");
const SHORTCODE_ALIASES_TXT: &str = include_str!("../data/shortcode_aliases.txt");

#[derive(Debug, Clone)]
pub struct EmojiEntry {
    pub index: usize,
    pub emoji: &'static str,
    pub name: &'static str,
    pub aliases: &'static str,
    pub subgroup: &'static str,
    pub normalized_code: String,
}

#[derive(Debug)]
pub struct EmojiCatalog {
    entries: Vec<EmojiEntry>,
    by_emoji: HashMap<&'static str, usize>,
    gemoji_aliases: Vec<(&'static str, &'static str)>, // (code, emoji)
}

static CATALOG: OnceLock<EmojiCatalog> = OnceLock::new();

impl EmojiCatalog {
    pub fn global() -> &'static Self {
        CATALOG.get_or_init(Self::build)
    }

    fn build() -> Self {
        let mut entries = Vec::with_capacity(1923);
        let mut by_emoji = HashMap::with_capacity(1923);
        for line in EMOJI_RECORDS_TXT.lines() {
            let mut parts = line.splitn(4, '|');
            let (Some(emoji), Some(name), Some(aliases), Some(subgroup)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let idx = entries.len();
            let normalized_code = normalize_shortcode(name);
            by_emoji.insert(emoji, idx);
            entries.push(EmojiEntry {
                index: idx,
                emoji,
                name,
                aliases,
                subgroup,
                normalized_code,
            });
        }

        let mut gemoji_aliases = Vec::with_capacity(638);
        for line in SHORTCODE_ALIASES_TXT.lines() {
            let mut parts = line.split(' ');
            let Some(emoji) = parts.next() else {
                continue;
            };
            for alias in parts {
                if !alias.is_empty() {
                    gemoji_aliases.push((alias, emoji));
                }
            }
        }

        Self {
            entries,
            by_emoji,
            gemoji_aliases,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&EmojiEntry> {
        self.entries.get(index)
    }

    pub fn index_of(&self, emoji: &str) -> Option<usize> {
        self.by_emoji.get(emoji).copied()
    }

    pub fn gemoji_alias_count(&self) -> usize {
        self.gemoji_aliases.len()
    }

    /// Builds the 8-entry frequent row from `recents` backfilled with `DEFAULT_EMOJI_RECENTS`
    /// (`emoji_picker.zig:73-90`).
    pub fn frequent_row(&self, recents: &[usize]) -> Vec<usize> {
        let mut out = Vec::with_capacity(MAX_EMOJI_RECENTS);
        for &idx in recents {
            if idx < self.entries.len() && !out.contains(&idx) {
                out.push(idx);
                if out.len() >= MAX_EMOJI_RECENTS {
                    return out;
                }
            }
        }
        for default_emoji in DEFAULT_EMOJI_RECENTS {
            if let Some(idx) = self.index_of(default_emoji)
                && !out.contains(&idx)
            {
                out.push(idx);
                if out.len() >= MAX_EMOJI_RECENTS {
                    break;
                }
            }
        }
        out
    }

    /// Exact port of `emoji_picker.matchRank` (`emoji_picker.zig:92-112`):
    /// - 4 = exact CLDR name match
    /// - 3 = whole-word match in name or aliases
    /// - 2 = word-start prefix match in name or aliases
    /// - 1 = substring match in name, aliases, or subgroup
    /// - 0 = no match
    pub fn match_rank(&self, index: usize, query_lower: &str) -> u8 {
        let Some(entry) = self.entries.get(index) else {
            return 0;
        };
        if query_lower.is_empty() {
            return 1;
        }
        if entry.name.eq_ignore_ascii_case(query_lower) {
            return 4;
        }
        let word = word_match(entry.name, query_lower).max(word_match(entry.aliases, query_lower));
        if word > 0 {
            return word + 1; // 2 -> 3 (whole word), 1 -> 2 (word prefix)
        }
        if contains_ascii_ci(entry.name, query_lower)
            || contains_ascii_ci(entry.aliases, query_lower)
            || contains_ascii_ci(entry.subgroup, query_lower)
        {
            return 1;
        }
        0
    }

    /// Searches catalog with exact rank ordering (`4 -> 3 -> 2 -> 1`) and stable catalog-order
    /// tie-breaking (`emoji_picker.zig:114-127`, `main.zig:13275-13283`).
    pub fn search(&self, query: &str, recents: &[usize]) -> Vec<usize> {
        let q = query.trim_matches(' ').to_ascii_lowercase();
        if q.is_empty() {
            let mut out = self.frequent_row(recents);
            out.reserve(self.entries.len());
            out.extend(0..self.entries.len());
            return out;
        }
        let mut buckets: [Vec<usize>; 4] = Default::default();
        for idx in 0..self.entries.len() {
            let rank = self.match_rank(idx, &q);
            if rank > 0 {
                buckets[(4 - rank) as usize].push(idx);
            }
        }
        let mut out = Vec::new();
        for b in buckets {
            out.extend(b);
        }
        out
    }

    /// Parses comma-separated persisted recents rejecting unknown/numeric tokens
    /// (`emoji_picker.zig:151-164`).
    pub fn parse_recents(&self, raw: &str) -> Vec<usize> {
        let mut out = Vec::with_capacity(MAX_EMOJI_RECENTS);
        for part in raw.split(',') {
            let trimmed = part.trim_matches(' ');
            if let Some(idx) = self.index_of(trimmed)
                && !out.contains(&idx)
            {
                out.push(idx);
                if out.len() >= MAX_EMOJI_RECENTS {
                    break;
                }
            }
        }
        out
    }

    pub fn format_recents(&self, recents: &[usize]) -> Vec<String> {
        recents
            .iter()
            .filter_map(|&idx| self.get(idx).map(|e| e.emoji.to_owned()))
            .take(MAX_EMOJI_RECENTS)
            .collect()
    }

    /// Exact port of `shortcodes.matches` (`shortcodes.zig:88-123`): returns up to
    /// `SHORTCODE_MAX_MATCHES` (6) shortest prefix matches deduplicated by emoji.
    pub fn match_shortcodes(&self, query: &str) -> Vec<ShortcodeMatch> {
        if query.is_empty() || query.len() > SHORTCODE_MAX_NAME {
            return Vec::new();
        }
        let q = query.to_ascii_lowercase();
        let mut out: Vec<ShortcodeMatch> = Vec::with_capacity(SHORTCODE_MAX_MATCHES);

        let mut offer = |code: &str, emoji: &'static str| {
            if !code.starts_with(&q) {
                return;
            }
            if let Some(pos) = out.iter().position(|m| m.emoji == emoji) {
                if code.len() < out[pos].code.len() {
                    out.remove(pos);
                } else {
                    return;
                }
            }
            let mut at = 0usize;
            while at < out.len() && out[at].code.len() <= code.len() {
                at += 1;
            }
            if at < SHORTCODE_MAX_MATCHES {
                if out.len() == SHORTCODE_MAX_MATCHES {
                    out.pop();
                }
                out.insert(
                    at,
                    ShortcodeMatch {
                        code: code.to_owned(),
                        emoji,
                    },
                );
            }
        };

        for entry in &self.entries {
            offer(&entry.normalized_code, entry.emoji);
        }
        for &(alias, emoji) in &self.gemoji_aliases {
            offer(alias, emoji);
        }
        out
    }

    /// Resolves a closed `:code:` token to its single shortest prefix match (`shortcodes.zig:132-136`).
    pub fn resolve_shortcode(&self, query: &str) -> Option<&'static str> {
        self.match_shortcodes(query).first().map(|m| m.emoji)
    }
}

/// Pushes `index` to front of recents capped at 8 (`emoji_picker.zig:180-195`).
pub fn push_recent(recents: &mut Vec<usize>, index: usize) {
    if let Some(pos) = recents.iter().position(|&i| i == index) {
        recents.remove(pos);
    }
    recents.insert(0, index);
    recents.truncate(MAX_EMOJI_RECENTS);
}

fn contains_ascii_ci(haystack: &str, needle_lower: &str) -> bool {
    let hb = haystack.as_bytes();
    let nb = needle_lower.as_bytes();
    if nb.is_empty() {
        return true;
    }
    if nb.len() > hb.len() {
        return false;
    }
    hb.windows(nb.len())
        .any(|w| w.iter().zip(nb).all(|(a, b)| a.to_ascii_lowercase() == *b))
}

/// Returns 2 for whole-word match, 1 for word-start prefix match, 0 for neither
/// (`emoji_picker.zig:92-112`).
fn word_match(text: &str, query_lower: &str) -> u8 {
    let tb = text.as_bytes();
    let qb = query_lower.as_bytes();
    if qb.is_empty() || qb.len() > tb.len() {
        return 0;
    }
    let mut best = 0u8;
    for start in 0..=tb.len() - qb.len() {
        if start > 0 && tb[start - 1].is_ascii_alphanumeric() {
            continue;
        }
        let matches = tb[start..start + qb.len()]
            .iter()
            .zip(qb)
            .all(|(a, b)| a.to_ascii_lowercase() == *b);
        if !matches {
            continue;
        }
        let end = start + qb.len();
        let whole = end == tb.len() || !tb[end].is_ascii_alphanumeric();
        if whole {
            return 2;
        }
        best = best.max(1);
    }
    best
}

/// Exact port of `shortcodes.normalize` (`shortcodes.zig:59-72`).
pub fn normalize_shortcode(cldr_name: &str) -> String {
    let mut out = String::with_capacity(cldr_name.len().min(SHORTCODE_MAX_NAME));
    for b in cldr_name.bytes() {
        if out.len() >= SHORTCODE_MAX_NAME {
            break;
        }
        let mapped = match b {
            b' ' | b'-' => b'_',
            c => c.to_ascii_lowercase(),
        };
        if mapped.is_ascii_alphanumeric() || matches!(mapped, b'_' | b'+') {
            out.push(mapped as char);
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcodeMatch {
    pub code: String,
    pub emoji: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcodeTyping {
    pub start: usize,
    pub end: usize,
    pub closed: bool,
    pub query: String,
}

fn is_shortcode_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'+' | b'-')
}

/// Exact port of `shortcodes.typing` (`shortcodes.zig:31-57`):
/// ignores times (`12:30`), URLs (`https://`), double colons (`::woo`), and mid-word colons (`a:bc`),
/// while permitting punctuation boundaries like `(:woo` and closed shortcodes like `:+1:`.
pub fn detect_shortcode_at_caret(text: &str, caret_byte: usize) -> Option<ShortcodeTyping> {
    if caret_byte == 0 || caret_byte > text.len() || !text.is_char_boundary(caret_byte) {
        return None;
    }
    let before = text.as_bytes()[..caret_byte].as_ref();
    let mut end = before.len();
    let mut closed = false;
    if end > 0 && before[end - 1] == b':' {
        closed = true;
        end -= 1;
    }
    let mut begin = end;
    while begin > 0 && is_shortcode_char(before[begin - 1]) {
        begin -= 1;
    }
    if begin == 0 || before[begin - 1] != b':' {
        return None;
    }
    let colon = begin - 1;
    if colon > 0 {
        let prev = before[colon - 1];
        if prev == b':' || prev == b'/' || prev == b'\\' || prev.is_ascii_alphanumeric() {
            return None;
        }
    }
    let len = end - begin;
    if len == 0 || len > SHORTCODE_MAX_NAME {
        return None;
    }
    if !closed && (len < SHORTCODE_MIN_QUERY || !before[begin].is_ascii_alphabetic()) {
        return None;
    }
    let query = std::str::from_utf8(&before[begin..end])
        .ok()?
        .to_ascii_lowercase();
    Some(ShortcodeTyping {
        start: colon,
        end: caret_byte,
        closed,
        query,
    })
}

/// Replaces `[typing.start..typing.end]` with `emoji`, returning `(new_text, new_caret_byte)`.
pub fn replace_shortcode_token(
    text: &str,
    typing: &ShortcodeTyping,
    emoji: &str,
) -> (String, usize) {
    if typing.start > typing.end
        || typing.end > text.len()
        || !text.is_char_boundary(typing.start)
        || !text.is_char_boundary(typing.end)
    {
        return (text.to_owned(), text.len());
    }
    let mut out = String::with_capacity(text.len() + emoji.len());
    out.push_str(&text[..typing.start]);
    out.push_str(emoji);
    let new_caret = out.len();
    out.push_str(&text[typing.end..]);
    (out, new_caret)
}

/// Mode of the `8×6` emoji picker overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmojiPickerMode {
    React { chat: ChatKey, msg_id: String },
    InsertIntoComposer { origin_chat: Option<ChatKey> },
}

/// Complete `8×6` emoji & reaction picker state machine (`emoji_picker.zig`, `main.zig:13164-13359`).
#[derive(Debug, Clone)]
pub struct EmojiPickerState {
    pub mode: EmojiPickerMode,
    pub query: String,
    pub matches: Vec<usize>,
    pub selected: usize,
}

impl EmojiPickerState {
    pub fn open(mode: EmojiPickerMode, recents: &[usize]) -> Self {
        let matches = EmojiCatalog::global().search("", recents);
        Self {
            mode,
            query: String::new(),
            matches,
            selected: 0,
        }
    }

    pub fn cue_text(&self) -> &'static str {
        match self.mode {
            EmojiPickerMode::React { .. } => "React: search emoji (thumbs, heart, laugh...)",
            EmojiPickerMode::InsertIntoComposer { .. } => "Search emoji",
        }
    }

    pub fn set_query(&mut self, query: String, recents: &[usize]) {
        self.query = query;
        self.matches = EmojiCatalog::global().search(&self.query, recents);
        self.selected = 0;
    }

    pub fn move_grid(&mut self, delta_rows: isize, delta_cols: isize) {
        if self.matches.is_empty() {
            self.selected = 0;
            return;
        }
        let delta = delta_rows * (EMOJI_GRID_COLUMNS as isize) + delta_cols;
        let next = (self.selected as isize + delta).clamp(0, (self.matches.len() - 1) as isize);
        self.selected = next as usize;
    }

    pub fn selected_entry(&self) -> Option<&'static EmojiEntry> {
        let idx = *self.matches.get(self.selected)?;
        EmojiCatalog::global().get(idx)
    }

    pub fn visible_slice(&self) -> (usize, &[usize]) {
        let page_size = EMOJI_GRID_COLUMNS * EMOJI_GRID_ROWS;
        let sel_row = self.selected / EMOJI_GRID_COLUMNS;
        let start_row = sel_row.saturating_sub(EMOJI_GRID_ROWS - 1);
        let start = (start_row * EMOJI_GRID_COLUMNS).min(self.matches.len());
        let end = (start + page_size).min(self.matches.len());
        (start, &self.matches[start..end])
    }
}

/// Reaction chip click decision (`reaction_chip.zig:3-16`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReactionChipDecision {
    Add(String),
    Remove,
}

pub fn decide_reaction_chip_click(
    chip_text: &str,
    segment_index: usize,
    my_reaction: &str,
) -> Option<ReactionChipDecision> {
    let segment = chip_text.split("  ").nth(segment_index)?;
    let trimmed = segment.trim_matches(' ');
    let token = trimmed.split(' ').next().unwrap_or("");
    if token.is_empty() {
        return None;
    }
    if token == my_reaction {
        Some(ReactionChipDecision::Remove)
    } else {
        Some(ReactionChipDecision::Add(token.to_owned()))
    }
}

/// Formats reactions into exact double-space-separated chip text (`"😂 2  ❤️ 1"`).
pub fn format_reactions_chip(reactions: &[Reaction]) -> String {
    reactions
        .iter()
        .map(|r| {
            if r.count > 1 {
                format!("{} {}", r.emoji, r.count)
            } else {
                r.emoji.clone()
            }
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// Applies an optimistic reaction toggle to a `Vec<Reaction>`, respecting `preserve_chip`
/// (`IGSH-674` multi-reaction preservation!).
pub fn apply_optimistic_reaction(
    existing: &[Reaction],
    emoji: &str,
    preserve_chip: bool,
) -> Vec<Reaction> {
    if !preserve_chip {
        let mut next: Vec<Reaction> = existing
            .iter()
            .filter_map(|r| {
                if r.from_me {
                    if r.count > 1 {
                        Some(Reaction {
                            emoji: r.emoji.clone(),
                            count: r.count - 1,
                            from_me: false,
                        })
                    } else {
                        None
                    }
                } else {
                    Some(r.clone())
                }
            })
            .collect();
        if !emoji.is_empty() {
            if let Some(slot) = next.iter_mut().find(|r| r.emoji == emoji) {
                slot.count += 1;
                slot.from_me = true;
            } else {
                next.push(Reaction {
                    emoji: emoji.to_owned(),
                    count: 1,
                    from_me: true,
                });
            }
        }
        return next;
    }
    // `preserve_chip == true`: preserve all existing chips while updating `from_me` markers.
    let mut next = existing.to_vec();
    for r in &mut next {
        r.from_me = !emoji.is_empty() && r.emoji == emoji;
    }
    if !emoji.is_empty() && !next.iter().any(|r| r.emoji == emoji) {
        next.push(Reaction {
            emoji: emoji.to_owned(),
            count: 1,
            from_me: true,
        });
    }
    next
}

/// Double-click decision on a message bubble (`message_double_click.zig:3-9`, `IGSH-463`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoubleClickDecision {
    Ignore,
    AddThumbsUp,
    RemoveThumbsUp,
}

pub fn decide_bubble_double_click(
    provider: Provider,
    from_me: bool,
    my_reaction: &str,
) -> DoubleClickDecision {
    if provider != Provider::Whatsapp || from_me {
        return DoubleClickDecision::Ignore;
    }
    if my_reaction == "👍" {
        DoubleClickDecision::RemoveThumbsUp
    } else {
        DoubleClickDecision::AddThumbsUp
    }
}

/// Pending optimistic reaction entry (`main.zig:554-568, 12889-12987`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingReaction {
    pub chat: ChatKey,
    pub msg_id: String,
    pub emoji: String,
    pub client_id: String,
    pub previous_reactions: Vec<Reaction>,
    pub preserve_chip: bool,
    pub retries: u8,
    pub retry_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReactionSettleOutcome {
    IgnoredStale,
    Confirmed {
        notice: &'static str,
    },
    UnknownDelivery409 {
        notice: &'static str,
    },
    RetryScheduled {
        delay_ms: u64,
        notice: &'static str,
    },
    RevertedTerminal {
        previous: Vec<Reaction>,
        notice: &'static str,
    },
}

/// Pending reaction retry & rollback queue (cap = 8, 3x retries `3s/10s/30s`, `409` unknown
/// delivery without revert, terminal failure revert).
#[derive(Debug, Default, Clone)]
pub struct PendingReactionQueue {
    entries: Vec<PendingReaction>,
}

impl PendingReactionQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn is_pending(&self, chat: &ChatKey, msg_id: &str) -> bool {
        self.entries
            .iter()
            .any(|e| &e.chat == chat && e.msg_id == msg_id)
    }

    pub fn entries(&self) -> &[PendingReaction] {
        &self.entries
    }

    /// Earliest `retry_at_ms` among all `RetryScheduled` entries (`None` when nothing needs a timer).
    pub fn next_retry_at_ms(&self) -> Option<u64> {
        self.entries
            .iter()
            .filter(|e| e.retry_at_ms > 0)
            .map(|e| e.retry_at_ms)
            .min()
    }

    /// Enqueues or supersedes a pending reaction. Reusing an existing `(chat, msg_id)` preserves
    /// the original rollback snapshot `previous_reactions` (`main.zig:12904-12915`).
    pub fn enqueue(
        &mut self,
        chat: ChatKey,
        msg_id: String,
        emoji: String,
        client_id: String,
        current_reactions: Vec<Reaction>,
        preserve_chip: bool,
    ) -> Result<&PendingReaction, &'static str> {
        if let Some(pos) = self
            .entries
            .iter()
            .position(|e| e.chat == chat && e.msg_id == msg_id)
        {
            let entry = &mut self.entries[pos];
            entry.emoji = emoji;
            entry.client_id = client_id;
            entry.preserve_chip = preserve_chip;
            entry.retries = 0;
            entry.retry_at_ms = 0;
            return Ok(&self.entries[pos]);
        }
        if self.entries.len() >= MAX_PENDING_REACTIONS {
            return Err("Too many reactions still sending; try again");
        }
        self.entries.push(PendingReaction {
            chat,
            msg_id,
            emoji,
            client_id,
            previous_reactions: current_reactions,
            preserve_chip,
            retries: 0,
            retry_at_ms: 0,
        });
        Ok(self.entries.last().expect("just pushed"))
    }

    /// Settles a Hub reaction attempt (`main.zig:12939-12987`).
    pub fn settle(
        &mut self,
        chat: &ChatKey,
        msg_id: &str,
        emoji: &str,
        ok: bool,
        http_status: Option<u16>,
        now_ms: u64,
    ) -> ReactionSettleOutcome {
        let Some(pos) = self
            .entries
            .iter()
            .position(|e| &e.chat == chat && e.msg_id == msg_id)
        else {
            return ReactionSettleOutcome::IgnoredStale;
        };
        if self.entries[pos].emoji != emoji {
            return ReactionSettleOutcome::IgnoredStale;
        }
        if http_status == Some(409) {
            self.entries.remove(pos);
            return ReactionSettleOutcome::UnknownDelivery409 {
                notice: "Reaction delivery unknown; check the chat",
            };
        }
        if ok {
            let removed_emoji = self.entries.remove(pos).emoji;
            let notice = if removed_emoji.is_empty() {
                "Reaction removed"
            } else {
                "Reaction sent"
            };
            return ReactionSettleOutcome::Confirmed { notice };
        }
        let entry = &mut self.entries[pos];
        entry.retries = entry.retries.saturating_add(1);
        if let Some(&delay_ms) = REACTION_RETRY_DELAYS_MS.get((entry.retries - 1) as usize) {
            entry.retry_at_ms = now_ms.saturating_add(delay_ms);
            ReactionSettleOutcome::RetryScheduled {
                delay_ms,
                notice: "Reaction not sent yet; retrying",
            }
        } else {
            let removed = self.entries.remove(pos);
            ReactionSettleOutcome::RevertedTerminal {
                previous: removed.previous_reactions,
                notice: "Reaction failed and was taken off",
            }
        }
    }

    /// Returns entries due for retry at `now_ms`, clearing their `retry_at_ms` so they aren't
    /// dispatched twice concurrently.
    pub fn due_retries(&mut self, now_ms: u64) -> Vec<PendingReaction> {
        let mut due = Vec::new();
        for entry in &mut self.entries {
            if entry.retry_at_ms > 0 && now_ms >= entry.retry_at_ms {
                entry.retry_at_ms = 0;
                due.push(entry.clone());
            }
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emoji_catalog_has_1923_entries_zero_skin_tones_and_638_gemoji_aliases() {
        let cat = EmojiCatalog::global();
        assert_eq!(cat.len(), 1923);
        assert_eq!(cat.gemoji_alias_count(), 638);
        for i in 0..cat.len() {
            let entry = cat.get(i).unwrap();
            assert!(!entry.emoji.is_empty());
            assert!(!entry.name.is_empty());
            for ch in entry.emoji.chars() {
                assert!(
                    !('\u{1F3FB}'..='\u{1F3FF}').contains(&ch),
                    "skin tone modifier found in {}",
                    entry.emoji
                );
            }
        }
    }

    #[test]
    fn search_ranking_recents_and_shortcodes_match_exact_spec() {
        let cat = EmojiCatalog::global();
        let recents = cat.parse_recents("🔥, 3, 17, 👍, 🔥");
        assert_eq!(recents.len(), 2);
        let freq = cat.frequent_row(&recents);
        assert_eq!(freq.len(), 8);
        assert_eq!(cat.get(freq[0]).unwrap().emoji, "🔥");
        assert_eq!(cat.get(freq[1]).unwrap().emoji, "👍");

        // Subgroup match rank 1
        let wave_idx = cat.index_of("👋").unwrap();
        assert_eq!(cat.match_rank(wave_idx, "fingers-open"), 1);

        // Exact > whole word > prefix > substring
        let fire_idx = cat.index_of("🔥").unwrap();
        assert_eq!(cat.match_rank(fire_idx, "fire"), 4);
        let results = cat.search("fire", &[]);
        assert_eq!(results[0], fire_idx);

        // Shortcodes boundary & resolution
        assert_eq!(detect_shortcode_at_caret("12:30", 5), None);
        assert_eq!(detect_shortcode_at_caret("https://a.b/c:woo", 17), None);
        assert_eq!(detect_shortcode_at_caret("a:bc", 4), None);
        assert_eq!(detect_shortcode_at_caret("::woo", 5), None);
        assert_eq!(detect_shortcode_at_caret(":+1", 3), None); // open non-alpha blocked
        let closed = detect_shortcode_at_caret(":+1:", 4).unwrap();
        assert!(closed.closed);
        assert_eq!(cat.resolve_shortcode(&closed.query), Some("👍"));
        assert_eq!(cat.resolve_shortcode("joy"), Some("😂"));
        assert_eq!(cat.resolve_shortcode("tada"), Some("🎉"));
        let paren = detect_shortcode_at_caret("(:wo", 4).unwrap();
        assert_eq!(paren.start, 1);
        assert!(cat.match_shortcodes(&paren.query).len() <= 6);
    }

    #[test]
    fn reaction_chips_double_click_and_pending_queue_retries_409_and_revert() {
        assert_eq!(
            decide_reaction_chip_click("😂 2  ❤️ 1", 0, "❤️"),
            Some(ReactionChipDecision::Add("😂".into()))
        );
        assert_eq!(
            decide_reaction_chip_click("😂 2  ❤️ 1", 1, "❤️"),
            Some(ReactionChipDecision::Remove)
        );
        assert_eq!(
            decide_bubble_double_click(Provider::Whatsapp, true, ""),
            DoubleClickDecision::Ignore
        );
        assert_eq!(
            decide_bubble_double_click(Provider::Whatsapp, false, ""),
            DoubleClickDecision::AddThumbsUp
        );
        assert_eq!(
            decide_bubble_double_click(Provider::Whatsapp, false, "👍"),
            DoubleClickDecision::RemoveThumbsUp
        );
        assert_eq!(
            decide_bubble_double_click(Provider::Slack, false, ""),
            DoubleClickDecision::Ignore
        );

        let chat = ChatKey::new(Provider::Whatsapp, "main", "1@s.whatsapp.net");
        let prev = vec![Reaction {
            emoji: "❤️".into(),
            count: 1,
            from_me: true,
        }];
        let mut q = PendingReactionQueue::new();
        q.enqueue(
            chat.clone(),
            "m1".into(),
            "👍".into(),
            "cid1".into(),
            prev.clone(),
            false,
        )
        .unwrap();

        // 3 retries: 3s, 10s, 30s, then terminal revert on 4th failure
        assert_eq!(
            q.settle(&chat, "m1", "👍", false, Some(500), 1000),
            ReactionSettleOutcome::RetryScheduled {
                delay_ms: 3_000,
                notice: "Reaction not sent yet; retrying"
            }
        );
        assert_eq!(
            q.settle(&chat, "m1", "👍", false, Some(500), 5000),
            ReactionSettleOutcome::RetryScheduled {
                delay_ms: 10_000,
                notice: "Reaction not sent yet; retrying"
            }
        );
        assert_eq!(
            q.settle(&chat, "m1", "👍", false, Some(500), 20_000),
            ReactionSettleOutcome::RetryScheduled {
                delay_ms: 30_000,
                notice: "Reaction not sent yet; retrying"
            }
        );
        assert_eq!(
            q.settle(&chat, "m1", "👍", false, Some(500), 60_000),
            ReactionSettleOutcome::RevertedTerminal {
                previous: prev.clone(),
                notice: "Reaction failed and was taken off"
            }
        );

        // 409 unknown delivery clears pending WITHOUT reverting
        q.enqueue(
            chat.clone(),
            "m2".into(),
            "🔥".into(),
            "cid2".into(),
            prev,
            false,
        )
        .unwrap();
        assert_eq!(
            q.settle(&chat, "m2", "🔥", false, Some(409), 1000),
            ReactionSettleOutcome::UnknownDelivery409 {
                notice: "Reaction delivery unknown; check the chat"
            }
        );
        assert!(!q.is_pending(&chat, "m2"));
    }

    #[test]
    fn replace_shortcode_token_guards_mid_utf8_reversed_and_oob_spans() {
        let s = "Übung :joy:";
        for bad in [
            ShortcodeTyping {
                start: 1,
                end: 5,
                query: String::new(),
                closed: false,
            },
            ShortcodeTyping {
                start: 9,
                end: 4,
                query: String::new(),
                closed: false,
            },
            ShortcodeTyping {
                start: 0,
                end: 999,
                query: String::new(),
                closed: false,
            },
        ] {
            let (out, caret) = replace_shortcode_token(s, &bad, "😂");
            assert_eq!(out, s);
            assert_eq!(caret, s.len());
        }
    }
}
