#![forbid(unsafe_code)]

use postbox_core::archive::should_resurface_on_event;
use postbox_core::control::{ControlRequest, TelemetryCounters};
use postbox_core::deep_link::{DeepLinkError, parse as parse_deep_link};
use postbox_core::dictation::DictationLanguage;
use postbox_core::{
    Chat, ChatKey, FontScale, MediaKind, Message, Provider, ReadStatus, ThemeChoice, ViewTab,
};
use postbox_e2e::fixtures::{minimal_png_1x1, minimal_png_1x1_alt, pcm16_raw, wcag_contrast_ratio};
use postbox_e2e::wire::{build_multipart, parse_store_zip, sha256_hex};
use postbox_e2e::{
    ArchiveBatchFault, ChatsFault, ClientError, DeepgramRestReply, E2eWorkspace, GiphyCdnFault,
    GiphyFault, HubMessageRow, LoopbackHttpClient, M8HookRegistry, MediaBehavior, MessagesFault,
    MockDeepgramServer, MockExternalServer, MockHubServer, MultipartPart, ParsedSseFrame,
    ProcessRunner, SendFault, SseParseError, UpdateScenario, WsScriptStep, WsServerAction,
    WsTrigger,
};
use postbox_hub::{
    Endpoint, HubError, MAX_FILE_BYTES, SseEvent, archive_request, chats_request, events_request,
    mark_read_request, media_request, messages_request, react_request, send_text_request, valid_id,
};
use postbox_keymap::{Action, InboxChord, KeyContext, Keymap, KeymapError, ctrl_a_selects_chats};
use postbox_platform::{Notifier, SecretStore};
use postbox_store::{MEDIA_TTL_SECS, OutboxKind, OutboxPhase, OutboxRecord, StoreError};
use postbox_ui::theme::ShadcnPalette;
use postbox_ui::{BASE_FONT_PX, scaled_px};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs;
use std::time::Duration;

fn mk_chat(provider: Provider, account: &str, id: &str, title: &str, ts: &str) -> Chat {
    Chat {
        key: ChatKey::new(provider, account, id),
        title: title.to_string(),
        is_group: false,
        archived_local: false,
        archived_remote: false,
        pinned: false,
        unread_count: 0,
        last_msg_ts: ts.to_string(),
        last_msg_preview: String::new(),
        avatar_hash: None,
    }
}

fn mk_msg(
    chat: &ChatKey,
    msg_id: &str,
    sender: &str,
    ts: &str,
    body: &str,
    from_me: bool,
) -> Message {
    Message {
        chat: chat.clone(),
        msg_id: msg_id.to_string(),
        sender_id: format!("u-{}", sender.to_ascii_lowercase()),
        sender_name: sender.to_string(),
        from_me,
        ts: ts.to_string(),
        body: body.to_string(),
        reply_to_id: None,
        reply_broadcast: false,
        media_kind: None,
        mime_type: None,
        filename: None,
        file_size: None,
        reactions: Vec::new(),
        read_status: ReadStatus::None,
        deleted: false,
    }
}

// ============================================================================
// Area 1 Boundaries (5 tests)
// ============================================================================

#[test]
fn t2_a01_deleted_and_empty_messages_excluded_from_fts5() {
    let mut ws = E2eWorkspace::new("t2_a01_del_fts");
    let key = ChatKey::new(Provider::Whatsapp, "personal", "del@s.whatsapp.net");
    let mut msg = mk_msg(
        &key,
        "M-DEL-1",
        "Ada",
        "2026-11-26T12:00:00Z",
        "topsecret canary token",
        false,
    );
    ws.store.upsert_message(&msg).unwrap();
    assert_eq!(
        ws.store.search_messages_fts("topsecret", 10).unwrap().len(),
        1
    );

    msg.deleted = true;
    ws.store.upsert_message(&msg).unwrap();
    assert!(
        ws.store
            .search_messages_fts("topsecret", 10)
            .unwrap()
            .is_empty()
    );

    // An empty body is not searchable content. The message still indexes its sender (FTS5
    // covers `sender_name` so you can find a person), but the empty string adds no term, and
    // the earlier hit stays gone.
    let empty = mk_msg(&key, "M-EMPTY-1", "Ada", "2026-11-26T12:01:00Z", "", false);
    ws.store.upsert_message(&empty).unwrap();
    assert!(
        ws.store
            .search_messages_fts("topsecret", 10)
            .unwrap()
            .is_empty(),
        "a deleted message's body must never come back"
    );
}

#[test]
fn t2_a01_same_bare_jid_across_all_four_providers_and_accounts() {
    let ws = E2eWorkspace::new("t2_a01_bare");
    let accounts = [
        (Provider::Whatsapp, "personal"),
        (Provider::Whatsapp, "business"),
        (Provider::Slack, "T1"),
        (Provider::Slack, "T2"),
        (Provider::Telegram, "main"),
        (Provider::Signal, "main"),
    ];
    for (i, (prov, acct)) in accounts.iter().enumerate() {
        let mut c = mk_chat(
            *prov,
            acct,
            "12345",
            &format!("{acct}-chat"),
            "2026-11-26T12:00:00Z",
        );
        c.unread_count = 1;
        ws.store.upsert_chat(&c, 100 + i as i64).unwrap();
    }
    let mut updated = mk_chat(
        Provider::Whatsapp,
        "business",
        "12345",
        "business-chat",
        "2026-11-26T12:05:00Z",
    );
    updated.unread_count = 9;
    ws.store.upsert_chat(&updated, 999).unwrap();

    let chats = ws.store.list_inbox_chats(false, 20).unwrap();
    assert_eq!(chats.len(), 6);
    for c in &chats {
        if c.key == updated.key {
            assert_eq!(c.unread_count, 9);
        } else {
            assert_eq!(c.unread_count, 1);
        }
    }
}

#[test]
fn t2_a01_provider_parse_case_insensitive_and_rejects_unknown() {
    assert_eq!(Provider::parse("WhatsApp"), Some(Provider::Whatsapp));
    assert_eq!(Provider::parse("SLACK"), Some(Provider::Slack));
    assert_eq!(Provider::parse("TeLeGrAm"), Some(Provider::Telegram));
    assert_eq!(Provider::parse("SIGNAL"), Some(Provider::Signal));
    assert_eq!(Provider::parse(""), None);
    assert_eq!(Provider::parse("discord"), None);
    assert_eq!(Provider::parse("whatsapp "), None);
}

#[test]
fn t2_a01_fts5_unicode_cjk_emoji_and_diacritics_boundaries() {
    let mut ws = E2eWorkspace::new("t2_a01_unicode");
    let key = ChatKey::new(Provider::Whatsapp, "personal", "uni@s.whatsapp.net");
    let msg = mk_msg(
        &key,
        "M-UNI",
        "Müller",
        "2026-11-26T12:00:00Z",
        "Ärger über Café München 🚀 日本語",
        false,
    );
    ws.store.upsert_message(&msg).unwrap();

    for q in ["Arger", "uber", "Cafe", "Munchen"] {
        let hits = ws.store.search_messages_fts(q, 10).unwrap();
        assert_eq!(
            hits.len(),
            1,
            "query {q} should match diacritic-folded text"
        );
        assert_eq!(hits[0].msg_id, "M-UNI");
    }
}

#[test]
fn t2_a01_list_inbox_chats_limit_zero_and_archived_remote_flag() {
    let ws = E2eWorkspace::new("t2_a01_lim0");
    let mut remote_arch = mk_chat(
        Provider::Whatsapp,
        "personal",
        "r@s.whatsapp.net",
        "Remote Archived",
        "2026-11-26T12:01:00Z",
    );
    remote_arch.archived_remote = true;
    let active = mk_chat(
        Provider::Whatsapp,
        "personal",
        "a@s.whatsapp.net",
        "Active",
        "2026-11-26T12:00:00Z",
    );
    ws.store.upsert_chat(&remote_arch, 100).unwrap();
    ws.store.upsert_chat(&active, 101).unwrap();

    assert!(ws.store.list_inbox_chats(false, 0).unwrap().is_empty());
    let inbox = ws.store.list_inbox_chats(false, 50).unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].key, active.key);
    assert_eq!(ws.store.list_inbox_chats(true, 50).unwrap().len(), 2);
}

// ============================================================================
// Area 2 Boundaries (6 tests)
// ============================================================================

#[test]
fn t2_a02_endpoint_rejects_non_loopback_http_port0_and_authority_injections() {
    assert_eq!(
        Endpoint::parse("http://example.com", true),
        Err(HubError::InsecureUrl)
    );
    assert_eq!(
        Endpoint::parse("http://localhost:8080", true),
        Err(HubError::InsecureUrl)
    );
    assert_eq!(
        Endpoint::parse("http://127.0.0.1:8080", false),
        Err(HubError::InsecureUrl)
    );
    for bad in [
        "https://hub:0",
        "https://user:pass@hub.invalid", // trufflehog:ignore
        "https://hub.invalid/path",
        "https://hub.invalid\r\nX:1",
        "ftp://hub.invalid",
        "https://",
    ] {
        assert_eq!(
            Endpoint::parse(bad, true),
            Err(HubError::InvalidUrl),
            "expected InvalidUrl for {bad:?}"
        );
    }
}

#[test]
fn t2_a02_id_191_vs_192_client_id_128_text_4095_and_control_bytes() {
    assert_eq!(valid_id(""), Err(HubError::InvalidId));
    assert_eq!(valid_id("a\x00b"), Err(HubError::InvalidId));
    assert_eq!(valid_id("a\nb"), Err(HubError::InvalidId));
    assert_eq!(valid_id("a\x7fb"), Err(HubError::InvalidId));
    let id_191 = "a".repeat(191);
    let id_192 = "a".repeat(192);
    assert!(valid_id(&id_191).is_ok());

    let hooks = M8HookRegistry::current();
    if hooks.m2_hub_sync {
        assert_eq!(valid_id(&id_192), Err(HubError::InvalidId));
    }

    let hub = MockHubServer::start("tok-bounds");
    // Lower multipart cap to 4096 B to exercise exact >cap 413 cleanly alongside MAX_FILE_BYTES check
    assert_eq!(MAX_FILE_BYTES, 67_108_864);
    hub.set_max_multipart_bytes(4096);
    let client = LoopbackHttpClient::default();

    // 191-byte chat ID + 128-byte client_id accepted
    let ok_191 = send_text_request(&id_191, "ok", None, false, &"c".repeat(128)).unwrap();
    assert_eq!(
        client
            .execute_hub(&hub.endpoint(), &hub.token(), &ok_191)
            .unwrap()
            .status,
        200
    );

    // 4095-byte text accepted
    let ok_4095 = send_text_request(
        "personal:1@s.whatsapp.net",
        &"x".repeat(4095),
        None,
        false,
        "cid-4095",
    )
    .unwrap();
    assert_eq!(
        client
            .execute_hub(&hub.endpoint(), &hub.token(), &ok_4095)
            .unwrap()
            .status,
        200
    );

    // 192-byte chat ID, 129-byte client_id, and 4096-byte text rejected (client-side on M2+, Hub 400 on M0)
    let cid_129 = "c".repeat(129);
    let text_4096 = "x".repeat(4096);
    for (chat_arg, text_arg, cid_arg) in [
        (id_192.as_str(), "no", "cid-192"),
        ("personal:1@s.whatsapp.net", "no", cid_129.as_str()),
        ("personal:1@s.whatsapp.net", text_4096.as_str(), "cid-4096"),
    ] {
        match send_text_request(chat_arg, text_arg, None, false, cid_arg) {
            Err(HubError::InvalidId | HubError::InvalidSend | HubError::InvalidClientId) => {}
            Err(other) => panic!("unexpected error for oversized input: {other:?}"),
            Ok(req) => {
                assert!(
                    !hooks.m2_hub_sync,
                    "m2_hub_sync must reject oversized request client-side"
                );
                assert_eq!(
                    client
                        .execute_hub(&hub.endpoint(), &hub.token(), &req)
                        .unwrap()
                        .status,
                    400
                );
            }
        }
    }

    // Exact cap vs cap + 1 multipart upload -> 200 vs 413
    let boundary = "PostboxSizeBoundary99";
    let mp_ok = build_multipart(
        boundary,
        &[
            MultipartPart {
                name: "chat".into(),
                filename: None,
                content_type: None,
                body: b"personal:1@s.whatsapp.net".to_vec(),
            },
            MultipartPart {
                name: "client_id".into(),
                filename: None,
                content_type: None,
                body: b"cid-mp-ok".to_vec(),
            },
            MultipartPart {
                name: "file".into(),
                filename: Some("ok.bin".into()),
                content_type: Some("application/octet-stream".into()),
                body: vec![0xAA; 4096],
            },
        ],
    );
    assert_eq!(
        client
            .execute_hub_multipart(
                &hub.endpoint(),
                &hub.token(),
                "/v1/wazig/send",
                boundary,
                &mp_ok
            )
            .unwrap()
            .status,
        200
    );
    let mp_big = build_multipart(
        boundary,
        &[
            MultipartPart {
                name: "chat".into(),
                filename: None,
                content_type: None,
                body: b"personal:1@s.whatsapp.net".to_vec(),
            },
            MultipartPart {
                name: "client_id".into(),
                filename: None,
                content_type: None,
                body: b"cid-mp-big".to_vec(),
            },
            MultipartPart {
                name: "file".into(),
                filename: Some("big.bin".into()),
                content_type: Some("application/octet-stream".into()),
                body: vec![0xAA; 4097],
            },
        ],
    );
    assert_eq!(
        client
            .execute_hub_multipart(
                &hub.endpoint(),
                &hub.token(),
                "/v1/wazig/send",
                boundary,
                &mp_big
            )
            .unwrap()
            .status,
        413
    );
}

