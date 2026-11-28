//! Reconciles locally-keyed optimistic messages against their synced server copies (PORU-25).
//!
//! When Hub confirms a send without a server `msg_id`, the confirmed message is stored under the
//! local 32-hex `client_id`. The next sync then ingests the real WhatsApp/Slack row under its
//! server `msg_id`, which used to leave both rows in the timeline. Pairing is strict: one local
//! row, one server row, same direction, close in time, same content. No server row is ever
//! merged away, and failed sends are never touched.

use crate::archive::unix_seconds;
use crate::outbox::is_valid_client_id;
use crate::{ChatKey, Message, ReadStatus};

/// Maximum gap in seconds between the locally stamped `ts` and the server `ts`.
pub const RECONCILE_WINDOW_SECS: i64 = 180;

/// Deterministic media cache key for `(chat, msg_id)`, mirroring `postbox_media::drain`.
///
/// Kept here so the store can migrate a cached voice note from its local `client_id` to the
/// server `msg_id` without depending on the downloader crate.
pub fn chat_msg_cache_key(chat: &ChatKey, msg_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for part in [
        chat.provider.as_str().as_bytes(),
        b"\0".as_slice(),
        chat.account.as_bytes(),
        b"\0".as_slice(),
        chat.id.as_bytes(),
        b"\0".as_slice(),
        msg_id.as_bytes(),
    ] {
        hasher.update(part);
    }
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for &b in digest.iter() {
        hex.push(HEX[(b >> 4) as usize] as char);
        hex.push(HEX[(b & 0x0F) as usize] as char);
    }
    format!("{}-{}", &hex[..16], &hex[16..32])
}

/// True when `msg` is keyed by a local `client_id`, which only optimistic sends do.
pub fn is_local_client_id_key(msg: &Message) -> bool {
    msg.from_me
        && matches!(msg.read_status, ReadStatus::Sending | ReadStatus::Sent)
        && is_valid_client_id(&msg.msg_id)
}

/// True when the row carries a media payload (filename, mime type or byte size).
fn has_media_payload(msg: &Message) -> bool {
    msg.filename.is_some() || msg.mime_type.is_some() || msg.file_size.is_some()
}

/// Pairs superseded local `client_id` rows with their authoritative server copies.
///
/// `locals` are the stored rows for one chat, `servers` the rows being ingested. Returns one
/// `(local_msg_id, server_msg_id)` pair per match. Each server row is consumed at most once, and a
/// row already stored under its server id is never a candidate on either side.
pub fn pair_superseded_local_messages<L, S>(locals: &[L], servers: &[S]) -> Vec<(String, String)>
where
    L: std::borrow::Borrow<Message>,
    S: std::borrow::Borrow<Message>,
{
    let server_ids: std::collections::HashSet<&str> = servers
        .iter()
        .map(|m| std::borrow::Borrow::<Message>::borrow(m).msg_id.as_str())
        .collect();
    let candidates: Vec<&Message> = locals
        .iter()
        .map(std::borrow::Borrow::borrow)
        .filter(|m: &&Message| is_local_client_id_key(m) && !server_ids.contains(m.msg_id.as_str()))
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }

    // Server rows that cannot win a local row: other people's messages, and rows still keyed by
    // a `client_id` (a second unresolved optimistic send is not evidence of a copy).
    let mut available: Vec<&Message> = servers
        .iter()
        .map(std::borrow::Borrow::borrow)
        .filter(|m: &&Message| m.from_me && !is_valid_client_id(&m.msg_id))
        .collect();

    let mut pairs = Vec::new();
    for local in candidates {
        let Some(idx) = available
            .iter()
            .position(|server| supersedes(server, local))
        else {
            continue;
        };
        let server = available.remove(idx);
        pairs.push((local.msg_id.clone(), server.msg_id.clone()));
    }
    pairs
}

