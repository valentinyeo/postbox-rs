use crate::error::HubError;
use crate::request::{MAX_ROWS, valid_id};
use crate::types::{HubChat, HubMessage};
use postbox_core::views::compare_message_order;
use postbox_core::{Provider, Reaction};
use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::collections::HashSet;

/// Extracts the prefix before the first `:` (or `""` if none) for `@lid` same-account check.
fn account_prefix(jid: &str) -> &str {
    jid.split_once(':').map(|(prefix, _)| prefix).unwrap_or("")
}

/// Exact port of `hub.zig:254-258`: allows `@lid -> @s.whatsapp.net` ONLY within the same account.
pub fn lid_alias(row_chat: &str, expected_chat: &str) -> bool {
    row_chat.ends_with("@lid")
        && expected_chat.ends_with("@s.whatsapp.net")
        && account_prefix(row_chat) == account_prefix(expected_chat)
}

/// Exact port of `hub.zig:318-334`: derives the UI view account for a Hub chat row.
pub fn view_account(
    row: &Map<String, Value>,
    provider: Provider,
    jid: &str,
    fallback: &str,
) -> String {
    if provider == Provider::Signal && jid.starts_with("signal:") {
        return "signal".to_owned();
    }
    if provider == Provider::Telegram && jid.starts_with("telegram:") {
        return "telegram".to_owned();
    }
    if let Some(acc) = row
        .get("account")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return acc.to_owned();
    }
    if provider == Provider::Whatsapp
        && let Some((prefix, rest)) = jid.split_once(':')
        && !prefix.is_empty()
        && rest.contains('@')
    {
        return prefix.to_owned();
    }
    if provider == Provider::Slack
        && let Some(rest) = jid.strip_prefix("slack:")
        && let Some((team, channel)) = rest.split_once(':')
        && !team.is_empty()
        && !channel.is_empty()
    {
        return team.to_owned();
    }
    fallback.to_owned()
}

pub fn source_account(row: &Map<String, Value>, fallback: &str) -> String {
    row.get("account")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

pub fn saved_messages(row: &Map<String, Value>, provider: Provider) -> bool {
    provider == Provider::Telegram && row.get("saved").and_then(Value::as_bool) == Some(true)
}

fn str_field(obj: &Map<String, Value>, keys: &[&str]) -> String {
    for &k in keys {
        if let Some(s) = obj.get(k).and_then(Value::as_str) {
            return s.to_owned();
        }
    }
    String::new()
}

fn opt_nonempty_str(obj: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    for &k in keys {
        if let Some(s) = obj.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()) {
            return Some(s.to_owned());
        }
    }
    None
}

fn bool_field(obj: &Map<String, Value>, keys: &[&str]) -> bool {
    for &k in keys {
        if let Some(b) = obj.get(k).and_then(Value::as_bool) {
            return b;
        }
    }
    false
}

fn i64_field(obj: &Map<String, Value>, keys: &[&str]) -> i64 {
    for &k in keys {
        if let Some(n) = obj.get(k).and_then(Value::as_i64) {
            return n;
        }
    }
    0
}

fn parse_strict_provider(raw: &str) -> Result<Provider, HubError> {
    match raw {
        "whatsapp" => Ok(Provider::Whatsapp),
        "slack" => Ok(Provider::Slack),
        "telegram" => Ok(Provider::Telegram),
        "signal" => Ok(Provider::Signal),
        _ => Err(HubError::BadProvider),
    }
}