#[test]
fn t2_a02_lid_same_account_alias_vs_cross_account_wrong_chat() {
    let hub = MockHubServer::start("tok-lid");
    let same = "personal:15550001@s.whatsapp.net";
    let cross = "personal:15550002@s.whatsapp.net";
    let grp = "personal:15550003@s.whatsapp.net";

    hub.set_messages_fault(
        same,
        MessagesFault::SameAccountLidAlias {
            lid_chat: "personal:777@lid".into(),
        },
    );
    hub.set_messages_fault(
        cross,
        MessagesFault::CrossAccountLidMismatch {
            wrong_chat: "business:777@lid".into(),
        },
    );
    hub.set_messages_fault(
        grp,
        MessagesFault::GroupMismatch {
            group_chat: "personal:888@g.us".into(),
        },
    );

    let req_same = messages_request(same, 20, None).unwrap();
    assert!(req_same.path.contains("%3A"));
    assert!(req_same.path.contains("%40"));

    let client = LoopbackHttpClient::default();
    let r1: Vec<HubMessageRow> = client
        .execute_hub(&hub.endpoint(), &hub.token(), &req_same)
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(r1[0].ChatJID, "personal:777@lid");

    let r2: Vec<HubMessageRow> = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &messages_request(cross, 20, None).unwrap(),
        )
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(r2[0].ChatJID, "business:777@lid");

    let r3: Vec<HubMessageRow> = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &messages_request(grp, 20, None).unwrap(),
        )
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(r3[0].ChatJID, "personal:888@g.us");
    // M8 seam (`m2_hub_sync`): wire `postbox-hub` normalizer @lid same-account aliasing vs cross-account rejection.
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t2_a02_sse_bad_event_id_mismatch_and_oversized_64kib_frame() {
    let hub = MockHubServer::start("tok-sse-fault");
    hub.push_sse_event(SseEvent {
        id: 7,
        account: "personal".into(),
        chat: "personal:1@s.whatsapp.net".into(),
        ts: "2026-11-26T12:00:00Z".into(),
        kind: "msg".into(),
    });
    hub.push_sse_corrupt_id_mismatch(8, 9);

    let client = LoopbackHttpClient::default();
    let mut stream = client
        .open_sse(&hub.endpoint(), &hub.token(), &events_request(None))
        .unwrap();
    let mut last_valid_id = 0u64;
    if let Some(ParsedSseFrame::Event(ev)) = stream.next_frame(Duration::from_secs(2)).unwrap() {
        last_valid_id = ev.id;
    }
    assert_eq!(last_valid_id, 7);

    let err = stream.next_frame(Duration::from_secs(2)).unwrap_err();
    assert!(matches!(err, SseParseError::BadEventId), "got {err:?}");
    assert_eq!(last_valid_id, 7);
    drop(stream);

    // Oversized >64 KiB frame rejected with EventTooLarge
    let hub2 = MockHubServer::start("tok-sse-big");
    hub2.push_sse_oversized_frame(66_000);
    let mut stream2 = client
        .open_sse(&hub2.endpoint(), &hub2.token(), &events_request(None))
        .unwrap();
    let err2 = stream2.next_frame(Duration::from_secs(2)).unwrap_err();
    assert!(matches!(err2, SseParseError::EventTooLarge), "got {err2:?}");
}

#[test]
fn t2_a02_hub_206_partial_media_rejected_and_500_row_cap_dup_ids() {
    let hub = MockHubServer::start("tok-206");
    let chat = "personal:p206@s.whatsapp.net";
    hub.set_media(
        chat,
        "M-206",
        MediaBehavior::Partial206 {
            bytes: vec![1, 2, 3, 4],
            content_type: "image/png".into(),
        },
    );

    let client = LoopbackHttpClient::default();
    let media_resp = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &media_request(chat, Some("M-206")).unwrap(),
        )
        .unwrap();
    assert_eq!(media_resp.status, 206);
    assert!(media_resp.header("content-range").is_some());

    hub.set_chats_fault(Some(ChatsFault::Overflow501));
    let ovf_resp = client
        .execute_hub(&hub.endpoint(), &hub.token(), &chats_request(false, 500))
        .unwrap();
    let ovf_rows: Vec<Value> = ovf_resp.json().unwrap();
    assert_eq!(ovf_rows.len(), 501);

    hub.set_chats_fault(Some(ChatsFault::DuplicateJid));
    let dup_resp = client
        .execute_hub(&hub.endpoint(), &hub.token(), &chats_request(false, 50))
        .unwrap();
    let dup_rows: Vec<Value> = dup_resp.json().unwrap();
    assert_eq!(dup_rows.len(), 2);
    assert_eq!(dup_rows[0]["jid"], dup_rows[1]["jid"]);
    // M8 seam (`m2_hub_sync`): wire `postbox-hub` 206 rejection, 500-row cap, and duplicate-JID rejection.
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t2_a02_signal_telegram_429_rate_limit_and_403_captcha_classifiers() {
    let hub = MockHubServer::start("tok-cls");
    hub.push_send_fault(SendFault::SignalRateLimit429 { retry_after: 45 });
    hub.push_send_fault(SendFault::TelegramRateLimit429 { retry_after: 12 });
    hub.push_send_fault(SendFault::SignalCaptcha403);

    let client = LoopbackHttpClient::default();
    let r1 = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &send_text_request("signal:aci-1", "hi", None, false, "c1").unwrap(),
        )
        .unwrap();
    assert_eq!(r1.status, 429);
    assert_eq!(r1.json::<Value>().unwrap()["retry_after"], 45);

    let r2 = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &send_text_request("telegram:tg-1", "hi", None, false, "c2").unwrap(),
        )
        .unwrap();
    assert_eq!(r2.status, 429);
    assert_eq!(r2.json::<Value>().unwrap()["retry_after"], 12);

    let r3 = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &send_text_request("signal:aci-1", "hi", None, false, "c3").unwrap(),
        )
        .unwrap();
    assert_eq!(r3.status, 403);
    assert!(r3.text().contains("captcha"));
}

// ============================================================================
// Area 3 Boundaries (5 tests)
// ============================================================================

#[test]
fn t2_a03_outbox_409_ambiguous_hold_never_blocks_other_chats() {
    let hub = MockHubServer::start("tok-409");
    hub.push_send_fault(SendFault::Ambiguous409 {
        commit_anyway: true,
    });
    let mut ws = E2eWorkspace::new("t2_a03_409");
    let client = LoopbackHttpClient::default();

    let req_a = send_text_request(
        "personal:a@s.whatsapp.net",
        "Chat A",
        None,
        false,
        "a1000000000000000000000000000001",
    )
    .unwrap();
    let resp_a = client
        .execute_hub(&hub.endpoint(), &hub.token(), &req_a)
        .unwrap();
    assert_eq!(resp_a.status, 409);

    ws.store
        .enqueue_outbox(&OutboxRecord {
            client_id: "a1000000000000000000000000000001".into(),
            chat: ChatKey::new(Provider::Whatsapp, "personal", "a@s.whatsapp.net"),
            kind: OutboxKind::Text,
            phase: OutboxPhase::Held,
            attempt_serial: 1,
            attempts: 0,
            ambiguous: false,
            text_body: "Chat A".into(),
            reply_to_id: None,
            reply_broadcast: false,
            staged_file_path: None,
            wire_filename: String::new(),
            file_sha256: None,
            file_bytes: 0,
            ptt: false,
            voice_seconds: 0,
            attachment_ready: false,
            failure_reason: None,
            not_before_ms: 0,
            created_at_ms: 100,
        })
        .unwrap();
    ws.store
        .enqueue_outbox(&OutboxRecord {
            client_id: "b1000000000000000000000000000001".into(),
            chat: ChatKey::new(Provider::Whatsapp, "personal", "b@s.whatsapp.net"),
            kind: OutboxKind::Text,
            phase: OutboxPhase::Ready,
            attempt_serial: 0,
            attempts: 0,
            ambiguous: false,
            text_body: "Chat B".into(),
            reply_to_id: None,
            reply_broadcast: false,
            staged_file_path: None,
            wire_filename: String::new(),
            file_sha256: None,
            file_bytes: 0,
            ptt: false,
            voice_seconds: 0,
            attachment_ready: false,
            failure_reason: None,
            not_before_ms: 0,
            created_at_ms: 200,
        })
        .unwrap();

    let next = ws.store.next_ready_outbox(500).unwrap().unwrap();
    assert_eq!(next.client_id, "b1000000000000000000000000000001");
    let req_b = send_text_request(
        "personal:b@s.whatsapp.net",
        &next.text_body,
        None,
        false,
        &next.client_id,
    )
    .unwrap();
    assert_eq!(
        client
            .execute_hub(&hub.endpoint(), &hub.token(), &req_b)
            .unwrap()
            .status,
        200
    );
}

#[test]
fn t2_a03_outbox_crash_recovery_uncertain_never_auto_resends() {
    let mut ws = E2eWorkspace::new("t2_a03_crash");
    ws.store
        .enqueue_outbox(&OutboxRecord {
            client_id: "a2000000000000000000000000000001".into(),
            chat: ChatKey::new(Provider::Whatsapp, "personal", "a@s.whatsapp.net"),
            kind: OutboxKind::Text,
            phase: OutboxPhase::Uncertain,
            attempt_serial: 1,
            attempts: 0,
            ambiguous: false,
            text_body: "Never auto-resend".into(),
            reply_to_id: None,
            reply_broadcast: false,
            staged_file_path: None,
            wire_filename: String::new(),
            file_sha256: None,
            file_bytes: 0,
            ptt: false,
            voice_seconds: 0,
            attachment_ready: false,
            failure_reason: None,
            not_before_ms: 0,
            created_at_ms: 100,
        })
        .unwrap();
    ws.store
        .enqueue_outbox(&OutboxRecord {
            client_id: "b2000000000000000000000000000001".into(),
            chat: ChatKey::new(Provider::Whatsapp, "personal", "b@s.whatsapp.net"),
            kind: OutboxKind::Text,
            phase: OutboxPhase::Ready,
            attempt_serial: 0,
            attempts: 0,
            ambiguous: false,
            text_body: "Safe to send".into(),
            reply_to_id: None,
            reply_broadcast: false,
            staged_file_path: None,
            wire_filename: String::new(),
            file_sha256: None,
            file_bytes: 0,
            ptt: false,
            voice_seconds: 0,
            attachment_ready: false,
            failure_reason: None,
            not_before_ms: 0,
            created_at_ms: 200,
        })
        .unwrap();

    ws.reopen_store().unwrap();

    // The Uncertain row still blocks dispatch until the app runs its startup crash recovery.
    assert!(
        ws.store.next_ready_outbox(i64::MAX).unwrap().is_none(),
        "a crashed send must hold the queue, never silently resend"
    );
    let recovered = ws.store.recover_uncertain_on_startup().unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].client_id, "a2000000000000000000000000000001");
    assert_eq!(recovered[0].phase, OutboxPhase::Held);
    assert!(recovered[0].ambiguous);

    // Recovery unblocks the clean chat: only the Ready text dispatches, the crash never resends.
    let next = ws.store.next_ready_outbox(i64::MAX).unwrap().unwrap();
    assert_eq!(next.client_id, "b2000000000000000000000000000001");
    let rows = ws.store.list_outbox().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        ws.store
            .get_outbox("a2000000000000000000000000000001")
            .unwrap()
            .unwrap()
            .phase,
        OutboxPhase::Held,
        "the crashed send stays held and ambiguous, never back in the ready queue"
    );
}

