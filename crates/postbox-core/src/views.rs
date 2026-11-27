use crate::{Account, Chat, ChatKey, Provider};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

pub const DEFAULT_PROVIDER_ORDER: [Provider; 4] = [
    Provider::Whatsapp,
    Provider::Slack,
    Provider::Telegram,
    Provider::Signal,
];

/// Parses `"provider:account"` (`messenger_view.zig:75-80`).
pub fn parse_view_key(raw: &str) -> Option<(Provider, &str)> {
    let (prov_str, account) = raw.split_once(':')?;
    if account.is_empty() {
        return None;
    }
    let provider = Provider::parse(prov_str)?;
    Some((provider, account))
}

pub fn format_view_key(provider: Provider, account: &str) -> String {
    format!("{}:{account}", provider.as_str())
}

/// Formats account tab display labels (`messenger_view.zig:15-26`).
pub fn format_account_label(provider: Provider, account: &str, workspace_or_name: &str) -> String {
    let prefix = match provider {
        Provider::Whatsapp => "WhatsApp",
        Provider::Slack => "Slack",
        Provider::Telegram => "Telegram",
        Provider::Signal => "Signal",
    };
    if provider == Provider::Signal || (provider == Provider::Telegram && account == "telegram") {
        return prefix.to_owned();
    }
    let suffix = if !workspace_or_name.is_empty() {
        workspace_or_name
    } else {
        account
    };
    if suffix.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix} {suffix}")
    }
}

