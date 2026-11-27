//! Milestone 7 UI workflow: `postbox://` deep-link routing into the right chat and view, the
//! non-modal dismissible "Chat not found" card, notification suppression and the update
//! command's state.

use postbox_core::control::{ControlRequest, ControlResponse, StatusReport, TelemetryCounters};
use postbox_core::toast::NotificationGate;
use postbox_core::update::UpdateManifest;
use postbox_core::{
    Account, Chat, ChatKey, FontScale, Message, Provider, ReadStatus, ThemeChoice,
    update::digest_hex,
};
use postbox_keymap::Keymap;
use postbox_platform::LinkOutcome;
use postbox_platform::{MockUrlHandler, Notifier};
use postbox_store::Store;
use postbox_ui::control::{LiveTelemetry, handle as handle_control};
use postbox_ui::{
    M7State, PaletteCommand, PaletteEntry, PaletteEntryKind, PostboxViewModel, StatusCard,
    UpdatePhase, deep_link_note_matches,
};

fn seed_store() -> Store {
    let store = Store::open_in_memory().unwrap();
    store
        .upsert_account(&Account {
            provider: Provider::Whatsapp,
            account_id: "biz".into(),
            label: "Business".into(),
            state: "connected".into(),
            sort_order: 1,
            last_event_at: None,
        })
        .unwrap();
    store
        .upsert_account(&Account {
            provider: Provider::Slack,
            account_id: "T01".into(),
            label: "Acme Slack".into(),
            state: "connected".into(),
            sort_order: 2,
            last_event_at: None,
        })
        .unwrap();
    store
        .upsert_chat(
            &Chat {
                key: ChatKey::new(Provider::Whatsapp, "biz", "491234@s.whatsapp.net"),
                title: "Ada".into(),
                is_group: false,
                archived_local: false,
                archived_remote: false,
                pinned: false,
                unread_count: 2,
                last_msg_ts: "2025-01-15T12:00:00Z".into(),
                last_msg_preview: "See you at 5".into(),
                avatar_hash: None,
            },
            1,
        )
        .unwrap();
    store
        .upsert_chat(
            &Chat {
                key: ChatKey::new(Provider::Slack, "T01", "C02"),
                title: "Engineering".into(),
                is_group: false,
                archived_local: false,
                archived_remote: false,
                pinned: false,
                unread_count: 0,
                last_msg_ts: "2025-01-15T11:00:00Z".into(),
                last_msg_preview: "standup in 5".into(),
                avatar_hash: None,
            },
            1,
        )
        .unwrap();
    store
}

