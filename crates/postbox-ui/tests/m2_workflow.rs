use postbox_core::archive::{ArchiveStatus, IncomingProbeMessage};
use postbox_core::threads::local_thread_root;
use postbox_core::{Account, Chat, ChatKey, Message, Provider, ReadStatus, ViewTab};
use postbox_keymap::Keymap;
use postbox_store::Store;
use postbox_ui::{
    HubConnectionStatus, PostboxViewModel, ReadTickDisplay, SyncCommand, SyncEvent, TimelineItem,
};

fn mk_chat(
    provider: Provider,
    account: &str,
    id: &str,
    title: &str,
    ts: &str,
    unread: u32,
) -> Chat {
    Chat {
        key: ChatKey::new(provider, account, id),
        title: title.into(),
        is_group: provider == Provider::Slack,
        archived_local: false,
        archived_remote: false,
        pinned: false,
        unread_count: unread,
        last_msg_ts: ts.into(),
        last_msg_preview: "preview".into(),
        avatar_hash: None,
    }
}

fn mk_msg(chat: &ChatKey, id: &str, from_me: bool, ts: &str, body: &str) -> Message {
    Message {
        chat: chat.clone(),
        msg_id: id.into(),
        sender_id: if from_me { "me".into() } else { "other".into() },
        sender_name: if from_me {
            "You".into()
        } else {
            "Other".into()
        },
        from_me,
        ts: ts.into(),
        body: body.into(),
        reply_to_id: None,
        reply_broadcast: false,
        media_kind: None,
        mime_type: None,
        filename: None,
        reactions: vec![],
        read_status: if from_me {
            ReadStatus::Read
        } else {
            ReadStatus::None
        },
        deleted: false,
    }
}

#[test]
fn combined_inbox_vs_account_views_saved_order_preserves_absent_slots() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .save_view_order(&[
            "whatsapp:personal".into(),
            "slack:T_DRAINED".into(),
            "telegram:telegram".into(),
        ])
        .unwrap();
    store
        .upsert_account(&Account {
            provider: Provider::Whatsapp,
            account_id: "personal".into(),
            label: "WhatsApp personal".into(),
            state: "connected".into(),
            sort_order: 0,
            last_event_at: None,
        })
        .unwrap();
    store
        .upsert_account(&Account {
            provider: Provider::Telegram,
            account_id: "telegram".into(),
            label: "Telegram".into(),
            state: "connected".into(),
            sort_order: 1,
            last_event_at: None,
        })
        .unwrap();

    let mut wa_old_pinned = mk_chat(
        Provider::Whatsapp,
        "personal",
        "1@s.whatsapp.net",
        "Pinned Old",
        "2026-11-26T08:00:00Z",
        0,
    );
    wa_old_pinned.pinned = true;
    let wa_new = mk_chat(
        Provider::Whatsapp,
        "personal",
        "2@s.whatsapp.net",
        "Unpinned New",
        "2026-11-26T12:00:00Z",
        0,
    );
    store.upsert_chat(&wa_old_pinned, 100).unwrap();
    store.upsert_chat(&wa_new, 100).unwrap();

    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0");
    // Absent Slack slot MUST stay anchored at Alt+3 so Telegram stays at Alt+4!
    assert_eq!(vm.accounts().len(), 3);
    assert_eq!(vm.accounts()[0].view_key(), "whatsapp:personal");
    assert_eq!(vm.accounts()[1].view_key(), "slack:T_DRAINED");
    assert_eq!(vm.accounts()[2].view_key(), "telegram:telegram");

    // Combined Inbox ignores pins -> Unpinned New first.
    assert_eq!(vm.chats()[0].title, "Unpinned New");
    assert_eq!(vm.chats()[1].title, "Pinned Old");

    // Account view (Alt+2) sorts pinned first!
    assert!(vm.switch_to_account_slot(2));
    assert_eq!(vm.chats()[0].title, "Pinned Old");
    assert_eq!(vm.chats()[1].title, "Unpinned New");

    // Switching to absent Alt+3 opens its saved view cleanly without shifting Alt+4!
    assert!(vm.switch_to_account_slot(3));
    assert_eq!(vm.accounts()[1].state, "offline");
    assert!(vm.chats().is_empty());
    assert!(vm.switch_to_account_slot(4));
    assert_eq!(vm.active_view().unwrap().view_key(), "telegram:telegram");
}

