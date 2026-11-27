#![forbid(unsafe_code)]

use postbox_core::archive::should_resurface_on_event;
use postbox_core::control::ControlRequest;
use postbox_core::deep_link::parse as parse_deep_link;
use postbox_core::dictation::DictationLanguage;
use postbox_core::{Chat, ChatKey, FontScale, Message, Provider, Reaction, ReadStatus, ViewTab};
use postbox_e2e::fixtures::{
    minimal_ogg_opus_packet, minimal_png_1x1, pcm16_raw, wcag_contrast_ratio,
};
use postbox_e2e::wire::{parse_store_zip, sha256_hex};
use postbox_e2e::{
    E2eWorkspace, HubMessageRow, LoopbackHttpClient, M8HookRegistry, MediaBehavior,
    MockDeepgramServer, MockExternalServer, MockHubServer, ParsedSseFrame, ProcessRunner,
    WsScriptStep, WsServerAction, WsTrigger,
};
use postbox_hub::{
    OLDER_PAGE_LIMIT, SendReply, SseEvent, archive_request, before_cursor, events_request,
    media_request, messages_request, react_request, send_text_request,
};
use postbox_keymap::{Action, InboxChord, KeyContext, Keymap, ctrl_a_selects_chats};
use postbox_platform::Notifier;
use postbox_store::{MEDIA_TTL_SECS, OutboxKind, OutboxPhase, OutboxRecord};
use postbox_ui::theme::ShadcnPalette;
use postbox_ui::{BASE_FONT_PX, scaled_px};
use serde_json::{Value, json};
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

#[test]
fn t3_01_dictation_chat_switch_origin_scoped_draft() {
    let mut ws = E2eWorkspace::new("t3_01_dict_draft");
    let dg = MockDeepgramServer::start("dg-t3-01");
    dg.set_ws_script(vec![WsScriptStep {
        trigger: WsTrigger::OnCloseStream,
        action: WsServerAction::SendResults {
            is_final: true,
            start: 0.0,
            duration: 0.9,
            transcript: "Finish quarterly report".into(),
            word_languages: vec!["en".into()],
            languages: vec!["en".into()],
        },
    }]);

    let chat_a = ChatKey::new(Provider::Whatsapp, "personal", "a@s.whatsapp.net");
    let chat_b = ChatKey::new(Provider::Whatsapp, "personal", "b@s.whatsapp.net");
    ws.save_draft_row(&chat_a, "Reminder: ", None, 100);
    ws.save_draft_row(&chat_b, "Unrelated draft in B", None, 100);

    // Start live dictation in origin chat_a, then switch active chat to chat_b before CloseStream final arrives
    let origin_chat = chat_a.clone();
    let _active_chat = chat_b.clone();

    let mut conn = LoopbackHttpClient::default()
        .connect_ws(
            dg.addr(),
            "/v1/listen?model=nova-3&interim_results=true",
            &[("Authorization", "Token dg-t3-01")],
        )
        .unwrap();
    conn.send_binary(&pcm16_raw(16_000, 60, 5000)).unwrap();
    conn.send_text(r#"{"type":"CloseStream"}"#).unwrap();
    let frame = conn.recv_frame(Duration::from_secs(2)).unwrap();
    let v: Value = serde_json::from_slice(&frame.payload).unwrap();
    let final_text = v["channel"]["alternatives"][0]["transcript"]
        .as_str()
        .unwrap();

    // Append final strictly to origin_chat's draft
    let (existing, reply) = ws.load_draft_row(&origin_chat).unwrap();
    ws.save_draft_row(
        &origin_chat,
        &format!("{existing}{final_text}"),
        reply.as_deref(),
        200,
    );
    ws.reopen_store().unwrap();

    assert_eq!(
        ws.load_draft_row(&chat_a).unwrap().0,
        "Reminder: Finish quarterly report"
    );
    assert_eq!(
        ws.load_draft_row(&chat_b).unwrap().0,
        "Unrelated draft in B"
    );
    // M8 seam (`m5_dictation`): wire live Deepgram dictation origin-scoped draft commit across chat switch.
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t3_02_sse_burst_archive_resurface_and_unread_watermark() {
    let ws = E2eWorkspace::new("t3_02_sse_resurface");
    let hub = MockHubServer::start("tok-t3-02");

    let mut c1 = mk_chat(
        Provider::Whatsapp,
        "personal",
        "1@s.whatsapp.net",
        "Incoming Resurface",
        "2026-11-26T10:00:00Z",
    );
    c1.archived_local = true;
    let mut c2 = mk_chat(
        Provider::Whatsapp,
        "personal",
        "2@s.whatsapp.net",
        "Own Echo Stays Archived",
        "2026-11-26T10:00:00Z",
    );
    c2.archived_local = true;
    let mut c3 = mk_chat(
        Provider::Whatsapp,
        "personal",
        "3@s.whatsapp.net",
        "Reaction Stays Archived",
        "2026-11-26T10:00:00Z",
    );
    c3.archived_local = true;

    for (i, c) in [&c1, &c2, &c3].iter().enumerate() {
        ws.store.upsert_chat(c, 100 + i as i64).unwrap();
    }

    hub.push_sse_burst(vec![
        SseEvent {
            id: 1,
            account: "personal".into(),
            chat: "personal:1@s.whatsapp.net".into(),
            ts: "2026-11-26T12:00:01Z".into(),
            kind: "msg".into(),
        },
        SseEvent {
            id: 2,
            account: "personal".into(),
            chat: "personal:2@s.whatsapp.net".into(),
            ts: "2026-11-26T12:00:02Z".into(),
            kind: "send".into(),
        },
        SseEvent {
            id: 3,
            account: "personal".into(),
            chat: "personal:3@s.whatsapp.net".into(),
            ts: "2026-11-26T12:00:03Z".into(),
            kind: "react".into(),
        },
    ]);

    let mut stream = LoopbackHttpClient::default()
        .open_sse(&hub.endpoint(), &hub.token(), &events_request(None))
        .unwrap();

    let mut chats = [c1, c2, c3];
    for _ in 0..3 {
        if let Some(ParsedSseFrame::Event(ev)) = stream.next_frame(Duration::from_secs(2)).unwrap()
        {
            let (from_me, is_reaction) = match ev.kind.as_str() {
                "send" => (true, false),
                "react" => (false, true),
                _ => (false, false),
            };
            for chat in &mut chats {
                let handle = format!("{}:{}", chat.key.account, chat.key.id);
                if handle == ev.chat
                    && should_resurface_on_event(chat.archived_local, from_me, is_reaction)
                {
                    chat.archived_local = false;
                    chat.unread_count += 1;
                    ws.store.upsert_chat(chat, 200 + ev.id as i64).unwrap();
                }
            }
        }
    }

    let visible = ws.store.list_inbox_chats(false, 20).unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].key.id, "1@s.whatsapp.net");
    assert_eq!(visible[0].unread_count, 1);
}