fn message(chat: &ChatKey, msg_id: &str, body: &str, from_me: bool) -> Message {
    Message {
        chat: chat.clone(),
        msg_id: msg_id.into(),
        sender_id: "491234".into(),
        sender_name: "Ada".into(),
        from_me,
        ts: "2025-01-15T12:30:00Z".into(),
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

fn vm() -> PostboxViewModel {
    let mut vm = PostboxViewModel::new(seed_store(), Keymap::postbox_defaults(), "0.1.0");
    vm.initialize();
    vm
}

fn telemetry() -> LiveTelemetry {
    LiveTelemetry::new(
        TelemetryCounters {
            frames_rendered: 900,
            working_set_kb: 61 * 1024,
            private_usage_kb: 54 * 1024,
            gpu_texture_bytes: 8 * 1024 * 1024,
            pending_outbox: 2,
        },
        60.0,
    )
}

const WA_LINK: &str = "postbox://whatsapp/biz/491234%40s.whatsapp.net";

#[test]
fn a_deep_link_switches_view_and_selects_the_chat() {
    let mut vm = vm();
    // Start somewhere that is not the WhatsApp account view.
    assert!(vm.switch_to_view_key("slack:T01"));

    let action = vm.open_deep_link(WA_LINK, 1_000);
    assert!(matches!(action.outcome, LinkOutcome::Focused { .. }));
    assert!(action.raise_window);

    let selected = vm.selected_chat().expect("a chat is selected");
    assert_eq!(selected.key.provider, Provider::Whatsapp);
    assert_eq!(selected.key.account, "biz");
    assert_eq!(selected.key.id, "491234@s.whatsapp.net");
    // The view switched to the account that owns the chat.
    assert_eq!(vm.active_view_key(), "whatsapp:biz");
    assert!(vm.status_card().is_none());
}

#[test]
fn a_deep_link_to_a_missing_chat_shows_a_dismissible_card_and_changes_nothing() {
    let mut vm = vm();
    let before = vm.selected_chat().map(|chat| chat.key.clone());

    let action = vm.open_deep_link("postbox://telegram/main/nope", 2_000);
    assert!(
        matches!(action.outcome, LinkOutcome::MissingChat { .. }),
        "{action:?}"
    );
    assert_eq!(vm.selected_chat().map(|chat| chat.key.clone()), before);

    let card = vm.status_card().expect("the card is shown");
    assert_eq!(card.title, "Chat not found");
    assert_eq!(card.note, "Could not find that Telegram chat.");
    assert!(deep_link_note_matches(Provider::Telegram, &card.note));

    // Esc dismisses it, and the app keeps working.
    vm.handle_escape();
    assert!(vm.status_card().is_none());
    assert!(vm.open_deep_link(WA_LINK, 3_000).raise_window);
}

#[test]
fn a_deep_link_with_a_message_id_highlights_that_message() {
    let chat = ChatKey::new(Provider::Whatsapp, "biz", "491234@s.whatsapp.net");
    let mut store = seed_store();
    store
        .upsert_message(&message(&chat, "MSG-1", "first", false))
        .unwrap();
    store
        .upsert_message(&message(&chat, "MSG-2", "second", false))
        .unwrap();
    let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0");
    vm.initialize();

    let action = vm.open_deep_link(&format!("{WA_LINK}/MSG-2"), 4_000);
    assert!(matches!(action.outcome, LinkOutcome::Focused { .. }));
    assert_eq!(
        vm.selected_chat().map(|c| c.key.id.clone()).as_deref(),
        Some("491234@s.whatsapp.net")
    );
    // The linked message is on screen and marked, so the window can scroll to it.
    assert!(
        vm.messages().iter().any(|msg| msg.msg_id == "MSG-2"),
        "the linked message is loaded"
    );
}

#[test]
fn a_focus_signal_only_raises_the_window() {
    let mut vm = vm();
    let before = vm.selected_chat().map(|chat| chat.key.clone());
    let action = vm.open_deep_link("focus", 5_000);
    assert_eq!(action.outcome, LinkOutcome::FocusRaised);
    assert!(action.open.is_none());
    assert_eq!(vm.selected_chat().map(|chat| chat.key.clone()), before);
}

#[test]
fn the_card_expires_on_its_own_and_never_blocks_input() {
    let mut vm = vm();
    vm.open_deep_link("postbox://slack/T01/missing", 0);
    assert!(vm.status_card().is_some());
    // Typing still works while the card is up.
    vm.set_composer_text("still typing");
    assert_eq!(vm.composer_text(), "still typing");
    assert!(!vm.tick_status_card(postbox_ui::CARD_DISMISS_AFTER_MS - 1));
    assert!(vm.status_card().is_some());
    assert!(vm.tick_status_card(postbox_ui::CARD_DISMISS_AFTER_MS));
    assert!(vm.status_card().is_none());
}

#[test]
fn the_notification_gate_keeps_own_messages_and_the_open_chat_quiet() {
    let mut vm = vm();
    let chat = ChatKey::new(Provider::Whatsapp, "biz", "491234@s.whatsapp.net");
    assert!(vm.select_chat_key(&chat));
    vm.prime_notifications();

    let notifier = postbox_platform::MemoryPlatform::default();
    let own = message(&chat, "m1", "sent by me", true);
    assert!(!vm.notify_incoming(&own, 1_800_000_000, false, &notifier));
    assert_eq!(notifier.notification_count(), 0);

    // A message in the focused chat stays silent even though it is incoming.
    let incoming = message(&chat, "m2", "hi", false);
    assert!(!vm.notify_incoming(&incoming, 1_800_000_001, true, &notifier));
    assert_eq!(notifier.notification_count(), 0);

    // Another chat, window blurred: one toast, and only one for that message.
    let other = message(
        &ChatKey::new(Provider::Slack, "T01", "C02"),
        "m3",
        "standup in 5",
        false,
    );
    assert!(vm.notify_incoming(&other, 1_800_000_002, false, &notifier));
    assert_eq!(notifier.notification_count(), 1);
    assert!(!vm.notify_incoming(&other, 1_800_000_002, false, &notifier));
    assert_eq!(notifier.notification_count(), 1);
}

#[test]
fn the_update_command_reaches_the_palette_and_tracks_state() {
    let mut vm = vm();
    // The command is in the Ctrl+K catalog and no toolbar exists.
    vm.open_palette();
    let labels: Vec<String> = vm
        .palette_entries()
        .iter()
        .map(|entry| entry.label.clone())
        .collect();
    assert!(
        labels
            .iter()
            .any(|label| label.to_ascii_lowercase().contains("check for updates")),
        "{labels:?}"
    );

    // Executing it sets the request flag exactly once.
    vm.execute_palette_entry(&update_entry(&vm));
    assert!(vm.take_update_check_request());
    assert!(!vm.take_update_check_request());

    // A newer manifest moves the state to "available".
    let outcome = vm.check_for_updates(&manifest_for("v0.2.0"));
    assert!(matches!(
        outcome,
        postbox_core::update::CheckOutcome::Available { .. }
    ));
    // A newer release starts the background download; nothing blocks the UI.
    assert_eq!(vm.m7().update_phase, UpdatePhase::Downloading);
    assert_eq!(vm.update_command_label(), "Downloading the update...");
}

fn manifest_for(tag: &str) -> UpdateManifest {
    let body = format!(
        r#"{{"tag":"{tag}","sha256":"{}","asset":"postbox-windows-x64.zip","assets":[{{"name":"postbox-windows-x64.zip","browser_download_url":"https://example.invalid/a.zip","size":10}}]}}"#,
        digest_hex(b"payload")
    );
    UpdateManifest::parse(&body).expect("manifest parses")
}

fn update_entry(vm: &PostboxViewModel) -> PaletteEntry {
    vm.palette_entries()
        .iter()
        .find(|entry| {
            matches!(
                entry.kind,
                PaletteEntryKind::Command(PaletteCommand::CheckForUpdates)
            )
        })
        .cloned()
        .expect("the update entry is listed")
}

fn restart_entry(vm: &PostboxViewModel) -> Option<PaletteEntry> {
    vm.palette_entries()
        .iter()
        .find(|entry| {
            matches!(
                entry.kind,
                PaletteEntryKind::Command(PaletteCommand::RestartToUpdate)
            )
        })
        .cloned()
}

/// The `Ctrl+K` check entry: the running version plus the live updater state, in every phase.
#[test]
fn the_palette_check_entry_shows_the_version_and_every_update_phase() {
    let mut vm = vm();
    vm.open_palette();

    // Idle, before anything is checked: the running version alone.
    let idle = update_entry(&vm);
    assert_eq!(idle.label, "Check for updates");
    assert_eq!(idle.subtitle.as_deref(), Some("v0.1.0"));
    assert!(restart_entry(&vm).is_none(), "nothing staged yet");

    // Checking.
    assert!(vm.m7_mut().begin_check(true));
    vm.refresh_update_palette();
    let checking = update_entry(&vm);
    assert_eq!(checking.label, "Checking for updates...");
    assert_eq!(
        checking.subtitle.as_deref(),
        Some("v0.1.0 \u{00B7} Checking...")
    );

    // Downloading v0.2.0.
    let asset = manifest_for("v0.2.0")
        .select_asset()
        .expect("the release asset");
    assert!(vm.m7_mut().offer(asset, true));
    vm.refresh_update_palette();
    let downloading = update_entry(&vm);
    assert_eq!(downloading.label, "Downloading the update...");
    assert_eq!(
        downloading.subtitle.as_deref(),
        Some("v0.1.0 \u{00B7} Downloading v0.2.0...")
    );

    // Ready to install, and the second entry names the staged release.
    vm.m7_mut().mark_staged("v0.2.0", "- Notes", 1_000);
    vm.refresh_update_palette();
    let ready = update_entry(&vm);
    assert_eq!(ready.label, "Check for updates now");
    assert_eq!(
        ready.subtitle.as_deref(),
        Some("v0.1.0 \u{00B7} v0.2.0 ready to install")
    );
    let restart = restart_entry(&vm).expect("the restart entry is listed once an update is staged");
    assert_eq!(restart.label, "Restart to install Postbox v0.2.0");
    assert_eq!(restart.subtitle, None);
}

/// A manual check on the newest build leaves a notice the user can read and dismiss.
#[test]
fn a_manual_check_on_the_newest_build_says_up_to_date() {
    let mut vm = vm();
    vm.open_palette();
    let outcome = vm.check_for_updates(&manifest_for("v0.1.0"));
    assert!(matches!(
        outcome,
        postbox_core::update::CheckOutcome::UpToDate { .. }
    ));
    assert_eq!(vm.m7().update_phase, UpdatePhase::UpToDate);
    assert_eq!(
        vm.status_notice(),
        Some("Postbox v0.1.0 is up to date"),
        "the user sees feedback straight away"
    );
    let entry = update_entry(&vm);
    assert_eq!(entry.label, "Check for updates");
    assert_eq!(
        entry.subtitle.as_deref(),
        Some("v0.1.0 \u{00B7} Up to date")
    );
    assert!(restart_entry(&vm).is_none());

    // A failed manual check reads as a retry, and says the build is untouched.
    vm.m7_mut().mark_failed("digest mismatch", true);
    vm.refresh_update_palette();
    let failed = update_entry(&vm);
    assert_eq!(failed.label, "Retry the update check");
    assert_eq!(
        failed.subtitle.as_deref(),
        Some("v0.1.0 \u{00B7} Update check failed")
    );
}

#[test]
fn single_instance_and_registry_doubles_behave_like_windows() {
    let registry = MockUrlHandler::new();
    let instance = postbox_platform::MockSingleInstance::new();
    let pipe = postbox_platform::MockLinkPipe::new();
    let role = postbox_platform::startup(
        r"C:\Program Files\Postbox\postbox.exe",
        "valentin",
        Some(WA_LINK),
        &registry,
        &instance,
        &pipe,
    )
    .unwrap();
    assert!(matches!(
        role,
        postbox_platform::StartupRole::Primary { .. }
    ));
    assert!(registry.is_registered());
    assert!(pipe.sent().is_empty());
}

// ---------------------------------------------------------------------------
// postboxctl against the real view model
// ---------------------------------------------------------------------------

#[test]
fn postboxctl_status_reports_live_ram_fps_and_the_selected_chat() {
    let mut vm = vm();
    assert!(vm.select_chat_key(&ChatKey::new(Provider::Slack, "T01", "C02")));
    let response = handle_control(&mut vm, &ControlRequest::Status, &telemetry());
    let status = response.status.expect("a status payload");
    assert_eq!(status.working_set_kb, 62_464);
    assert_eq!(status.private_usage_kb, 55_296);
    assert_eq!(status.gpu_texture_bytes, 8 * 1024 * 1024);
    assert_eq!(status.selected_chat.as_deref(), Some("slack/T01/C02"));
    assert!((status.fps - 60.0).abs() < f64::EPSILON);
    assert_eq!(status.frames_rendered, 900);
}

#[test]
fn postboxctl_commands_change_the_view_model_over_the_json_protocol() {
    let mut vm = vm();
    let telemetry = telemetry();

    let theme = handle_control(
        &mut vm,
        &ControlRequest::Theme {
            theme: ThemeChoice::Amoled,
        },
        &telemetry,
    );
    assert!(theme.ok);
    assert_eq!(vm.theme(), ThemeChoice::Amoled);

    let font = handle_control(&mut vm, &ControlRequest::Font { percent: 140 }, &telemetry);
    assert!(font.ok);
    assert_eq!(vm.font_scale(), FontScale::clamped(140));

    let view = handle_control(
        &mut vm,
        &ControlRequest::View {
            target: "whatsapp:biz".into(),
        },
        &telemetry,
    );
    assert!(view.ok, "{view:?}");
    assert_eq!(vm.active_view_key(), "whatsapp:biz");

    let select = handle_control(
        &mut vm,
        &ControlRequest::Select {
            id: "491234@s.whatsapp.net".into(),
            provider: Some(Provider::Whatsapp),
            account: None,
        },
        &telemetry,
    );
    assert!(select.ok, "{select:?}");

    let archive = handle_control(
        &mut vm,
        &ControlRequest::Archive { id: "C02".into() },
        &telemetry,
    );
    assert!(archive.ok, "{archive:?}");
    assert!(
        vm.store()
            .get_chat(&ChatKey::new(Provider::Slack, "T01", "C02"))
            .unwrap()
            .unwrap()
            .is_archived()
    );

    // Every answer is one JSON line that the CLI can decode.
    for request in [
        ControlRequest::Status,
        ControlRequest::Chats {
            provider: None,
            archived: false,
            all: true,
            limit: Some(5),
        },
        ControlRequest::Search {
            text: "standup".into(),
        },
        ControlRequest::Messages {
            id: Some("C02".into()),
            limit: Some(5),
        },
    ] {
        let response = handle_control(&mut vm, &request, &telemetry);
        let line = postbox_core::control::encode_response(&response);
        assert!(
            !line.trim_end_matches(['\r', '\n']).contains('\n'),
            "{}",
            request.command_name()
        );
        assert!(postbox_core::control::decode_response(&line).is_ok());
    }
}

#[test]
fn a_whole_request_line_written_by_the_cli_drives_the_view_model() {
    // Exactly what `postboxctl font 130` sends on the wire.
    let line = postbox_core::control::encode_request(&ControlRequest::Font { percent: 130 });
    let request = postbox_core::control::decode_request(&line).unwrap();
    let mut vm = vm();
    let response = handle_control(&mut vm, &request, &telemetry());
    assert!(response.ok);
    assert_eq!(vm.font_scale().percent(), 130);

    // And the response line the app sends back decodes on the CLI side.
    let reply = postbox_core::control::encode_response(&response);
    let parsed = postbox_core::control::decode_response(&reply).unwrap();
    assert_eq!(parsed.message.as_deref(), Some("Font scale set to 130%"));
}

#[test]
fn a_staged_update_can_be_restarted_from_ctrl_k_and_esc_does_not_break_modals() {
    let mut vm = vm();
    let asset = {
        let body = format!(
            r#"{{"tag":"v0.2.0","sha256":"{}","asset":"postbox-windows-x64.zip","assets":[{{"name":"postbox-windows-x64.zip","browser_download_url":"https://example.invalid/a.zip","size":10}}]}}"#,
            digest_hex(b"payload")
        );
        UpdateManifest::parse(&body)
            .unwrap()
            .select_asset()
            .unwrap()
    };
    assert!(vm.m7_mut().offer(asset, false));
    vm.m7_mut().mark_staged("v0.2.0", "- Notes", 1_000);
    assert_eq!(vm.update_command_label(), "Check for updates now");
    let prompt = vm.update_prompt().expect("the card is offered");
    assert_eq!(prompt.headline(), "Postbox v0.2.0 is ready");

    // `Esc` with a modal open closes the modal and leaves the restart card alone.
    vm.open_palette();
    assert!(vm.palette_open());
    vm.handle_escape();
    assert!(!vm.palette_open());
    assert!(
        vm.update_prompt().is_some(),
        "the non-modal card survives a modal's Esc"
    );

    // `Esc` with nothing modal hides the card for this session.
    vm.handle_escape();
    assert!(vm.update_prompt().is_none());
    assert_eq!(vm.m7().staged_tag(), Some("v0.2.0"));

    // The `Ctrl+K` restart entry asks the shell for the swap exactly once.
    vm.open_palette();
    let restart = vm
        .palette_entries()
        .iter()
        .find(|entry| entry.label == "Restart to install Postbox v0.2.0")
        .cloned()
        .expect("the restart entry is listed");
    vm.execute_palette_entry(&restart);
    assert!(vm.take_update_restart_request());
    assert!(!vm.take_update_restart_request());
}

#[test]
fn a_bad_manifest_offer_never_reaches_the_restart_card() {
    let mut vm = vm();
    // A manifest with a short digest selects no asset, so nothing can be staged.
    let body = r#"{"tag":"v0.2.0","sha256":"abc","assets":[{"name":"postbox-windows-x64.zip","browser_download_url":"https://example.invalid/a.zip","size":1}]}"#;
    let manifest = UpdateManifest::parse(body).unwrap();
    let outcome = vm.check_for_updates(&manifest);
    assert!(matches!(
        outcome,
        postbox_core::update::CheckOutcome::UpToDate { .. }
    ));
    assert!(vm.update_prompt().is_none());
    assert_eq!(vm.m7().staged_tag(), None);
}

#[test]
fn the_status_card_and_update_state_start_empty() {
    let state = M7State::new();
    assert!(state.card.is_none());
    assert_eq!(state.update_phase, UpdatePhase::Idle);
    assert_eq!(state.update_label(), "Check for updates");
    assert_eq!(
        state.update_hint("0.1.0"),
        "v0.1.0",
        "no status before a check"
    );
    let card = StatusCard::new("x", "t", "n", 0);
    assert!(!card.is_missing_chat());
    assert!(
        NotificationGate::new()
            .high_water(&ChatKey::new(Provider::Signal, "a", "b"))
            .is_none()
    );
    assert!(
        StatusReport::from_counters(&TelemetryCounters::default(), 0.0, "0.1.0", "all", None).fps
            == 0.0
    );
    let _ = ControlResponse::ok();
    let _ = MockUrlHandler::new();
    let _: Option<&dyn Notifier> = None;
}
