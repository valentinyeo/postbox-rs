use postbox_core::Message;
use std::collections::BTreeSet;

/// A single virtualized row inside the variable-height message timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineItem {
    DateDivider {
        date_label: String,
    },
    MessageRow {
        index: usize,
        message: Box<Message>,
        grouped_with_prev: bool,
        grouped_with_next: bool,
        selected: bool,
        marked: bool,
    },
}

/// Extracts the `YYYY-MM-DD` date prefix from an ISO-8601/RFC3339 timestamp string.
pub fn date_prefix(ts: &str) -> &str {
    if ts.len() >= 10 && ts.is_char_boundary(10) {
        &ts[..10]
    } else if !ts.is_empty() {
        ts
    } else {
        "Unknown date"
    }
}

/// Extracts `HH:MM` from an RFC3339 timestamp (`YYYY-MM-DDTHH:MM:...`), falling back cleanly.
pub fn format_time_hm(ts: &str) -> String {
    if let Some((_, time_part)) = ts.split_once('T')
        && time_part.len() >= 5
        && time_part.is_char_boundary(5)
    {
        return time_part[..5].to_owned();
    }
    if ts.len() >= 5 && ts.is_char_boundary(5) {
        return ts[..5].to_owned();
    }
    ts.to_owned()
}

fn can_group(a: &Message, b: &Message) -> bool {
    !a.deleted
        && !b.deleted
        && a.from_me == b.from_me
        && a.sender_id == b.sender_id
        && date_prefix(&a.ts) == date_prefix(&b.ts)
        && a.reply_to_id.is_none()
        && b.reply_to_id.is_none()
}

/// Builds chronological timeline items (date dividers + grouped message rows) from
/// an oldest-to-newest message slice.
pub fn build_timeline_items(
    messages: &[Message],
    selected_idx: Option<usize>,
    marked_ids: &BTreeSet<String>,
) -> Vec<TimelineItem> {
    let mut items = Vec::with_capacity(messages.len() + 4);
    let mut last_date: Option<&str> = None;

    for (idx, msg) in messages.iter().enumerate() {
        let day = date_prefix(&msg.ts);
        if last_date != Some(day) {
            items.push(TimelineItem::DateDivider {
                date_label: day.to_owned(),
            });
            last_date = Some(day);
        }
        let grouped_with_prev = idx > 0 && can_group(&messages[idx - 1], msg);
        let grouped_with_next = idx + 1 < messages.len() && can_group(msg, &messages[idx + 1]);
        items.push(TimelineItem::MessageRow {
            index: idx,
            message: Box::new(msg.clone()),
            grouped_with_prev,
            grouped_with_next,
            selected: selected_idx == Some(idx),
            marked: marked_ids.contains(&msg.msg_id),
        });
    }

    items
}

/// Extracts `http://` / `https://` URLs and any media attachment filename from a message
/// for `Ctrl+O` ("Open links and media in selected message").
pub fn extract_links_and_media(message: &Message) -> Vec<String> {
    let mut out = Vec::new();
    for token in message.body.split_whitespace() {
        let cleaned = token
            .trim_start_matches(['(', '[', '<', '"', '\''])
            .trim_end_matches(['.', '!', '?', ',', ';', ')', ']', '>', '"', '\'']);
        if (cleaned.starts_with("https://") || cleaned.starts_with("http://"))
            && cleaned.len() > 8
            && !out.iter().any(|existing| existing == cleaned)
        {
            out.push(cleaned.to_owned());
        }
    }
    if let Some(filename) = message.filename.as_deref().filter(|f| !f.is_empty())
        && !out.iter().any(|existing| existing == filename)
    {
        out.push(filename.to_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::{ChatKey, MediaKind, Provider, ReadStatus};

    fn msg(id: &str, sender: &str, from_me: bool, ts: &str, body: &str) -> Message {
        Message {
            chat: ChatKey::new(Provider::Slack, "T1", "C1"),
            msg_id: id.into(),
            sender_id: sender.into(),
            sender_name: sender.into(),
            from_me,
            ts: ts.into(),
            body: body.into(),
            reply_to_id: None,
            reply_broadcast: false,
            media_kind: None,
            mime_type: None,
            filename: None,
            reactions: vec![],
            read_status: ReadStatus::Read,
            deleted: false,
        }
    }

    #[test]
    fn timeline_inserts_date_dividers_and_groups_consecutive_messages() {
        let m1 = msg("1", "u1", false, "2026-11-25T10:00:00Z", "Hello");
        let m2 = msg("2", "u1", false, "2026-11-25T10:01:00Z", "Still me");
        let mut m3 = msg("3", "u1", false, "2026-11-25T10:02:00Z", "Replying");
        m3.reply_to_id = Some("1".into());
        let m4 = msg("4", "u1", false, "2026-11-26T09:00:00Z", "Next day");

        let mut marked = BTreeSet::new();
        marked.insert("2".to_owned());

        let items = build_timeline_items(&[m1, m2, m3, m4], Some(1), &marked);
        assert_eq!(items.len(), 6);
        assert_eq!(
            items[0],
            TimelineItem::DateDivider {
                date_label: "2026-11-25".into()
            }
        );
        match &items[1] {
            TimelineItem::MessageRow {
                grouped_with_prev,
                grouped_with_next,
                selected,
                marked,
                ..
            } => {
                assert!(!grouped_with_prev);
                assert!(grouped_with_next);
                assert!(!selected);
                assert!(!marked);
            }
            _ => panic!("expected message row"),
        }
        match &items[2] {
            TimelineItem::MessageRow {
                grouped_with_prev,
                grouped_with_next,
                selected,
                marked,
                ..
            } => {
                assert!(grouped_with_prev);
                assert!(!grouped_with_next); // m3 has reply_to_id
                assert!(selected);
                assert!(marked);
            }
            _ => panic!("expected message row"),
        }
        assert_eq!(
            items[4],
            TimelineItem::DateDivider {
                date_label: "2026-11-26".into()
            }
        );
    }

    #[test]
    fn extract_links_and_media_finds_urls_and_attachment() {
        let mut m = msg(
            "1",
            "u1",
            false,
            "2026-11-26T12:34:00Z",
            "Check https://example.com/docs, and (http://127.0.0.1:8080/test).",
        );
        m.media_kind = Some(MediaKind::Document);
        m.filename = Some("spec.pdf".into());

        let links = extract_links_and_media(&m);
        assert_eq!(
            links,
            vec![
                "https://example.com/docs",
                "http://127.0.0.1:8080/test",
                "spec.pdf",
            ]
        );
        assert_eq!(format_time_hm(&m.ts), "12:34");
    }
}