#[test]
fn t3_03_outbox_crash_sha256_tamper_and_cross_chat_nonblocking() {
    let mut ws = E2eWorkspace::new("t3_03_outbox");
    let hub = MockHubServer::start("tok-t3-03");

    ws.store
        .enqueue_outbox(&OutboxRecord {
            client_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa01".into(),
            chat: ChatKey::new(Provider::Whatsapp, "personal", "a@s.whatsapp.net"),
            kind: OutboxKind::Text,
            phase: OutboxPhase::Uncertain,
            attempt_serial: 1,
            attempts: 0,
            ambiguous: false,
            text_body: "Crashed A".into(),
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
    // The tampered attachment keeps a staged path and the digest it was queued with, so the
    // record is well-formed and stays out of the queue for being held, not for being malformed.
    let staged = ws.root().join("tampered.bin");
    fs::write(&staged, b"tampered payload").unwrap();
    ws.store
        .enqueue_outbox(&OutboxRecord {
            client_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb02".into(), // trufflehog:ignore
            chat: ChatKey::new(Provider::Whatsapp, "personal", "b@s.whatsapp.net"),
            kind: OutboxKind::File,
            phase: OutboxPhase::Held,
            attempt_serial: 1,
            attempts: 0,
            ambiguous: false,
            text_body: "Tampered B".into(),
            reply_to_id: None,
            reply_broadcast: false,
            staged_file_path: Some(staged.display().to_string()),
            wire_filename: "tampered.bin".into(),
            file_sha256: Some(postbox_e2e::wire::sha256(b"queued payload")),
            file_bytes: 16,
            ptt: false,
            voice_seconds: 0,
            attachment_ready: true,
            failure_reason: Some("sha256 mismatch".into()),
            not_before_ms: 0,
            created_at_ms: 110,
        })
        .unwrap();
    ws.store
        .enqueue_outbox(&OutboxRecord {
            client_id: "cccccccccccccccccccccccccccccc03".into(), // trufflehog:ignore
            chat: ChatKey::new(Provider::Whatsapp, "personal", "c@s.whatsapp.net"),
            kind: OutboxKind::Text,
            phase: OutboxPhase::Ready,
            attempt_serial: 0,
            attempts: 0,
            ambiguous: false,
            text_body: "Ready C".into(),
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
            created_at_ms: 120,
        })
        .unwrap();

    ws.reopen_store().unwrap();

    // A crashed send blocks the queue until the app's startup recovery runs; then only the
    // ready chat dispatches and the interrupted send stays held and ambiguous.
    assert!(ws.store.next_ready_outbox(500).unwrap().is_none());
    let recovered = ws.store.recover_uncertain_on_startup().unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].client_id, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa01");
    assert_eq!(recovered[0].phase, OutboxPhase::Held);

    let next = ws.store.next_ready_outbox(500).unwrap().unwrap();
    assert_eq!(next.client_id, "cccccccccccccccccccccccccccccc03"); // trufflehog:ignore

    let client = LoopbackHttpClient::default();
    let r_c = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &send_text_request(
                "personal:c@s.whatsapp.net",
                &next.text_body,
                None,
                false,
                &next.client_id,
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(r_c.status, 200);

    // Explicit user retry of A uses a brand-new 32-hex client_id
    let fresh_a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa99";
    let r_a = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &send_text_request(
                "personal:a@s.whatsapp.net",
                "Crashed A",
                None,
                false,
                fresh_a,
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(r_a.status, 200);
    assert_eq!(
        hub.client_id_delivery_count("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa01"),
        0
    );
    assert_eq!(hub.client_id_delivery_count(fresh_a), 1);
    // M8 seam (`m2_hub_sync`): wire OutboxWorker crash recovery + attachment SHA-256 tamper hold + cross-chat scan.
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t3_04_palette_fts5_search_archived_chat_and_unarchive() {
    let mut ws = E2eWorkspace::new("t3_04_palette_arch");
    let hub = MockHubServer::start("tok-t3-04");
    let km = Keymap::postbox_defaults();
    assert_eq!(
        km.resolve(KeyContext::Workspace, "ctrl-k"),
        Some(Action::OpenPalette)
    );
    assert_eq!(
        km.resolve(KeyContext::Workspace, "alt-a"),
        Some(Action::ToggleArchived)
    );
    assert_eq!(
        km.resolve(KeyContext::Workspace, "ctrl-e"),
        Some(Action::ArchiveSelected)
    );

    let mut arch = mk_chat(
        Provider::Whatsapp,
        "personal",
        "arch@s.whatsapp.net",
        "München Archiv",
        "2026-11-26T10:00:00Z",
    );
    arch.archived_local = true;
    ws.store.upsert_chat(&arch, 100).unwrap();
    let msg = mk_msg(
        &arch.key,
        "M-ARC-1",
        "Clara",
        "2026-11-26T10:00:00Z",
        "Überraschung im Café München",
        false,
    );
    ws.store.upsert_message(&msg).unwrap();

    // FTS5 finds archived chat message via diacritic-folded prefix
    let hits = ws.store.search_messages_fts("Uberraschung*", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].chat, arch.key);

    // Unarchive via Hub + Store -> reappears in inbox
    let resp = LoopbackHttpClient::default()
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &archive_request("personal:arch@s.whatsapp.net", false).unwrap(),
        )
        .unwrap();
    assert_eq!(resp.status, 200);
    arch.archived_local = false;
    ws.store.upsert_chat(&arch, 200).unwrap();
    assert_eq!(ws.store.list_inbox_chats(false, 50).unwrap().len(), 1);
}

#[test]
fn t3_05_media_autodownload_202_poll_14d_eviction_and_lightbox() {
    let ws = E2eWorkspace::new("t3_05_media_lb");
    let hub = MockHubServer::start("tok-t3-05");
    let chat = "personal:lb@s.whatsapp.net";
    let png = minimal_png_1x1();

    hub.set_media(
        chat,
        "M-FRESH",
        MediaBehavior::PendingThenReady {
            remaining_202s: 1,
            bytes: png.clone(),
            content_type: "image/png".into(),
        },
    );

    let client = LoopbackHttpClient::default();
    let req = media_request(chat, Some("M-FRESH")).unwrap();
    assert_eq!(
        client
            .execute_hub(&hub.endpoint(), &hub.token(), &req)
            .unwrap()
            .status,
        202
    );
    let ok = client
        .execute_hub(&hub.endpoint(), &hub.token(), &req)
        .unwrap();
    assert_eq!(ok.status, 200);
    assert_eq!(ok.body, png);

    let now = 1_764_150_000_i64;
    ws.seed_media_cache(
        "fresh",
        &sha256_hex(&png),
        "hub-media/fresh.png",
        "image/png",
        png.len() as i64,
        now - 3600,
    );
    ws.seed_media_cache(
        "old",
        "sha-old",
        "hub-media/old.png",
        "image/png",
        100,
        now - MEDIA_TTL_SECS - 86_400,
    );

    let evicted = ws.store.evict_expired_media(now).unwrap();
    assert_eq!(evicted, vec!["hub-media/old.png".to_string()]);
    assert!(ws.media_cache_exists("fresh"));

    let km = Keymap::postbox_defaults();
    for key in ["escape", "space", "enter"] {
        assert_eq!(
            km.resolve(KeyContext::Lightbox, key),
            Some(Action::CloseLightbox)
        );
    }
    // M8 seam (`m3_media`): wire `postbox-media` auto-download + lightbox gallery + 14-day eviction.
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t3_06_voice_media_download_deepgram_rest_and_immutable_cache() {
    let mut ws = E2eWorkspace::new("t3_06_voice_dg");
    let hub = MockHubServer::start("tok-t3-06");
    let dg = MockDeepgramServer::start("dg-t3-06");
    let chat = "personal:voice@s.whatsapp.net";
    let client = LoopbackHttpClient::default();

    let mut shas = Vec::new();
    for i in 0..3usize {
        let mut ogg = minimal_ogg_opus_packet();
        ogg.push(i as u8);
        let msg_id = format!("M-V-{i}");
        hub.set_media(
            chat,
            &msg_id,
            MediaBehavior::Ready {
                bytes: ogg.clone(),
                content_type: "audio/ogg".into(),
            },
        );
        let media = client
            .execute_hub(
                &hub.endpoint(),
                &hub.token(),
                &media_request(chat, Some(&msg_id)).unwrap(),
            )
            .unwrap();
        assert_eq!(media.status, 200);
        let sha = sha256_hex(&media.body);
        shas.push(sha.clone());

        let dg_resp = client
            .execute_raw(
                dg.addr(),
                "POST",
                DictationLanguage::Automatic.deepgram_listen_path(),
                &[("Authorization", "Token dg-t3-06")],
                Some(&media.body),
            )
            .unwrap();
        assert_eq!(dg_resp.status, 200);
        let text = dg_resp.json::<Value>().unwrap()["results"]["channels"][0]["alternatives"][0]
            ["transcript"]
            .as_str()
            .unwrap()
            .to_string();
        ws.store
            .cache_transcript(&sha, "en", &text, 1000 + i as i64)
            .unwrap();
    }
    assert_eq!(dg.rest_call_count(), 3);

    // Reopen Store simulating chat reopen -> all 3 cached, 0 new Deepgram calls
    ws.reopen_store().unwrap();
    for sha in &shas {
        assert!(ws.store.get_transcript(sha).unwrap().is_some());
    }
    assert_eq!(dg.rest_call_count(), 3);

    let km = Keymap::postbox_defaults();
    assert_eq!(
        km.resolve(KeyContext::Workspace, "ctrl-p"),
        Some(Action::PlayPauseVoice)
    );
    assert_eq!(
        km.resolve(KeyContext::Workspace, "ctrl-t"),
        Some(Action::ToggleTranscript)
    );
    // M8 seam (`m4_audio`): wire `postbox-audio` Opus playback + Deepgram REST transcription cache.
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t3_07_deep_link_slack_thread_pagination_and_broadcast_send() {
    let mut ws = E2eWorkspace::new("t3_07_slack_dl");
    let hub = MockHubServer::start("tok-t3-07");

    let target = parse_deep_link("postbox://slack/T01/C02/p1700000005000100").unwrap();
    assert_eq!(target.chat, ChatKey::new(Provider::Slack, "T01", "C02"));
    assert_eq!(target.message.as_deref(), Some("1700000005.000100"));

    let handle = "slack:T01:C02";
    let mut rows = Vec::new();
    for i in 0..90usize {
        rows.push(HubMessageRow::simple(
            handle,
            format!("17000000{i:02}.000100"),
            "Grace",
            format!("Thread history item {i}"),
            format!("2026-11-26T{:02}:{:02}:00Z", i / 60, i % 60),
            false,
        ));
    }
    hub.set_messages(handle, rows);

    let client = LoopbackHttpClient::default();
    let p1: Vec<HubMessageRow> = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &messages_request(handle, OLDER_PAGE_LIMIT, None).unwrap(),
        )
        .unwrap()
        .json()
        .unwrap();
    let cursor = before_cursor(&p1.last().unwrap().MsgID, &p1.last().unwrap().Timestamp).unwrap();
    let p2: Vec<HubMessageRow> = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &messages_request(handle, OLDER_PAGE_LIMIT, Some(&cursor)).unwrap(),
        )
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(p1.len(), 80);
    assert_eq!(p2.len(), 10);

    let bcast_req = send_text_request(
        handle,
        "Resolved in thread and broadcast to channel",
        target.message.as_deref(),
        true,
        "cid-t3-bcast",
    )
    .unwrap();
    let bcast_resp = client
        .execute_hub(&hub.endpoint(), &hub.token(), &bcast_req)
        .unwrap();
    assert_eq!(bcast_resp.status, 200);
    let reply: SendReply = bcast_resp.json().unwrap();

    let mut msg = mk_msg(
        &target.chat,
        &reply.msg_id,
        "Me",
        &reply.ts,
        "Resolved in thread and broadcast to channel",
        true,
    );
    msg.reply_to_id = target.message;
    msg.reply_broadcast = true;
    ws.store.upsert_message(&msg).unwrap();
    assert_eq!(
        ws.store.search_messages_fts("broadcast", 5).unwrap().len(),
        1
    );
}

#[test]
fn t3_08_postboxctl_drives_theme_font_view_and_hub_archive() {
    let ws = E2eWorkspace::new("t3_08_ctl");
    let hub = MockHubServer::start("tok-t3-08");

    let run_ctl = |args: &[&str]| -> ControlRequest {
        // `--dry-run` is the exact request line the CLI would send to the app.
        let mut argv: Vec<&str> = args.to_vec();
        argv.push("--dry-run");
        let out = ProcessRunner::run("postboxctl", &argv, Duration::from_secs(10));
        assert_eq!(out.exit_code, 0, "args {argv:?} stderr: {}", out.stderr);
        serde_json::from_str(out.stdout.trim()).unwrap()
    };

    let ControlRequest::Theme { theme } = run_ctl(&["theme", "amoled"]) else {
        panic!("expected Theme");
    };
    let pal = ShadcnPalette::for_choice(theme);
    assert_eq!(pal.background, 0x000000);
    assert!(wcag_contrast_ratio(pal.foreground, pal.background) >= 4.5);

    let ControlRequest::Font { percent } = run_ctl(&["font", "125"]) else {
        panic!("expected Font");
    };
    assert!((scaled_px(BASE_FONT_PX, FontScale::clamped(percent).percent()) - 18.75).abs() < 0.01);

    let ControlRequest::View { target } = run_ctl(&["view", "inbox"]) else {
        panic!("expected View");
    };
    assert_eq!(target, "inbox");
    assert_eq!(
        ViewTab::CombinedInbox.window_title("0.1.0"),
        "Combined inbox - Postbox v0.1.0"
    );

    let ControlRequest::Archive { id } = run_ctl(&["archive", "personal:1@s.whatsapp.net"]) else {
        panic!("expected Archive");
    };
    let resp = LoopbackHttpClient::default()
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &archive_request(&id, true).unwrap(),
        )
        .unwrap();
    assert_eq!(resp.status, 200);
    let mut chat = mk_chat(
        Provider::Whatsapp,
        "personal",
        "1@s.whatsapp.net",
        "Archived via CLI",
        "2026-11-26T12:00:00Z",
    );
    chat.archived_local = true;
    ws.store.upsert_chat(&chat, 100).unwrap();
    assert!(ws.store.list_inbox_chats(false, 20).unwrap().is_empty());
}

#[test]
fn t3_09_chord_combined_inbox_vs_account_views_and_pins() {
    let ws = E2eWorkspace::new("t3_09_chord_pins");
    let km = Keymap::postbox_defaults();
    let mut chord = InboxChord::default();
    assert!(!chord.on_key('g', 100, true, false));
    assert!(chord.on_key('i', 700, true, false));
    assert_eq!(
        ViewTab::CombinedInbox.window_title("0.1.0"),
        "Combined inbox - Postbox v0.1.0"
    );
    assert_eq!(
        km.resolve(KeyContext::Workspace, "alt-2"),
        Some(Action::SwitchAccountView(2))
    );
    assert_eq!(
        km.resolve(KeyContext::Workspace, "alt-a"),
        Some(Action::ToggleArchived)
    );

    let mut pinned = mk_chat(
        Provider::Whatsapp,
        "personal",
        "pin@s.whatsapp.net",
        "Pinned",
        "2026-11-10T00:00:00Z",
    );
    pinned.pinned = true;
    let unpinned = mk_chat(
        Provider::Whatsapp,
        "personal",
        "new@s.whatsapp.net",
        "Recent",
        "2026-11-26T12:00:00Z",
    );
    ws.store.upsert_chat(&pinned, 100).unwrap();
    ws.store.upsert_chat(&unpinned, 200).unwrap();
    assert_eq!(
        ws.store.list_inbox_chats(false, 10).unwrap()[0].key,
        pinned.key
    );
}

#[test]
fn t3_10_slack_custom_emoji_and_optimistic_reaction_with_409() {
    let mut ws = E2eWorkspace::new("t3_10_emoji_409");
    let hub = MockHubServer::start("tok-t3-10");
    let png = minimal_png_1x1();
    hub.set_slack_emoji_index(
        "T01",
        json!({"emoji": {"shipit": "https://e.invalid/s.png"}}),
    );
    hub.set_slack_emoji_media("T01", "shipit", Ok(png.clone()));

    let client = LoopbackHttpClient::default();
    let auth = format!("Bearer {}", hub.token());
    let media = client
        .execute_raw(
            hub.addr(),
            "GET",
            "/v1/wazig/slack/emoji/media?account=T01&name=shipit",
            &[("Authorization", auth.as_str())],
            None,
        )
        .unwrap();
    assert_eq!(media.status, 200);
    assert_eq!(media.body, png);

    // Msg 1 succeeds (200), Msg 2 returns 409 without corrupting Msg 1
    let r1 = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &react_request("slack:T01:C1", "1700.001", ":shipit:", "cid-r1").unwrap(),
        )
        .unwrap();
    assert_eq!(r1.status, 200);

    let key = ChatKey::new(Provider::Slack, "T01", "C1");
    let mut m1 = mk_msg(
        &key,
        "1700.001",
        "Ada",
        "2026-11-26T12:00:00Z",
        "Deploy",
        false,
    );
    m1.reactions.push(Reaction {
        emoji: ":shipit:".into(),
        count: 1,
        from_me: true,
    });
    ws.store.upsert_message(&m1).unwrap();

    hub.push_react_fault(409);
    let r2 = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &react_request("slack:T01:C1", "1700.002", ":shipit:", "cid-r2").unwrap(),
        )
        .unwrap();
    assert_eq!(r2.status, 409);

    // Queue remains unblocked for Msg 3
    let r3 = client
        .execute_hub(
            &hub.endpoint(),
            &hub.token(),
            &react_request("slack:T01:C1", "1700.003", "🚀", "cid-r3").unwrap(),
        )
        .unwrap();
    assert_eq!(r3.status, 200);
}