#[test]
fn igsh_902_wa_resurface_probe_ignores_own_and_reactions_and_respects_keep() {
    let store = Store::open_in_memory().unwrap();
    let wa = mk_chat(
        Provider::Whatsapp,
        "personal",
        "wa1@s.whatsapp.net",
        "WA Normal",
        "2026-11-26T10:00:00Z",
        0,
    );
    let wa_kept = mk_chat(
        Provider::Whatsapp,
        "personal",
        "wa2@s.whatsapp.net",
        "WA Kept",
        "2026-11-26T10:00:00Z",
        0,
    );
    store.upsert_chat(&wa, 100).unwrap();
    store.upsert_chat(&wa_kept, 100).unwrap();

    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0");
    vm.select_chat_key(&wa.key);
    vm.archive_selected();
    vm.select_chat_key(&wa_kept.key);
    vm.keep_archived_selected();

    let mut tbl = vm.store().load_archive_table().unwrap();
    assert_eq!(tbl.status(&wa.key, true), ArchiveStatus::Archived);
    assert_eq!(tbl.status(&wa_kept.key, true), ArchiveStatus::KeptArchived);
    // Normalize baseline to known timestamp for deterministic probe verification.
    tbl.set_baseline(&wa.key, "2026-11-26T10:00:00Z");
    tbl.set_baseline(&wa_kept.key, "2026-11-26T10:00:00Z");

    // Own messages and reactions do NOT resurface.
    assert!(!tbl.apply_incoming_probe(
        &wa.key,
        "2026-11-26T10:05:00Z",
        &[
            IncomingProbeMessage {
                from_me: true,
                is_reaction: false,
                ts: "2026-11-26T10:05:00Z"
            },
            IncomingProbeMessage {
                from_me: false,
                is_reaction: true,
                ts: "2026-11-26T10:04:00Z"
            },
        ]
    ));
    // Real incoming message resurfaces normal archived chat, NOT kept chat.
    let real_incoming = [IncomingProbeMessage {
        from_me: false,
        is_reaction: false,
        ts: "2026-11-26T10:10:00Z",
    }];
    assert!(tbl.apply_incoming_probe(&wa.key, "2026-11-26T10:10:00Z", &real_incoming));
    assert!(!tbl.is_archived(&wa.key, true));
    assert!(!tbl.apply_incoming_probe(&wa_kept.key, "2026-11-26T10:10:00Z", &real_incoming));
    assert!(tbl.is_archived(&wa_kept.key, true));
}

#[test]
fn archive_opposing_intent_and_bulk_archive_ctrl_a_ctrl_e() {
    let store = Store::open_in_memory().unwrap();
    let c1 = mk_chat(
        Provider::Whatsapp,
        "personal",
        "1@s.whatsapp.net",
        "Chat 1",
        "2026-11-26T12:00:00Z",
        0,
    );
    let c2 = mk_chat(
        Provider::Whatsapp,
        "personal",
        "2@s.whatsapp.net",
        "Chat 2",
        "2026-11-26T11:00:00Z",
        0,
    );
    store.upsert_chat(&c1, 100).unwrap();
    store.upsert_chat(&c2, 100).unwrap();

    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0");
    vm.select_all_chats();
    assert_eq!(vm.bulk_selected_count(), 2);
    assert!(
        vm.status_notice()
            .unwrap()
            .contains("Ctrl+E archives, Esc cancels")
    );
    vm.archive_selected();
    assert_eq!(vm.bulk_selected_count(), 0);
    assert!(vm.chats().is_empty());

    let cmds = vm.take_emitted_sync_commands();
    assert!(cmds.contains(&SyncCommand::WakeArchive));
    let pending = vm.store().list_ready_pending_archives(i64::MAX).unwrap();
    assert_eq!(pending.len(), 2);
}