/// True when `server` is the authoritative copy of the locally-keyed `local` row.
fn supersedes(server: &Message, local: &Message) -> bool {
    let (Some(local_secs), Some(server_secs)) = (unix_seconds(&local.ts), unix_seconds(&server.ts))
    else {
        return false;
    };
    if (local_secs - server_secs).abs() > RECONCILE_WINDOW_SECS {
        return false;
    }

    if local.media_kind.is_some() || has_media_payload(local) {
        // Media sends carry a filename on both sides; Hub may normalise a voice note to a
        // filename-less row, so fall back to the caption alone when a side has no filename.
        let kind_matches = server.media_kind == local.media_kind
            || server.media_kind.is_some()
            || has_media_payload(server);
        if !kind_matches {
            return false;
        }
        match (local.filename.as_deref(), server.filename.as_deref()) {
            (Some(a), Some(b)) => a == b && local.body == server.body,
            _ => local.body == server.body,
        }
    } else {
        !has_media_payload(server) && !local.body.is_empty() && local.body == server.body
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MediaKind, Message, Provider};

    fn chat() -> ChatKey {
        ChatKey::new(Provider::Whatsapp, "main", "dedup@s.whatsapp.net")
    }

    fn msg(id: &str, ts: &str, body: &str) -> Message {
        Message {
            chat: chat(),
            msg_id: id.into(),
            sender_id: "me".into(),
            sender_name: "You".into(),
            from_me: true,
            ts: ts.into(),
            body: body.into(),
            reply_to_id: None,
            reply_broadcast: false,
            media_kind: None,
            mime_type: None,
            filename: None,
            file_size: None,
            reactions: vec![],
            read_status: ReadStatus::Sent,
            deleted: false,
        }
    }

    fn cid(byte: u8) -> String {
        crate::outbox::format_client_id([byte; 16])
    }

    #[test]
    fn pairs_one_local_row_with_its_server_copy() {
        let local = msg(&cid(1), "2026-11-26T10:57:04Z", "hello");
        let server = msg("3EB0A", "2026-11-26T10:57:00Z", "hello");
        assert_eq!(
            pair_superseded_local_messages(
                std::slice::from_ref(&local),
                std::slice::from_ref(&server),
            ),
            vec![(local.msg_id.clone(), server.msg_id.clone())]
        );
    }

    #[test]
    fn never_pairs_outside_the_window_or_on_different_content() {
        let local = msg(&cid(1), "2026-11-26T10:57:00Z", "hello");
        let far = msg("3EB0A", "2026-11-26T11:05:00Z", "hello");
        let other = msg("3EB0B", "2026-11-26T10:57:00Z", "different");
        assert!(pair_superseded_local_messages(std::slice::from_ref(&local), &[far]).is_empty());
        assert!(pair_superseded_local_messages(&[local], &[other]).is_empty());
    }

    #[test]
    fn consumes_each_server_row_at_most_once() {
        let first = msg(&cid(1), "2026-11-26T10:57:00Z", "same");
        let second = msg(&cid(2), "2026-11-26T10:57:01Z", "same");
        let server = msg("3EB0A", "2026-11-26T10:57:00Z", "same");
        let pairs = pair_superseded_local_messages(&[first, second], &[server]);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, cid(1));
    }

    #[test]
    fn skips_failed_incoming_and_incoming_keyed_by_a_client_id() {
        let local = msg(&cid(1), "2026-11-26T10:57:00Z", "hello");
        let mut failed = local.clone();
        failed.read_status = ReadStatus::Failed;
        assert!(
            pair_superseded_local_messages(
                &[failed],
                &[msg("3EB0A", "2026-11-26T10:57:00Z", "hello")]
            )
            .is_empty()
        );

        let incoming = msg("3EB0B", "2026-11-26T10:57:00Z", "hello");
        let mut from_them = incoming.clone();
        from_them.from_me = false;
        assert!(
            pair_superseded_local_messages(std::slice::from_ref(&local), &[from_them]).is_empty()
        );

        let unresolved = msg(&cid(9), "2026-11-26T10:57:00Z", "hello");
        assert!(pair_superseded_local_messages(&[local], &[unresolved]).is_empty());
    }

    #[test]
    fn matches_media_on_filename_and_falls_back_to_kind_and_caption() {
        let mut local = msg(&cid(3), "2026-11-26T10:57:00Z", "");
        local.media_kind = Some(MediaKind::Document);
        local.filename = Some("pic.png".into());
        let mut same = msg("3EB0A", "2026-11-26T10:57:00Z", "");
        same.media_kind = Some(MediaKind::Document);
        same.filename = Some("pic.png".into());
        same.file_size = Some(115_000);
        assert_eq!(
            pair_superseded_local_messages(std::slice::from_ref(&local), &[same]).len(),
            1
        );

        let mut renamed = msg("3EB0B", "2026-11-26T10:57:00Z", "");
        renamed.media_kind = Some(MediaKind::Document);
        renamed.filename = Some("other.png".into());
        assert!(
            pair_superseded_local_messages(std::slice::from_ref(&local), &[renamed]).is_empty()
        );

        // A voice note Hub reports without a filename still pairs on kind plus caption.
        let mut voice = msg(&cid(4), "2026-11-26T10:57:00Z", "");
        voice.media_kind = Some(MediaKind::Voice);
        voice.filename = Some("voice.ogg".into());
        let mut hub_voice = msg("3EB0C", "2026-11-26T10:57:02Z", "");
        hub_voice.media_kind = Some(MediaKind::Voice);
        assert_eq!(
            pair_superseded_local_messages(&[voice], &[hub_voice]).len(),
            1
        );
    }

    #[test]
    fn cache_key_matches_the_media_drainer() {
        // Literal expectation guards against a silent divergence from `postbox_media::drain`.
        let key = chat_msg_cache_key(&chat(), "3EB0A");
        assert_eq!(key.len(), 33);
        assert_eq!(key, chat_msg_cache_key(&chat(), "3EB0A"));
        assert_ne!(key, chat_msg_cache_key(&chat(), "3EB0B"));
    }
}