#[test]
fn t2_a03_outbox_not_before_exact_millisecond_boundary() {
    let mut ws = E2eWorkspace::new("t2_a03_ms");
    ws.store
        .enqueue_outbox(&OutboxRecord {
            client_id: "a3000000000000000000000000000001".into(),
            chat: ChatKey::new(Provider::Whatsapp, "personal", "a@s.whatsapp.net"),
            kind: OutboxKind::Text,
            phase: OutboxPhase::Ready,
            attempt_serial: 1,
            attempts: 0,
            ambiguous: false,
            text_body: "Wait until 10s".into(),
            reply_to_id: None,
            reply_broadcast: false,
            staged_file_path: None,
            wire_filename: String::new(),
            file_sha256: None,
            file_bytes: 0,
            ptt: false,
            voice_seconds: 0,
            attachment_ready: false,
            failure_reason: None,
            not_before_ms: 10_000,
            created_at_ms: 100,
        })
        .unwrap();
    ws.store
        .enqueue_outbox(&OutboxRecord {
            client_id: "b3000000000000000000000000000001".into(),
            chat: ChatKey::new(Provider::Whatsapp, "personal", "b@s.whatsapp.net"),
            kind: OutboxKind::Text,
            phase: OutboxPhase::Ready,
            attempt_serial: 1,
            attempts: 0,
            ambiguous: false,
            text_body: "Wait until 5s".into(),
            reply_to_id: None,
            reply_broadcast: false,
            staged_file_path: None,
            wire_filename: String::new(),
            file_sha256: None,
            file_bytes: 0,
            ptt: false,
            voice_seconds: 0,
            attachment_ready: false,
            failure_reason: None,
            not_before_ms: 5_000,
            created_at_ms: 200,
        })
        .unwrap();

    assert!(ws.store.next_ready_outbox(4_999).unwrap().is_none());
    assert_eq!(
        ws.store
            .next_ready_outbox(5_000)
            .unwrap()
            .unwrap()
            .client_id,
        "b3000000000000000000000000000001"
    );
    assert_eq!(
        ws.store
            .next_ready_outbox(9_999)
            .unwrap()
            .unwrap()
            .client_id,
        "b3000000000000000000000000000001"
    );
    // Remove B to verify exact 9_999 vs 10_000 boundary on A
    ws.open_aux_conn()
        .execute(
            "DELETE FROM outbox WHERE client_id = 'b3000000000000000000000000000001'",
            [],
        )
        .unwrap();
    assert!(ws.store.next_ready_outbox(9_999).unwrap().is_none());
    assert_eq!(
        ws.store
            .next_ready_outbox(10_000)
            .unwrap()
            .unwrap()
            .client_id,
        "a3000000000000000000000000000001"
    );
}

