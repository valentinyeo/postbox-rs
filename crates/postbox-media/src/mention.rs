use postbox_core::{Message, Provider};
use std::collections::HashSet;

pub const MAX_MENTION_QUERY_BYTES: usize = 64;
pub const MAX_MENTION_SCORED: usize = 64;
pub const MAX_MENTION_POPUP_ROWS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionCandidate {
    pub handle: String,
    pub display_name: String,
    pub in_chat: bool,
    pub online: bool,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionSpan {
    pub start: usize,
    pub end: usize,
    pub query: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionMatch {
    pub candidate: Option<MentionCandidate>,
    pub is_here: bool,
    pub score: i32,
}

impl MentionMatch {
    pub fn display_label(&self) -> &str {
        if self.is_here {
            "@here"
        } else if let Some(c) = &self.candidate {
            if !c.display_name.is_empty() {
                &c.display_name
            } else {
                &c.handle
            }
        } else {
            ""
        }
    }

    pub fn in_chat(&self) -> bool {
        self.is_here || self.candidate.as_ref().is_some_and(|c| c.in_chat)
    }
}

/// Exact port of `mention_index.activeToken` (`mention_index.zig:85-105`):
/// detects `@<query>` at `caret_byte` (max 64B, start/whitespace boundary, ignores mid-word `@`
/// like `mail@pau` and rejects whitespace/`@` inside query).
pub fn detect_mention_at_caret(text: &str, caret_byte: usize) -> Option<MentionSpan> {
    if text.is_empty()
        || caret_byte == 0
        || caret_byte > text.len()
        || !text.is_char_boundary(caret_byte)
    {
        return None;
    }
    let bytes = text.as_bytes();
    let mut start = caret_byte;
    while start > 0 && !matches!(bytes[start - 1], b' ' | b'\n' | b'\t') {
        start -= 1;
    }
    if start >= caret_byte || bytes[start] != b'@' {
        return None;
    }
    let query_bytes = &bytes[start + 1..caret_byte];
    if query_bytes.len() > MAX_MENTION_QUERY_BYTES {
        return None;
    }
    if query_bytes
        .iter()
        .any(|&b| matches!(b, b' ' | b'\n' | b'\t' | b'@'))
    {
        return None;
    }
    Some(MentionSpan {
        start,
        end: caret_byte,
        query: text[start + 1..caret_byte].to_owned(),
    })
}

fn contains_ascii_ci(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let hb = haystack.as_bytes();
    let nb = needle.as_bytes();
    if nb.len() > hb.len() {
        return false;
    }
    hb.windows(nb.len())
        .any(|w| w.iter().zip(nb).all(|(a, b)| a.eq_ignore_ascii_case(b)))
}

fn starts_with_ascii_ci(haystack: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    let hb = haystack.as_bytes();
    let pb = prefix.as_bytes();
    hb.len() >= pb.len()
        && hb[..pb.len()]
            .iter()
            .zip(pb)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// Exact port of `mention_index.score` (`mention_index.zig:26-37`):
/// - excludes deactivated (`!active`)
/// - requires ASCII case-insensitive substring match in `display_name` OR `handle`
/// - `in_chat` +100 vs +40, `online` +10, `display_name` prefix (or empty query) +30.
pub fn score_mention(candidate: &MentionCandidate, query: &str) -> Option<i32> {
    if !candidate.active {
        return None;
    }
    if !contains_ascii_ci(&candidate.display_name, query)
        && !contains_ascii_ci(&candidate.handle, query)
    {
        return None;
    }
    let mut score = if candidate.in_chat { 100 } else { 40 };
    if candidate.online {
        score += 10;
    }
    if query.is_empty() || starts_with_ascii_ci(&candidate.display_name, query) {
        score += 30;
    }
    Some(score)
}

/// Ranks mention candidates with stable descending sort, appending Slack `@here` (score 60)
/// when `provider == Provider::Slack`, `query.len() <= 4`, and `"here"` contains `query`.
/// Safely handles `@here` even when `candidates` is empty!
pub fn rank_mentions(
    query: &str,
    candidates: &[MentionCandidate],
    provider: Provider,
) -> Vec<MentionMatch> {
    let mut scored: Vec<MentionMatch> =
        Vec::with_capacity(candidates.len().min(MAX_MENTION_SCORED) + 1);
    for cand in candidates {
        if scored.len() >= MAX_MENTION_SCORED {
            break;
        }
        if let Some(score) = score_mention(cand, query) {
            scored.push(MentionMatch {
                candidate: Some(cand.clone()),
                is_here: false,
                score,
            });
        }
    }
    if provider == Provider::Slack
        && scored.len() < MAX_MENTION_SCORED
        && query.len() <= 4
        && contains_ascii_ci("here", query)
    {
        scored.push(MentionMatch {
            candidate: None,
            is_here: true,
            score: 60,
        });
    }
    // Rust's `sort_by` is a stable sort: equal scores preserve original roster order with `@here`
    // after people of equal score (`mention_index.zig:52-58`).
    scored.sort_by_key(|a| std::cmp::Reverse(a.score));
    scored
}

/// Formats provider-specific mention insertion syntax (`mention_index.zig:64-73`):
/// - Slack: `<@handle>` (or `<!here>` for `@here`)
/// - WhatsApp: `@handle`
/// - Signal / Telegram: `@display_name`
pub fn format_mention_insertion(provider: Provider, m: &MentionMatch) -> String {
    if m.is_here {
        return match provider {
            Provider::Slack => "<!here>".to_owned(),
            _ => "@here".to_owned(),
        };
    }
    let Some(c) = &m.candidate else {
        return String::new();
    };
    let handle = if !c.handle.is_empty() {
        c.handle.as_str()
    } else {
        c.display_name.as_str()
    };
    match provider {
        Provider::Slack => format!("<@{handle}>"),
        Provider::Whatsapp => format!("@{handle}"),
        Provider::Signal | Provider::Telegram => format!("@{}", c.display_name),
    }
}

/// Replaces `[span.start..span.end]` with `replacement`, returning `(new_text, new_caret_byte)`.
pub fn replace_mention_token(text: &str, span: &MentionSpan, replacement: &str) -> (String, usize) {
    if span.start > span.end
        || span.end > text.len()
        || !text.is_char_boundary(span.start)
        || !text.is_char_boundary(span.end)
    {
        return (text.to_owned(), text.len());
    }
    let mut out = String::with_capacity(text.len() + replacement.len());
    out.push_str(&text[..span.start]);
    out.push_str(replacement);
    let new_caret = out.len();
    out.push_str(&text[span.end..]);
    (out, new_caret)
}

/// Builds a deduplicated participant roster (up to 32 active in-chat members) from loaded chat
/// messages (`main.zig:11496-11519`).
pub fn roster_from_messages(messages: &[Message]) -> Vec<MentionCandidate> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for msg in messages.iter().rev() {
        if out.len() >= 32 {
            break;
        }
        let name = if !msg.sender_name.is_empty() {
            msg.sender_name.clone()
        } else if !msg.sender_id.is_empty() {
            msg.sender_id.clone()
        } else {
            continue;
        };
        if seen.insert(name.clone()) {
            out.push(MentionCandidate {
                handle: msg.sender_id.clone(),
                display_name: name,
                in_chat: true,
                online: false,
                active: true,
            });
        }
    }
    out
}

/// Formats status bar summary while mention popup is open (`main.zig:11405-11415`).
pub fn format_mention_status(matches: &[MentionMatch]) -> String {
    if matches.is_empty() {
        return String::new();
    }
    let names: Vec<&str> = matches
        .iter()
        .take(3)
        .map(MentionMatch::display_label)
        .collect();
    if matches.len() > 3 {
        format!("Mention: {} +{}", names.join(", "), matches.len() - 3)
    } else {
        format!("Mention: {}", names.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mention_detection_ranking_here_and_provider_formatting() {
        assert_eq!(detect_mention_at_caret("mail@pau", 8), None);
        assert_eq!(detect_mention_at_caret("@a@b", 4), None);
        let bare = detect_mention_at_caret("hi @", 4).unwrap();
        assert_eq!((bare.start, bare.end, bare.query.as_str()), (3, 4, ""));
        let tok = detect_mention_at_caret("hi @pau rest", 7).unwrap();
        assert_eq!((tok.start, tok.end, tok.query.as_str()), (3, 7, "pau"));

        let long_q = format!("@{}", "a".repeat(65));
        assert_eq!(detect_mention_at_caret(&long_q, long_q.len()), None);

        let people = vec![
            MentionCandidate {
                handle: "U_OFF".into(),
                display_name: "Paula".into(),
                in_chat: false,
                online: false,
                active: false, // deactivated -> MUST be excluded!
            },
            MentionCandidate {
                handle: "U_OTHER".into(),
                display_name: "Other".into(),
                in_chat: false,
                online: false,
                active: true,
            },
            MentionCandidate {
                handle: "U_CHAT".into(),
                display_name: "Paula".into(),
                in_chat: true,
                online: true,
                active: true,
            },
        ];

        let ranked = rank_mentions("pau", &people, Provider::Slack);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].score, 140); // 100 + 30 + 10
        assert_eq!(
            format_mention_insertion(Provider::Slack, &ranked[0]),
            "<@U_CHAT>"
        );
        assert_eq!(
            format_mention_insertion(Provider::Whatsapp, &ranked[0]),
            "@U_CHAT"
        );
        assert_eq!(
            format_mention_insertion(Provider::Signal, &ranked[0]),
            "@Paula"
        );
        assert_eq!(
            format_mention_insertion(Provider::Telegram, &ranked[0]),
            "@Paula"
        );

        // Slack `@here` works safely even on an EMPTY roster (no panic!)
        let here_only = rank_mentions("her", &[], Provider::Slack);
        assert_eq!(here_only.len(), 1);
        assert!(here_only[0].is_here);
        assert_eq!(here_only[0].score, 60);
        assert_eq!(here_only[0].display_label(), "@here");
        assert_eq!(
            format_mention_insertion(Provider::Slack, &here_only[0]),
            "<!here>"
        );

        // Replacement preserves trailing text and advances caret
        let (replaced, caret) = replace_mention_token("hi @pau rest", &tok, "<@U_CHAT>");
        assert_eq!(replaced, "hi <@U_CHAT> rest");
        assert_eq!(caret, 12);
    }

    #[test]
    fn replace_mention_token_guards_mid_utf8_reversed_and_oob_spans() {
        let s = "Übung @ali";
        for bad in [
            MentionSpan {
                start: 1,
                end: 5,
                query: String::new(),
            },
            MentionSpan {
                start: 8,
                end: 3,
                query: String::new(),
            },
            MentionSpan {
                start: 0,
                end: 999,
                query: String::new(),
            },
        ] {
            let (out, caret) = replace_mention_token(s, &bad, "@alice");
            assert_eq!(out, s);
            assert_eq!(caret, s.len());
        }
    }
}