/// Exact Slack workspace name precedence (`hub.zig:336-349`):
/// `direct_name > row_team_name > cached_name`. Preserves existing label if all empty.
pub fn resolve_slack_workspace_name(
    direct_name: &str,
    row_team_name: &str,
    cached_name: &str,
    existing_label: Option<&str>,
) -> Option<String> {
    for candidate in [direct_name, row_team_name, cached_name] {
        let trimmed = candidate.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }
    existing_label
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Merges saved view order with currently known accounts (`messenger_view.zig:82-102`).
/// Crucially retains absent/drained saved accounts in their exact `Alt+2..9` slots!
pub fn merge_order(saved: &[String], known: &[Account]) -> Vec<Account> {
    let mut emitted = HashSet::<(Provider, String)>::new();
    let mut result = Vec::with_capacity(saved.len() + known.len());

    for raw in saved {
        let Some((provider, account)) = parse_view_key(raw) else {
            continue;
        };
        let key = (provider, account.to_owned());
        if !emitted.insert(key) {
            continue;
        }
        if let Some(found) = known
            .iter()
            .find(|a| a.provider == provider && a.account_id == account)
        {
            result.push(found.clone());
        } else {
            result.push(Account {
                provider,
                account_id: account.to_owned(),
                label: format_account_label(provider, account, ""),
                state: "offline".to_owned(),
                sort_order: result.len() as i32,
                last_event_at: None,
            });
        }
    }

    for provider in DEFAULT_PROVIDER_ORDER {
        for acc in known.iter().filter(|a| a.provider == provider) {
            let key = (acc.provider, acc.account_id.clone());
            if emitted.insert(key) {
                result.push(acc.clone());
            }
        }
    }

    for (idx, acc) in result.iter_mut().enumerate() {
        acc.sort_order = idx as i32;
    }
    result
}

/// Saves reordered present views while preserving absent saved slots verbatim (`messenger_view.zig:109-143`).
pub fn save_order(previous_saved: &[String], current_views: &[Account]) -> Vec<String> {
    let current_keys: Vec<String> = current_views.iter().map(Account::view_key).collect();
    let present_set: HashSet<&str> = current_keys.iter().map(String::as_str).collect();
    let mut seen = HashSet::<String>::new();
    let mut out = Vec::with_capacity(previous_saved.len().max(current_keys.len()));
    let mut next_current = 0usize;

    for saved_key in previous_saved {
        if parse_view_key(saved_key).is_none() {
            continue;
        }
        if !seen.insert(saved_key.clone()) {
            continue;
        }
        if present_set.contains(saved_key.as_str()) {
            if let Some(replacement) = current_keys.get(next_current) {
                out.push(replacement.clone());
                next_current += 1;
            }
        } else {
            out.push(saved_key.clone());
        }
    }

    while let Some(remaining) = current_keys.get(next_current) {
        if !out.contains(remaining) {
            out.push(remaining.clone());
        }
        next_current += 1;
    }
    out
}

/// Maps `Alt+2..=9` to account index `0..=7`; `Alt+1` returns `None` (Combined Inbox).
pub fn account_index_for_alt_shortcut(alt_digit: u8, views: &[Account]) -> Option<usize> {
    if !(2..=9).contains(&alt_digit) {
        return None;
    }
    let idx = (alt_digit - 2) as usize;
    if idx < views.len() { Some(idx) } else { None }
}

pub fn view_index_for_number(one_based: usize, views: &[Account]) -> Option<usize> {
    if one_based == 0 || one_based > views.len() {
        None
    } else {
        Some(one_based - 1)
    }
}

/// Normalizes a timestamp for lexical comparison (`chat_order.zig:4-46`):
/// trims trailing `'Z'`, truncates to 46 bytes, replaces byte 10 `' '` with `'T'`, appends `'Z'`.
pub fn normalize_timestamp(ts: &str) -> String {
    let trimmed = ts.trim().trim_end_matches('Z');
    if trimmed.is_empty() {
        return String::new();
    }
    let mut end = trimmed.len().min(46);
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    let mut bytes = trimmed.as_bytes()[..end].to_vec();
    if bytes.len() > 10 && bytes[10] == b' ' {
        bytes[10] = b'T';
    }
    let mut out = String::from_utf8(bytes).unwrap_or_default();
    out.push('Z');
    out
}

/// Tracks monotonic latest chat activity timestamps (`chat_order.zig:4-46`).
#[derive(Debug, Default, Clone)]
pub struct ChatActivity {
    latest: HashMap<ChatKey, String>,
}

impl ChatActivity {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn retain(&mut self, key: &ChatKey, timestamp: &str) -> &str {
        let normalized = normalize_timestamp(timestamp);
        let entry = self.latest.entry(key.clone()).or_default();
        if !normalized.is_empty() {
            let cur_cmp = entry.trim_end_matches('Z');
            let new_cmp = normalized.trim_end_matches('Z');
            if new_cmp > cur_cmp {
                *entry = normalized;
            }
        }
        entry.as_str()
    }

    pub fn get(&self, key: &ChatKey) -> Option<&str> {
        self.latest.get(key).map(String::as_str)
    }
}

fn compare_chat_ts_desc(left: &str, right: &str) -> Ordering {
    let ln = normalize_timestamp(left);
    let rn = normalize_timestamp(right);
    match (ln.is_empty(), rn.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => rn.trim_end_matches('Z').cmp(ln.trim_end_matches('Z')),
    }
}

/// Combined Inbox comparator (`messenger_view.zig:145-147, 214-225`):
/// strictly recency-only (IGNORES `pinned`), tie-breaking by `ChatKey` ASC.
pub fn compare_combined_inbox(a: &Chat, b: &Chat) -> Ordering {
    compare_chat_ts_desc(&a.last_msg_ts, &b.last_msg_ts).then_with(|| a.key.cmp(&b.key))
}

/// Per-account view comparator (`chat_order.zig:66-98`):
/// pinned-first, then recency DESC, then title ASC, then chat ID ASC.
pub fn compare_account_view(a: &Chat, b: &Chat) -> Ordering {
    b.pinned
        .cmp(&a.pinned)
        .then_with(|| compare_chat_ts_desc(&a.last_msg_ts, &b.last_msg_ts))
        .then_with(|| a.title.cmp(&b.title))
        .then_with(|| a.key.id.cmp(&b.key.id))
}

/// Produces a canonical lexicographically sortable timestamp key whose ASCII byte order
/// matches `compare_message_order` identically (including fractional RFC3339 seconds and
/// space-separated timestamps):
/// - trims surrounding whitespace
/// - normalizes space at byte index 10 to `'T'`
/// - strips trailing `'Z'`
/// - splits at `'.'`, trims trailing `'0'`s from the fractional part, and drops `'.'` if empty
pub fn canonical_ts_sort_key(ts: &str) -> String {
    let trimmed = ts.trim().trim_end_matches('Z');
    if trimmed.is_empty() {
        return String::new();
    }
    let mut normalized = trimmed.to_owned();
    let bytes = normalized.as_bytes();
    if bytes.len() > 10 && bytes[10] == b' ' {
        normalized.replace_range(10..11, "T");
    }
    if let Some((sec, frac)) = normalized.split_once('.') {
        let trimmed_frac = frac.trim_end_matches('0');
        if trimmed_frac.is_empty() {
            sec.to_owned()
        } else {
            format!("{sec}.{trimmed_frac}")
        }
    } else {
        normalized
    }
}

/// Exact port of `message_filter.zig:18-26`:
/// strips trailing `'Z'`, splits at `'.'`, compares seconds segment, then fractional segment
/// with trailing `'0'` trimmed, then `msg_id`.
pub fn compare_message_order(
    left_ts: &str,
    left_id: &str,
    right_ts: &str,
    right_id: &str,
) -> Ordering {
    let l = left_ts.trim_end_matches('Z');
    let r = right_ts.trim_end_matches('Z');
    let (l_sec, l_frac) = l.split_once('.').unwrap_or((l, ""));
    let (r_sec, r_frac) = r.split_once('.').unwrap_or((r, ""));
    l_sec
        .cmp(r_sec)
        .then_with(|| {
            l_frac
                .trim_end_matches('0')
                .cmp(r_frac.trim_end_matches('0'))
        })
        .then_with(|| left_id.cmp(right_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_account(provider: Provider, id: &str, label: &str) -> Account {
        Account {
            provider,
            account_id: id.into(),
            label: label.into(),
            state: "connected".into(),
            sort_order: 0,
            last_event_at: None,
        }
    }

    fn sample_chat(provider: Provider, acc: &str, id: &str, pinned: bool, ts: &str) -> Chat {
        Chat {
            key: ChatKey::new(provider, acc, id),
            title: id.into(),
            is_group: false,
            archived_local: false,
            archived_remote: false,
            pinned,
            unread_count: 0,
            last_msg_ts: ts.into(),
            last_msg_preview: String::new(),
            avatar_hash: None,
        }
    }

    #[test]
    fn merge_and_save_order_preserve_absent_slots() {
        let saved = vec![
            "whatsapp:personal".into(),
            "slack:T_ABSENT".into(),
            "telegram:telegram".into(),
        ];
        let known = vec![
            sample_account(Provider::Telegram, "telegram", "Telegram"),
            sample_account(Provider::Whatsapp, "personal", "WhatsApp personal"),
            sample_account(Provider::Signal, "signal", "Signal"),
        ];
        let merged = merge_order(&saved, &known);
        assert_eq!(merged.len(), 4);
        assert_eq!(merged[0].view_key(), "whatsapp:personal");
        assert_eq!(merged[1].view_key(), "slack:T_ABSENT");
        assert_eq!(merged[1].state, "offline");
        assert_eq!(merged[2].view_key(), "telegram:telegram");
        assert_eq!(merged[3].view_key(), "signal:signal");

        assert_eq!(account_index_for_alt_shortcut(1, &merged), None);
        assert_eq!(account_index_for_alt_shortcut(2, &merged), Some(0));
        assert_eq!(account_index_for_alt_shortcut(3, &merged), Some(1));
        assert_eq!(account_index_for_alt_shortcut(4, &merged), Some(2));

        // Saving order with only live accounts retains absent slot at index 1!
        let reordered_live = vec![
            sample_account(Provider::Telegram, "telegram", "Telegram"),
            sample_account(Provider::Whatsapp, "personal", "WhatsApp personal"),
        ];
        let next_saved = save_order(&saved, &reordered_live);
        assert_eq!(
            next_saved,
            vec!["telegram:telegram", "slack:T_ABSENT", "whatsapp:personal"]
        );
    }

    #[test]
    fn slack_workspace_name_precedence_and_chat_activity_monotonicity() {
        assert_eq!(
            resolve_slack_workspace_name("Direct", "RowTeam", "Cached", Some("Existing"))
                .as_deref(),
            Some("Direct")
        );
        assert_eq!(
            resolve_slack_workspace_name("", "RowTeam", "Cached", Some("Existing")).as_deref(),
            Some("RowTeam")
        );
        assert_eq!(
            resolve_slack_workspace_name("", "", "Cached", Some("Existing")).as_deref(),
            Some("Cached")
        );
        assert_eq!(
            resolve_slack_workspace_name("", "", "", Some("Existing")).as_deref(),
            Some("Existing")
        );

        let key = ChatKey::new(Provider::Whatsapp, "p", "1");
        let mut activity = ChatActivity::new();
        assert_eq!(
            activity.retain(&key, "2026-11-26 10:00:00"),
            "2026-11-26T10:00:00Z"
        );
        // Older timestamp does not regress!
        assert_eq!(
            activity.retain(&key, "2026-11-26T09:00:00Z"),
            "2026-11-26T10:00:00Z"
        );
        assert_eq!(
            activity.retain(&key, "2026-11-26T10:05:00Z"),
            "2026-11-26T10:05:00Z"
        );
    }

    #[test]
    fn combined_inbox_ignores_pins_while_account_view_sorts_pinned_first() {
        let older_pinned = sample_chat(Provider::Whatsapp, "p", "a", true, "2026-11-26T09:00:00Z");
        let newer_unpinned =
            sample_chat(Provider::Whatsapp, "p", "b", false, "2026-11-26T10:00:00Z");
        assert_eq!(
            compare_combined_inbox(&newer_unpinned, &older_pinned),
            Ordering::Less
        );
        assert_eq!(
            compare_account_view(&older_pinned, &newer_unpinned),
            Ordering::Less
        );

        // Fractional-zero normalization in message order.
        assert_eq!(
            compare_message_order(
                "2026-11-26T10:00:00.500Z",
                "m1",
                "2026-11-26T10:00:00.5Z",
                "m2"
            ),
            Ordering::Less
        );
    }

    #[test]
    fn canonical_ts_sort_key_matches_compare_message_order_across_all_pairs() {
        assert_eq!(
            canonical_ts_sort_key("2026-11-26T10:00:00Z"),
            "2026-11-26T10:00:00"
        );
        assert_eq!(
            canonical_ts_sort_key("2026-11-26T10:00:00.000Z"),
            "2026-11-26T10:00:00"
        );
        assert_eq!(
            canonical_ts_sort_key("2026-11-26T10:00:00.50Z"),
            "2026-11-26T10:00:00.5"
        );
        assert_eq!(
            canonical_ts_sort_key("2026-11-26 10:00:00.050Z"),
            "2026-11-26T10:00:00.05"
        );
        assert_eq!(canonical_ts_sort_key(""), "");

        let samples = [
            "2026-11-26T09:59:59.999Z",
            "2026-11-26T10:00:00Z",
            "2026-11-26T10:00:00.000Z",
            "2026-11-26T10:00:00.05Z",
            "2026-11-26T10:00:00.5Z",
            "2026-11-26T10:00:00.500Z",
            "2026-11-26T10:00:00.51Z",
            "2026-11-26T10:00:01Z",
        ];
        for &a in &samples {
            for &b in &samples {
                let expected = compare_message_order(a, "m", b, "m");
                let actual = canonical_ts_sort_key(a).cmp(&canonical_ts_sort_key(b));
                assert_eq!(actual, expected, "order mismatch for {a} vs {b}");
            }
        }
    }
}