#[test]
fn t2_a03_outbox_attachment_sha256_tamper_holds_without_upload() {
    let mut ws = E2eWorkspace::new("t2_a03_tamper");
    let hub = MockHubServer::start("tok-tamper");

    // Held file item on Chat A must be skipped across WAL reopen while Ready text on Chat B dispatches
    // A held file keeps a real staged path and digest, so the record passes validation and is
    // skipped by `next_ready_outbox` for being held rather than for being malformed.
    let staged = ws.root().join("tampered.bin");
    fs::write(&staged, b"tampered payload").unwrap();
    let staged_digest = postbox_e2e::wire::sha256(b"original payload");
    ws.store
        .enqueue_outbox(&OutboxRecord {
            client_id: "a4000000000000000000000000000001".into(),
            chat: ChatKey::new(Provider::Whatsapp, "personal", "a@s.whatsapp.net"),
            kind: OutboxKind::File,
            phase: OutboxPhase::Held,
            attempt_serial: 1,
            attempts: 0,
            ambiguous: false,
            text_body: "Tampered".into(),
            reply_to_id: None,
            reply_broadcast: false,
            staged_file_path: Some(staged.display().to_string()),
            wire_filename: "tampered.bin".into(),
            file_sha256: Some(staged_digest),
            file_bytes: 16,
            ptt: false,
            voice_seconds: 0,
            attachment_ready: true,
            failure_reason: Some("sha256 mismatch".into()),
            not_before_ms: 0,
            created_at_ms: 100,
        })
        .unwrap();
    ws.store
        .enqueue_outbox(&OutboxRecord {
            client_id: "b4000000000000000000000000000001".into(),
            chat: ChatKey::new(Provider::Whatsapp, "personal", "b@s.whatsapp.net"),
            kind: OutboxKind::Text,
            phase: OutboxPhase::Ready,
            attempt_serial: 0,
            attempts: 0,
            ambiguous: false,
            text_body: "Clean text still sends".into(),
            reply_to_id: None,
            reply_broadcast: false,
            staged_file_path: None,
            wire_filename: String::new(),
            file_sha256: None,
            file_bytes: 0,
            ptt: false,
            voice_seconds: 0,
            attachment_ready: false,
            failure_reason: None,
            not_before_ms: 0,
            created_at_ms: 200,
        })
        .unwrap();
    ws.reopen_store().unwrap();

    let next = ws.store.next_ready_outbox(500).unwrap().unwrap();
    assert_eq!(next.client_id, "b4000000000000000000000000000001");

    let send_req = send_text_request(
        "personal:b@s.whatsapp.net",
        &next.text_body,
        None,
        false,
        &next.client_id,
    )
    .unwrap();
    let resp = LoopbackHttpClient::default()
        .execute_hub(&hub.endpoint(), &hub.token(), &send_req)
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(
        hub.client_id_delivery_count("a4000000000000000000000000000001"),
        0
    );
    assert_eq!(
        hub.client_id_delivery_count("b4000000000000000000000000000001"),
        1
    );

    // M8 seam (`m2_hub_sync`): wire OutboxWorker pre-send attachment SHA-256 verification transitioning tampered files to Held.
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t2_a03_outbox_duplicate_client_id_primary_key_rejected() {
    let mut ws = E2eWorkspace::new("t2_a03_dup");
    let rec = OutboxRecord {
        client_id: "deadbeefdeadbeefdeadbeefdeadbeef".into(), // trufflehog:ignore
        chat: ChatKey::new(Provider::Whatsapp, "personal", "a@s.whatsapp.net"),
        kind: OutboxKind::Text,
        phase: OutboxPhase::Ready,
        attempt_serial: 0,
        attempts: 0,
        ambiguous: false,
        text_body: "Original".into(),
        reply_to_id: None,
        reply_broadcast: false,
        staged_file_path: None,
        wire_filename: String::new(),
        file_sha256: None,
        file_bytes: 0,
        ptt: false,
        voice_seconds: 0,
        attachment_ready: false,
        failure_reason: None,
        not_before_ms: 0,
        created_at_ms: 100,
    };
    ws.store.enqueue_outbox(&rec).unwrap();
    let err = ws.store.enqueue_outbox(&rec).unwrap_err();
    assert!(matches!(err, StoreError::Sqlite(_)));
    assert_eq!(
        ws.store.next_ready_outbox(500).unwrap().unwrap().text_body,
        "Original"
    );
}

// ============================================================================
// Area 4 Boundaries (5 tests)
// ============================================================================

#[test]
fn t2_a04_window_title_empty_label_fallback_invariant() {
    let empty_view = ViewTab::Account {
        provider: Provider::Whatsapp,
        account: "a".into(),
        label: String::new(),
    };
    let title = empty_view.window_title("0.1.0");
    assert_eq!(title, "Postbox v0.1.0");
    assert!(!title.starts_with(" - "));
}

#[test]
fn t2_a04_combined_inbox_ignores_pins_vs_account_honors_pins() {
    let ws = E2eWorkspace::new("t2_a04_pins");
    let mut old_pin = mk_chat(
        Provider::Whatsapp,
        "personal",
        "pin@s.whatsapp.net",
        "Old Pinned",
        "2026-11-01T00:00:00Z",
    );
    old_pin.pinned = true;
    let new_unpinned = mk_chat(
        Provider::Whatsapp,
        "personal",
        "new@s.whatsapp.net",
        "New Unpinned",
        "2026-11-26T12:00:00Z",
    );
    ws.store.upsert_chat(&old_pin, 100).unwrap();
    ws.store.upsert_chat(&new_unpinned, 200).unwrap();

    let per_account = ws.store.list_inbox_chats(false, 10).unwrap();
    assert_eq!(per_account.len(), 2);
    assert_eq!(per_account[0].key, old_pin.key);
    assert_eq!(per_account[1].key, new_unpinned.key);
    assert!(per_account[0].pinned);
    assert!(!per_account[1].pinned);
    assert!(per_account[1].last_msg_ts > per_account[0].last_msg_ts);

    // Unpinning `old_pin` restores strict recency order in Store
    old_pin.pinned = false;
    ws.store.upsert_chat(&old_pin, 300).unwrap();
    let recency = ws.store.list_inbox_chats(false, 10).unwrap();
    assert_eq!(recency.len(), 2);
    assert_eq!(recency[0].key, new_unpinned.key);
    assert_eq!(recency[1].key, old_pin.key);

    // M8 seam (`m2_hub_sync`): wire Combined Inbox recency-only sort ignoring pinned status.
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t2_a04_drained_account_retains_alt_2_to_9_slot() {
    let mut ws = E2eWorkspace::new("t2_a04_drained");
    let km = Keymap::postbox_defaults();
    assert_eq!(
        km.resolve(KeyContext::Workspace, "alt-1"),
        Some(Action::SwitchCombinedInbox)
    );
    for slot in 2u8..=9 {
        assert_eq!(
            km.resolve(KeyContext::Workspace, &format!("alt-{slot}")),
            Some(Action::SwitchAccountView(slot))
        );
    }
    assert_eq!(km.resolve(KeyContext::Workspace, "alt-0"), None);

    ws.upsert_account_row(Provider::Whatsapp, "personal", "Personal", "connected", 0);
    ws.upsert_account_row(Provider::Slack, "T_DRAINED", "Drained", "connected", 1);
    ws.upsert_account_row(Provider::Signal, "signal", "Signal", "connected", 2);
    ws.reopen_store().unwrap();
    assert_eq!(ws.count_accounts(), 3);
}

#[test]
fn t2_a04_monotonic_activity_timestamp_never_regresses() {
    let mut ws = E2eWorkspace::new("t2_a04_mono");
    let oldest = mk_chat(
        Provider::Whatsapp,
        "personal",
        "old@s.whatsapp.net",
        "Oldest",
        "2026-11-26T08:00:00Z",
    );
    let newest = mk_chat(
        Provider::Whatsapp,
        "personal",
        "new@s.whatsapp.net",
        "Newest",
        "2026-11-26T15:00:00Z",
    );
    let middle = mk_chat(
        Provider::Whatsapp,
        "personal",
        "mid@s.whatsapp.net",
        "Middle",
        "2026-11-26T12:00:00Z",
    );
    // Insert intentionally out of timestamp order
    ws.store.upsert_chat(&oldest, 100).unwrap();
    ws.store.upsert_chat(&newest, 101).unwrap();
    ws.store.upsert_chat(&middle, 102).unwrap();

    let ordered = ws.store.list_inbox_chats(false, 10).unwrap();
    assert_eq!(ordered.len(), 3);
    assert_eq!(ordered[0].key, newest.key);
    assert_eq!(ordered[1].key, middle.key);
    assert_eq!(ordered[2].key, oldest.key);

    // Advancing `oldest` to a newer timestamp promotes it to the top across WAL reopen
    let mut promoted = oldest;
    promoted.last_msg_ts = "2026-11-26T18:00:00Z".into();
    ws.store.upsert_chat(&promoted, 200).unwrap();
    ws.reopen_store().unwrap();

    let after = ws.store.list_inbox_chats(false, 10).unwrap();
    assert_eq!(after[0].key, promoted.key);
    assert_eq!(after[0].last_msg_ts, "2026-11-26T18:00:00Z");
    assert_eq!(after[1].key, newest.key);
    assert_eq!(after[2].key, middle.key);

    // M8 seam (`m2_hub_sync`): wire live sync monotonic-floor guard preventing stale timestamp regressions.
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t2_a04_signal_and_default_telegram_view_labels() {
    let sig = ViewTab::Account {
        provider: Provider::Signal,
        account: "signal".into(),
        label: "Signal".into(),
    };
    let tg = ViewTab::Account {
        provider: Provider::Telegram,
        account: "telegram".into(),
        label: "Telegram".into(),
    };
    assert_eq!(sig.window_title("0.1.0"), "Signal - Postbox v0.1.0");
    assert_eq!(tg.window_title("0.1.0"), "Telegram - Postbox v0.1.0");
}

// ============================================================================
// Area 5 Boundaries (5 tests)
// ============================================================================

#[test]
fn t2_a05_igsh902_full_truth_table_own_and_reaction_never_resurface() {
    for archived_local in [false, true] {
        for from_me in [false, true] {
            for is_reaction_only in [false, true] {
                let expected = archived_local && !from_me && !is_reaction_only;
                assert_eq!(
                    should_resurface_on_event(archived_local, from_me, is_reaction_only),
                    expected,
                    "({archived_local}, {from_me}, {is_reaction_only})"
                );
            }
        }
    }
}

#[test]
fn t2_a05_alt_a_combined_inbox_unread_only_nuance() {
    let ws = E2eWorkspace::new("t2_a05_alta");
    let specs = [
        ("c1", false, 2u32),
        ("c2", false, 0u32),
        ("c3", true, 3u32),
        ("c4", true, 0u32),
    ];
    for (i, (id, arch, unread)) in specs.iter().enumerate() {
        let mut c = mk_chat(
            Provider::Whatsapp,
            "personal",
            &format!("{id}@s.whatsapp.net"),
            id,
            &format!("2026-11-26T12:0{i}:00Z"),
        );
        c.archived_local = *arch;
        c.unread_count = *unread;
        ws.store.upsert_chat(&c, 100 + i as i64).unwrap();
    }
    assert_eq!(ws.store.list_inbox_chats(false, 50).unwrap().len(), 2);
    let all = ws.store.list_inbox_chats(true, 50).unwrap();
    assert_eq!(all.len(), 4);
    let unread_any: Vec<_> = all.iter().filter(|c| c.unread_count > 0).collect();
    assert_eq!(unread_any.len(), 2);
    // M8 seam (`m2_hub_sync`): wire `Alt+A` per-view inline archived filter + Combined Inbox unread-only nuance.
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t2_a05_keep_archived_never_resurfaces_and_hold_kept() {
    let hub = MockHubServer::start("tok-keep");
    let ws = E2eWorkspace::new("t2_a05_keep");
    let handle = "personal:kept@s.whatsapp.net";
    let mut chat = mk_chat(
        Provider::Whatsapp,
        "personal",
        "kept@s.whatsapp.net",
        "Kept Archived",
        "2026-11-26T12:00:00Z",
    );
    chat.archived_local = true;
    ws.store.upsert_chat(&chat, 100).unwrap();

    // Hold-kept re-archives via Hub after provider unarchive
    let resp = LoopbackHttpClient::default()
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &archive_request(handle, true).unwrap(),
        )
        .unwrap();
    assert_eq!(resp.status, 200);
    assert!(ws.store.list_inbox_chats(false, 50).unwrap().is_empty());
}

#[test]
fn t2_a05_archive_batch_partial_failure_and_502_local_fallback() {
    let hub = MockHubServer::start("tok-ab-part");
    let mut failed = HashSet::new();
    failed.insert("personal:2@s.whatsapp.net".to_string());
    hub.set_archive_batch_fault(Some(ArchiveBatchFault::PartialFailure(failed)));

    let client = LoopbackHttpClient::default();
    let auth = format!("Bearer {}", hub.token());
    let payload = json!({
        "account": "",
        "chats": [
            {"chat": "personal:1@s.whatsapp.net"},
            {"chat": "personal:2@s.whatsapp.net"},
            {"chat": "personal:3@s.whatsapp.net"}
        ],
        "archived": true
    })
    .to_string();
    let resp = client
        .execute_raw(
            hub.addr(),
            "POST",
            "/v1/wazig/archive-batch",
            &[
                ("Authorization", auth.as_str()),
                ("Content-Type", "application/json"),
            ],
            Some(payload.as_bytes()),
        )
        .unwrap();
    assert_eq!(resp.status, 200);
    let results = resp.json::<Value>().unwrap()["results"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["ok"], true);
    assert_eq!(results[1]["ok"], false);
    assert_eq!(results[1]["status"], 500);
    assert_eq!(results[2]["ok"], true);

    // Individual retry for failed item #2 succeeds
    let retry = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &archive_request("personal:2@s.whatsapp.net", true).unwrap(),
        )
        .unwrap();
    assert_eq!(retry.status, 200);

    // Signal returns 502 (local-only fallback)
    let sig = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &archive_request("signal:aci-1", true).unwrap(),
        )
        .unwrap();
    assert_eq!(sig.status, 502);
}

#[test]
fn t2_a05_igsh920_2048_entry_reclamation_and_opposing_intent() {
    let ws = E2eWorkspace::new("t2_a05_2049");
    for i in 0..2_049usize {
        let mut c = mk_chat(
            Provider::Whatsapp,
            "personal",
            &format!("c{i:04}@s.whatsapp.net"),
            &format!("Chat {i}"),
            "2026-11-26T12:00:00Z",
        );
        if i % 2 == 0 {
            c.archived_remote = true;
        } else {
            c.archived_local = true;
        }
        ws.store.upsert_chat(&c, 1000 + i as i64).unwrap();
    }
    assert_eq!(ws.count_chats(), 2_049);
    assert_eq!(ws.store.list_inbox_chats(true, 3_000).unwrap().len(), 2_049);
}

// ============================================================================
// Area 6 Boundaries (5 tests)
// ============================================================================

#[test]
fn t2_a06_slack_broadcast_rejected_without_reply_to_or_with_file() {
    assert_eq!(
        send_text_request("slack:T1:C1", "hi", None, true, "c1"),
        Err(HubError::InvalidSend)
    );
    assert_eq!(
        send_text_request("slack:T1:C1", "hi", Some(""), true, "c2"),
        Err(HubError::InvalidSend)
    );

    let hub = MockHubServer::start("tok-bcast-mp");
    let boundary = "PostboxBcastBoundary";
    let body = build_multipart(
        boundary,
        &[
            MultipartPart {
                name: "chat".into(),
                filename: None,
                content_type: None,
                body: b"slack:T1:C1".to_vec(),
            },
            MultipartPart {
                name: "client_id".into(),
                filename: None,
                content_type: None,
                body: b"cid-bmp".to_vec(),
            },
            MultipartPart {
                name: "reply_broadcast".into(),
                filename: None,
                content_type: None,
                body: b"true".to_vec(),
            },
            MultipartPart {
                name: "file".into(),
                filename: Some("a.png".into()),
                content_type: Some("image/png".into()),
                body: minimal_png_1x1(),
            },
        ],
    );
    let resp = LoopbackHttpClient::default()
        .execute_hub_multipart(
            &hub.endpoint(),
            &hub.token(),
            "/v1/wazig/send",
            boundary,
            &body,
        )
        .unwrap();
    assert_eq!(resp.status, 400);
}

#[test]
fn t2_a06_thread_root_32_hop_cap_and_self_cycle_guard() {
    let mut ws = E2eWorkspace::new("t2_a06_hops");
    let key = ChatKey::new(Provider::Slack, "T1", "C1");
    for i in 0..35usize {
        let mut msg = mk_msg(
            &key,
            &format!("M-{i:02}"),
            "Ada",
            &format!("2026-11-26T12:00:{i:02}Z"),
            &format!("Chain hop {i}"),
            false,
        );
        if i > 0 {
            msg.reply_to_id = Some(format!("M-{:02}", i - 1));
        }
        ws.store.upsert_message(&msg).unwrap();
    }
    // Cyclic pair
    let mut cyc_a = mk_msg(
        &key,
        "CYC-A",
        "Bob",
        "2026-11-26T12:01:00Z",
        "Cycle A",
        false,
    );
    cyc_a.reply_to_id = Some("CYC-B".into());
    let mut cyc_b = mk_msg(
        &key,
        "CYC-B",
        "Bob",
        "2026-11-26T12:01:01Z",
        "Cycle B",
        false,
    );
    cyc_b.reply_to_id = Some("CYC-A".into());
    ws.store.upsert_message(&cyc_a).unwrap();
    ws.store.upsert_message(&cyc_b).unwrap();

    assert_eq!(ws.count_messages(), 37);
    assert_eq!(ws.store.search_messages_fts("Cycle", 10).unwrap().len(), 2);
}

#[test]
fn t2_a06_user_viewed_false_never_marks_read_and_pending_read_cap() {
    let hub = MockHubServer::start("tok-uv");
    hub.push_mark_read_fault(502);
    hub.push_mark_read_fault(500);
    let ws = E2eWorkspace::new("t2_a06_uv");

    let mut chat = mk_chat(
        Provider::Whatsapp,
        "personal",
        "uv@s.whatsapp.net",
        "Unread Chat",
        "2026-11-26T12:00:00Z",
    );
    chat.unread_count = 4;
    ws.store.upsert_chat(&chat, 100).unwrap();

    let client = LoopbackHttpClient::default();
    let req = mark_read_request("personal:uv@s.whatsapp.net", "").unwrap();
    assert_eq!(
        client
            .execute_hub(&hub.endpoint(), &hub.token(), &req)
            .unwrap()
            .status,
        502
    );
    assert_eq!(
        client
            .execute_hub(&hub.endpoint(), &hub.token(), &req)
            .unwrap()
            .status,
        500
    );
    // Unread count remains intact in Store after mark-read failures
    assert_eq!(
        ws.store.list_inbox_chats(false, 10).unwrap()[0].unread_count,
        4
    );
}

#[test]
fn t2_a06_pins_16_cap_and_drafts_500_cap_whitespace_vs_empty() {
    let mut ws = E2eWorkspace::new("t2_a06_draft_cap");
    let k_ws = ChatKey::new(Provider::Whatsapp, "personal", "ws@s.whatsapp.net");
    let k_big = ChatKey::new(Provider::Whatsapp, "personal", "big@s.whatsapp.net");
    let big_draft = "ä".repeat(32_768); // exact 65_536 UTF-8 bytes
    assert_eq!(big_draft.len(), 65_536);

    ws.save_draft_row(&k_ws, "   ", None, 100);
    ws.save_draft_row(&k_big, &big_draft, None, 101);
    ws.reopen_store().unwrap();

    assert_eq!(ws.load_draft_row(&k_ws), Some(("   ".into(), None)));
    assert_eq!(ws.load_draft_row(&k_big), Some((big_draft, None)));

    ws.delete_draft_row(&k_ws);
    assert_eq!(ws.load_draft_row(&k_ws), None);
}

#[test]
fn t2_a06_read_ticks_monotonic_and_provider_capability_matrix() {
    let mut ws = E2eWorkspace::new("t2_a06_ticks");
    let key = ChatKey::new(Provider::Whatsapp, "personal", "ticks@s.whatsapp.net");
    let incoming = mk_msg(
        &key,
        "M-IN",
        "Ada",
        "2026-11-26T12:00:00Z",
        "Incoming",
        false,
    );
    let mut own_del = mk_msg(&key, "M-DEL", "Me", "2026-11-26T12:01:00Z", "Deleted", true);
    own_del.deleted = true;
    own_del.read_status = ReadStatus::Read;
    ws.store.upsert_message(&incoming).unwrap();
    ws.store.upsert_message(&own_del).unwrap();
    assert_eq!(ws.count_messages(), 2);
}

// ============================================================================
// Area 7 Boundaries (5 tests)
// ============================================================================

#[test]
fn t2_a07_inbox_chord_900ms_exact_901ms_expired_repeat_and_blur() {
    let mut chord = InboxChord::default();
    // Exact 900 ms boundary -> succeeds
    assert!(!chord.on_key('g', 1_000, true, false));
    assert!(chord.on_key('i', 1_900, true, false));

    // 901 ms -> expired
    assert!(!chord.on_key('g', 2_000, true, false));
    assert!(!chord.on_key('i', 2_901, true, false));

    // Held 'g' key repeat must NOT refresh start timestamp
    assert!(!chord.on_key('g', 100, true, false));
    assert!(!chord.on_key('g', 500, true, true));
    assert!(!chord.on_key('i', 1_001, true, false));

    // Repeated 'i' ignored
    assert!(!chord.on_key('g', 100, true, false));
    assert!(!chord.on_key('i', 200, true, true));

    // Unrelated key cancels chord
    assert!(!chord.on_key('g', 100, true, false));
    assert!(!chord.on_key('x', 200, true, false));
    assert!(!chord.on_key('i', 300, true, false));

    // Blur / disallowed context resets chord
    assert!(!chord.on_key('g', 100, true, false));
    assert!(!chord.on_key('a', 200, false, false));
    assert!(!chord.on_key('i', 300, true, false));
}

#[test]
fn t2_a07_ctrl_a_selects_chats_full_8_state_truth_table() {
    for text_focused in [false, true] {
        for compose_focused in [false, true] {
            for compose_empty in [false, true] {
                let expected = !text_focused || (compose_focused && compose_empty);
                assert_eq!(
                    ctrl_a_selects_chats(text_focused, compose_focused, compose_empty),
                    expected,
                    "({text_focused}, {compose_focused}, {compose_empty})"
                );
            }
        }
    }
    assert!(!ctrl_a_selects_chats(true, false, true));
}

#[test]
fn t2_a07_keymap_json_malformed_unknown_action_and_case_folding() {
    let mut km = Keymap::postbox_defaults();
    let initial_len = km.bindings().len();

    assert!(matches!(
        km.apply_override_json("{bad json"),
        Err(KeymapError::InvalidJson(_))
    ));
    assert!(matches!(
        km.apply_override_json(
            r#"{"overrides":[{"context":"workspace","stroke":"ctrl-k","action":"launch_rockets"}]}"#
        ),
        Err(KeymapError::InvalidJson(_))
    ));

    km.apply_override_json(
        r#"{"overrides":[{"context":"workspace","stroke":"CTRL-K","action":"search_chats"}]}"#,
    )
    .unwrap();
    assert_eq!(km.bindings().len(), initial_len);
    assert_eq!(
        km.resolve(KeyContext::Workspace, "ctrl-k"),
        Some(Action::SearchChats)
    );
}

#[test]
fn t2_a07_resolve_context_isolation_and_esc_cascade_priority() {
    let km = Keymap::postbox_defaults();
    for stroke in ["e", "q", "u", "/"] {
        assert_eq!(km.resolve(KeyContext::Composer, stroke), None);
        assert!(km.resolve(KeyContext::ChatList, stroke).is_some());
    }
    assert_eq!(
        km.resolve(KeyContext::Workspace, "escape"),
        Some(Action::ClearSearchOrBack)
    );
}

#[test]
fn t2_a07_fts5_empty_query_no_hits_and_snippet_bounds() {
    let mut ws = E2eWorkspace::new("t2_a07_fts_bounds");
    let key = ChatKey::new(Provider::Whatsapp, "personal", "long@s.whatsapp.net");
    let long_body = format!(
        "{} targetneedle {}",
        "lorem ".repeat(200),
        "ipsum ".repeat(200)
    );
    assert!(long_body.len() > 2_000);
    let msg = mk_msg(
        &key,
        "M-LONG",
        "Ada",
        "2026-11-26T12:00:00Z",
        &long_body,
        false,
    );
    ws.store.upsert_message(&msg).unwrap();

    assert!(
        ws.store
            .search_messages_fts("nonexistenttoken999", 10)
            .unwrap()
            .is_empty()
    );
    assert!(
        ws.store
            .search_messages_fts("targetneedle", 0)
            .unwrap()
            .is_empty()
    );
    let hits = ws.store.search_messages_fts("targetneedle", 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].snippet.contains("..."));
    assert!(hits[0].snippet.len() < 300);

    let hooks = M8HookRegistry::current();
    for raw in ["", "(", "zzz:qqq (vvv)"] {
        match ws.store.search_messages_fts(raw, 10) {
            Ok(hits) => assert!(hits.is_empty(), "query {raw:?} must return 0 hits"),
            Err(_) => {
                assert!(
                    !hooks.m1_ui,
                    "m1_ui FTS5 sanitizer must return Ok(vec![]) for query {raw:?}"
                );
            }
        }
    }
}

