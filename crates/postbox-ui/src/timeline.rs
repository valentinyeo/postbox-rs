use postbox_core::outbox::{OutboxPhase, OutboxRecord};
use postbox_core::read::ReceiptCapability;
use postbox_core::threads::{channel_message_visible, thread_reply_count};
use postbox_core::{Message, Provider, ReadStatus};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboxBadge {
    Sending,
    Failed { reason: String, ambiguous: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadTickDisplay {
    None,
    SingleMuted,
    DoubleMuted,
    DoubleRead,
}

impl ReadTickDisplay {
    pub fn symbol(self) -> &'static str {
        match self {
            Self::None => "",
            Self::SingleMuted => "✓",
            Self::DoubleMuted | Self::DoubleRead => "✓✓",
        }
    }
}

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
        thread_reply_count: usize,
        outbox_status: Option<OutboxBadge>,
        read_ticks: ReadTickDisplay,
    },
}

pub struct TimelineOptions<'a> {
    pub provider: Provider,
    pub is_group: bool,
    pub fold_replies: bool,
    pub outbox_by_client_id: &'a BTreeMap<String, &'a OutboxRecord>,
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

fn compute_read_tick_display(msg: &Message, provider: Provider, is_group: bool) -> ReadTickDisplay {
    if !msg.from_me || msg.deleted {
        return ReadTickDisplay::None;
    }
    let cap = ReceiptCapability::for_chat(provider, is_group);
    if matches!(
        cap,
        ReceiptCapability::Slack | ReceiptCapability::Telegram | ReceiptCapability::Signal
    ) {
        return ReadTickDisplay::None;
    }
    match msg.read_status {
        ReadStatus::Sent => ReadTickDisplay::SingleMuted,
        ReadStatus::Delivered => ReadTickDisplay::DoubleMuted,
        ReadStatus::Read => ReadTickDisplay::DoubleRead,
        ReadStatus::None | ReadStatus::Sending | ReadStatus::Failed => ReadTickDisplay::None,
    }
}

fn compute_outbox_badge(
    msg: &Message,
    outbox_by_client_id: &BTreeMap<String, &OutboxRecord>,
) -> Option<OutboxBadge> {
    if let Some(rec) = outbox_by_client_id.get(&msg.msg_id) {
        return match rec.phase {
            OutboxPhase::Ready | OutboxPhase::Uncertain => Some(OutboxBadge::Sending),
            OutboxPhase::Held => Some(OutboxBadge::Failed {
                reason: rec
                    .failure_reason
                    .clone()
                    .unwrap_or_else(|| "send failed".to_owned()),
                ambiguous: rec.ambiguous,
            }),
        };
    }
    match msg.read_status {
        ReadStatus::Sending => Some(OutboxBadge::Sending),
        ReadStatus::Failed => Some(OutboxBadge::Failed {
            reason: "send failed".to_owned(),
            ambiguous: false,
        }),
        _ => None,
    }
}

/// Builds chronological timeline items (date dividers + grouped message rows) with default options.
pub fn build_timeline_items(
    messages: &[Message],
    selected_idx: Option<usize>,
    marked_ids: &BTreeSet<String>,
) -> Vec<TimelineItem> {
    let empty_outbox = BTreeMap::new();
    let provider = messages
        .first()
        .map(|m| m.chat.provider)
        .unwrap_or(Provider::Whatsapp);
    build_timeline_items_with_options(
        messages,
        selected_idx,
        marked_ids,
        &TimelineOptions {
            provider,
            is_group: false,
            fold_replies: false,
            outbox_by_client_id: &empty_outbox,
        },
    )
}

/// Builds chronological timeline items honoring Slack folded threads, outbox badges, and read ticks.
pub fn build_timeline_items_with_options(
    messages: &[Message],
    selected_idx: Option<usize>,
    marked_ids: &BTreeSet<String>,
    opts: &TimelineOptions<'_>,
) -> Vec<TimelineItem> {
    let visible: Vec<(usize, &Message)> = messages
        .iter()
        .enumerate()
        .filter(|(_, msg)| channel_message_visible(msg, messages, opts.fold_replies, opts.provider))
        .collect();

    let mut items = Vec::with_capacity(visible.len() + 4);
    let mut last_date: Option<&str> = None;

    for (vis_pos, &(orig_idx, msg)) in visible.iter().enumerate() {
        let day = date_prefix(&msg.ts);
        if last_date != Some(day) {
            items.push(TimelineItem::DateDivider {
                date_label: day.to_owned(),
            });
            last_date = Some(day);
        }
        let grouped_with_prev = vis_pos > 0 && can_group(visible[vis_pos - 1].1, msg);
        let grouped_with_next =
            vis_pos + 1 < visible.len() && can_group(msg, visible[vis_pos + 1].1);

        let reply_count = if opts.fold_replies {
            thread_reply_count(messages, &msg.msg_id, opts.provider, true, 0)
        } else {
            0
        };
        let outbox_status = compute_outbox_badge(msg, opts.outbox_by_client_id);
        let read_ticks = if outbox_status.is_some() {
            ReadTickDisplay::None
        } else {
            compute_read_tick_display(msg, opts.provider, opts.is_group)
        };

        items.push(TimelineItem::MessageRow {
            index: orig_idx,
            message: Box::new(msg.clone()),
            grouped_with_prev,
            grouped_with_next,
            selected: selected_idx == Some(orig_idx),
            marked: marked_ids.contains(&msg.msg_id),
            thread_reply_count: reply_count,
            outbox_status,
            read_ticks,
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
    use postbox_core::{ChatKey, MediaKind};

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
            file_size: None,
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
                assert!(!grouped_with_next);
                assert!(selected);
                assert!(marked);
            }
            _ => panic!("expected message row"),
        }
    }

    #[test]
    fn folded_slack_threads_hide_replies_and_count_on_root() {
        let root = msg("1700.1", "U1", false, "2026-11-26T10:00:00Z", "Root");
        let mut r1 = msg("1700.2", "U2", false, "2026-11-26T10:01:00Z", "Reply 1");
        r1.reply_to_id = Some("1700.1".into());
        let mut r2 = msg("1700.3", "U1", true, "2026-11-26T10:02:00Z", "Broadcast");
        r2.reply_to_id = Some("1700.1".into());
        r2.reply_broadcast = true;

        let empty = BTreeMap::new();
        let items = build_timeline_items_with_options(
            &[root, r1, r2],
            None,
            &BTreeSet::new(),
            &TimelineOptions {
                provider: Provider::Slack,
                is_group: true,
                fold_replies: true,
                outbox_by_client_id: &empty,
            },
        );
        // Divider + root + broadcast (r1 folded away!)
        assert_eq!(items.len(), 3);
        match &items[1] {
            TimelineItem::MessageRow {
                thread_reply_count,
                read_ticks,
                ..
            } => {
                assert_eq!(*thread_reply_count, 2);
                assert_eq!(*read_ticks, ReadTickDisplay::None);
            }
            _ => panic!("expected root row"),
        }
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