/// Normalizes a Hub `/v1/wazig/chats` JSON array (`hub.zig:240-270`).
pub fn normalize_chats(raw: &str, fallback_account: &str) -> Result<Vec<HubChat>, HubError> {
    let val: Value = serde_json::from_str(raw).map_err(|_| HubError::BadResponse)?;
    let arr = val.as_array().ok_or(HubError::BadResponse)?;
    if arr.len() > MAX_ROWS {
        return Err(HubError::BadResponse);
    }

    let mut seen = HashSet::with_capacity(arr.len());
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let obj = item.as_object().ok_or(HubError::BadResponse)?;
        let jid = obj
            .get("jid")
            .and_then(Value::as_str)
            .ok_or(HubError::InvalidId)?;
        valid_id(jid)?;
        if !seen.insert(jid.to_owned()) {
            return Err(HubError::DuplicateId);
        }
        let provider_raw = obj
            .get("provider")
            .and_then(Value::as_str)
            .ok_or(HubError::BadProvider)?;
        let provider = parse_strict_provider(provider_raw)?;

        let view_acc = view_account(obj, provider, jid, fallback_account);
        let src_acc = source_account(obj, fallback_account);
        let team_name = str_field(obj, &["team_name", "TeamName", "workspace_name"]);
        let name = str_field(obj, &["name", "Name", "title"]);
        let last_message_ts = str_field(
            obj,
            &["last_message_ts", "LastMessageTS", "last_msg_ts", "ts"],
        );
        let last_message_preview = str_field(
            obj,
            &["last_message_preview", "LastMessagePreview", "preview"],
        );
        let unread_count = i64_field(obj, &["unread_count", "UnreadCount"]).max(0) as u32;
        let unread = bool_field(obj, &["unread", "Unread"]) || unread_count > 0;
        let archived = bool_field(obj, &["archived", "Archived"]);
        let kind = str_field(obj, &["kind", "Kind"]);
        let kind_is_group = {
            let lower = kind.to_ascii_lowercase();
            lower.starts_with("group")
                || lower.starts_with("channel")
                || lower.starts_with("supergroup")
        };
        let is_group = bool_field(obj, &["is_group", "IsGroup", "group"])
            || kind_is_group
            || jid.ends_with("@g.us")
            || jid.starts_with("signal:group:")
            || jid.starts_with("telegram:group:")
            || jid.starts_with("telegram:channel:")
            || (provider == Provider::Slack && jid.contains(":C"));
        let saved = saved_messages(obj, provider);
        let avatar_hash = opt_nonempty_str(obj, &["avatar_hash", "AvatarHash"]);

        out.push(HubChat {
            jid: jid.to_owned(),
            provider,
            view_account: view_acc,
            source_account: src_acc,
            team_name,
            name,
            last_message_ts,
            last_message_preview,
            unread,
            unread_count,
            archived,
            is_group,
            saved,
            avatar_hash,
            raw: item.clone(),
        });
    }
    Ok(out)
}

/// Merges `/v1/wazig/chats?archived=true` and `?archived=false` preserving archived rows first
/// without mutating provider `archived` flags (`hub.zig:272-297`).
pub fn merge_archived_chats(
    archived_raw: &str,
    inbox_raw: &str,
    fallback_account: &str,
) -> Result<Vec<HubChat>, HubError> {
    let mut out = normalize_chats(archived_raw, fallback_account)?;
    let inbox = normalize_chats(inbox_raw, fallback_account)?;
    let mut seen: HashSet<String> = out.iter().map(|c| c.jid.clone()).collect();
    for chat in inbox {
        if seen.insert(chat.jid.clone()) {
            out.push(chat);
        }
    }
    Ok(out)
}

/// Maps common standard Slack reaction shortcodes to Unicode emoji (`slack_emoji.tsv` subset for M2).
pub fn standard_slack_emoji(name: &str) -> Option<&'static str> {
    let base = name.split("::").next().unwrap_or(name);
    match base {
        "+1" | "thumbsup" => Some("👍"),
        "-1" | "thumbsdown" => Some("👎"),
        "heart" => Some("❤️"),
        "fire" => Some("🔥"),
        "tada" => Some("🎉"),
        "eyes" => Some("👀"),
        "white_check_mark" => Some("✅"),
        "rocket" => Some("🚀"),
        "smile" => Some("😄"),
        "joy" => Some("😂"),
        "100" => Some("💯"),
        "tea" => Some("🍵"),
        "coffee" => Some("☕"),
        "pray" => Some("🙏"),
        "clap" => Some("👏"),
        "thinking_face" => Some("🤔"),
        "wave" => Some("👋"),
        "ok_hand" => Some("👌"),
        "sparkles" => Some("✨"),
        "star" => Some("⭐"),
        "warning" => Some("⚠️"),
        "x" => Some("❌"),
        "check" | "heavy_check_mark" => Some("✔️"),
        "raised_hands" => Some("🙌"),
        "muscle" => Some("💪"),
        "sunglasses" => Some("😎"),
        "sob" => Some("😭"),
        "grinning" => Some("😀"),
        "wink" => Some("😉"),
        "cry" => Some("😢"),
        "party_popper" => Some("🎉"),
        _ => None,
    }
}

/// Strips the `"(message)"` placeholder stub (`message_filter.zig:11-16`).
pub fn strip_placeholder(text: &str) -> &str {
    if text == "(message)" { "" } else { text }
}

/// Exact port of `message_filter.zig:28-31`: filters out empty protocol stubs.
pub fn is_real_message(msg: &HubMessage) -> bool {
    !msg.msg_id.is_empty()
        && (!msg.effective_text.is_empty()
            || !msg.media_type.is_empty()
            || msg.filename.as_deref().is_some_and(|s| !s.is_empty())
            || !msg.reaction_to_id.is_empty()
            || msg.revoked)
}

fn parse_reaction_emoji_string(raw: &str) -> Vec<Reaction> {
    if raw.trim().is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for chunk in raw.split("  ") {
        let trimmed = chunk.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        if let Some(emoji) = parts.next() {
            let count = parts
                .next()
                .and_then(|c| c.parse::<u32>().ok())
                .unwrap_or(1)
                .max(1);
            out.push(Reaction {
                emoji: emoji.to_owned(),
                count,
                from_me: false,
            });
        }
    }
    out
}

