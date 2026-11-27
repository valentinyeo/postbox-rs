use crate::{Message, Provider, ReadStatus};

pub const MAX_THREAD_HOPS: usize = 32;

/// Exact port of `slack.zig:1521-1523`.
pub fn is_thread_reply(msg_id: &str, thread_ts: &str) -> bool {
    !thread_ts.is_empty() && msg_id != thread_ts
}

/// Exact port of `slack.zig:1525-1527`.
pub fn in_thread(msg_id: &str, thread_ts: &str, selected_root: &str) -> bool {
    !selected_root.is_empty() && (msg_id == selected_root || thread_ts == selected_root)
}

/// Exact port of `slack.zig:1547-1549`: Slack defaults to folded threads; other providers inline.
pub fn replies_fold(chat_override: Option<bool>, provider: Provider) -> bool {
    chat_override.unwrap_or(provider == Provider::Slack)
}

/// Exact port of `slack.zig:1529-1532`.
pub fn replies_line(count: usize) -> String {
    match count {
        0 => String::new(),
        1 => "1 reply".to_owned(),
        n => format!("{n} replies"),
    }
}

/// Resolves the root message ID of a reply chain up to `MAX_THREAD_HOPS` (32) with cycle detection
/// (`main.zig:25511-25528`).
pub fn local_thread_root<'a>(
    messages: &'a [Message],
    message: &'a Message,
    provider: Provider,
) -> Option<&'a str> {
    let mut current = message;
    let mut hops = 0usize;
    let mut visited: [&str; MAX_THREAD_HOPS] = [""; MAX_THREAD_HOPS];

    while hops < MAX_THREAD_HOPS {
        let Some(parent_id) = current.reply_to_id.as_deref().filter(|s| !s.is_empty()) else {
            break;
        };
        if parent_id == current.msg_id {
            break;
        }
        visited[hops] = current.msg_id.as_str();
        hops += 1;
        if let Some(parent) = messages.iter().find(|m| m.msg_id == parent_id) {
            if visited[..hops].contains(&parent.msg_id.as_str()) {
                return None;
            }
            current = parent;
        } else {
            if provider == Provider::Slack {
                return Some(parent_id);
            }
            break;
        }
    }

    if current.msg_id == message.msg_id {
        None
    } else {
        Some(current.msg_id.as_str())
    }
}

/// Exact port of `IGSH-1898` (`unified_snapshot.zig:90`, `main.zig:25578-25580`):
/// broadcast replies are Slack-only, require a non-empty `reply_to`, and are text-only (no attachment).
pub fn can_broadcast(provider: Provider, reply_to: Option<&str>, has_attachment: bool) -> bool {
    provider == Provider::Slack && reply_to.is_some_and(|s| !s.is_empty()) && !has_attachment
}

/// Exact port of `main.zig:25530-25536`: determines if `message` is visible in the main channel timeline.
pub fn channel_message_visible(
    message: &Message,
    messages: &[Message],
    fold_replies: bool,
    provider: Provider,
) -> bool {
    if !fold_replies {
        return true;
    }
    if message.reply_broadcast {
        return true;
    }
    if message.from_me
        && matches!(
            message.read_status,
            ReadStatus::Sending | ReadStatus::Failed
        )
    {
        return true;
    }
    let Some(parent) = message.reply_to_id.as_deref().filter(|s| !s.is_empty()) else {
        return true;
    };
    if provider == Provider::Slack && is_thread_reply(&message.msg_id, parent) {
        return false;
    }
    local_thread_root(messages, message, provider).is_none()
}

/// Combines locally loaded folded thread replies with optional server reply count.
pub fn thread_reply_count(
    messages: &[Message],
    parent_id: &str,
    provider: Provider,
    fold_replies: bool,
    server_reply_count: usize,
) -> usize {
    if !fold_replies {
        return server_reply_count;
    }
    let local_count = messages
        .iter()
        .filter(|m| {
            m.msg_id != parent_id && local_thread_root(messages, m, provider) == Some(parent_id)
        })
        .count();
    local_count.max(server_reply_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChatKey;

    fn mk_msg(id: &str, reply_to: Option<&str>) -> Message {
        Message {
            chat: ChatKey::new(Provider::Slack, "T1", "slack:T1:C1"),
            msg_id: id.into(),
            sender_id: "U1".into(),
            sender_name: "Ada".into(),
            from_me: false,
            ts: "2026-11-26T10:00:00Z".into(),
            body: id.into(),
            reply_to_id: reply_to.map(str::to_owned),
            reply_broadcast: false,
            media_kind: None,
            mime_type: None,
            filename: None,
            reactions: Vec::new(),
            read_status: ReadStatus::None,
            deleted: false,
        }
    }

    #[test]
    fn thread_root_handles_chains_cycles_and_slack_external_roots() {
        let msgs = vec![
            mk_msg("root", None),
            mk_msg("r1", Some("root")),
            mk_msg("r2", Some("r1")),
        ];
        assert_eq!(local_thread_root(&msgs, &msgs[0], Provider::Slack), None);
        assert_eq!(
            local_thread_root(&msgs, &msgs[2], Provider::Slack),
            Some("root")
        );

        // 2-cycle A -> B -> A returns None without infinite loop.
        let cycle = vec![mk_msg("A", Some("B")), mk_msg("B", Some("A"))];
        assert_eq!(local_thread_root(&cycle, &cycle[0], Provider::Slack), None);

        // Lasso cycle A -> B -> C -> B returns None.
        let lasso = vec![
            mk_msg("A", Some("B")),
            mk_msg("B", Some("C")),
            mk_msg("C", Some("B")),
        ];
        assert_eq!(local_thread_root(&lasso, &lasso[0], Provider::Slack), None);

        // Slack external parent not in loaded window returns parent ID.
        let ext = vec![mk_msg("child", Some("older_root"))];
        assert_eq!(
            local_thread_root(&ext, &ext[0], Provider::Slack),
            Some("older_root")
        );
        assert_eq!(local_thread_root(&ext, &ext[0], Provider::Whatsapp), None);
    }

    #[test]
    fn broadcast_and_folding_rules() {
        assert!(can_broadcast(Provider::Slack, Some("1700.1"), false));
        assert!(!can_broadcast(Provider::Slack, None, false));
        assert!(!can_broadcast(Provider::Slack, Some("1700.1"), true));
        assert!(!can_broadcast(Provider::Whatsapp, Some("m1"), false));

        assert!(replies_fold(None, Provider::Slack));
        assert!(!replies_fold(None, Provider::Whatsapp));
        assert!(!replies_fold(Some(false), Provider::Slack));
        assert_eq!(replies_line(0), "");
        assert_eq!(replies_line(1), "1 reply");
        assert_eq!(replies_line(4), "4 replies");
    }
}