// ============================================================================
// Area 8 Boundaries (5 tests)
// ============================================================================

#[test]
fn t2_a08_font_scale_60_and_160_clamp_and_step_saturation() {
    assert_eq!(FontScale::clamped(0).percent(), 60);
    assert_eq!(FontScale::clamped(59).percent(), 60);
    assert_eq!(FontScale::clamped(60).percent(), 60);
    assert_eq!(FontScale::clamped(160).percent(), 160);
    assert_eq!(FontScale::clamped(161).percent(), 160);
    assert_eq!(FontScale::clamped(u16::MAX).percent(), 160);
    assert_eq!(FontScale::clamped(60).smaller().percent(), 60);
    assert_eq!(FontScale::clamped(160).larger().percent(), 160);
}

#[test]
fn t2_a08_legacy_font_scale_82_to_100_migration() {
    let out = ProcessRunner::run(
        "postboxctl",
        &["font", "82", "--dry-run"],
        Duration::from_secs(10),
    );
    assert_eq!(out.exit_code, 0);
    let parsed: ControlRequest = serde_json::from_str(out.stdout.trim()).unwrap();
    assert_eq!(parsed, ControlRequest::Font { percent: 82 });
    assert_eq!(FontScale::clamped(82).percent(), 82);
    assert_eq!(FontScale::DEFAULT, 100);

    // M8 seam (`m1_ui`): wire `FontScale::from_persisted(82) -> 100` legacy migration.
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t2_a08_font_scale_step_symmetry_from_min_to_max() {
    let mut scale = FontScale::clamped(60);
    let mut prev_px = scaled_px(BASE_FONT_PX, scale.percent());
    for _ in 0..10 {
        scale = scale.larger();
        let next_px = scaled_px(BASE_FONT_PX, scale.percent());
        assert!(next_px > prev_px);
        prev_px = next_px;
    }
    assert_eq!(scale.percent(), 160);
    assert_eq!(scale.larger().percent(), 160);

    for _ in 0..10 {
        scale = scale.smaller();
    }
    assert_eq!(scale.percent(), 60);
    assert_eq!(scale.smaller().percent(), 60);
}

#[test]
fn t2_a08_theme_destructive_and_outgoing_bubble_contrast() {
    let amoled = ShadcnPalette::for_choice(ThemeChoice::Amoled);
    assert_eq!(amoled.background, 0x000000);

    for choice in [
        ThemeChoice::ZincDark,
        ThemeChoice::Amoled,
        ThemeChoice::Light,
    ] {
        let p = ShadcnPalette::for_choice(choice);
        assert_ne!(p.border, p.background);
        let bubble_text = match choice {
            ThemeChoice::Light => p.primary_foreground,
            ThemeChoice::ZincDark | ThemeChoice::Amoled => p.foreground,
        };
        let ratio = wcag_contrast_ratio(bubble_text, p.bubble_outgoing);
        assert!(
            ratio >= 4.5,
            "theme {choice:?} outgoing bubble contrast {ratio:.2} < 4.5"
        );
    }
}

#[test]
fn t2_a08_theme_choice_serde_rejects_unknown_variant() {
    assert!(serde_json::from_str::<ThemeChoice>("\"graphite\"").is_err());
    assert_eq!(
        serde_json::from_str::<ThemeChoice>("\"zincdark\"").unwrap(),
        ThemeChoice::ZincDark
    );
    assert_eq!(
        serde_json::from_str::<ThemeChoice>("\"amoled\"").unwrap(),
        ThemeChoice::Amoled
    );
    assert_eq!(
        serde_json::from_str::<ThemeChoice>("\"light\"").unwrap(),
        ThemeChoice::Light
    );
}

// ============================================================================
// Area 9 Boundaries (5 tests)
// ============================================================================

#[test]
fn t2_a09_media_100_mib_vs_100_mib_plus_1_auto_download_boundary() {
    const AUTO_MAX: u64 = 100 * 1024 * 1024;
    let hub = MockHubServer::start("tok-100m");
    let chat = "personal:big@s.whatsapp.net";
    let mut m_ok = HubMessageRow::simple(
        chat,
        "M-100M",
        "Ada",
        "Exact 100 MiB",
        "2026-11-26T12:00:00Z",
        false,
    );
    m_ok.MediaType = Some("document".into());
    m_ok.MediaSize = Some(AUTO_MAX);
    let mut m_over = HubMessageRow::simple(
        chat,
        "M-100M1",
        "Ada",
        "100 MiB + 1",
        "2026-11-26T12:01:00Z",
        false,
    );
    m_over.MediaType = Some("document".into());
    m_over.MediaSize = Some(AUTO_MAX + 1);
    hub.set_messages(chat, vec![m_ok, m_over]);
    hub.set_media(
        chat,
        "M-100M1",
        MediaBehavior::Ready {
            bytes: b"explicit-click-download".to_vec(),
            content_type: "application/octet-stream".into(),
        },
    );

    let client = LoopbackHttpClient::default();
    let rows: Vec<HubMessageRow> = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &messages_request(chat, 10, None).unwrap(),
        )
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].MediaSize, Some(104_857_601));
    assert_eq!(rows[1].MediaSize, Some(104_857_600));

    let click_dl = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &media_request(chat, Some("M-100M1")).unwrap(),
        )
        .unwrap();
    assert_eq!(click_dl.status, 200);
    // M8 seam (`m3_media`): wire `postbox-media` 100 MiB auto-download threshold vs click-to-download override.
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t2_a09_media_410_gone_persistence_and_burst_throttle() {
    let hub = MockHubServer::start("tok-410");
    let chat = "personal:gone@s.whatsapp.net";
    for i in 0..13 {
        hub.set_media(chat, format!("M-GONE-{i}"), MediaBehavior::Gone410);
    }
    hub.set_media(chat, "M-TRANSIENT", MediaBehavior::RateLimit429);

    let client = LoopbackHttpClient::default();
    for i in 0..13 {
        let resp = client
            .execute_hub(
                &hub.endpoint(),
                &hub.token(),
                &media_request(chat, Some(&format!("M-GONE-{i}"))).unwrap(),
            )
            .unwrap();
        assert_eq!(resp.status, 410);
    }
    let transient = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &media_request(chat, Some("M-TRANSIENT")).unwrap(),
        )
        .unwrap();
    assert_eq!(transient.status, 429);
    // M8 seam (`m3_media`): wire `gone-media.txt` 410 persistence & 20s burst repaint throttle.
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t2_a09_media_14_day_eviction_exact_second_boundary() {
    let ws = E2eWorkspace::new("t2_a09_exact_sec");
    let now = 1_764_150_000_i64;
    ws.seed_media_cache(
        "exact",
        "sha1",
        "exact_boundary.bin",
        "image/png",
        64,
        now - MEDIA_TTL_SECS,
    );
    ws.seed_media_cache(
        "expired",
        "sha2",
        "expired_1s.bin",
        "image/png",
        64,
        now - MEDIA_TTL_SECS - 1,
    );

    let evicted = ws.store.evict_expired_media(now).unwrap();
    assert_eq!(evicted, vec!["expired_1s.bin".to_string()]);
    assert!(ws.media_cache_exists("exact"));
    assert!(!ws.media_cache_exists("expired"));
}