/// Normalizes a Hub `/v1/wazig/messages` JSON array (`hub.zig:173-198, 254-264`):
/// - Enforces `<= 500` rows, `valid_id`, and unique `MsgID`s.
/// - Same-account `@lid -> @s.whatsapp.net` aliasing (rejects cross-account or `@g.us` with `WrongChat`).
/// - Slack `reactions` & `attachments` synthesis when `expected_chat.starts_with("slack:")`.
/// - Always strips untrusted `SlackEmojiPaths`, `LocalPath`, `local_path`.
pub fn normalize_messages(raw: &str, expected_chat: &str) -> Result<Vec<HubMessage>, HubError> {
    valid_id(expected_chat)?;
    let val: Value = serde_json::from_str(raw).map_err(|_| HubError::BadResponse)?;
    let arr = val.as_array().ok_or(HubError::BadResponse)?;
    if arr.len() > MAX_ROWS {
        return Err(HubError::BadResponse);
    }

    let is_slack = expected_chat.starts_with("slack:");
    let mut seen = HashSet::with_capacity(arr.len());
    let mut out = Vec::with_capacity(arr.len());

    for item in arr {
        let mut obj = item.as_object().ok_or(HubError::BadResponse)?.clone();
        let msg_id = obj
            .get("MsgID")
            .and_then(Value::as_str)
            .ok_or(HubError::InvalidId)?
            .to_owned();
        valid_id(&msg_id)?;
        if !seen.insert(msg_id.clone()) {
            return Err(HubError::DuplicateId);
        }

        let row_chat = obj
            .get("ChatJID")
            .and_then(Value::as_str)
            .ok_or(HubError::WrongChat)?;
        if row_chat != expected_chat {
            if lid_alias(row_chat, expected_chat) {
                obj.insert(
                    "ChatJID".to_owned(),
                    Value::String(expected_chat.to_owned()),
                );
            } else {
                return Err(HubError::WrongChat);
            }
        }

        // Always strip untrusted local filesystem paths from server responses.
        obj.remove("SlackEmojiPaths");
        obj.remove("LocalPath");
        obj.remove("local_path");

        let mut reactions = Vec::new();
        if is_slack {
            // Synthesize Slack reactions into ReactionEmoji when ReactionEmoji is empty.
            let existing_reaction_emoji = obj
                .get("ReactionEmoji")
                .and_then(Value::as_str)
                .unwrap_or("");
            if existing_reaction_emoji.is_empty()
                && let Some(r_arr) = obj.get("reactions").and_then(Value::as_array)
            {
                let mut formatted = Vec::new();
                for r in r_arr.iter().take(24) {
                    let Some(r_obj) = r.as_object() else {
                        continue;
                    };
                    let name = r_obj.get("name").and_then(Value::as_str).unwrap_or("");
                    let count = r_obj.get("count").and_then(Value::as_i64).unwrap_or(0);
                    if name.is_empty() || count <= 0 {
                        continue;
                    }
                    let token = if let Some(u) = standard_slack_emoji(name) {
                        u.to_owned()
                    } else {
                        format!(":{name}:")
                    };
                    formatted.push(format!("{token} {count}"));
                    reactions.push(Reaction {
                        emoji: token,
                        count: count as u32,
                        from_me: r_obj
                            .get("from_me")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    });
                }
                if !formatted.is_empty() {
                    obj.insert(
                        "ReactionEmoji".to_owned(),
                        Value::String(formatted.join("  ")),
                    );
                }
            }

            // Synthesize Slack image/GIF attachments when MediaType is empty.
            let existing_media = obj.get("MediaType").and_then(Value::as_str).unwrap_or("");
            if existing_media.is_empty()
                && let Some(atts) = obj.get("attachments").and_then(Value::as_array)
                && let Some(att) = atts.iter().find_map(|a| {
                    let o = a.as_object()?;
                    let url = o.get("image_url").and_then(Value::as_str)?;
                    if url.is_empty() { None } else { Some((o, url)) }
                })
            {
                let (att_obj, url) = att;
                let service = att_obj
                    .get("service_name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let is_gif = url.contains(".gif") || service.eq_ignore_ascii_case("Giphy");
                let mime = att_obj
                    .get("image_mimetype")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        if is_gif {
                            "image/gif".to_owned()
                        } else {
                            "image/png".to_owned()
                        }
                    });
                let fname = if is_gif {
                    "attachment.gif".to_owned()
                } else {
                    "attachment.png".to_owned()
                };
                obj.insert("MediaType".to_owned(), Value::String("image".to_owned()));
                obj.insert("MimeType".to_owned(), Value::String(mime));
                obj.insert("Filename".to_owned(), Value::String(fname));
            }
        }

        let display_text = str_field(&obj, &["DisplayText", "display_text"]);
        let text = str_field(&obj, &["Text", "text"]);
        let stripped_display = strip_placeholder(&display_text);
        let stripped_text = strip_placeholder(&text);
        let effective_text = if !stripped_display.is_empty() {
            stripped_display.to_owned()
        } else {
            stripped_text.to_owned()
        };

        let reaction_emoji = str_field(&obj, &["ReactionEmoji", "reaction_emoji"]);
        if reactions.is_empty() && !reaction_emoji.is_empty() {
            reactions = parse_reaction_emoji_string(&reaction_emoji);
        }

        let sender_id = str_field(&obj, &["SenderJID", "Sender", "sender_id"]);
        let sender_name = opt_nonempty_str(&obj, &["SenderName", "PushName", "sender_name"])
            .unwrap_or_else(|| sender_id.clone());

        let file_size = u64_size_field(
            &obj,
            &[
                "FileSize",
                "FileLength",
                "MediaSize",
                "Size",
                "file_size",
                "file_length",
                "media_size",
                "size",
            ],
        );

        out.push(HubMessage {
            msg_id,
            chat_jid: expected_chat.to_owned(),
            sender_id,
            sender_name,
            from_me: bool_field(&obj, &["FromMe", "from_me"]),
            ts: str_field(&obj, &["Timestamp", "ts"]),
            display_text,
            text,
            effective_text,
            reply_to_id: opt_nonempty_str(&obj, &["ReplyToID", "reply_to_id"]),
            thread_ts: opt_nonempty_str(&obj, &["ThreadTS", "thread_ts"]),
            reply_broadcast: bool_field(&obj, &["ReplyBroadcast", "reply_broadcast"]),
            media_type: str_field(&obj, &["MediaType", "media_type"]),
            mime_type: opt_nonempty_str(&obj, &["MimeType", "mime_type"]),
            filename: opt_nonempty_str(&obj, &["Filename", "filename"]),
            file_size,
            reaction_to_id: str_field(&obj, &["ReactionToID", "reaction_to_id"]),
            reaction_emoji,
            reactions,
            revoked: bool_field(&obj, &["Revoked", "revoked", "Deleted", "deleted"]),
            delivered_count: i64_field(
                &obj,
                &[
                    "DeliveredTo",
                    "Delivered",
                    "DeliveredCount",
                    "delivered_to",
                    "delivered",
                    "delivered_count",
                ],
            )
            .clamp(0, 1000),
            read_by_count: i64_field(&obj, &["ReadBy", "ReadByCount", "read_by", "read_by_count"])
                .clamp(0, 1000),
            raw: Value::Object(obj),
        });
    }

    Ok(out)
}