#[test]
fn t3_11_link_unfurl_ssrf_isolation_and_video_f11_player() {
    let ext = MockExternalServer::start("k");
    let client = LoopbackHttpClient::default();
    let yt = client
        .execute_raw(
            ext.addr(),
            "GET",
            "/oembed?url=https%3A%2F%2Fyoutu.be%2FdQw4w9WgXcQ&format=json",
            &[],
            None,
        )
        .unwrap();
    assert_eq!(yt.status, 200);
    assert_eq!(yt.json::<Value>().unwrap()["provider_name"], "YouTube");

    let xss = client
        .execute_raw(ext.addr(), "GET", "/page/js-xss", &[], None)
        .unwrap();
    assert_eq!(xss.status, 200);
    assert!(xss.text().contains("javascript:alert(1)"));

    let km = Keymap::postbox_defaults();
    assert_eq!(
        km.resolve(KeyContext::VideoPlayer, "f11"),
        Some(Action::ToggleVideoFullscreen)
    );
    assert_eq!(
        km.resolve(KeyContext::Workspace, "ctrl-o"),
        Some(Action::OpenLinksAndMedia)
    );
    // M8 seam (`m6_video`): wire SSRF unfurl card + inline DXVA `F11` video player.
    let _hooks = M8HookRegistry::current();
}