#[test]
fn slack_folded_threads_32_hop_root_panel_and_broadcast_auto_reset() {
    let mut store = Store::open_in_memory().unwrap();
    let chat = mk_chat(
        Provider::Slack,
        "T1",
        "slack:T1:C1",
        "#eng",
        "2026-11-26T12:00:00Z",
        0,
    );
    store.upsert_chat(&chat, 100).unwrap();

    let root = mk_msg(&chat.key, "1700.1", false, "2026-11-26T10:00:00Z", "Root");
    let mut r1 = mk_msg(
        &chat.key,
        "1700.2",
        false,
        "2026-11-26T10:01:00Z",
        "Reply 1",
    );
    r1.reply_to_id = Some("1700.1".into());
    let mut r2 = mk_msg(&chat.key, "1700.3", false, "2026-11-26T10:02:00Z", "Nested");
    r2.reply_to_id = Some("1700.2".into());
    store.upsert_messages_batch(&[root, r1, r2]).unwrap();

    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0");
    // Root resolution traces multi-hop chain back to 1700.1.
    assert_eq!(
        local_thread_root(vm.messages(), &vm.messages()[2], Provider::Slack),
        Some("1700.1")
    );
    // Main timeline folds r1 & r2 away; root row shows thread_reply_count == 2.
    let msg_rows: Vec<_> = vm
        .timeline_items()
        .iter()
        .filter_map(|it| match it {
            TimelineItem::MessageRow {
                message,
                thread_reply_count,
                ..
            } => Some((message.msg_id.as_str(), *thread_reply_count)),
            _ => None,
        })
        .collect();
    assert_eq!(msg_rows, vec![("1700.1", 2)]);

    // Open thread panel, toggle broadcast, submit -> reply_broadcast=true in outbox & auto-reset!
    assert!(vm.open_thread("1700.1"));
    vm.set_thread_broadcast(true);
    assert!(vm.thread_panel().unwrap().broadcast);
    vm.set_thread_composer_text("Also visible in channel");
    assert!(vm.submit_thread_reply());
    assert!(!vm.thread_panel().unwrap().broadcast); // IGSH-1898 auto-reset!

    let outbox = vm.store().list_outbox().unwrap();
    assert_eq!(outbox.len(), 1);
    assert!(outbox[0].reply_broadcast);
    assert_eq!(outbox[0].reply_to_id.as_deref(), Some("1700.1"));
}

#[test]
fn unread_filter_user_viewed_mark_read_pins_16_cap_drafts_origin_and_read_ticks() {
    let mut store = Store::open_in_memory().unwrap();
    let wa = mk_chat(
        Provider::Whatsapp,
        "personal",
        "wa1@s.whatsapp.net",
        "Alice",
        "2026-11-26T12:00:00Z",
        3,
    );
    let wa2 = mk_chat(
        Provider::Whatsapp,
        "personal",
        "wa2@s.whatsapp.net",
        "Bob",
        "2026-11-26T11:00:00Z",
        0,
    );
    store.upsert_chat(&wa, 100).unwrap();
    store.upsert_chat(&wa2, 100).unwrap();
    store
        .upsert_message(&mk_msg(&wa.key, "m1", true, "2026-11-26T12:00:00Z", "Hi"))
        .unwrap();

    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0");
    // Startup selection MUST NOT mark Alice read (`user_viewed == false`)!
    assert!(!vm.user_viewed());
    assert_eq!(vm.chats()[0].unread_count, 3);
    assert!(vm.pending_reads().is_empty());

    // Read ticks shown for WA DM own read message.
    let ticks: Vec<ReadTickDisplay> = vm
        .timeline_items()
        .iter()
        .filter_map(|it| match it {
            TimelineItem::MessageRow { read_ticks, .. } => Some(*read_ticks),
            _ => None,
        })
        .collect();
    assert_eq!(ticks, vec![ReadTickDisplay::DoubleRead]);

    // Explicit user view marks Alice read and enqueues pending mark-read.
    assert!(vm.select_chat_index(0));
    assert!(vm.user_viewed());
    assert_eq!(vm.chats()[0].unread_count, 0);
    assert_eq!(vm.pending_reads().len(), 1);
    assert!(
        vm.take_emitted_sync_commands()
            .contains(&SyncCommand::WakeMarkRead)
    );

    // Whitespace draft preserved + origin-scoped append does NOT leak into switched chat.
    vm.set_composer_text("   ");
    assert_eq!(vm.draft_for_chat(&wa.key).unwrap().text_body, "   ");
    vm.select_chat_index(1); // switch to Bob
    assert!(!vm.append_to_chat_draft(&wa.key, "dictated text"));
    assert_eq!(vm.composer_text(), "");
    assert_eq!(
        vm.draft_for_chat(&wa.key).unwrap().text_body,
        "    dictated text"
    );

    // Pin limit 16 enforced cleanly.
    for i in 0..16 {
        let k = ChatKey::new(
            Provider::Whatsapp,
            "personal",
            format!("p{i}@s.whatsapp.net"),
        );
        vm.store_mut().pin_chat(&k, i).unwrap();
    }
    vm.toggle_pin_selected(); // 17th pin rejected!
    assert_eq!(vm.status_notice(), Some("Pin limit reached (16 chats)"));
}