fn u64_size_field(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<u64> {
    for &key in keys {
        let Some(val) = obj.get(key) else {
            continue;
        };
        if let Some(n) = val.as_u64() {
            if n > 0 {
                return Some(n);
            }
        } else if let Some(s) = val.as_str()
            && let Ok(n) = s.trim().parse::<u64>()
            && n > 0
        {
            return Some(n);
        }
    }
    None
}

/// Exact port of `hub.zig:70-99`: merges `second` into `first` deduplicating by `msg_id`,
/// sorts descending (newest first via RFC3339 fractional-zero + `msg_id`), and returns the
/// count of newly added messages.
pub fn unite_messages(first: &mut Vec<HubMessage>, second: Vec<HubMessage>) -> usize {
    let mut seen: HashSet<String> = first.iter().map(|m| m.msg_id.clone()).collect();
    let mut added = 0usize;
    for msg in second {
        if !msg.msg_id.is_empty() && seen.insert(msg.msg_id.clone()) {
            first.push(msg);
            added += 1;
        }
    }
    first.sort_by(
        |a, b| match compare_message_order(&a.ts, &a.msg_id, &b.ts, &b.msg_id) {
            Ordering::Greater => Ordering::Less,
            Ordering::Less => Ordering::Greater,
            Ordering::Equal => Ordering::Equal,
        },
    );
    added
}

/// Exact port of `hub.zig:382-399`: selects the newest incoming non-reaction `MsgID` for
/// Signal/Telegram `/v1/wazig/mark-read` (expects newest-first order as returned by Hub).
pub fn read_message_id(messages: &[HubMessage]) -> Result<String, HubError> {
    for msg in messages {
        if !msg.reaction_to_id.is_empty() || msg.from_me {
            continue;
        }
        valid_id(&msg.msg_id)?;
        return Ok(msg.msg_id.clone());
    }
    Err(HubError::NoIncomingMessage)
}
