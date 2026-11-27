use postbox_core::{Account, Chat, ChatKey, MediaKind, Message, Provider, Reaction, ReadStatus};
use postbox_keymap::{Action, Keymap};
use postbox_media::drain::MediaItemStatus;
use postbox_media::giphy::{GiphyItem, GiphyPage, GiphyRendition, GiphyStatus};
use postbox_media::paste::{PasteStageResult, encode_png};
use postbox_media::unfurl::{UnfurlCard, UnfurlProvider, classify_url};
use postbox_platform::ClipboardPayload;
use postbox_store::Store;
use postbox_ui::{PostboxViewModel, SyncCommand, SyncEvent, TimelineItem};

fn mk_chat(provider: Provider, account: &str, id: &str, title: &str, ts: &str) -> Chat {
    Chat {
        key: ChatKey::new(provider, account, id),
        title: title.into(),
        is_group: false,
        archived_local: false,
        archived_remote: false,
        pinned: false,
        unread_count: 0,
        last_msg_ts: ts.into(),
        last_msg_preview: String::new(),
        avatar_hash: None,
    }
}

fn mk_media_msg(
    chat: &ChatKey,
    id: &str,
    from_me: bool,
    ts: &str,
    body: &str,
    media: Option<(MediaKind, &'static str, u64)>,
) -> Message {
    let (kind, mime, file_size) = match media {
        Some((k, m, sz)) => (Some(k), Some(m.to_owned()), Some(sz)),
        None => (None, None, None),
    };
    Message {
        chat: chat.clone(),
        msg_id: id.into(),
        sender_id: if from_me { "me".into() } else { "U1".into() },
        sender_name: if from_me {
            "You".into()
        } else {
            "Paula".into()
        },
        from_me,
        ts: ts.into(),
        body: body.into(),
        reply_to_id: None,
        reply_broadcast: false,
        media_kind: kind,
        mime_type: mime,
        filename: kind.map(|k| format!("{id}.{}", k.as_str())),
        file_size,
        reactions: vec![],
        read_status: ReadStatus::Read,
        deleted: false,
    }
}

#[test]
fn chat_open_immediately_triggers_newest_first_auto_download_and_respects_100mib_cap() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert_account(&Account {
            provider: Provider::Whatsapp,
            account_id: "main".into(),
            label: "WhatsApp".into(),
            state: "connected".into(),
            sort_order: 1,
            last_event_at: None,
        })
        .unwrap();
    let chat = mk_chat(
        Provider::Whatsapp,
        "main",
        "paula@s.whatsapp.net",
        "Paula",
        "2026-11-26T12:00:00Z",
    );
    store.upsert_chat(&chat, 1000).unwrap();
    store
        .upsert_message(&mk_media_msg(
            &chat.key,
            "m1",
            false,
            "2026-11-26T10:00:00Z",
            "",
            Some((MediaKind::Image, "image/png", 2048)),
        ))
        .unwrap();
    store
        .upsert_message(&mk_media_msg(
            &chat.key,
            "m2_big",
            false,
            "2026-11-26T11:00:00Z",
            "",
            Some((MediaKind::Document, "application/pdf", 104_857_601)),
        ))
        .unwrap();
    store
        .upsert_message(&mk_media_msg(
            &chat.key,
            "m3_newest",
            false,
            "2026-11-26T12:00:00Z",
            "",
            Some((MediaKind::Audio, "audio/mpeg", 104_857_600)),
        ))
        .unwrap();

    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0");
    let cmds = vm.take_emitted_sync_commands();
    let dl = cmds.iter().find_map(|c| match c {
        SyncCommand::DownloadChatMedia { items, .. } => Some(items),
        _ => None,
    });
    let items = dl.expect("opening chat MUST immediately emit DownloadChatMedia");
    let ids: Vec<&str> = items.iter().map(|i| i.msg_id.as_str()).collect();
    // Newest-first order, skipping oversized m2_big (> 100 MiB)
    assert_eq!(ids, vec!["m3_newest", "m1"]);
    assert_eq!(
        vm.media_status_for(&chat.key, "m2_big"),
        MediaItemStatus::Oversized
    );
}