#[test]
fn hub_status_transitions_and_palette_refresh_restart_sync() {
    let store = Store::open_in_memory().unwrap();
    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0");
    assert!(matches!(
        vm.hub_status(),
        HubConnectionStatus::NotConfigured { .. }
    ));

    vm.apply_sync_event(SyncEvent::StatusChanged(
        HubConnectionStatus::Reconnecting {
            url: "https://hub.example".into(),
            attempt: 2,
            retry_in_secs: 4,
            last_error: "eof".into(),
        },
    ));
    assert!(vm.window_title().contains("Reconnecting to Hub"));

    vm.open_palette();
    assert!(
        vm.palette_entries()
            .iter()
            .any(|e| e.label.contains("Hub: Reconnecting (retry in 4s)"))
    );
    vm.set_palette_query("Restart live sync");
    assert!(vm.execute_palette_selected());
    assert!(
        vm.take_emitted_sync_commands()
            .contains(&SyncCommand::RestartLiveSync)
    );

    vm.apply_sync_event(SyncEvent::StatusChanged(HubConnectionStatus::Connected {
        url: "https://hub.example".into(),
        last_event_id: Some(42),
    }));
    assert_eq!(vm.window_title(), "Combined inbox - Postbox v0.1.0");
    assert_eq!(vm.active_view(), Some(&ViewTab::CombinedInbox));
}

#[test]
fn older_history_pagination_grows_past_200_messages_and_resets_on_chat_switch() {
    let mut store = Store::open_in_memory().unwrap();
    let chat1 = mk_chat(
        Provider::Whatsapp,
        "personal",
        "c1@s.whatsapp.net",
        "Long Chat",
        "2026-11-26T20:00:00Z",
        0,
    );
    let chat2 = mk_chat(
        Provider::Whatsapp,
        "personal",
        "c2@s.whatsapp.net",
        "Other Chat",
        "2026-11-26T19:00:00Z",
        0,
    );
    store.upsert_chat(&chat1, 100).unwrap();
    store.upsert_chat(&chat2, 90).unwrap();

    // Seed 200 newest messages (`m0100..m0299`).
    let mut initial = Vec::new();
    for i in 100..300 {
        let ts = format!("2026-11-26T{:02}:{:02}:{:02}Z", 10 + (i / 60), i % 60, 0);
        initial.push(mk_msg(
            &chat1.key,
            &format!("m{i:04}"),
            false,
            &ts,
            &format!("msg {i}"),
        ));
    }
    store.upsert_messages_batch(&initial).unwrap();

    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0");
    assert_eq!(vm.messages().len(), 200);
    assert_eq!(vm.messages()[0].msg_id, "m0100");
    assert!(vm.request_older_messages());

    // Simulate sync engine fetching 80 older messages (`m0020..m0099`) and pushing MessagesUpdated.
    let mut older = Vec::new();
    for i in 20..100 {
        let ts = format!("2026-11-26T{:02}:{:02}:{:02}Z", 10 + (i / 60), i % 60, 0);
        older.push(mk_msg(
            &chat1.key,
            &format!("m{i:04}"),
            false,
            &ts,
            &format!("msg {i}"),
        ));
    }
    vm.store_mut().upsert_messages_batch(&older).unwrap();
    let prepended_items = vm.apply_sync_event(SyncEvent::MessagesUpdated {
        chat: chat1.key.clone(),
        prepended: 80,
        exhausted: false,
    });
    assert!(prepended_items >= 80);
    assert_eq!(vm.messages().len(), 280);
    assert_eq!(vm.messages()[0].msg_id, "m0020");

    // Switching to another chat resets window back to 200.
    assert!(vm.select_chat_key(&chat2.key));
    assert!(vm.select_chat_key(&chat1.key));
    assert_eq!(vm.messages().len(), 200);
}