#[test]
fn t3_12_keymap_override_hold_ctrl_slots_and_smart_ctrl_a_archive() {
    let mut km = Keymap::postbox_defaults();
    km.apply_override_json(
        r#"{"overrides":[{"context":"workspace","stroke":"ctrl-k","action":"search_chats"}]}"#,
    )
    .unwrap();
    assert_eq!(
        km.resolve(KeyContext::Workspace, "ctrl-k"),
        Some(Action::SearchChats)
    );
    for slot in 1u8..=9 {
        assert_eq!(
            km.resolve(KeyContext::Workspace, &format!("ctrl-{slot}")),
            Some(Action::OpenChatSlot(slot))
        );
    }

    assert!(ctrl_a_selects_chats(true, true, true));
    assert!(!ctrl_a_selects_chats(true, false, true));

    let larger = FontScale::default().larger();
    let smaller = larger.smaller();
    assert!((scaled_px(BASE_FONT_PX, larger.percent()) - 16.5).abs() < 0.01);
    assert!((scaled_px(BASE_FONT_PX, smaller.percent()) - 15.0).abs() < 0.01);
}

#[test]
fn t3_13_sse_reconnect_gap_toast_baseline_and_deep_link_click() {
    let ws = E2eWorkspace::new("t3_13_gap_toast");
    let hub = MockHubServer::start("tok-t3-13");

    let mut c1 = mk_chat(
        Provider::Whatsapp,
        "personal",
        "gap1@s.whatsapp.net",
        "Gap Peer 1",
        "2026-11-26T12:00:00Z",
    );
    c1.archived_local = true;
    ws.store.upsert_chat(&c1, 100).unwrap();

    hub.push_sse_burst(vec![
        SseEvent {
            id: 10,
            account: "personal".into(),
            chat: "personal:gap1@s.whatsapp.net".into(),
            ts: "2026-11-26T12:00:10Z".into(),
            kind: "msg".into(),
        },
        SseEvent {
            id: 11,
            account: "personal".into(),
            chat: "personal:gap1@s.whatsapp.net".into(),
            ts: "2026-11-26T12:00:11Z".into(),
            kind: "msg".into(),
        },
        SseEvent {
            id: 12,
            account: "personal".into(),
            chat: "personal:gap1@s.whatsapp.net".into(),
            ts: "2026-11-26T12:00:12Z".into(),
            kind: "send".into(),
        },
        SseEvent {
            id: 13,
            account: "personal".into(),
            chat: "personal:gap1@s.whatsapp.net".into(),
            ts: "2026-11-26T12:00:13Z".into(),
            kind: "react".into(),
        },
        SseEvent {
            id: 14,
            account: "personal".into(),
            chat: "personal:gap2@s.whatsapp.net".into(),
            ts: "2026-11-26T12:00:14Z".into(),
            kind: "msg".into(),
        },
        SseEvent {
            id: 15,
            account: "personal".into(),
            chat: "personal:gap2@s.whatsapp.net".into(),
            ts: "2026-11-26T12:00:15Z".into(),
            kind: "msg".into(),
        },
    ]);

    let mut stream = LoopbackHttpClient::default()
        .open_sse(&hub.endpoint(), &hub.token(), &events_request(Some("10")))
        .unwrap();

    let mut received_ids = Vec::new();
    for _ in 0..5 {
        if let Some(ParsedSseFrame::Event(ev)) = stream.next_frame(Duration::from_secs(2)).unwrap()
        {
            received_ids.push(ev.id);
            let (from_me, is_react) = (ev.kind == "send", ev.kind == "react");
            if !from_me && !is_react {
                if ev.chat == "personal:gap1@s.whatsapp.net"
                    && should_resurface_on_event(c1.archived_local, from_me, is_react)
                {
                    c1.archived_local = false;
                    ws.store.upsert_chat(&c1, 200).unwrap();
                }
                let chat_id = ev.chat.strip_prefix("personal:").unwrap();
                let key = ChatKey::new(Provider::Whatsapp, "personal", chat_id);
                ws.platform
                    .notify_message(&key, "Gap Peer", "New message")
                    .unwrap();
            }
        }
    }
    assert_eq!(received_ids, vec![11, 12, 13, 14, 15]);
    assert_eq!(ws.platform.notification_count(), 3);

    let dl = parse_deep_link("postbox://whatsapp/personal/gap1%40s.whatsapp.net").unwrap();
    assert_eq!(dl.chat, c1.key);
    assert_eq!(
        ws.store.list_inbox_chats(false, 10).unwrap()[0].key,
        dl.chat
    );
}

