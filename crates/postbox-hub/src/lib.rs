pub mod client;
pub mod error;
pub mod normalize;
pub mod request;
pub mod sse;
pub mod sync;
pub mod types;

pub use client::{
    DEFAULT_MEDIA_POLL_INTERVAL, DEFAULT_MEDIA_POLL_MAX_WAIT, EventStream, HUB_MEDIA_TIMEOUT,
    HubClient,
};
pub use error::{
    HubError, SignalSendFailure, TelegramSendFailure, classify_signal_send_failure,
    classify_telegram_send_failure,
};
pub use normalize::{
    is_real_message, lid_alias, merge_archived_chats, normalize_chats, normalize_messages,
    read_message_id, saved_messages, source_account, standard_slack_emoji, strip_placeholder,
    unite_messages, view_account,
};
pub use request::{
    MAX_CACHED_HISTORY, MAX_CLIENT_ID_LEN, MAX_DOWNLOAD_BYTES, MAX_FILE_BYTES, MAX_ID_LEN,
    MAX_ROWS, MAX_SLACK_EMOJI_INDEX_BYTES, MAX_SLACK_EMOJI_MEDIA_BYTES, MAX_TEXT_LEN,
    OLDER_PAGE_LIMIT, USER_AGENT, archive_batch_request, archive_request, before_cursor,
    chats_request, delete_request, encode_send_file_multipart, encode_uri_component,
    events_request, generate_boundary, mark_read_request, media_request, messages_request,
    react_request, send_text_request, slack_emoji_index_request, slack_emoji_media_request,
    valid_client_id, valid_id, valid_last_event_id, valid_team, valid_text, valid_token,
    workspace_icon_request,
};
pub use sse::{MAX_SSE_FRAME_BYTES, SseParser};
pub use sync::{
    HUB_INBOX_SAFETY_MS, HUB_RECONNECT_BACKOFF_MS, HUB_WAKE_GAP_MS, HUB_WAKE_GAP_SECS,
    SseReconnectState, hub_reconnect_wait_ms, should_run_safety_poll,
};
pub use types::{
    ArchiveBatchItemResult, ArchiveOutcome, Endpoint, HubChat, HubMessage, HubRequest,
    HubSyncSignal, MediaFetchDetailed, MediaFetchState, SendFileRequest, SendReply, SseEvent,
};

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::Provider;

    #[test]
    fn enforces_https_except_explicit_loopback() {
        assert_eq!(
            Endpoint::parse("http://example.com:8080", true),
            Err(HubError::InsecureUrl)
        );
        assert_eq!(
            Endpoint::parse("http://127.0.0.1:8080", false),
            Err(HubError::InsecureUrl)
        );
        let loopback = Endpoint::parse("http://127.0.0.1:9000", true).unwrap();
        assert_eq!(loopback.port, 9000);
        assert!(!loopback.secure);
        assert_eq!(loopback.base_url(), "http://127.0.0.1:9000");

        let tls = Endpoint::parse("https://hub.example.invalid/", false).unwrap();
        assert!(tls.secure);
        assert_eq!(tls.port, 443);
        assert_eq!(tls.base_url(), "https://hub.example.invalid");
    }

    #[test]
    fn validates_ids_teams_client_ids_tokens_and_text_bounds() {
        assert_eq!(valid_id(""), Err(HubError::InvalidId));
        assert_eq!(valid_id(&"a".repeat(192)), Err(HubError::InvalidId));
        assert!(valid_id(&"a".repeat(191)).is_ok());
        assert_eq!(valid_id("bad\nid"), Err(HubError::InvalidId));

        assert_eq!(valid_team("T:1"), Err(HubError::InvalidTeam));
        assert_eq!(valid_team("."), Err(HubError::InvalidTeam));
        assert_eq!(valid_team(".."), Err(HubError::InvalidTeam));
        assert!(valid_team("T012345").is_ok());

        assert_eq!(
            valid_client_id(&"c".repeat(129)),
            Err(HubError::InvalidClientId)
        );
        assert!(valid_client_id(&"c".repeat(128)).is_ok());

        assert_eq!(valid_token("bad token"), Err(HubError::InvalidToken));
        assert_eq!(valid_token("bad\r\ntoken"), Err(HubError::InvalidToken));
        assert!(valid_token("good-token_123").is_ok());

        assert_eq!(valid_text(&"x".repeat(4096)), Err(HubError::InvalidSend));
        assert!(valid_text(&"x".repeat(4095)).is_ok());
    }

    #[test]
    fn builds_paginated_messages_broadcast_archive_batch_and_multipart_requests() {
        let cursor = before_cursor("M100", "2026-11-26T10:00:00Z").unwrap();
        let req = messages_request("whatsapp:biz:49123@s.whatsapp.net", 80, Some(&cursor)).unwrap();
        assert!(
            req.path
                .starts_with("/v1/wazig/messages?chat=whatsapp%3Abiz%3A")
        );
        assert!(req.path.contains("limit=80"));

        assert_eq!(
            send_text_request("slack:T1:C1", "hi", None, true, "cid-1"),
            Err(HubError::InvalidSend)
        );
        let broadcast =
            send_text_request("slack:T1:C1", "hi", Some("1700.1"), true, "cid-2").unwrap();
        assert!(broadcast.body.unwrap().contains("\"reply_broadcast\":true"));

        let batch = archive_batch_request(&["whatsapp:p:1@s.whatsapp.net"], true).unwrap();
        let batch_body = batch.body.unwrap();
        assert!(batch_body.contains("\"account\":\"\""));

        let boundary = "0123456789abcdef0123456789abcdef";
        let (ct, body) = encode_send_file_multipart(
            &SendFileRequest {
                chat: "whatsapp:p:1@s.whatsapp.net",
                text: "caption",
                reply_to: "",
                client_id: "cid12345678901234567890123456789",
                ptt: true,
                filename: "ignored/evil.ogg",
                bytes: b"OggS-audio-bytes",
            },
            boundary,
        )
        .unwrap();
        assert_eq!(ct, format!("multipart/form-data; boundary={boundary}"));
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("name=\"account\"\r\n\r\nhub\r\n"));
        assert!(s.contains("name=\"ptt\"\r\n\r\ntrue\r\n"));
        assert!(s.contains("filename=\"voice.ogg\""));
        assert!(s.contains("Content-Type: audio/ogg"));

        // Boundary collision rejected.
        let collided = encode_send_file_multipart(
            &SendFileRequest {
                chat: "whatsapp:p:1@s.whatsapp.net",
                text: boundary,
                reply_to: "",
                client_id: "cid12345678901234567890123456789",
                ptt: false,
                filename: "a.png",
                bytes: b"png",
            },
            boundary,
        );
        assert_eq!(collided, Err(HubError::InvalidBoundary));
    }

    #[test]
    fn normalizer_enforces_lid_aliasing_slack_synthesis_path_stripping_and_classifiers() {
        // Same-account @lid -> @s.whatsapp.net allowed & rewritten.
        let raw_lid = r#"[{
            "MsgID": "m1",
            "ChatJID": "personal:12345@lid",
            "SenderName": "Alice",
            "FromMe": false,
            "Timestamp": "2026-11-26T10:00:00Z",
            "Text": "hello",
            "LocalPath": "/etc/passwd",
            "local_path": "/etc/shadow",
            "SlackEmojiPaths": {"x": "/tmp/x"}
        }]"#;
        let msgs = normalize_messages(raw_lid, "personal:49123@s.whatsapp.net").unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].chat_jid, "personal:49123@s.whatsapp.net");
        assert!(msgs[0].raw.get("LocalPath").is_none());
        assert!(msgs[0].raw.get("local_path").is_none());
        assert!(msgs[0].raw.get("SlackEmojiPaths").is_none());

        // Cross-account @lid and @lid -> @g.us MUST be rejected.
        assert_eq!(
            normalize_messages(raw_lid, "business:49123@s.whatsapp.net"),
            Err(HubError::WrongChat)
        );
        assert_eq!(
            normalize_messages(raw_lid, "personal:999@g.us"),
            Err(HubError::WrongChat)
        );

        // Slack synthesis: reactions + GIF attachment.
        let raw_slack = r#"[{
            "MsgID": "1700.1",
            "ChatJID": "slack:T1:C1",
            "SenderName": "Bob",
            "FromMe": false,
            "Timestamp": "2026-11-26T10:00:00Z",
            "DisplayText": "(message)",
            "Text": "Check this",
            "reactions": [{"name": "tea", "count": 1}, {"name": "custom_fixture", "count": 2}],
            "attachments": [{"image_url": "https://media.giphy.com/x.gif", "service_name": "Giphy"}]
        }]"#;
        let slack_msgs = normalize_messages(raw_slack, "slack:T1:C1").unwrap();
        assert_eq!(slack_msgs[0].effective_text, "Check this");
        assert_eq!(slack_msgs[0].reaction_emoji, "🍵 1  :custom_fixture: 2");
        assert_eq!(slack_msgs[0].media_type, "image");
        assert_eq!(slack_msgs[0].mime_type.as_deref(), Some("image/gif"));
        assert_eq!(slack_msgs[0].filename.as_deref(), Some("attachment.gif"));

        // Signal/Telegram classifiers & chat provider validation.
        assert_eq!(
            classify_signal_send_failure(
                429,
                r#"{"error":"signal rate limited","retry_after":30}"#
            ),
            Some(SignalSendFailure::RateLimited {
                retry_after: Some(30)
            })
        );
        assert_eq!(
            classify_signal_send_failure(403, r#"{"error":"signal captcha required"}"#),
            Some(SignalSendFailure::CaptchaRequired)
        );
        assert_eq!(
            classify_telegram_send_failure(
                429,
                r#"{"error":"telegram rate limited","retry_after":12}"#
            ),
            Some(TelegramSendFailure::RateLimited {
                retry_after: Some(12)
            })
        );

        let chats = normalize_chats(
            r#"[{"jid":"signal:u1","provider":"signal","account":"ignored","name":"Sig"},{"jid":"telegram:1","provider":"telegram","saved":true,"name":"Saved"}]"#,
            "hub",
        )
        .unwrap();
        assert_eq!(chats[0].provider, Provider::Signal);
        assert_eq!(chats[0].view_account, "signal");
        assert!(chats[1].saved);
    }

    #[test]
    fn sse_parser_validates_id_match_and_backoff_handles_eof_and_wake_poll() {
        let mut parser = SseParser::new();
        let good = b": heartbeat\r\n\r\nid: 42\r\ndata: {\"id\":42,\"kind\":\"msg\",\"chat\":\"c1\"}\r\n\r\n";
        let events = parser.push_bytes(good).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, 42);
        assert_eq!(parser.last_event_id(), Some("42"));

        // Mismatched id MUST error and MUST NOT advance last_event_id!
        let bad = b"id: 99\ndata: {\"id\":100,\"kind\":\"msg\"}\n\n";
        assert_eq!(parser.push_bytes(bad), Err(HubError::BadEventId));
        assert_eq!(parser.last_event_id(), Some("42"));

        // 64 KiB cap enforced.
        let huge = vec![b'a'; MAX_SSE_FRAME_BYTES + 1];
        assert_eq!(parser.push_bytes(&huge), Err(HubError::EventTooLarge));

        // Backoff ladder & clean EOF reset.
        let mut state = SseReconnectState::new();
        assert_eq!(state.on_error(), 1_000);
        assert_eq!(state.on_error(), 2_000);
        assert_eq!(state.on_error(), 4_000);
        assert_eq!(state.on_error(), 8_000);
        assert_eq!(state.on_error(), 15_000);
        assert_eq!(state.on_error(), 30_000);
        assert_eq!(state.on_error(), 30_000);
        assert_eq!(state.on_clean_eof(), 1_000);
        assert_eq!(state.failures(), 0);

        // Wake & safety poll checks BOTH monotonic and wall-clock deltas.
        assert!(!should_run_safety_poll(59_999, 59));
        assert!(should_run_safety_poll(60_000, 10));
        assert!(should_run_safety_poll(1_000, 60));
    }

    #[test]
    fn parses_real_hub_wire_fields_gif_saved_endpoint_and_del_filename() {
        // Real Hub ReadBy / DeliveredTo / Delivered wire fields + clamping 0..=1000.
        let raw_wa = r#"[
            {"MsgID":"m1","ChatJID":"p:1@s.whatsapp.net","FromMe":true,"Timestamp":"2026-11-26T10:00:00Z","Text":"hi","DeliveredTo":1,"ReadBy":1},
            {"MsgID":"m2","ChatJID":"p:1@s.whatsapp.net","FromMe":true,"Timestamp":"2026-11-26T10:01:00Z","Text":"yo","Delivered":1,"ReadBy":0},
            {"MsgID":"m3","ChatJID":"p:1@s.whatsapp.net","FromMe":true,"Timestamp":"2026-11-26T10:02:00Z","Text":"clamp","DeliveredTo":9999,"ReadBy":-5}
        ]"#;
        let msgs = normalize_messages(raw_wa, "p:1@s.whatsapp.net").unwrap();
        let chat_key = postbox_core::ChatKey::new(Provider::Whatsapp, "p", "p:1@s.whatsapp.net");
        assert_eq!(msgs[0].delivered_count, 1);
        assert_eq!(msgs[0].read_by_count, 1);
        assert_eq!(
            msgs[0].to_core_message(&chat_key, false).read_status,
            postbox_core::ReadStatus::Read
        );
        assert_eq!(msgs[1].delivered_count, 1);
        assert_eq!(msgs[1].read_by_count, 0);
        assert_eq!(
            msgs[1].to_core_message(&chat_key, false).read_status,
            postbox_core::ReadStatus::Delivered
        );
        assert_eq!(msgs[2].delivered_count, 1000);
        assert_eq!(msgs[2].read_by_count, 0);

        // Real Hub kind / group / channel detection + Telegram Saved Messages title.
        let chats = normalize_chats(
            r#"[
                {"jid":"signal:group:fam1","provider":"signal","kind":"group","name":"Family"},
                {"jid":"telegram:channel:news1","provider":"telegram","kind":"Channel","name":"News"},
                {"jid":"telegram:sg1","provider":"telegram","Kind":"SUPERGROUP","name":"Rust"},
                {"jid":"telegram:me","provider":"telegram","saved":true,"name":"Alice"}
            ]"#,
            "hub",
        )
        .unwrap();
        assert!(chats[0].is_group);
        assert!(chats[1].is_group);
        assert!(chats[2].is_group);
        assert!(!chats[3].is_group);
        assert_eq!(chats[3].to_core_chat(false, false).title, "Saved Messages");

        // Slack synthesized GIF attachment maps to MediaKind::Gif (not Image).
        let raw_gif = r#"[{
            "MsgID": "1700.9",
            "ChatJID": "slack:T1:C1",
            "FromMe": false,
            "Timestamp": "2026-11-26T10:00:00Z",
            "Text": "gif",
            "attachments": [{"image_url": "https://media.giphy.com/a.gif", "service_name": "Giphy"}]
        }]"#;
        let gif_msgs = normalize_messages(raw_gif, "slack:T1:C1").unwrap();
        let slack_key = postbox_core::ChatKey::new(Provider::Slack, "T1", "slack:T1:C1");
        assert_eq!(
            gif_msgs[0].to_core_message(&slack_key, true).media_kind,
            Some(postbox_core::MediaKind::Gif)
        );

        // Endpoint::parse rejects spaces, control bytes, and DEL (0x7F) in authority.
        assert_eq!(
            Endpoint::parse("https://hub .example.com", false),
            Err(HubError::InvalidUrl)
        );
        assert_eq!(
            Endpoint::parse("https://hub\x7f.example.com", false),
            Err(HubError::InvalidUrl)
        );

        // sanitize_wire_filename replaces 0x7F (DEL) cleanly.
        let (_, body) = encode_send_file_multipart(
            &SendFileRequest {
                chat: "whatsapp:p:1@s.whatsapp.net",
                text: "",
                reply_to: "",
                client_id: "0123456789abcdef0123456789abcdef",
                ptt: false,
                filename: "evil\x7fname.png",
                bytes: b"png",
            },
            "fedcba9876543210fedcba9876543210",
        )
        .unwrap();
        let body_str = String::from_utf8_lossy(&body);
        assert!(body_str.contains("filename=\"evil_name.png\""));
        assert!(!body.contains(&0x7F));
    }
}