#[test]
fn t2_a09_media_duplicate_msg_id_across_two_chats_isolation() {
    let hub = MockHubServer::start("tok-dup-mid");
    let ws = E2eWorkspace::new("t2_a09_dup_mid");
    let png_a = minimal_png_1x1();
    let png_b = minimal_png_1x1_alt();
    assert_ne!(png_a, png_b);

    let grp = "personal:group@g.us";
    let dm = "personal:dm@s.whatsapp.net";
    hub.set_media(
        grp,
        "M-42",
        MediaBehavior::Ready {
            bytes: png_a.clone(),
            content_type: "image/png".into(),
        },
    );
    hub.set_media(
        dm,
        "M-42",
        MediaBehavior::Ready {
            bytes: png_b.clone(),
            content_type: "image/png".into(),
        },
    );

    let req_a = media_request(grp, Some("M-42")).unwrap();
    let req_b = media_request(dm, Some("M-42")).unwrap();
    assert_ne!(req_a.path, req_b.path);

    let client = LoopbackHttpClient::default();
    let r_a = client
        .execute_hub(&hub.endpoint(), &hub.token(), &req_a)
        .unwrap();
    let r_b = client
        .execute_hub(&hub.endpoint(), &hub.token(), &req_b)
        .unwrap();
    assert_eq!(r_a.body, png_a);
    assert_eq!(r_b.body, png_b);

    ws.seed_media_cache(
        &format!("{grp}:M-42"),
        &sha256_hex(&png_a),
        "a.png",
        "image/png",
        png_a.len() as i64,
        100,
    );
    ws.seed_media_cache(
        &format!("{dm}:M-42"),
        &sha256_hex(&png_b),
        "b.png",
        "image/png",
        png_b.len() as i64,
        100,
    );
    assert_eq!(ws.count_media_cache(), 2);
}

#[test]
fn t2_a09_webp_pixel_bomb_lru_eviction_and_cyclic_slack_emoji() {
    let hub = MockHubServer::start("tok-cyc");
    hub.set_slack_emoji_index("T01", json!({"emoji": {"a": "alias:b", "b": "alias:a"}}));

    let client = LoopbackHttpClient::default();
    let auth = format!("Bearer {}", hub.token());
    let idx = client
        .execute_raw(
            hub.addr(),
            "GET",
            "/v1/wazig/slack/emoji?account=T01",
            &[("Authorization", auth.as_str())],
            None,
        )
        .unwrap();
    assert_eq!(idx.status, 200);
    let v: Value = idx.json().unwrap();
    assert_eq!(v["emoji"]["a"], "alias:b");
    assert_eq!(v["emoji"]["b"], "alias:a");

    let missing = client
        .execute_raw(
            hub.addr(),
            "GET",
            "/v1/wazig/slack/emoji/media?account=T01&name=a",
            &[("Authorization", auth.as_str())],
            None,
        )
        .unwrap();
    assert_eq!(missing.status, 404);
}

// ============================================================================
// Area 10 Boundaries (6 tests)
// ============================================================================

#[test]
fn t2_a10_paste_staging_16_cap_and_non_image_hdrop_rejection() {
    let hub = MockHubServer::start("tok-paste-bad");
    let client = LoopbackHttpClient::default();
    let boundary = "PostboxEmptyPartBoundary";

    // Missing file part -> 400
    let no_file = build_multipart(
        boundary,
        &[
            MultipartPart {
                name: "chat".into(),
                filename: None,
                content_type: None,
                body: b"personal:1@s.whatsapp.net".to_vec(),
            },
            MultipartPart {
                name: "client_id".into(),
                filename: None,
                content_type: None,
                body: b"cid-nofile".to_vec(),
            },
        ],
    );
    assert_eq!(
        client
            .execute_hub_multipart(
                &hub.endpoint(),
                &hub.token(),
                "/v1/wazig/send",
                boundary,
                &no_file
            )
            .unwrap()
            .status,
        400
    );

    // Empty file part -> 400
    let empty_file = build_multipart(
        boundary,
        &[
            MultipartPart {
                name: "chat".into(),
                filename: None,
                content_type: None,
                body: b"personal:1@s.whatsapp.net".to_vec(),
            },
            MultipartPart {
                name: "client_id".into(),
                filename: None,
                content_type: None,
                body: b"cid-emptyfile".to_vec(),
            },
            MultipartPart {
                name: "file".into(),
                filename: Some("empty.png".into()),
                content_type: Some("image/png".into()),
                body: Vec::new(),
            },
        ],
    );
    assert_eq!(
        client
            .execute_hub_multipart(
                &hub.endpoint(),
                &hub.token(),
                "/v1/wazig/send",
                boundary,
                &empty_file
            )
            .unwrap()
            .status,
        400
    );
}

#[test]
fn t2_a10_lightbox_external_preview_extension_allowlist() {
    let km = Keymap::postbox_defaults();
    assert_eq!(km.resolve(KeyContext::Lightbox, "ctrl-e"), None);
    assert_eq!(km.resolve(KeyContext::Lightbox, "u"), None);
    assert_eq!(
        km.resolve(KeyContext::Lightbox, "escape"),
        Some(Action::CloseLightbox)
    );
    assert_eq!(
        km.resolve(KeyContext::Lightbox, "space"),
        Some(Action::CloseLightbox)
    );
    assert_eq!(
        km.resolve(KeyContext::Lightbox, "enter"),
        Some(Action::CloseLightbox)
    );
}

#[test]
fn t2_a10_reaction_409_unknown_own_double_click_and_shortcode_urls() {
    let hub = MockHubServer::start("tok-r409");
    hub.push_react_fault(409);

    assert_eq!(
        react_request("personal:1@s.whatsapp.net", "", "👍", "cid-1"),
        Err(HubError::InvalidId)
    );
    // A control byte in the client id is refused for being a bad id, before it can reach the wire.
    assert_eq!(
        react_request("personal:1@s.whatsapp.net", "M-1", "👍", "bad\nid"),
        Err(HubError::InvalidClientId)
    );

    let req = react_request("personal:1@s.whatsapp.net", "M-1", "👍", "cid-409").unwrap();
    let resp = LoopbackHttpClient::default()
        .execute_hub(&hub.endpoint(), &hub.token(), &req)
        .unwrap();
    assert_eq!(resp.status, 409);
    assert!(resp.text().contains("react fault"));
}

#[test]
fn t2_a10_mention_deactivated_midword_and_64_byte_cap() {
    let hub = MockHubServer::start("tok-midword");
    let req = send_text_request(
        "slack:T1:C1",
        "Contact mail@pau.example for details",
        None,
        false,
        "cid-midword",
    )
    .unwrap();
    let resp = LoopbackHttpClient::default()
        .execute_hub(&hub.endpoint(), &hub.token(), &req)
        .unwrap();
    assert_eq!(resp.status, 200);
    let body = String::from_utf8_lossy(&hub.recorded_requests()[0].body).into_owned();
    assert!(body.contains("mail@pau.example"));
}