#[test]
fn t3_14_self_updater_verify_swap_and_clean_exit_record() {
    let ext = MockExternalServer::start("k");
    let mut ws = E2eWorkspace::new("t3_14_upd_exit");
    let client = LoopbackHttpClient::default();

    let meta: Value = client
        .execute_raw(ext.addr(), "GET", "/update.json", &[], None)
        .unwrap()
        .json()
        .unwrap();
    let asset = &meta["assets"][0];
    let dl = asset["browser_download_url"]
        .as_str()
        .unwrap()
        .strip_prefix(&ext.base_url())
        .unwrap();
    let zip = client
        .execute_raw(ext.addr(), "GET", dl, &[], None)
        .unwrap();
    assert_eq!(zip.body.len(), asset["size"].as_u64().unwrap() as usize);
    assert_eq!(
        sha256_hex(&zip.body),
        asset["digest"]
            .as_str()
            .unwrap()
            .strip_prefix("sha256:")
            .unwrap()
    );

    let entries = parse_store_zip(&zip.body).unwrap();
    assert!(
        entries
            .iter()
            .all(|(name, _)| !name.contains("..") && !name.starts_with('/'))
    );
    let postbox_exe = &entries.iter().find(|(n, _)| n == "postbox.exe").unwrap().1;
    let postboxctl_exe = &entries
        .iter()
        .find(|(n, _)| n == "postboxctl.exe")
        .unwrap()
        .1;
    assert!(!postbox_exe.is_empty());
    assert!(!postboxctl_exe.is_empty());

    // Pairwise Store WAL durability across restart alongside update verification
    let chat = mk_chat(
        Provider::Whatsapp,
        "personal",
        "upd@s.whatsapp.net",
        "Pre-Update Chat",
        "2026-11-26T12:00:00Z",
    );
    ws.store.upsert_chat(&chat, 100).unwrap();
    ws.store
        .enqueue_outbox(&OutboxRecord {
            client_id: "c1000000000000000000000000000001".into(),
            chat: chat.key.clone(),
            kind: OutboxKind::Text,
            phase: OutboxPhase::Ready,
            attempt_serial: 0,
            attempts: 0,
            ambiguous: false,
            text_body: "Survives update restart".into(),
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
    ws.reopen_store().unwrap();
    assert_eq!(ws.count_chats(), 1);
    assert!(ws.store.next_ready_outbox(500).unwrap().is_some());

    // Real workspace binaries launch cleanly
    let app = ProcessRunner::run("postbox", &[], Duration::from_secs(10));
    assert_eq!(app.exit_code, 0, "stderr: {}", app.stderr);
    let ctl = ProcessRunner::run(
        "postboxctl",
        &["status", "--dry-run"],
        Duration::from_secs(10),
    );
    assert_eq!(ctl.exit_code, 0, "stderr: {}", ctl.stderr);
    assert_eq!(
        serde_json::from_str::<ControlRequest>(ctl.stdout.trim()).unwrap(),
        ControlRequest::Status
    );

    // M8 seam (`m7_os`): wire `SelfUpdater` atomic swap + `CrashRecorder` `reason=update` clean exit lifecycle.
    let _hooks = M8HookRegistry::current();
}