#[test]
fn chat_archive_status_respects_returned_and_local_unarchived_overrides() {
    let mut store = Store::open_in_memory().unwrap();
    let mut wa = mk_chat(
        Provider::Whatsapp,
        "personal",
        "wa_ret@s.whatsapp.net",
        "WA Returned",
        "2026-11-26T10:10:00Z",
        0,
    );
    wa.archived_remote = true;
    let mut slack = mk_chat(
        Provider::Slack,
        "T1",
        "slack:T1:C_UNARCH",
        "#unarchived",
        "2026-11-26T10:00:00Z",
        0,
    );
    slack.archived_remote = true;
    store.upsert_chat(&wa, 100).unwrap();
    store.upsert_chat(&slack, 100).unwrap();

    let mut tbl = postbox_core::archive::ArchiveTable::new();
    tbl.archive(&wa.key, "2026-11-26T10:00:00Z", false);
    assert!(tbl.apply_incoming_probe(
        &wa.key,
        "2026-11-26T10:10:00Z",
        &[IncomingProbeMessage {
            from_me: false,
            is_reaction: false,
            ts: "2026-11-26T10:10:00Z",
        }]
    ));
    tbl.set_local_unarchived(&slack.key);
    store.save_archive_table(&tbl).unwrap();

    let vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0");
    assert_eq!(vm.chat_archive_status(&wa), ArchiveStatus::Incoming);
    assert_eq!(vm.chat_archive_status(&slack), ArchiveStatus::Incoming);
}

#[test]
fn refresh_chats_preserves_reply_draft_and_staged_image_and_select_chat_key_exits_combined_alt_a() {
    let store = Store::open_in_memory().unwrap();
    let c1 = mk_chat(
        Provider::Whatsapp,
        "personal",
        "1@s.whatsapp.net",
        "Read Chat",
        "2026-11-26T12:00:00Z",
        0,
    );
    let c2 = mk_chat(
        Provider::Whatsapp,
        "personal",
        "2@s.whatsapp.net",
        "Unread Chat",
        "2026-11-26T11:00:00Z",
        2,
    );
    let c3 = mk_chat(
        Provider::Whatsapp,
        "personal",
        "3@s.whatsapp.net",
        "Second Read Chat",
        "2026-11-26T10:00:00Z",
        0,
    );
    store.upsert_chat(&c1, 100).unwrap();
    store.upsert_chat(&c2, 100).unwrap();
    store.upsert_chat(&c3, 100).unwrap();

    // Save a draft on c2 with BOTH reply_to_id and staged_image_path.
    store
        .save_draft(&postbox_core::Draft {
            chat: c2.key.clone(),
            text_body: "Reply draft".into(),
            reply_to_id: Some("orig_msg_1".into()),
            staged_image_path: Some("/tmp/img.png".into()),
            updated_at: 100,
        })
        .unwrap();

    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0");
    assert_eq!(vm.selected_chat().unwrap().key, c1.key);

    // Archiving c1 triggers `refresh_chats()` -> chat changes to c2 -> MUST restore reply_to_msg_id!
    vm.archive_selected();
    assert_eq!(vm.selected_chat().unwrap().key, c2.key);
    assert_eq!(vm.composer_text(), "Reply draft");
    assert_eq!(vm.reply_to_msg_id(), Some("orig_msg_1"));

    // Updating composer text preserves existing `staged_image_path` on c2!
    vm.set_composer_text("Reply draft edited");
    assert_eq!(
        vm.draft_for_chat(&c2.key)
            .and_then(|d| d.staged_image_path.as_deref()),
        Some("/tmp/img.png")
    );

    // Enable Combined Inbox Alt+A (unread-only!), then jump to read chat c3 via select_chat_key!
    vm.toggle_archived_inline();
    assert!(vm.show_archived_inline());
    // Under Combined Inbox Alt+A, c3 (read) is hidden; select_chat_key MUST still locate & select it!
    assert!(vm.select_chat_key(&c3.key));
    assert_eq!(vm.selected_chat().unwrap().key, c3.key);
    assert!(!vm.show_archived_inline());
}