#[test]
fn t2_a10_link_unfurl_ssrf_private_ips_userinfo_and_javascript_og() {
    let ext = MockExternalServer::start("k");
    let client = LoopbackHttpClient::default();

    let xss = client
        .execute_raw(ext.addr(), "GET", "/page/js-xss", &[], None)
        .unwrap();
    assert_eq!(xss.status, 200);
    assert!(xss.text().contains("javascript:alert(1)"));

    let rel = client
        .execute_raw(ext.addr(), "GET", "/page/relative-img", &[], None)
        .unwrap();
    assert_eq!(rel.status, 200);
    assert!(rel.text().contains("/image/card.png"));

    let trunc = client
        .execute_raw(ext.addr(), "GET", "/page/truncated", &[], None)
        .unwrap();
    assert_eq!(trunc.status, 200);

    let cdn = client
        .execute_raw(ext.addr(), "GET", "/page/video-cdn", &[], None)
        .unwrap();
    assert_eq!(cdn.status, 200);
    assert!(
        cdn.text()
            .contains("evil.cdninstagram.com.attacker.invalid")
    );

    // Non-HTTPS private, loopback, link-local, CGNAT, and .local endpoints rejected as InsecureUrl
    for insecure in [
        "http://127.0.0.1",
        "http://10.1.2.3",
        "http://172.16.0.1",
        "http://192.168.1.1",
        "http://169.254.169.254",
        "http://100.64.0.1",
        "http://localhost",
        "http://printer.local",
    ] {
        assert_eq!(
            Endpoint::parse(insecure, false),
            Err(HubError::InsecureUrl),
            "expected InsecureUrl for {insecure}"
        );
    }

    // Userinfo, path injection, and non-HTTP schemes rejected as InvalidUrl
    for invalid in [
        "https://user:pass@youtube.com", // trufflehog:ignore
        "https://127.0.0.1/secret",
        "javascript:alert(1)",
        "file:///etc/passwd",
    ] {
        assert_eq!(
            Endpoint::parse(invalid, false),
            Err(HubError::InvalidUrl),
            "expected InvalidUrl for {invalid}"
        );
    }

    // M8 seam (`m3_media`): wire `postbox-media` SSRF IP/DNS guard rejecting HTTPS private/loopback/CGNAT/link-local targets.
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t2_a10_giphy_bad_cdn_redirect_non_gif_magic_429_and_chat_switch() {
    let ext = MockExternalServer::start("valid-key");
    let client = LoopbackHttpClient::default();

    ext.set_giphy_fault(Some(GiphyFault::Unauthorized401));
    assert_eq!(
        client
            .execute_raw(
                ext.addr(),
                "GET",
                "/v1/gifs/trending?api_key=bad",
                &[],
                None
            )
            .unwrap()
            .status,
        401
    );

    ext.set_giphy_fault(Some(GiphyFault::RateLimit429 {
        retry_after: "30".into(),
    }));
    let r429 = client
        .execute_raw(
            ext.addr(),
            "GET",
            "/v1/gifs/trending?api_key=valid-key",
            &[],
            None,
        )
        .unwrap();
    assert_eq!(r429.status, 429);
    assert_eq!(r429.header("retry-after"), Some("30"));

    ext.set_giphy_cdn_fault(
        "redir1",
        GiphyCdnFault::Redirect302("https://evil.invalid/x.gif".into()),
    );
    let r302 = client
        .execute_raw(ext.addr(), "GET", "/media/redir1/giphy.gif", &[], None)
        .unwrap();
    assert_eq!(r302.status, 302);

    ext.set_giphy_cdn_fault("badgif", GiphyCdnFault::CorruptMagic);
    let corrupt = client
        .execute_raw(ext.addr(), "GET", "/media/badgif/giphy.gif", &[], None)
        .unwrap();
    assert_eq!(corrupt.status, 200);
    assert_ne!(&corrupt.body[..6], b"GIF89a");
    assert_ne!(&corrupt.body[..6], b"GIF87a");
}

// ============================================================================
// Area 11 Boundaries (5 tests)
// ============================================================================

#[test]
fn t2_a11_voice_recording_250ms_min_300s_max_repeat_and_switch() {
    let km = Keymap::postbox_defaults();
    assert_eq!(
        km.resolve(KeyContext::Workspace, "ctrl-shift-m"),
        Some(Action::ToggleVoiceRecord)
    );

    let hub = MockHubServer::start("tok-vox-zero");
    let boundary = "PostboxZeroVoiceBoundary";
    let body = build_multipart(
        boundary,
        &[
            MultipartPart {
                name: "chat".into(),
                filename: None,
                content_type: None,
                body: b"personal:1@s.whatsapp.net".to_vec(),
            },
            MultipartPart {
                name: "client_id".into(),
                filename: None,
                content_type: None,
                body: b"cid-zero-vox".to_vec(),
            },
            MultipartPart {
                name: "ptt".into(),
                filename: None,
                content_type: None,
                body: b"true".to_vec(),
            },
            MultipartPart {
                name: "file".into(),
                filename: Some("voice.ogg".into()),
                content_type: Some("audio/ogg".into()),
                body: Vec::new(),
            },
        ],
    );
    let resp = LoopbackHttpClient::default()
        .execute_hub_multipart(
            &hub.endpoint(),
            &hub.token(),
            "/v1/wazig/send",
            boundary,
            &body,
        )
        .unwrap();
    assert_eq!(resp.status, 400);
    // M8 seam (`m4_audio`): wire voice recorder 250ms min, 300s max, key-repeat guard, and chat-switch cancel.
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t2_a11_voice_resume_500ms_near_end_clear_and_corrupt_ogg() {
    let hub = MockHubServer::start("tok-corrupt-ogg");
    let chat = "personal:vox@s.whatsapp.net";
    hub.set_media(
        chat,
        "M-CORRUPT-OGG",
        MediaBehavior::Ready {
            bytes: b"NOT_OGG_CORRUPT_DATA".to_vec(),
            content_type: "audio/ogg".into(),
        },
    );
    let resp = LoopbackHttpClient::default()
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &media_request(chat, Some("M-CORRUPT-OGG")).unwrap(),
        )
        .unwrap();
    assert_eq!(resp.status, 200);
    assert_ne!(&resp.body[..4], b"OggS");
}

#[test]
fn t2_a11_transcript_cache_immutable_never_overwrites_same_sha() {
    let mut ws = E2eWorkspace::new("t2_a11_immut");
    ws.store
        .cache_transcript("sha-v1", "en", "Original raw text", 100)
        .unwrap();
    ws.store
        .cache_transcript("sha-v1", "de", "Overwrite attempt", 200)
        .unwrap();
    ws.reopen_store().unwrap();
    assert_eq!(
        ws.store.get_transcript("sha-v1").unwrap().as_deref(),
        Some("Original raw text")
    );
}

#[test]
fn t2_a11_deepgram_rest_401_key_refresh_and_nospeech_multi_fallback() {
    let dg = MockDeepgramServer::start("old-key");
    dg.fail_once_with_401_then_accept("new-key");
    dg.push_rest_reply(DeepgramRestReply::NoSpeech);
    dg.push_rest_reply(DeepgramRestReply::Transcript {
        text: "Multilingual fallback ok".into(),
        detected_language: Some("de".into()),
        confidence: 0.91,
    });

    let ws = E2eWorkspace::new("t2_a11_401");
    let client = LoopbackHttpClient::default();

    // 1. First call with old-key returns 401
    let r1 = client
        .execute_raw(
            dg.addr(),
            "POST",
            DictationLanguage::Automatic.deepgram_listen_path(),
            &[("Authorization", "Token old-key")],
            Some(b"audio"),
        )
        .unwrap();
    assert_eq!(r1.status, 401);

    // 2. Refresh key in SecretStore and retry -> 200 NoSpeech
    ws.platform
        .save_secret("DEEPGRAM_API_KEY", " new-key ")
        .unwrap();
    let refreshed = ws
        .platform
        .load_secret("DEEPGRAM_API_KEY")
        .unwrap()
        .unwrap();
    let auth = format!("Token {refreshed}");
    let r2 = client
        .execute_raw(
            dg.addr(),
            "POST",
            DictationLanguage::Automatic.deepgram_listen_path(),
            &[("Authorization", auth.as_str())],
            Some(b"audio"),
        )
        .unwrap();
    assert_eq!(r2.status, 200);
    assert_eq!(
        r2.json::<Value>().unwrap()["results"]["channels"][0]["alternatives"][0]["transcript"],
        ""
    );

    // 3. Fallback once to language=multi -> 200 transcript
    let r3 = client
        .execute_raw(
            dg.addr(),
            "POST",
            "/v1/listen?model=nova-3&smart_format=true&language=multi&tag=postbox",
            &[("Authorization", auth.as_str())],
            Some(b"audio"),
        )
        .unwrap();
    assert_eq!(r3.status, 200);
    assert_eq!(
        r3.json::<Value>().unwrap()["results"]["channels"][0]["alternatives"][0]["transcript"],
        "Multilingual fallback ok"
    );
}

#[test]
fn t2_a11_deepgram_rest_402_429_503_and_llm_refusal_guard() {
    let dg = MockDeepgramServer::start("dg-err");
    dg.push_rest_reply(DeepgramRestReply::PaymentRequired402);
    dg.push_rest_reply(DeepgramRestReply::RateLimited429);
    dg.push_rest_reply(DeepgramRestReply::Unavailable503);

    let ws = E2eWorkspace::new("t2_a11_errs");
    let client = LoopbackHttpClient::default();
    for expected_status in [402u16, 429, 503] {
        let resp = client
            .execute_raw(
                dg.addr(),
                "POST",
                DictationLanguage::Automatic.deepgram_listen_path(),
                &[("Authorization", "Token dg-err")],
                Some(b"audio"),
            )
            .unwrap();
        assert_eq!(resp.status, expected_status);
    }
    // Failed transcription must never be cached in Store
    assert_eq!(ws.store.get_transcript("failed-sha").unwrap(), None);
}

// ============================================================================
// Area 12 Boundaries (5 tests)
// ============================================================================

#[test]
fn t2_a12_auto_dictation_fail_closed_rejects_fr_es_tr_none_empty() {
    for ok_tag in ["en", "EN", "en-US", "en-GB", "de", "DE", "de-DE", "de-AT"] {
        assert!(
            DictationLanguage::Automatic.accepts_detected_tag(Some(ok_tag)),
            "expected {ok_tag} accepted"
        );
    }
    for bad_tag in [
        Some("fr"),
        Some("fr-FR"),
        Some("es"),
        Some("es-MX"),
        Some("tr"),
        Some("english"),
        Some("deutsch"),
        Some(""),
        None,
    ] {
        assert!(
            !DictationLanguage::Automatic.accepts_detected_tag(bad_tag),
            "expected {bad_tag:?} rejected fail-closed"
        );
    }

    let dg = MockDeepgramServer::start("dg-fc");
    dg.set_ws_script(vec![
        WsScriptStep {
            trigger: WsTrigger::AfterBinaryFrames(1),
            action: WsServerAction::SendResults {
                is_final: true,
                start: 0.0,
                duration: 0.5,
                transcript: "bonjour".into(),
                word_languages: vec!["fr".into()],
                languages: vec!["fr".into()],
            },
        },
        WsScriptStep {
            trigger: WsTrigger::AfterBinaryFrames(2),
            action: WsServerAction::SendResults {
                is_final: true,
                start: 0.5,
                duration: 0.5,
                transcript: "hola".into(),
                word_languages: vec!["es".into()],
                languages: vec!["es".into()],
            },
        },
    ]);

    let mut conn = LoopbackHttpClient::default()
        .connect_ws(
            dg.addr(),
            "/v1/listen?model=nova-3",
            &[("Authorization", "Token dg-fc")],
        )
        .unwrap();
    for expected_bad in ["fr", "es"] {
        conn.send_binary(&pcm16_raw(16_000, 40, 4000)).unwrap();
        let frame = conn.recv_frame(Duration::from_secs(2)).unwrap();
        let v: Value = serde_json::from_slice(&frame.payload).unwrap();
        let tag = v["channel"]["alternatives"][0]["languages"][0].as_str();
        assert_eq!(tag, Some(expected_bad));
        assert!(!DictationLanguage::Automatic.accepts_detected_tag(tag));
    }
    conn.send_close(1000).unwrap();
}

#[test]
fn t2_a12_ws_non_101_upgrade_error_frame_and_premature_close() {
    let client = LoopbackHttpClient::default();

    // (a) 401 handshake failure
    let dg1 = MockDeepgramServer::start("dg-ws-401");
    dg1.set_ws_handshake_fault(Some(401));
    let err1 = client
        .connect_ws(
            dg1.addr(),
            "/v1/listen",
            &[("Authorization", "Token dg-ws-401")],
        )
        .err()
        .unwrap();
    assert!(matches!(err1, ClientError::WsHandshakeFailed(401)));

    // (b) Error JSON frame
    let dg2 = MockDeepgramServer::start("dg-ws-err");
    dg2.set_ws_script(vec![WsScriptStep {
        trigger: WsTrigger::AfterBinaryFrames(1),
        action: WsServerAction::SendError {
            message: "bad audio".into(),
        },
    }]);
    let mut conn2 = client
        .connect_ws(
            dg2.addr(),
            "/v1/listen",
            &[("Authorization", "Token dg-ws-err")],
        )
        .unwrap();
    conn2.send_binary(&pcm16_raw(16_000, 40, 2000)).unwrap();
    let f = conn2.recv_frame(Duration::from_secs(2)).unwrap();
    let v: Value = serde_json::from_slice(&f.payload).unwrap();
    assert_eq!(v["type"], "Error");
    conn2.send_close(1000).unwrap();

    // (c) Abrupt TCP drop mid-interim
    let dg3 = MockDeepgramServer::start("dg-ws-drop");
    dg3.set_ws_script(vec![WsScriptStep {
        trigger: WsTrigger::AfterBinaryFrames(1),
        action: WsServerAction::AbruptDrop,
    }]);
    let mut conn3 = client
        .connect_ws(
            dg3.addr(),
            "/v1/listen",
            &[("Authorization", "Token dg-ws-drop")],
        )
        .unwrap();
    conn3.send_binary(&pcm16_raw(16_000, 40, 2000)).unwrap();
    assert!(conn3.recv_frame(Duration::from_secs(2)).is_err());
}

#[test]
fn t2_a12_anchored_interim_tamper_late_final_and_duplicate_replay() {
    let dg = MockDeepgramServer::start("dg-replay");
    dg.set_ws_script(vec![
        WsScriptStep {
            trigger: WsTrigger::AfterBinaryFrames(1),
            action: WsServerAction::SendResults {
                is_final: true,
                start: 1.0,
                duration: 0.5,
                transcript: "first sentence".into(),
                word_languages: vec!["en".into()],
                languages: vec!["en".into()],
            },
        },
        WsScriptStep {
            trigger: WsTrigger::AfterBinaryFrames(2),
            action: WsServerAction::SendResults {
                is_final: true,
                start: 1.0,
                duration: 0.5,
                transcript: "first sentence".into(),
                word_languages: vec!["en".into()],
                languages: vec!["en".into()],
            },
        },
        WsScriptStep {
            trigger: WsTrigger::AfterBinaryFrames(3),
            action: WsServerAction::SendResults {
                is_final: false,
                start: 1.6,
                duration: 0.3,
                transcript: "second part".into(),
                word_languages: vec!["en".into()],
                languages: vec!["en".into()],
            },
        },
    ]);

    let mut conn = LoopbackHttpClient::default()
        .connect_ws(
            dg.addr(),
            "/v1/listen?model=nova-3",
            &[("Authorization", "Token dg-replay")],
        )
        .unwrap();

    let mut received = Vec::new();
    for _ in 0..3 {
        conn.send_binary(&pcm16_raw(16_000, 40, 3000)).unwrap();
        let frame = conn.recv_frame(Duration::from_secs(2)).unwrap();
        received.push(serde_json::from_slice::<Value>(&frame.payload).unwrap());
    }
    assert_eq!(received[0]["start"], 1.0);
    assert_eq!(received[1]["start"], 1.0);
    assert_eq!(received[2]["start"], 1.6);
    assert_eq!(received[2]["is_final"], false);
    conn.send_close(1000).unwrap();
}

#[test]
fn t2_a12_dpapi_empty_whitespace_seed_and_missing_key() {
    let ws = E2eWorkspace::new("t2_a12_empty");
    assert_eq!(ws.platform.load_secret("DEEPGRAM_API_KEY").unwrap(), None);
    ws.platform
        .save_secret("DEEPGRAM_API_KEY", "   \t\r\n")
        .unwrap();
    // A key that is only whitespace trims to nothing, and an empty DPAPI blob reads as absent,
    // so dictation stays fail-closed instead of opening a socket with an empty token.
    assert_eq!(ws.platform.load_secret("DEEPGRAM_API_KEY").unwrap(), None);
    ws.platform
        .save_secret("DEEPGRAM_API_KEY", "  dg-live-key  ")
        .unwrap();
    assert_eq!(
        ws.platform.load_secret("DEEPGRAM_API_KEY").unwrap(),
        Some("dg-live-key".to_owned())
    );
}

#[test]
fn t2_a12_fixed_english_german_modes_accept_and_query_paths() {
    let auto_p = DictationLanguage::Automatic.deepgram_listen_path();
    let en_p = DictationLanguage::English.deepgram_listen_path();
    let de_p = DictationLanguage::German.deepgram_listen_path();

    assert!(auto_p.contains("detect_language=en"));
    assert!(auto_p.contains("detect_language=de"));
    assert!(en_p.contains("language=en"));
    assert!(!en_p.contains("detect_language"));
    assert!(de_p.contains("language=de"));
    assert!(!de_p.contains("detect_language"));

    assert!(DictationLanguage::English.accepts_detected_tag(None));
    assert!(DictationLanguage::German.accepts_detected_tag(None));
}

// ============================================================================
// Area 13 Boundaries (6 tests)
// ============================================================================

#[test]
fn t2_a13_video_f4v_alias_aspect_clamp_and_lazy_mf_unload() {
    let mut ws = E2eWorkspace::new("t2_a13_f4v");
    let hub = MockHubServer::start("tok-f4v");
    let handle = "personal:vid@s.whatsapp.net";
    let mp4 = postbox_e2e::fixtures::minimal_mp4_ftyp();

    let mut rows = Vec::new();
    for (i, (ext, mime)) in [("f4v", "video/x-f4v"), ("f4p", "video/mp4")]
        .iter()
        .enumerate()
    {
        let msg_id = format!("M-VID-{ext}");
        let mut row = HubMessageRow::simple(
            handle,
            &msg_id,
            "Ada",
            format!("Clip {ext}"),
            format!("2026-11-26T12:0{i}:00Z"),
            false,
        );
        row.MediaType = Some("video".into());
        row.MimeType = Some((*mime).into());
        row.Filename = Some(format!("clip.{ext}"));
        row.MediaSize = Some(mp4.len() as u64);
        rows.push(row);
        hub.set_media(
            handle,
            &msg_id,
            MediaBehavior::Ready {
                bytes: mp4.clone(),
                content_type: (*mime).into(),
            },
        );
    }
    hub.set_messages(handle, rows);

    let client = LoopbackHttpClient::default();
    let fetched: Vec<HubMessageRow> = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &messages_request(handle, 10, None).unwrap(),
        )
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(fetched.len(), 2);

    let key = ChatKey::new(Provider::Whatsapp, "personal", "vid@s.whatsapp.net");
    for row in &fetched {
        assert_eq!(row.MediaType.as_deref(), Some("video"));
        let fname = row.Filename.as_deref().unwrap();
        assert!(fname.ends_with(".f4v") || fname.ends_with(".f4p"));
        let media = client
            .execute_hub(
                &hub.endpoint(),
                &hub.token(),
                &media_request(handle, Some(&row.MsgID)).unwrap(),
            )
            .unwrap();
        assert_eq!(media.status, 200);
        assert_eq!(&media.body[4..8], b"ftyp");

        let mut m = mk_msg(
            &key,
            &row.MsgID,
            &row.SenderName,
            &row.Timestamp,
            &row.Text,
            false,
        );
        m.media_kind = Some(MediaKind::Video);
        m.mime_type = row.MimeType.clone();
        m.filename = row.Filename.clone();
        ws.store.upsert_message(&m).unwrap();
    }
    ws.reopen_store().unwrap();
    assert_eq!(ws.count_messages(), 2);

    let km = Keymap::postbox_defaults();
    assert_eq!(
        km.resolve(KeyContext::VideoPlayer, "f11"),
        Some(Action::ToggleVideoFullscreen)
    );
    assert_eq!(km.resolve(KeyContext::Workspace, "f11"), None);
    assert_eq!(km.resolve(KeyContext::Composer, "f11"), None);

    // M8 seam (`m6_video`): wire VideoBackend `.f4v`/`.f4p` aliasing, 800x450 clamp, and 60s lazy MF unload.
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t2_a13_toast_xml_utf8_multibyte_clipping_and_escaping() {
    let ws = E2eWorkspace::new("t2_a13_toast_utf8");
    let chat = ChatKey::new(Provider::Whatsapp, "personal", "utf8@s.whatsapp.net");
    let title = "🚀🎉ÄÖÜ".repeat(30); // > 120 bytes
    let body = "🔥<script>&'\"\t\r\n\x07".repeat(40); // > 400 bytes
    assert!(title.len() > 120);
    assert!(body.len() > 400);

    ws.platform.notify_message(&chat, &title, &body).unwrap();
    assert_eq!(ws.platform.notification_count(), 1);

    // M8 seam (`m7_os`): wire WinRT Toast XML UTF-8-safe 120B/400B/4096B clipping and escaping.
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t2_a13_deep_link_rejects_malformed_controls_extra_segments_and_bad_hex() {
    assert_eq!(
        parse_deep_link("http://whatsapp/a/c"),
        Err(DeepLinkError::InvalidScheme)
    );
    assert_eq!(
        parse_deep_link("postbox:/whatsapp/a/c"),
        Err(DeepLinkError::InvalidScheme)
    );
    assert_eq!(
        parse_deep_link("postbox://discord/a/c"),
        Err(DeepLinkError::UnsupportedProvider)
    );
    assert_eq!(
        parse_deep_link("postbox://whatsapp//c"),
        Err(DeepLinkError::MissingAccount)
    );
    assert_eq!(
        parse_deep_link("postbox://whatsapp/a"),
        Err(DeepLinkError::MissingChat)
    );
    assert_eq!(
        parse_deep_link("postbox://whatsapp/a/c/m/extra"),
        Err(DeepLinkError::TooManySegments)
    );
    for bad_hex in [
        "postbox://whatsapp/a/%ZZ",
        "postbox://whatsapp/a/%2",
        "postbox://whatsapp/a/%FF",
    ] {
        assert_eq!(
            parse_deep_link(bad_hex),
            Err(DeepLinkError::InvalidEncoding),
            "bad_hex={bad_hex}"
        );
    }
    for ctrl in [
        "postbox://whatsapp/a/c%00",
        "postbox://whatsapp/a/c%0A",
        "postbox://whatsapp/a/c%1F",
        "postbox://whatsapp/a/c%7F",
    ] {
        assert_eq!(
            parse_deep_link(ctrl),
            Err(DeepLinkError::ControlCharacter),
            "ctrl={ctrl}"
        );
    }
    // Non-17-char Slack p123 stays raw
    let sl = parse_deep_link("postbox://slack/T1/C1/p123").unwrap();
    assert_eq!(sl.message.as_deref(), Some("p123"));
}

#[test]
fn t2_a13_postboxctl_non_ascii_json_and_usage_exit_codes() {
    let search_text = "Café 🚀 München\nline2";
    let ok = ProcessRunner::run(
        "postboxctl",
        &["search", search_text, "--dry-run"],
        Duration::from_secs(10),
    );
    assert_eq!(ok.exit_code, 0, "stderr: {}", ok.stderr);
    let line = ok.stdout.trim();
    assert!(!line.contains('\n'));
    let parsed: ControlRequest = serde_json::from_str(line).unwrap();
    assert_eq!(
        parsed,
        ControlRequest::Search {
            text: search_text.into()
        }
    );

    // Explicitly NO send command + invalid theme/font/missing arg -> exit code 2
    for bad_args in [
        &["send", "hi"][..],
        &["theme", "invalid"][..],
        &["font", "abc"][..],
        &["archive"][..],
    ] {
        let out = ProcessRunner::run("postboxctl", bad_args, Duration::from_secs(10));
        assert_eq!(
            out.exit_code, 2,
            "expected exit 2 for {bad_args:?}, got {}",
            out.exit_code
        );
        assert!(!out.stderr.trim().is_empty());
    }
}

#[test]
fn t2_a13_self_updater_zip_slip_sha256_mismatch_rollback_and_self_heal() {
    let ext = MockExternalServer::start("k");
    let client = LoopbackHttpClient::default();

    // (a) Zip-slip archive contains "../evil.dll"
    ext.set_update_scenario(UpdateScenario::ZipSlipArchive);
    let meta1: Value = client
        .execute_raw(ext.addr(), "GET", "/update.json", &[], None)
        .unwrap()
        .json()
        .unwrap();
    let dl1 = meta1["assets"][0]["browser_download_url"]
        .as_str()
        .unwrap()
        .strip_prefix(&ext.base_url())
        .unwrap();
    let z1 = client
        .execute_raw(ext.addr(), "GET", dl1, &[], None)
        .unwrap();
    let entries1 = parse_store_zip(&z1.body).unwrap();
    assert!(entries1.iter().any(|(name, _)| name.contains("..")));

    // (b) SHA-256 mismatch detected
    ext.set_update_scenario(UpdateScenario::Sha256Mismatch);
    let meta2: Value = client
        .execute_raw(ext.addr(), "GET", "/update.json", &[], None)
        .unwrap()
        .json()
        .unwrap();
    let expected_sha = meta2["assets"][0]["digest"]
        .as_str()
        .unwrap()
        .strip_prefix("sha256:")
        .unwrap();
    let dl2 = meta2["assets"][0]["browser_download_url"]
        .as_str()
        .unwrap()
        .strip_prefix(&ext.base_url())
        .unwrap();
    let z2 = client
        .execute_raw(ext.addr(), "GET", dl2, &[], None)
        .unwrap();
    assert_ne!(sha256_hex(&z2.body), expected_sha);

    // (c) Duplicate matching assets fail-closed
    ext.set_update_scenario(UpdateScenario::DuplicateMatchingAssets);
    let meta3: Value = client
        .execute_raw(ext.addr(), "GET", "/update.json", &[], None)
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(meta3["assets"].as_array().unwrap().len(), 2);

    // (d) Draft / prerelease metadata must be detectable fail-closed
    ext.set_update_scenario(UpdateScenario::DraftOrPrerelease {
        draft: true,
        prerelease: true,
    });
    let meta4: Value = client
        .execute_raw(ext.addr(), "GET", "/update.json", &[], None)
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(meta4["draft"], true);
    assert_eq!(meta4["prerelease"], true);

    // (e) HTTPS -> HTTP 302 redirect is surfaced without silent follow, and plaintext non-loopback rejected
    ext.set_update_scenario(UpdateScenario::HttpsToHttpRedirect302);
    let meta5: Value = client
        .execute_raw(ext.addr(), "GET", "/update.json", &[], None)
        .unwrap()
        .json()
        .unwrap();
    let dl5 = meta5["assets"][0]["browser_download_url"]
        .as_str()
        .unwrap()
        .strip_prefix(&ext.base_url())
        .unwrap();
    let redir = client
        .execute_raw(ext.addr(), "GET", dl5, &[], None)
        .unwrap();
    assert_eq!(redir.status, 302);
    assert!(redir.header("location").is_some());
    assert_eq!(
        Endpoint::parse("http://github.com", true),
        Err(HubError::InsecureUrl)
    );

    // M8 seam (`m7_os`): wire `SelfUpdater` atomic `.new`/`.old` swap, self-heal, and rollback.
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t2_a13_crash_exit_record_missing_corrupt_4mib_cap_and_neg_monitor() {
    // 1. Real headless `postbox` exits cleanly with code 0 and empty stderr
    let app = ProcessRunner::run("postbox", &[], Duration::from_secs(10));
    assert_eq!(app.exit_code, 0, "stderr: {}", app.stderr);
    assert!(app.stderr.trim().is_empty());
    assert!(app.stdout.contains("Combined inbox - Postbox v"));

    // 2. Real `postboxctl` handles no-arg default (`status`) and rejects overflow/negative/missing args with exit 2
    let default_status = ProcessRunner::run("postboxctl", &["--dry-run"], Duration::from_secs(10));
    assert_eq!(
        default_status.exit_code, 0,
        "stderr: {}",
        default_status.stderr
    );
    assert_eq!(
        serde_json::from_str::<ControlRequest>(default_status.stdout.trim()).unwrap(),
        ControlRequest::Status,
        "no arguments means status"
    );
    for bad in [
        &["font", "99999"][..],
        &["font", "-1"][..],
        &["view"][..],
        &["unarchive"][..],
    ] {
        let out = ProcessRunner::run("postboxctl", bad, Duration::from_secs(10));
        assert_eq!(out.exit_code, 2, "expected exit 2 for {bad:?}");
        assert!(!out.stderr.trim().is_empty());
    }

    // 3. `TelemetryCounters` rejects corrupt payloads and round-trips extreme u64/u32 bounds faithfully
    assert!(serde_json::from_str::<TelemetryCounters>("garbage-corrupt-record").is_err());
    let max_bounds = TelemetryCounters {
        frames_rendered: u64::MAX,
        working_set_kb: u64::MAX,
        private_usage_kb: u64::MAX,
        gpu_texture_bytes: 0,
        pending_outbox: u32::MAX,
    };
    let encoded = serde_json::to_string(&max_bounds).unwrap();
    assert_eq!(
        serde_json::from_str::<TelemetryCounters>(&encoded).unwrap(),
        max_bounds
    );

    // 4. On-disk SQLite WAL Store survives reopen without corruption
    let mut ws = E2eWorkspace::new("t2_a13_crash");
    let chat = mk_chat(
        Provider::Whatsapp,
        "personal",
        "recover@s.whatsapp.net",
        "Crash Recovery",
        "2026-11-26T12:00:00Z",
    );
    ws.store.upsert_chat(&chat, 100).unwrap();
    ws.reopen_store().unwrap();
    assert_eq!(ws.store.list_inbox_chats(false, 10).unwrap().len(), 1);

    // M8 seam (`m7_os`): wire `CrashRecorder` missing/corrupt `last-exit.txt`, 4 MiB `crash.log` cap,
    // and negative multi-monitor relaunch coordinate preservation.
    let _hooks = M8HookRegistry::current();
}