#[test]
fn paste_to_attach_stages_44x44_thumb_propagates_text_and_submits_to_outbox() {
    let store = Store::open_in_memory().unwrap();
    let chat = mk_chat(
        Provider::Whatsapp,
        "main",
        "paula@s.whatsapp.net",
        "Paula",
        "2026-11-26T12:00:00Z",
    );
    store.upsert_chat(&chat, 1000).unwrap();
    let dir = std::env::temp_dir().join(format!(
        "postbox_ui_paste_test_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0")
        .with_data_dir(dir.clone());

    // Text clipboard MUST propagate to Textarea paste!
    assert_eq!(
        vm.handle_paste_clipboard_payload(ClipboardPayload::Text),
        PasteStageResult::PropagateTextPaste
    );

    let png = encode_png(120, 80, &vec![180u8; 120 * 80 * 4]).unwrap();
    assert!(matches!(
        vm.handle_paste_clipboard_payload(ClipboardPayload::PngBytes(png)),
        PasteStageResult::Staged { count: 1, .. }
    ));
    assert_eq!(vm.staged_attachments().len(), 1);
    vm.set_composer_text("Here is the screenshot");
    assert!(vm.submit_composer_message());
    assert!(vm.staged_attachments().is_empty());
    assert!(vm.composer_text().is_empty());

    // Esc discards staged attachments when no reply is active
    let png2 = encode_png(32, 32, &vec![90u8; 32 * 32 * 4]).unwrap();
    vm.handle_paste_clipboard_payload(ClipboardPayload::PngBytes(png2));
    assert_eq!(vm.staged_attachments().len(), 1);
    vm.handle_escape();
    assert!(vm.staged_attachments().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reaction_picker_chips_wa_double_click_shortcodes_mentions_lightbox_unfurl_and_giphy_flow() {
    let mut store = Store::open_in_memory().unwrap();
    let chat = mk_chat(
        Provider::Slack,
        "T1",
        "slack:T1:C1",
        "General",
        "2026-11-26T12:00:00Z",
    );
    let wa_chat = mk_chat(
        Provider::Whatsapp,
        "main",
        "paula@s.whatsapp.net",
        "Paula",
        "2026-11-26T11:00:00Z",
    );
    store.upsert_chat(&chat, 1000).unwrap();
    store.upsert_chat(&wa_chat, 900).unwrap();
    store
        .upsert_message(&mk_media_msg(
            &chat.key,
            "img1",
            false,
            "2026-11-26T11:50:00Z",
            "Check https://example.com/guide",
            Some((MediaKind::Image, "image/png", 500)),
        ))
        .unwrap();
    store
        .upsert_message(&mk_media_msg(
            &chat.key,
            "img2",
            false,
            "2026-11-26T12:00:00Z",
            "Second photo",
            Some((MediaKind::Gif, "image/gif", 600)),
        ))
        .unwrap();
    store
        .upsert_message(&mk_media_msg(
            &wa_chat.key,
            "wa1",
            false,
            "2026-11-26T11:00:00Z",
            "Hey!",
            None,
        ))
        .unwrap();

    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0")
        .with_giphy_key_available(true);

    // 1. Lightbox gallery opens, navigates without wrap, closes on Esc
    assert!(vm.open_selected_image_lightbox(1000));
    assert_eq!(vm.lightbox().unwrap().position_label(), "2 / 2");
    assert!(vm.lightbox_prev());
    assert_eq!(vm.lightbox().unwrap().position_label(), "1 / 2");
    assert!(!vm.lightbox_prev());
    vm.handle_escape();
    assert!(vm.lightbox().is_none());

    // 2. Ctrl+R reaction picker opens, searches, activates -> optimistic reaction + SyncCommand
    vm.handle_action(Action::ReactToMessage, 2000);
    assert!(vm.emoji_picker().is_some());
    vm.set_emoji_picker_query("fire");
    assert!(vm.activate_emoji_picker());
    assert!(vm.emoji_picker().is_none());
    let reacted = vm.messages().iter().find(|m| m.msg_id == "img2").unwrap();
    assert_eq!(
        reacted.reactions,
        vec![Reaction {
            emoji: "🔥".into(),
            count: 1,
            from_me: true
        }]
    );
    // Simulate HTTP 409 unknown delivery -> keeps chip, clears pending, exact notice
    vm.apply_sync_event(SyncEvent::ReactionSettled {
        chat: chat.key.clone(),
        msg_id: "img2".into(),
        emoji: "🔥".into(),
        ok: false,
        http_status: Some(409),
    });
    assert!(!vm.is_reaction_pending(&chat.key, "img2"));
    assert_eq!(
        vm.status_notice(),
        Some("Reaction delivery unknown; check the chat")
    );

    // 3. @mention priority over :shortcode:, Slack `<@handle>` insertion, and closed `:tada:` auto-replace
    vm.set_composer_text("hi @pau");
    assert!(vm.mention_popup().is_some());
    assert!(vm.shortcode_popup().is_none());
    assert!(vm.accept_composer_popup());
    assert_eq!(vm.composer_text(), "hi <@U1>");

    vm.set_composer_text("ship :tada:");
    assert_eq!(vm.composer_text(), "ship 🎉");

    // 4. Link unfurl card applied via SyncEvent
    let cls = classify_url("https://example.com/guide").unwrap();
    vm.apply_sync_event(SyncEvent::UnfurlReady {
        target: cls.clone(),
        card: Some(UnfurlCard {
            provider: UnfurlProvider::Page,
            id: cls.id.clone(),
            canonical: cls.canonical.clone(),
            site: "example.com".into(),
            title: "Guide".into(),
            author: "example.com".into(),
            description: "Docs".into(),
            play_url: None,
            thumb_path: None,
        }),
    });
    let m_img1 = vm.messages().iter().find(|m| m.msg_id == "img1").unwrap();
    assert_eq!(vm.unfurl_card_for_message(m_img1).unwrap().0.title, "Guide");

    // 5. Giphy picker open -> results -> target-change race guard drops GIF instead of mis-staging!
    assert!(vm.open_giphy_picker());
    let gen_id = vm.giphy_picker().generation;
    let target = vm.giphy_picker().target.clone().unwrap();
    vm.apply_sync_event(SyncEvent::GiphyResults {
        generation: gen_id,
        target: target.clone(),
        page: Some(GiphyPage {
            items: vec![GiphyItem {
                id: "Gif99".into(),
                title: "Celebrate".into(),
                preview: GiphyRendition {
                    url: "https://media1.giphy.com/media/Gif99/200w.gif".into(),
                    width: 200,
                    height: 150,
                    size: 1000,
                },
                original: GiphyRendition {
                    url: "https://media1.giphy.com/media/Gif99/giphy.gif".into(),
                    width: 400,
                    height: 300,
                    size: 2000,
                },
            }],
            total_count: 1,
            offset: 0,
        }),
        error: None,
    });
    assert_eq!(vm.giphy_picker().status, GiphyStatus::Ready);

    // Switch chat mid-download -> race guard MUST drop GIF!
    assert!(vm.select_chat_key(&wa_chat.key));
    vm.apply_sync_event(SyncEvent::GiphyDownloadReady {
        generation: gen_id,
        download_generation: 1,
        target,
        index: 0,
        gif_id: "Gif99".into(),
        bytes: b"GIF89a\x01\x00\x01\x00\x00\x00\x00;".to_vec(),
    });
    assert!(vm.staged_attachments().is_empty());
    assert_eq!(
        vm.status_notice(),
        Some("GIF dropped because the chat or reply changed")
    );

    // 6. Double-click received WhatsApp bubble toggles 👍
    assert!(vm.double_click_message_bubble("wa1"));
    let wa_msg = vm.messages().iter().find(|m| m.msg_id == "wa1").unwrap();
    assert_eq!(wa_msg.reactions[0].emoji, "👍");
}

#[test]
fn m3_regression_stale_giphy_race_reaction_reload_preservation_and_origin_chat_draft() {
    let dir = std::env::temp_dir().join(format!("postbox_m3_reg_{}", std::process::id()));
    let mut store = Store::open_in_memory().unwrap();
    let chat_a = ChatKey::new(Provider::Whatsapp, "main", "a@s.whatsapp.net");
    let chat_b = ChatKey::new(Provider::Whatsapp, "main", "b@s.whatsapp.net");
    for (key, title, ts) in [
        (&chat_a, "Alice", "2026-11-26T10:05:00Z"),
        (&chat_b, "Bob", "2026-11-26T10:04:00Z"),
    ] {
        store
            .upsert_chat(
                &Chat {
                    key: key.clone(),
                    title: title.into(),
                    is_group: false,
                    archived_local: false,
                    archived_remote: false,
                    pinned: false,
                    unread_count: 0,
                    last_msg_ts: ts.into(),
                    last_msg_preview: "hello".into(),
                    avatar_hash: None,
                },
                1000,
            )
            .unwrap();
    }
    store
        .upsert_message(&Message {
            chat: chat_a.clone(),
            msg_id: "m1".into(),
            sender_id: "a@s.whatsapp.net".into(),
            sender_name: "Alice".into(),
            from_me: false,
            ts: "2026-11-26T10:05:00Z".into(),
            body: "Hi".into(),
            reply_to_id: None,
            reply_broadcast: false,
            media_kind: None,
            mime_type: None,
            filename: None,
            file_size: None,
            reactions: vec![],
            read_status: ReadStatus::None,
            deleted: false,
        })
        .unwrap();

    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0")
        .with_data_dir(dir.clone())
        .with_giphy_key_available(true);
    assert!(vm.select_chat_key(&chat_a));

    // 1. Reaction reload preservation: pending reaction survives a stale Hub sync wiping DB reactions!
    assert!(vm.double_click_message_bubble("m1"));
    assert_eq!(vm.messages()[0].reactions.len(), 1);
    assert_eq!(vm.messages()[0].reactions[0].emoji, "👍");
    // Simulate Hub refresh overwriting SQLite reactions before reaction settles
    vm.store_mut()
        .update_message_reactions(&chat_a, "m1", &[])
        .unwrap();
    vm.refresh_messages();
    assert_eq!(
        vm.messages()[0].reactions.len(),
        1,
        "refresh_messages must reapply active pending reactions"
    );
    assert_eq!(vm.messages()[0].reactions[0].emoji, "👍");

    // 2. Stale-generation GiphyDownloadReady MUST NOT corrupt an active newer Giphy picker,
    //    and cancel() MUST clear `target`.
    assert!(vm.open_giphy_picker());
    let gen1 = vm.giphy_picker().generation;
    let target1 = vm.giphy_picker().target.clone().unwrap();
    vm.close_giphy_picker();
    assert!(
        vm.giphy_picker().target.is_none(),
        "cancel() must clear target"
    );

    assert!(vm.select_chat_key(&chat_b));
    assert!(vm.open_giphy_picker());
    let gen2 = vm.giphy_picker().generation;
    let target2 = vm.giphy_picker().target.clone().unwrap();
    let items20: Vec<GiphyItem> = (0..20)
        .map(|i| GiphyItem {
            id: format!("Gif{i}"),
            title: format!("Item {i}"),
            preview: GiphyRendition {
                url: format!("https://media1.giphy.com/media/Gif{i}/200w.gif"),
                width: 200,
                height: 150,
                size: 1000,
            },
            original: GiphyRendition {
                url: format!("https://media1.giphy.com/media/Gif{i}/giphy.gif"),
                width: 400,
                height: 300,
                size: 2000,
            },
        })
        .collect();
    vm.apply_sync_event(SyncEvent::GiphyResults {
        generation: gen2,
        target: target2,
        page: Some(GiphyPage {
            items: items20,
            total_count: 20,
            offset: 0,
        }),
        error: None,
    });
    assert_eq!(vm.giphy_picker().status, GiphyStatus::Ready);

    // Deliver stale gen1 download -> must be silently ignored; gen2 picker stays Ready!
    vm.apply_sync_event(SyncEvent::GiphyDownloadReady {
        generation: gen1,
        download_generation: 1,
        target: target1,
        index: 0,
        gif_id: "OldGif".into(),
        bytes: b"GIF89a\x01\x00\x01\x00\x00\x00\x00;".to_vec(),
    });
    assert!(vm.giphy_picker().is_open());
    assert_eq!(vm.giphy_picker().status, GiphyStatus::Ready);
    assert_eq!(vm.giphy_picker().generation, gen2);
    assert!(vm.staged_attachments().is_empty());

    // 3. GiphyPickerState::visible_slice() scrolls across all 20 items cleanly
    for _ in 0..4 {
        vm.move_giphy_picker(1, 0);
    }
    let (start, slice) = vm.giphy_picker().visible_slice();
    assert!(start > 0);
    assert!(
        vm.giphy_picker().selected >= start && vm.giphy_picker().selected < start + slice.len()
    );
    vm.close_giphy_picker();

    // 4. InsertIntoComposer routes to origin_chat's draft if active chat switched before activation
    assert!(vm.select_chat_key(&chat_a));
    assert!(vm.open_insert_emoji_picker());
    assert!(vm.select_chat_key(&chat_b));
    assert!(vm.activate_emoji_picker());
    assert!(
        vm.composer_text().is_empty(),
        "must not leak emoji into chat_b composer"
    );
    assert!(vm.select_chat_key(&chat_a));
    assert!(
        !vm.composer_text().is_empty(),
        "emoji must be saved in origin_chat (chat_a) draft"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn m3_regression_stale_giphy_results_error_never_clobbers_status_notice() {
    let store = Store::open_in_memory().unwrap();
    let chat_a = mk_chat(
        Provider::Whatsapp,
        "main",
        "a@s.whatsapp.net",
        "Alice",
        "2026-11-26T10:05:00Z",
    );
    let chat_b = mk_chat(
        Provider::Whatsapp,
        "main",
        "b@s.whatsapp.net",
        "Bob",
        "2026-11-26T10:04:00Z",
    );
    store.upsert_chat(&chat_a, 1000).unwrap();
    store.upsert_chat(&chat_b, 900).unwrap();

    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0")
        .with_giphy_key_available(true);
    assert!(vm.select_chat_key(&chat_a.key));

    // Open Giphy picker in Chat A (gen=1)
    assert!(vm.open_giphy_picker());
    let gen1 = vm.giphy_picker().generation;
    let target1 = vm.giphy_picker().target.clone().unwrap();

    // Switch to Chat B, open a newer picker (gen=2), mark it Ready, and set a baseline notice
    assert!(vm.select_chat_key(&chat_b.key));
    assert!(vm.open_giphy_picker());
    let gen2 = vm.giphy_picker().generation;
    let target2 = vm.giphy_picker().target.clone().unwrap();
    assert!(gen2 > gen1);
    vm.apply_sync_event(SyncEvent::GiphyResults {
        generation: gen2,
        target: target2,
        page: Some(GiphyPage {
            items: vec![GiphyItem {
                id: "GifNew".into(),
                title: "New".into(),
                preview: GiphyRendition {
                    url: "https://media1.giphy.com/media/GifNew/200w.gif".into(),
                    width: 200,
                    height: 150,
                    size: 1000,
                },
                original: GiphyRendition {
                    url: "https://media1.giphy.com/media/GifNew/giphy.gif".into(),
                    width: 400,
                    height: 300,
                    size: 2000,
                },
            }],
            total_count: 1,
            offset: 0,
        }),
        error: None,
    });
    assert_eq!(vm.giphy_picker().status, GiphyStatus::Ready);
    vm.apply_sync_event(SyncEvent::Notice("Baseline notice B".into()));

    // Case A: Stale gen=1 GiphyResults error must NOT clobber active gen=2 picker or status_notice
    vm.apply_sync_event(SyncEvent::GiphyResults {
        generation: gen1,
        target: target1.clone(),
        page: None,
        error: Some("GIF search failed. Press Enter to retry".into()),
    });
    assert_eq!(vm.giphy_picker().generation, gen2);
    assert_eq!(vm.giphy_picker().status, GiphyStatus::Ready);
    assert_eq!(vm.status_notice(), Some("Baseline notice B"));

    // Case B: Closed picker must ALSO ignore stale gen=1 GiphyResults error without touching status_notice
    vm.close_giphy_picker();
    assert!(!vm.giphy_picker().is_open());
    vm.apply_sync_event(SyncEvent::Notice("Quiet B".into()));
    vm.apply_sync_event(SyncEvent::GiphyResults {
        generation: gen1,
        target: target1,
        page: None,
        error: Some("GIF search failed. Press Enter to retry".into()),
    });
    assert!(!vm.giphy_picker().is_open());
    assert_eq!(vm.status_notice(), Some("Quiet B"));

    // Case C: Matching active Loading picker error DOES transition to Failed and sets status_notice
    assert!(vm.open_giphy_picker());
    let gen3 = vm.giphy_picker().generation;
    let target3 = vm.giphy_picker().target.clone().unwrap();
    vm.apply_sync_event(SyncEvent::GiphyResults {
        generation: gen3,
        target: target3,
        page: None,
        error: Some("GIF search failed. Press Enter to retry".into()),
    });
    assert_eq!(vm.giphy_picker().status, GiphyStatus::Failed);
    assert_eq!(
        vm.status_notice(),
        Some("GIF search failed. Press Enter to retry")
    );
}

#[test]
fn m3_regression_slack_thread_panel_immediate_reaction_and_terminal_rollback() {
    let mut store = Store::open_in_memory().unwrap();
    let chat = mk_chat(
        Provider::Slack,
        "T1",
        "slack:T1:C1",
        "General",
        "2026-11-26T12:05:00Z",
    );
    store.upsert_chat(&chat, 1000).unwrap();

    let initial_reactions = vec![
        Reaction {
            emoji: "😂".into(),
            count: 2,
            from_me: false,
        },
        Reaction {
            emoji: "❤️".into(),
            count: 1,
            from_me: true,
        },
    ];
    let expected_optimistic = vec![
        Reaction {
            emoji: "😂".into(),
            count: 2,
            from_me: true,
        },
        Reaction {
            emoji: "❤️".into(),
            count: 1,
            from_me: false,
        },
    ];

    let root = mk_media_msg(
        &chat.key,
        "m_root",
        false,
        "2026-11-26T12:00:00Z",
        "Thread root",
        None,
    );
    let mut reply = mk_media_msg(
        &chat.key,
        "m_reply",
        false,
        "2026-11-26T12:05:00Z",
        "Thread reply",
        None,
    );
    reply.reply_to_id = Some("m_root".into());
    reply.reactions = initial_reactions.clone();
    store.upsert_message(&root).unwrap();
    store.upsert_message(&reply).unwrap();

    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0");
    assert!(vm.select_chat_key(&chat.key));
    assert!(vm.open_thread("m_root"));

    let panel_reply_before = vm
        .thread_panel()
        .unwrap()
        .messages
        .iter()
        .find(|m| m.msg_id == "m_reply")
        .unwrap();
    assert_eq!(panel_reply_before.reactions, initial_reactions);

    // 1. Clicking reaction chip 0 ("😂") immediately updates main timeline, SQLite, AND open thread panel
    assert!(vm.click_reaction_chip("m_reply", 0));
    assert!(vm.is_reaction_pending(&chat.key, "m_reply"));

    let main_reply = vm
        .messages()
        .iter()
        .find(|m| m.msg_id == "m_reply")
        .unwrap();
    assert_eq!(main_reply.reactions, expected_optimistic);

    let panel_reply_after = vm
        .thread_panel()
        .unwrap()
        .messages
        .iter()
        .find(|m| m.msg_id == "m_reply")
        .unwrap();
    assert_eq!(
        panel_reply_after.reactions, expected_optimistic,
        "thread_panel.messages must update immediately on optimistic reaction"
    );

    let db_reply_after = vm
        .store()
        .list_messages(&chat.key, 50, None)
        .unwrap()
        .into_iter()
        .find(|m| m.msg_id == "m_reply")
        .unwrap();
    assert_eq!(db_reply_after.reactions, expected_optimistic);

    // 2. Fail the reaction 4 times terminally -> retries 1..3 keep optimistic state, 4th reverts everywhere
    for (attempt, now_ms) in [(1, 10_000u64), (2, 13_000), (3, 23_000), (4, 53_000)] {
        vm.settle_reaction_event(&chat.key, "m_reply", "😂", false, Some(500), now_ms);
        if attempt < 4 {
            assert!(vm.is_reaction_pending(&chat.key, "m_reply"));
            let panel_mid = vm
                .thread_panel()
                .unwrap()
                .messages
                .iter()
                .find(|m| m.msg_id == "m_reply")
                .unwrap();
            assert_eq!(panel_mid.reactions, expected_optimistic);
        }
    }

    assert!(!vm.is_reaction_pending(&chat.key, "m_reply"));
    assert_eq!(
        vm.status_notice(),
        Some("Reaction failed and was taken off")
    );

    let main_reply_reverted = vm
        .messages()
        .iter()
        .find(|m| m.msg_id == "m_reply")
        .unwrap();
    assert_eq!(main_reply_reverted.reactions, initial_reactions);

    let panel_reply_reverted = vm
        .thread_panel()
        .unwrap()
        .messages
        .iter()
        .find(|m| m.msg_id == "m_reply")
        .unwrap();
    assert_eq!(
        panel_reply_reverted.reactions, initial_reactions,
        "thread_panel.messages must revert immediately on 4th-failure terminal rollback"
    );

    let db_reply_reverted = vm
        .store()
        .list_messages(&chat.key, 50, None)
        .unwrap()
        .into_iter()
        .find(|m| m.msg_id == "m_reply")
        .unwrap();
    assert_eq!(db_reply_reverted.reactions, initial_reactions);
}

fn assert_thread_reaction_consistent(
    vm: &PostboxViewModel,
    chat: &ChatKey,
    msg_id: &str,
    expected: &[Reaction],
    context: &str,
) {
    let main_msg = vm
        .messages()
        .iter()
        .find(|m| &m.chat == chat && m.msg_id == msg_id)
        .unwrap_or_else(|| panic!("{context}: missing {msg_id} in vm.messages()"));
    assert_eq!(
        main_msg.reactions, expected,
        "{context}: vm.messages() reactions mismatch"
    );

    let panel = vm
        .thread_panel()
        .unwrap_or_else(|| panic!("{context}: expected open thread_panel"));
    let panel_msg = panel
        .messages
        .iter()
        .find(|m| m.msg_id == msg_id)
        .unwrap_or_else(|| panic!("{context}: missing {msg_id} in thread_panel.messages"));
    assert_eq!(
        panel_msg.reactions, expected,
        "{context}: thread_panel.messages reactions mismatch"
    );

    let panel_item_reactions = panel
        .timeline_items
        .iter()
        .find_map(|item| match item {
            TimelineItem::MessageRow { message, .. } if message.msg_id == msg_id => {
                Some(message.reactions.as_slice())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("{context}: missing {msg_id} in thread_panel.timeline_items"));
    assert_eq!(
        panel_item_reactions, expected,
        "{context}: thread_panel.timeline_items reactions mismatch"
    );

    let db_direct = vm
        .store()
        .get_message_reactions(chat, msg_id)
        .unwrap()
        .unwrap_or_else(|| panic!("{context}: missing {msg_id} in SQLite"));
    assert_eq!(
        db_direct, expected,
        "{context}: SQLite get_message_reactions mismatch"
    );

    let db_listed = vm
        .store()
        .list_messages(chat, 50, None)
        .unwrap()
        .into_iter()
        .find(|m| m.msg_id == msg_id)
        .unwrap_or_else(|| panic!("{context}: missing {msg_id} in SQLite list_messages"));
    assert_eq!(
        db_listed.reactions, expected,
        "{context}: SQLite list_messages reactions mismatch"
    );
}

#[test]
fn m3_regression_b1_interleaved_messages_updated_confirmed_and_409_preserve_thread_panel_and_db() {
    let mut store = Store::open_in_memory().unwrap();
    let chat = mk_chat(
        Provider::Slack,
        "T1",
        "slack:T1:C1",
        "General",
        "2026-11-26T12:05:00Z",
    );
    store.upsert_chat(&chat, 1000).unwrap();

    let root = mk_media_msg(
        &chat.key,
        "m_root",
        false,
        "2026-11-26T12:00:00Z",
        "Thread root",
        None,
    );
    let mut reply = mk_media_msg(
        &chat.key,
        "m_reply",
        false,
        "2026-11-26T12:05:00Z",
        "Thread reply",
        None,
    );
    reply.reply_to_id = Some("m_root".into());
    reply.reactions = vec![Reaction {
        emoji: "😂".into(),
        count: 1,
        from_me: false,
    }];
    store.upsert_message(&root).unwrap();
    store.upsert_message(&reply).unwrap();

    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0");
    assert!(vm.select_chat_key(&chat.key));
    assert!(vm.open_thread("m_root"));

    // Case A: preserve_chip = false -> interleaved stale MessagesUpdated -> Confirmed (HTTP 200)
    assert!(vm.react_to_message_with_chip(&chat.key, "m_reply", "🔥", false));
    let expected_reply = vec![
        Reaction {
            emoji: "😂".into(),
            count: 1,
            from_me: false,
        },
        Reaction {
            emoji: "🔥".into(),
            count: 1,
            from_me: true,
        },
    ];
    assert_thread_reaction_consistent(&vm, &chat.key, "m_reply", &expected_reply, "optimistic");

    // Hub sync overwrites SQLite with pre-reaction snapshot before reaction settles
    vm.store_mut().upsert_message(&reply).unwrap();
    vm.apply_sync_event(SyncEvent::MessagesUpdated {
        chat: chat.key.clone(),
        prepended: 0,
        exhausted: false,
    });
    assert!(vm.is_reaction_pending(&chat.key, "m_reply"));
    assert_eq!(
        vm.thread_panel()
            .unwrap()
            .messages
            .iter()
            .find(|m| m.msg_id == "m_reply")
            .unwrap()
            .reactions,
        expected_reply
    );

    vm.settle_reaction_event(&chat.key, "m_reply", "🔥", true, Some(200), 10_000);
    assert!(!vm.is_reaction_pending(&chat.key, "m_reply"));
    assert_eq!(vm.status_notice(), Some("Reaction sent"));
    assert_thread_reaction_consistent(
        &vm,
        &chat.key,
        "m_reply",
        &expected_reply,
        "Confirmed after interleaved MessagesUpdated",
    );

    // Case B: preserve_chip = false -> interleaved stale MessagesUpdated -> UnknownDelivery409
    assert!(vm.react_to_message_with_chip(&chat.key, "m_root", "👍", false));
    let expected_root = vec![Reaction {
        emoji: "👍".into(),
        count: 1,
        from_me: true,
    }];
    assert_thread_reaction_consistent(&vm, &chat.key, "m_root", &expected_root, "409 optimistic");

    // Hub sync overwrites SQLite with pre-reaction empty snapshot before 409 arrives
    vm.store_mut().upsert_message(&root).unwrap();
    vm.apply_sync_event(SyncEvent::MessagesUpdated {
        chat: chat.key.clone(),
        prepended: 0,
        exhausted: false,
    });

    vm.settle_reaction_event(&chat.key, "m_root", "👍", false, Some(409), 20_000);
    assert!(!vm.is_reaction_pending(&chat.key, "m_root"));
    assert_eq!(
        vm.status_notice(),
        Some("Reaction delivery unknown; check the chat")
    );
    assert_thread_reaction_consistent(
        &vm,
        &chat.key,
        "m_root",
        &expected_root,
        "UnknownDelivery409 after interleaved MessagesUpdated",
    );
}

#[test]
fn m3_regression_b1_interleaved_messages_updated_during_retry_backoff_preserves_chip_click() {
    let mut store = Store::open_in_memory().unwrap();
    let chat = mk_chat(
        Provider::Slack,
        "T1",
        "slack:T1:C1",
        "General",
        "2026-11-26T12:05:00Z",
    );
    store.upsert_chat(&chat, 1000).unwrap();

    let root = mk_media_msg(
        &chat.key,
        "m_root",
        false,
        "2026-11-26T12:00:00Z",
        "Thread root",
        None,
    );
    let mut reply = mk_media_msg(
        &chat.key,
        "m_reply",
        false,
        "2026-11-26T12:05:00Z",
        "Thread reply",
        None,
    );
    reply.reply_to_id = Some("m_root".into());
    reply.reactions = vec![Reaction {
        emoji: "😂".into(),
        count: 1,
        from_me: false,
    }];
    store.upsert_message(&root).unwrap();
    store.upsert_message(&reply).unwrap();

    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0");
    assert!(vm.select_chat_key(&chat.key));
    assert!(vm.open_thread("m_root"));

    // Click existing chip "😂" (preserve_chip = true)
    assert!(vm.click_reaction_chip("m_reply", 0));
    let expected = vec![Reaction {
        emoji: "😂".into(),
        count: 1,
        from_me: true,
    }];
    assert_thread_reaction_consistent(&vm, &chat.key, "m_reply", &expected, "chip click");

    // Attempt 1 fails -> scheduled for 3s backoff
    vm.settle_reaction_event(&chat.key, "m_reply", "😂", false, Some(500), 10_000);
    assert!(vm.is_reaction_pending(&chat.key, "m_reply"));
    assert_eq!(vm.next_reaction_retry_at_ms(), Some(13_000));
    assert_eq!(vm.status_notice(), Some("Reaction not sent yet; retrying"));

    // Interleaved Hub MessagesUpdated writes pre-reaction snapshot into SQLite during backoff
    vm.store_mut().upsert_message(&reply).unwrap();
    vm.apply_sync_event(SyncEvent::MessagesUpdated {
        chat: chat.key.clone(),
        prepended: 0,
        exhausted: false,
    });
    assert_eq!(
        vm.messages()
            .iter()
            .find(|m| m.msg_id == "m_reply")
            .unwrap()
            .reactions,
        expected
    );
    assert_eq!(
        vm.thread_panel()
            .unwrap()
            .messages
            .iter()
            .find(|m| m.msg_id == "m_reply")
            .unwrap()
            .reactions,
        expected
    );

    // Retry fires at 13_000 and settles Confirmed
    let _ = vm.take_emitted_sync_commands();
    vm.retry_due_reactions(13_000);
    let cmds = vm.take_emitted_sync_commands();
    assert!(cmds.iter().any(|c| matches!(
        c,
        SyncCommand::ReactToMessage { msg_id, emoji, .. } if msg_id == "m_reply" && emoji == "😂"
    )));

    vm.settle_reaction_event(&chat.key, "m_reply", "😂", true, Some(200), 13_200);
    assert!(!vm.is_reaction_pending(&chat.key, "m_reply"));
    assert_eq!(vm.status_notice(), Some("Reaction sent"));
    assert_thread_reaction_consistent(
        &vm,
        &chat.key,
        "m_reply",
        &expected,
        "Retry 2 Confirmed after interleaved MessagesUpdated",
    );
}

#[test]
fn m3_regression_b1_cross_chat_settlement_after_interleaved_messages_updated_persists_db() {
    let mut store = Store::open_in_memory().unwrap();
    let chat_s = mk_chat(
        Provider::Slack,
        "T1",
        "slack:T1:C1",
        "General",
        "2026-11-26T12:05:00Z",
    );
    let chat_w = mk_chat(
        Provider::Whatsapp,
        "main",
        "paula@s.whatsapp.net",
        "Paula",
        "2026-11-26T12:00:00Z",
    );
    store.upsert_chat(&chat_s, 1000).unwrap();
    store.upsert_chat(&chat_w, 900).unwrap();

    let mut m1 = mk_media_msg(
        &chat_s.key,
        "m1",
        false,
        "2026-11-26T12:01:00Z",
        "One",
        None,
    );
    m1.reactions = vec![Reaction {
        emoji: "😂".into(),
        count: 1,
        from_me: false,
    }];
    let mut m2 = mk_media_msg(
        &chat_s.key,
        "m2",
        false,
        "2026-11-26T12:02:00Z",
        "Two",
        None,
    );
    m2.reactions = vec![Reaction {
        emoji: "❤️".into(),
        count: 1,
        from_me: false,
    }];
    let mut m3 = mk_media_msg(
        &chat_s.key,
        "m3",
        false,
        "2026-11-26T12:03:00Z",
        "Three",
        None,
    );
    m3.reactions = vec![Reaction {
        emoji: "😂".into(),
        count: 1,
        from_me: false,
    }];
    let w1 = mk_media_msg(
        &chat_w.key,
        "w1",
        false,
        "2026-11-26T12:00:00Z",
        "Hey",
        None,
    );

    for m in [&m1, &m2, &m3, &w1] {
        store.upsert_message(m).unwrap();
    }

    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0");
    assert!(vm.select_chat_key(&chat_s.key));

    // Queue 3 pending reactions in chat_s
    assert!(vm.react_to_message_with_chip(&chat_s.key, "m1", "🔥", false));
    assert!(vm.react_to_message_with_chip(&chat_s.key, "m2", "👍", false));
    assert!(vm.click_reaction_chip("m3", 0)); // preserve_chip = true, emoji = "😂"

    // Switch to chat_w so chat_s messages are no longer in vm.messages()
    assert!(vm.select_chat_key(&chat_w.key));
    assert!(vm.messages().iter().all(|m| m.chat == chat_w.key));

    // Interleaved Hub snapshot for chat_s:
    // - m1 & m2 get overwritten with stale pre-reaction snapshots
    // - m3 gets an already-updated Hub snapshot ([😂 2 true]) to verify idempotency
    vm.store_mut().upsert_message(&m1).unwrap();
    vm.store_mut().upsert_message(&m2).unwrap();
    let mut m3_hub_updated = m3.clone();
    m3_hub_updated.reactions = vec![Reaction {
        emoji: "😂".into(),
        count: 2,
        from_me: true,
    }];
    vm.store_mut().upsert_message(&m3_hub_updated).unwrap();
    vm.apply_sync_event(SyncEvent::MessagesUpdated {
        chat: chat_s.key.clone(),
        prepended: 0,
        exhausted: false,
    });

    // Settle all 3 cross-chat while viewing chat_w
    vm.settle_reaction_event(&chat_s.key, "m1", "🔥", true, Some(200), 10_000);
    vm.settle_reaction_event(&chat_s.key, "m2", "👍", false, Some(409), 10_100);
    vm.settle_reaction_event(&chat_s.key, "m3", "😂", true, Some(200), 10_200);

    let expected_m1 = vec![
        Reaction {
            emoji: "😂".into(),
            count: 1,
            from_me: false,
        },
        Reaction {
            emoji: "🔥".into(),
            count: 1,
            from_me: true,
        },
    ];
    let expected_m2 = vec![
        Reaction {
            emoji: "❤️".into(),
            count: 1,
            from_me: false,
        },
        Reaction {
            emoji: "👍".into(),
            count: 1,
            from_me: true,
        },
    ];
    let expected_m3 = vec![Reaction {
        emoji: "😂".into(),
        count: 2,
        from_me: true,
    }];

    assert_eq!(
        vm.store()
            .get_message_reactions(&chat_s.key, "m1")
            .unwrap()
            .unwrap(),
        expected_m1
    );
    assert_eq!(
        vm.store()
            .get_message_reactions(&chat_s.key, "m2")
            .unwrap()
            .unwrap(),
        expected_m2
    );
    assert_eq!(
        vm.store()
            .get_message_reactions(&chat_s.key, "m3")
            .unwrap()
            .unwrap(),
        expected_m3
    );

    // Switching back to chat_s displays all 3 settled reactions consistently
    assert!(vm.select_chat_key(&chat_s.key));
    assert_eq!(
        vm.messages()
            .iter()
            .find(|m| m.msg_id == "m1")
            .unwrap()
            .reactions,
        expected_m1
    );
    assert_eq!(
        vm.messages()
            .iter()
            .find(|m| m.msg_id == "m2")
            .unwrap()
            .reactions,
        expected_m2
    );
    assert_eq!(
        vm.messages()
            .iter()
            .find(|m| m.msg_id == "m3")
            .unwrap()
            .reactions,
        expected_m3
    );
}

#[test]
fn m3_regression_o1_superseding_chip_click_with_ctrl_r_uses_baseline_without_count_drift() {
    let mut store = Store::open_in_memory().unwrap();
    let chat = mk_chat(
        Provider::Slack,
        "T1",
        "slack:T1:C1",
        "General",
        "2026-11-26T12:05:00Z",
    );
    store.upsert_chat(&chat, 1000).unwrap();

    let baseline = vec![
        Reaction {
            emoji: "😂".into(),
            count: 2,
            from_me: false,
        },
        Reaction {
            emoji: "❤️".into(),
            count: 1,
            from_me: true,
        },
    ];
    let root = mk_media_msg(
        &chat.key,
        "m_root",
        false,
        "2026-11-26T12:00:00Z",
        "Thread root",
        None,
    );
    let mut reply = mk_media_msg(
        &chat.key,
        "m_reply",
        false,
        "2026-11-26T12:05:00Z",
        "Thread reply",
        None,
    );
    reply.reply_to_id = Some("m_root".into());
    reply.reactions = baseline.clone();
    store.upsert_message(&root).unwrap();
    store.upsert_message(&reply).unwrap();

    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0");
    assert!(vm.select_chat_key(&chat.key));
    assert!(vm.open_thread("m_root"));

    // 1. Click chip "😂" (preserve_chip = true) -> counts stay 2 & 1, from_me flips
    assert!(vm.click_reaction_chip("m_reply", 0));
    let after_chip = vec![
        Reaction {
            emoji: "😂".into(),
            count: 2,
            from_me: true,
        },
        Reaction {
            emoji: "❤️".into(),
            count: 1,
            from_me: false,
        },
    ];
    assert_thread_reaction_consistent(&vm, &chat.key, "m_reply", &after_chip, "after chip click");

    // 2. Before settlement, supersede with "❤️" via Ctrl+R (preserve_chip = false):
    //    MUST compute against original baseline [😂 2 false, ❤️ 1 true] -> [😂 2 false, ❤️ 1 true],
    //    NOT drifting to [😂 1 false, ❤️ 2 true]!
    assert!(vm.react_to_message_with_chip(&chat.key, "m_reply", "❤️", false));
    assert_thread_reaction_consistent(
        &vm,
        &chat.key,
        "m_reply",
        &baseline,
        "supersede chip with ❤️ via Ctrl+R",
    );

    // Stale settlement for superseded "😂" MUST be ignored
    vm.settle_reaction_event(&chat.key, "m_reply", "😂", true, Some(200), 5_000);
    assert!(vm.is_reaction_pending(&chat.key, "m_reply"));
    assert_thread_reaction_consistent(
        &vm,
        &chat.key,
        "m_reply",
        &baseline,
        "after stale settlement ignored",
    );

    // 3. Supersede again with a new emoji "🔥" (preserve_chip = false):
    //    Computed against original baseline [😂 2 false, ❤️ 1 true] -> [😂 2 false, 🔥 1 true]
    assert!(vm.react_to_message_with_chip(&chat.key, "m_reply", "🔥", false));
    let expected_fire = vec![
        Reaction {
            emoji: "😂".into(),
            count: 2,
            from_me: false,
        },
        Reaction {
            emoji: "🔥".into(),
            count: 1,
            from_me: true,
        },
    ];
    assert_thread_reaction_consistent(
        &vm,
        &chat.key,
        "m_reply",
        &expected_fire,
        "supersede with 🔥 via Ctrl+R",
    );

    // 4. 4th terminal failure restores original pre-first-reaction snapshot cleanly everywhere
    for now_ms in [10_000u64, 13_000, 23_000, 53_000] {
        vm.settle_reaction_event(&chat.key, "m_reply", "🔥", false, Some(500), now_ms);
    }
    assert!(!vm.is_reaction_pending(&chat.key, "m_reply"));
    assert_eq!(
        vm.status_notice(),
        Some("Reaction failed and was taken off")
    );
    assert_thread_reaction_consistent(
        &vm,
        &chat.key,
        "m_reply",
        &baseline,
        "4th terminal rollback after supersede",
    );
}

#[test]
fn m3_regression_f1_cross_chat_staged_attachments_never_leak_wipe_or_block_text() {
    let dir = std::env::temp_dir().join(format!(
        "postbox_m3_f1_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = Store::open_in_memory().unwrap();
    let chat_a = mk_chat(
        Provider::Whatsapp,
        "main",
        "a@s.whatsapp.net",
        "Alice",
        "2026-11-26T10:05:00Z",
    );
    let chat_b = mk_chat(
        Provider::Whatsapp,
        "main",
        "b@s.whatsapp.net",
        "Bob",
        "2026-11-26T10:04:00Z",
    );
    store.upsert_chat(&chat_a, 1000).unwrap();
    store.upsert_chat(&chat_b, 900).unwrap();

    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0")
        .with_data_dir(dir.clone());
    let png_a = encode_png(16, 16, &vec![100u8; 16 * 16 * 4]).unwrap();
    let png_b = encode_png(16, 16, &vec![200u8; 16 * 16 * 4]).unwrap();

    // 1. Paste in Chat A -> switch to Chat B -> Chat A attachment discarded -> plain text sends to Chat B!
    assert!(vm.select_chat_key(&chat_a.key));
    assert!(matches!(
        vm.handle_paste_clipboard_payload(ClipboardPayload::PngBytes(png_a.clone())),
        PasteStageResult::Staged { count: 1, .. }
    ));
    assert_eq!(vm.staged_attachments().len(), 1);

    assert!(vm.select_chat_key(&chat_b.key));
    assert!(
        vm.staged_attachments().is_empty(),
        "switching chats must discard prior chat's staged attachments"
    );
    vm.set_composer_text("plain text for B");
    assert!(
        vm.submit_composer_message(),
        "stale Chat A attachment must never block plain-text send in Chat B"
    );
    let outbox_b = vm.store().list_outbox_for_chat(&chat_b.key).unwrap();
    assert_eq!(outbox_b.len(), 1);
    assert_eq!(outbox_b[0].text_body, "plain text for B");
    assert!(outbox_b[0].staged_file_path.is_none());

    // 2. Paste in Chat A -> switch to Chat B -> paste in Chat B -> send in Chat B -> ONLY Chat B's image sent!
    assert!(vm.select_chat_key(&chat_a.key));
    assert!(matches!(
        vm.handle_paste_clipboard_payload(ClipboardPayload::PngBytes(png_a.clone())),
        PasteStageResult::Staged { count: 1, .. }
    ));
    assert!(vm.select_chat_key(&chat_b.key));
    assert!(matches!(
        vm.handle_paste_clipboard_payload(ClipboardPayload::PngBytes(png_b.clone())),
        PasteStageResult::Staged { count: 1, .. }
    ));
    assert_eq!(vm.staged_attachments().len(), 1);
    assert_eq!(vm.staged_attachments().items()[0].chat, chat_b.key);
    assert!(vm.submit_composer_message());
    let outbox_b2 = vm.store().list_outbox_for_chat(&chat_b.key).unwrap();
    assert_eq!(outbox_b2.len(), 2);
    assert!(outbox_b2[1].staged_file_path.is_some());

    // 3. Paste in Chat A -> switch to Chat B -> paste secret_B -> switch back to Chat A -> NEVER leaks to Chat A!
    assert!(vm.select_chat_key(&chat_a.key));
    assert!(matches!(
        vm.handle_paste_clipboard_payload(ClipboardPayload::PngBytes(png_a)),
        PasteStageResult::Staged { count: 1, .. }
    ));
    assert!(vm.select_chat_key(&chat_b.key));
    assert!(matches!(
        vm.handle_paste_clipboard_payload(ClipboardPayload::PngBytes(png_b)),
        PasteStageResult::Staged { count: 1, .. }
    ));
    assert!(vm.select_chat_key(&chat_a.key));
    assert!(vm.staged_attachments().is_empty());
    assert!(!vm.submit_composer_message());
    assert!(
        vm.store()
            .list_outbox_for_chat(&chat_a.key)
            .unwrap()
            .is_empty()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn m3_regression_f2_modal_exclusivity_and_escape_without_zombie_palette() {
    let mut store = Store::open_in_memory().unwrap();
    let chat = mk_chat(
        Provider::Whatsapp,
        "main",
        "a@s.whatsapp.net",
        "Alice",
        "2026-11-26T10:05:00Z",
    );
    store.upsert_chat(&chat, 1000).unwrap();
    store
        .upsert_message(&mk_media_msg(
            &chat.key,
            "img1",
            false,
            "2026-11-26T10:05:00Z",
            "Photo",
            Some((MediaKind::Image, "image/png", 1024)),
        ))
        .unwrap();

    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0")
        .with_giphy_key_available(true);
    assert!(vm.select_chat_key(&chat.key));

    let count_open_modals = |vm: &PostboxViewModel| -> usize {
        usize::from(vm.lightbox().is_some())
            + usize::from(vm.emoji_picker().is_some())
            + usize::from(vm.giphy_picker().is_open())
            + usize::from(vm.palette_open())
            + usize::from(vm.shortcuts_open())
    };

    // 1. Lightbox -> Ctrl+K Palette -> Lightbox closed, only Palette open, Escape closes cleanly
    assert!(vm.open_lightbox_for_message("img1", 1000));
    assert_eq!(count_open_modals(&vm), 1);
    assert_eq!(vm.focus(), postbox_ui::FocusTarget::Lightbox);

    vm.open_palette();
    assert_eq!(count_open_modals(&vm), 1);
    assert!(vm.palette_open());
    assert!(vm.lightbox().is_none());
    assert_eq!(vm.focus(), postbox_ui::FocusTarget::Palette);

    // 2. Palette -> F1 Shortcuts -> Palette closed, only Shortcuts open
    vm.toggle_shortcuts();
    assert_eq!(count_open_modals(&vm), 1);
    assert!(vm.shortcuts_open());
    assert!(!vm.palette_open());
    assert_eq!(vm.focus(), postbox_ui::FocusTarget::ShortcutsOverlay);

    // 3. Shortcuts -> Ctrl+R Reaction Picker -> Shortcuts closed, only EmojiPicker open
    assert!(vm.open_reaction_picker());
    assert_eq!(count_open_modals(&vm), 1);
    assert!(vm.emoji_picker().is_some());
    assert!(!vm.shortcuts_open());
    assert_eq!(vm.focus(), postbox_ui::FocusTarget::EmojiPicker);

    // 4. EmojiPicker -> GiphyPicker -> EmojiPicker closed, only GiphyPicker open
    assert!(vm.open_giphy_picker());
    let giphy_gen = vm.giphy_picker().generation;
    assert_eq!(count_open_modals(&vm), 1);
    assert!(vm.giphy_picker().is_open());
    assert!(vm.emoji_picker().is_none());
    assert_eq!(vm.focus(), postbox_ui::FocusTarget::GiphyPicker);

    // Drain prior commands, then open Palette while GiphyPicker is open -> MUST emit CancelGiphy!
    let _ = vm.take_emitted_sync_commands();
    vm.open_palette();
    assert_eq!(count_open_modals(&vm), 1);
    assert!(vm.palette_open());
    assert!(!vm.giphy_picker().is_open());
    assert_eq!(vm.focus(), postbox_ui::FocusTarget::Palette);
    let cmds = vm.take_emitted_sync_commands();
    assert!(
        cmds.iter().any(
            |cmd| matches!(cmd, SyncCommand::CancelGiphy { generation } if *generation == giphy_gen)
        ),
        "dismissing GiphyPicker via Ctrl+K must emit CancelGiphy"
    );

    // 5. Escape closes the single active modal with ZERO zombie overlays left behind
    vm.handle_escape();
    assert_eq!(count_open_modals(&vm), 0);
    assert_eq!(vm.focus(), postbox_ui::FocusTarget::ChatList);
}

#[test]
fn m3_regression_f3_open_chat_cache_hit_touches_sqlite_and_disk_mtime_survives_14d_eviction() {
    let dir = std::env::temp_dir().join(format!(
        "postbox_m3_f3_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let hub_dir = dir.join("hub-media");
    std::fs::create_dir_all(&hub_dir).unwrap();

    let mut store = Store::open_in_memory().unwrap();
    let chat = mk_chat(
        Provider::Whatsapp,
        "main",
        "a@s.whatsapp.net",
        "Alice",
        "2026-11-26T10:05:00Z",
    );
    store.upsert_chat(&chat, 1000).unwrap();
    store
        .upsert_message(&mk_media_msg(
            &chat.key,
            "img1",
            false,
            "2026-11-26T10:05:00Z",
            "Photo",
            Some((MediaKind::Image, "image/png", 1024)),
        ))
        .unwrap();

    let now = postbox_media::age::now_unix_secs();
    let old_unix = now - 15 * 86_400;
    let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(15 * 86_400);
    let cached_file = hub_dir.join("cached_img1.png");
    std::fs::write(&cached_file, b"png-bytes").unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&cached_file)
        .unwrap()
        .set_modified(old_time)
        .unwrap();
    let cache_key = postbox_media::drain::media_cache_key(&chat.key, "img1");
    store
        .upsert_media_cache(&postbox_store::MediaCacheEntry {
            cache_key: cache_key.clone(),
            sha256: "f".repeat(64),
            rel_path: "cached_img1.png".into(),
            mime_type: "image/png".into(),
            byte_size: 9,
            last_accessed_at: old_unix,
        })
        .unwrap();

    let mut vm = PostboxViewModel::new_uninitialized(store, Keymap::postbox_defaults(), "0.1.0")
        .with_data_dir(dir.clone());
    vm.initialize();
    assert!(vm.select_chat_key(&chat.key));
    assert!(matches!(
        vm.media_status_for(&chat.key, "img1"),
        MediaItemStatus::Ready(_)
    ));

    // SQLite last_accessed_at AND on-disk mtime must BOTH have been refreshed!
    let updated = vm.store().get_media_cache(&cache_key).unwrap().unwrap();
    assert!(updated.last_accessed_at >= now - 2);
    let mtime = std::fs::metadata(&cached_file)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!(mtime >= now - 5);

    // 14-day eviction run now MUST preserve the re-accessed file on disk and in SQLite!
    let evicted = postbox_media::age::evict_expired_media_cache(vm.store(), &hub_dir, now, 14);
    assert_eq!(evicted, 0);
    assert!(cached_file.exists());
    assert!(vm.store().get_media_cache(&cache_key).unwrap().is_some());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn m3_regression_f4_giphy_rapid_tile_switch_drops_superseded_tile_download() {
    let dir = std::env::temp_dir().join(format!(
        "postbox_m3_f4_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = Store::open_in_memory().unwrap();
    let chat = mk_chat(
        Provider::Whatsapp,
        "main",
        "a@s.whatsapp.net",
        "Alice",
        "2026-11-26T10:05:00Z",
    );
    store.upsert_chat(&chat, 1000).unwrap();

    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0")
        .with_data_dir(dir.clone())
        .with_giphy_key_available(true);
    assert!(vm.select_chat_key(&chat.key));
    assert!(vm.open_giphy_picker());

    let gen_id = vm.giphy_picker().generation;
    let target = vm.giphy_picker().target.clone().unwrap();
    let items: Vec<GiphyItem> = (0..3)
        .map(|i| GiphyItem {
            id: format!("Tile{i}"),
            title: format!("Tile {i}"),
            preview: GiphyRendition {
                url: format!("https://media1.giphy.com/media/Tile{i}/200w.gif"),
                width: 200,
                height: 150,
                size: 1000,
            },
            original: GiphyRendition {
                url: format!("https://media1.giphy.com/media/Tile{i}/giphy.gif"),
                width: 400,
                height: 300,
                size: 2000,
            },
        })
        .collect();
    vm.apply_sync_event(SyncEvent::GiphyResults {
        generation: gen_id,
        target: target.clone(),
        page: Some(GiphyPage {
            items,
            total_count: 3,
            offset: 0,
        }),
        error: None,
    });
    assert_eq!(vm.giphy_picker().status, GiphyStatus::Ready);

    // Rapidly click tile 0 then tile 2 within the same search generation
    assert!(vm.request_giphy_tile_download(0));
    let dl_gen_0 = vm.giphy_picker().active_download_generation.unwrap();
    assert_eq!(vm.giphy_picker().downloading_index, Some(0));
    assert!(vm.request_giphy_tile_download(2));
    let dl_gen_2 = vm.giphy_picker().active_download_generation.unwrap();
    assert!(dl_gen_2 > dl_gen_0);
    assert_eq!(vm.giphy_picker().downloading_index, Some(2));
    assert_eq!(
        vm.status_notice(),
        Some(postbox_media::giphy::GIPHY_MSG_DOWNLOADING)
    );

    // Late tile 0 GiphyError MUST be silently ignored (must NOT clear downloading_index or
    // clobber status_notice so tile 2 can still complete cleanly)!
    vm.apply_sync_event(SyncEvent::GiphyError {
        generation: gen_id,
        index: 0,
        download_generation: dl_gen_0,
        message: postbox_media::giphy::GIPHY_MSG_DOWNLOAD_FAILED.into(),
    });
    assert!(vm.giphy_picker().is_open());
    assert_eq!(vm.giphy_picker().downloading_index, Some(2));
    assert_eq!(vm.giphy_picker().active_download_generation, Some(dl_gen_2));
    assert_eq!(
        vm.status_notice(),
        Some(postbox_media::giphy::GIPHY_MSG_DOWNLOADING)
    );

    // Late tile 0 download completion MUST be silently ignored!
    vm.apply_sync_event(SyncEvent::GiphyDownloadReady {
        generation: gen_id,
        download_generation: dl_gen_0,
        target: target.clone(),
        index: 0,
        gif_id: "Tile0".into(),
        bytes: b"GIF89a\x01\x00\x01\x00\x00\x00\x00;".to_vec(),
    });
    assert!(vm.giphy_picker().is_open());
    assert_eq!(vm.giphy_picker().downloading_index, Some(2));
    assert!(vm.staged_attachments().is_empty());

    // Tile 2 download completion stages Tile2.gif cleanly
    vm.apply_sync_event(SyncEvent::GiphyDownloadReady {
        generation: gen_id,
        download_generation: dl_gen_2,
        target,
        index: 2,
        gif_id: "Tile2".into(),
        bytes: b"GIF89a\x01\x00\x01\x00\x00\x00\x00;".to_vec(),
    });
    assert!(!vm.giphy_picker().is_open());
    assert_eq!(vm.staged_attachments().len(), 1);
    assert_eq!(vm.staged_attachments().items()[0].filename, "Tile2.gif");

    // --- ABA race (tile 0 -> tile 2 -> tile 0) AND active tile GiphyError ---
    assert!(vm.discard_staged_attachments());
    assert!(vm.open_giphy_picker());
    let gen_id2 = vm.giphy_picker().generation;
    let target2 = vm.giphy_picker().target.clone().unwrap();
    let items2: Vec<GiphyItem> = (0..3)
        .map(|i| GiphyItem {
            id: format!("Aba{i}"),
            title: format!("ABA {i}"),
            preview: GiphyRendition {
                url: format!("https://media1.giphy.com/media/Aba{i}/200w.gif"),
                width: 200,
                height: 150,
                size: 1000,
            },
            original: GiphyRendition {
                url: format!("https://media1.giphy.com/media/Aba{i}/giphy.gif"),
                width: 400,
                height: 300,
                size: 2000,
            },
        })
        .collect();
    vm.apply_sync_event(SyncEvent::GiphyResults {
        generation: gen_id2,
        target: target2.clone(),
        page: Some(GiphyPage {
            items: items2,
            total_count: 3,
            offset: 0,
        }),
        error: None,
    });
    assert_eq!(vm.giphy_picker().status, GiphyStatus::Ready);

    // Tile 0 (first) -> Tile 2 -> Tile 0 (second)
    assert!(vm.request_giphy_tile_download(0));
    let aba_dl_0_first = vm.giphy_picker().active_download_generation.unwrap();
    assert!(
        aba_dl_0_first > dl_gen_2,
        "download_generation must never reset on cancel"
    );
    assert!(vm.request_giphy_tile_download(2));
    let aba_dl_2 = vm.giphy_picker().active_download_generation.unwrap();
    assert!(aba_dl_2 > aba_dl_0_first);
    assert!(vm.request_giphy_tile_download(0));
    let aba_dl_0_second = vm.giphy_picker().active_download_generation.unwrap();
    assert!(aba_dl_0_second > aba_dl_2);
    assert_eq!(vm.giphy_picker().downloading_index, Some(0));

    // Stale first tile-0 GiphyError must be ignored even though index == 0 matches!
    vm.apply_sync_event(SyncEvent::GiphyError {
        generation: gen_id2,
        index: 0,
        download_generation: aba_dl_0_first,
        message: "stale first attempt failed".into(),
    });
    assert_eq!(vm.giphy_picker().downloading_index, Some(0));
    assert_eq!(
        vm.giphy_picker().active_download_generation,
        Some(aba_dl_0_second)
    );
    assert_eq!(
        vm.status_notice(),
        Some(postbox_media::giphy::GIPHY_MSG_DOWNLOADING)
    );

    // Stale first tile-0 GiphyDownloadReady must ALSO be ignored even though index == 0 matches!
    vm.apply_sync_event(SyncEvent::GiphyDownloadReady {
        generation: gen_id2,
        download_generation: aba_dl_0_first,
        target: target2.clone(),
        index: 0,
        gif_id: "StaleAba0".into(),
        bytes: b"GIF89a\x01\x00\x01\x00\x00\x00\x00;".to_vec(),
    });
    assert!(vm.giphy_picker().is_open());
    assert_eq!(vm.giphy_picker().downloading_index, Some(0));
    assert!(vm.staged_attachments().is_empty());

    // Active tile-0 GiphyError DOES clear downloading_index and set status_notice!
    vm.apply_sync_event(SyncEvent::GiphyError {
        generation: gen_id2,
        index: 0,
        download_generation: aba_dl_0_second,
        message: postbox_media::giphy::GIPHY_MSG_DOWNLOAD_FAILED.into(),
    });
    assert!(vm.giphy_picker().is_open());
    assert_eq!(vm.giphy_picker().status, GiphyStatus::Ready);
    assert_eq!(vm.giphy_picker().downloading_index, None);
    assert_eq!(vm.giphy_picker().active_download_generation, None);
    assert_eq!(
        vm.status_notice(),
        Some(postbox_media::giphy::GIPHY_MSG_DOWNLOAD_FAILED)
    );

    // User retries tile 0 (third attempt) -> succeeds and stages Aba0.gif!
    assert!(vm.request_giphy_tile_download(0));
    let aba_dl_0_third = vm.giphy_picker().active_download_generation.unwrap();
    assert!(aba_dl_0_third > aba_dl_0_second);
    vm.apply_sync_event(SyncEvent::GiphyDownloadReady {
        generation: gen_id2,
        download_generation: aba_dl_0_third,
        target: target2,
        index: 0,
        gif_id: "Aba0".into(),
        bytes: b"GIF89a\x01\x00\x01\x00\x00\x00\x00;".to_vec(),
    });
    assert!(!vm.giphy_picker().is_open());
    assert_eq!(vm.staged_attachments().len(), 1);
    assert_eq!(vm.staged_attachments().items()[0].filename, "Aba0.gif");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn m3_regression_f5_giphy_capacity_full_cleans_up_focus_and_preview_map() {
    let dir = std::env::temp_dir().join(format!(
        "postbox_m3_f5_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let store = Store::open_in_memory().unwrap();
    let chat = mk_chat(
        Provider::Whatsapp,
        "main",
        "a@s.whatsapp.net",
        "Alice",
        "2026-11-26T10:05:00Z",
    );
    store.upsert_chat(&chat, 1000).unwrap();

    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0")
        .with_data_dir(dir.clone())
        .with_giphy_key_available(true);
    assert!(vm.select_chat_key(&chat.key));

    // Fill staged attachments to full capacity (16)
    let png = encode_png(12, 12, &vec![150u8; 12 * 12 * 4]).unwrap();
    for _ in 0..16 {
        assert!(matches!(
            vm.handle_paste_clipboard_payload(ClipboardPayload::PngBytes(png.clone())),
            PasteStageResult::Staged { count: 1, .. }
        ));
    }
    assert_eq!(vm.staged_attachments().len(), 16);

    assert!(vm.open_giphy_picker());
    let gen_id = vm.giphy_picker().generation;
    let target = vm.giphy_picker().target.clone().unwrap();
    vm.apply_sync_event(SyncEvent::GiphyResults {
        generation: gen_id,
        target: target.clone(),
        page: Some(GiphyPage {
            items: vec![GiphyItem {
                id: "FullGif".into(),
                title: "Full".into(),
                preview: GiphyRendition {
                    url: "https://media1.giphy.com/media/FullGif/200w.gif".into(),
                    width: 200,
                    height: 150,
                    size: 1000,
                },
                original: GiphyRendition {
                    url: "https://media1.giphy.com/media/FullGif/giphy.gif".into(),
                    width: 400,
                    height: 300,
                    size: 2000,
                },
            }],
            total_count: 1,
            offset: 0,
        }),
        error: None,
    });
    vm.apply_sync_event(SyncEvent::GiphyPreviewReady {
        generation: gen_id,
        index: 0,
        path: dir.join("preview0.gif"),
    });
    assert!(vm.giphy_preview_path(0).is_some());

    assert!(vm.request_giphy_tile_download(0));
    let dl_gen = vm.giphy_picker().active_download_generation.unwrap();
    vm.apply_sync_event(SyncEvent::GiphyDownloadReady {
        generation: gen_id,
        download_generation: dl_gen,
        target,
        index: 0,
        gif_id: "FullGif".into(),
        bytes: b"GIF89a\x01\x00\x01\x00\x00\x00\x00;".to_vec(),
    });

    assert!(!vm.giphy_picker().is_open());
    assert_eq!(vm.focus(), postbox_ui::FocusTarget::Composer);
    assert!(
        vm.giphy_preview_path(0).is_none(),
        "CapacityFull failure must clear giphy_preview_paths"
    );
    assert_eq!(
        vm.status_notice(),
        Some(postbox_media::giphy::GIPHY_MSG_ATTACH_FAILED)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn m3_regression_insert_emoji_picker_captures_post_dismiss_origin_chat() {
    let store = Store::open_in_memory().unwrap();
    let chat = mk_chat(
        Provider::Whatsapp,
        "main",
        "a@s.whatsapp.net",
        "Alice",
        "2026-11-26T10:05:00Z",
    );
    store.upsert_chat(&chat, 1000).unwrap();

    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0");
    assert!(vm.select_chat_key(&chat.key));

    // Open SearchChats with a zero-match query so selected_chat() is None while search is active
    vm.open_search_palette();
    vm.set_palette_query("zzzz_no_match");
    assert!(vm.selected_chat().is_none());

    // Opening InsertIntoComposer emoji picker dismisses SearchChats (restoring chats) BEFORE
    // capturing origin_chat, so origin_chat matches the restored active chat!
    assert!(vm.open_insert_emoji_picker());
    assert_eq!(
        vm.emoji_picker().map(|p| &p.mode),
        Some(&postbox_media::emoji::EmojiPickerMode::InsertIntoComposer {
            origin_chat: Some(chat.key),
        })
    );
}
