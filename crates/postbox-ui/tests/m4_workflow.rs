//! Headless Milestone 4 workflow tests covering Features #27, #28, and #29:
//! - Voice playback (`Ctrl+P`), speed cycling/persistence, played set, resume positions (500 ms clear), auto-advance (cached & download-triggered)
//! - Voice recording (`Ctrl+Shift+M`), `Enter` finish -> durable outbox (`ptt: true`, `voice.ogg`), queue-full retention, `Esc`/chat-switch cancel, `ThreadComposer`/inactive-window block, `voice_sent` echo matching
//! - Deepgram cached transcription (immutable SQLite + `.raw.txt`/`.txt` never re-requested), 1 retry on next chat open, `Ctrl+T` toggle, `Copy transcript` & `Copy all transcripts` (raw oldest-first `\r\n\r\n`, legacy-formatted guard, missing summary), dynamic microphones in `Ctrl+K`

use postbox_audio::{encode_voice_note, transcript_txt_path, write_transcript_sidecars};
use postbox_core::outbox::{MAX_PENDING_SENDS, OutboxKind, OutboxRecord};
use postbox_core::voice::{
    AudioDeviceInfo, PlaybackSpeed, TranscriptError, TranscriptStatus, VoicePlaybackStatus,
    VoiceRecordState,
};
use postbox_core::{Account, Chat, ChatKey, MediaKind, Message, Provider, ReadStatus};
use postbox_keymap::{Action, Keymap};
use postbox_store::Store;
use postbox_ui::{FocusTarget, PaletteCommand, PostboxViewModel, SyncCommand, SyncEvent};
use std::path::PathBuf;

fn sample_ogg_bytes() -> Vec<u8> {
    let pcm: Vec<i16> = (0..24_000)
        .map(|i| {
            let s = (2.0 * std::f32::consts::PI * 440.0 * (i as f32) / 48_000.0).sin();
            (s * 12_000.0) as i16
        })
        .collect();
    encode_voice_note(&pcm).unwrap().ogg_bytes
}

fn seeded_voice_vm(dir: &PathBuf) -> (PostboxViewModel, ChatKey, ChatKey) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();

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

    let chat1 = ChatKey::new(Provider::Whatsapp, "main", "alice@s.whatsapp.net");
    let chat2 = ChatKey::new(Provider::Whatsapp, "main", "bob@s.whatsapp.net");

    store
        .upsert_chat(
            &Chat {
                key: chat1.clone(),
                title: "Alice".into(),
                is_group: false,
                archived_local: false,
                archived_remote: false,
                pinned: false,
                unread_count: 0,
                last_msg_ts: "2026-11-27T10:05:00Z".into(),
                last_msg_preview: "Voice message".into(),
                avatar_hash: None,
            },
            1_764_237_900,
        )
        .unwrap();
    store
        .upsert_chat(
            &Chat {
                key: chat2.clone(),
                title: "Bob".into(),
                is_group: false,
                archived_local: false,
                archived_remote: false,
                pinned: false,
                unread_count: 0,
                last_msg_ts: "2026-11-27T09:00:00Z".into(),
                last_msg_preview: "Hi".into(),
                avatar_hash: None,
            },
            1_764_234_000,
        )
        .unwrap();

    for (id, ts) in [
        ("v1", "2026-11-27T10:01:00Z"),
        ("v2", "2026-11-27T10:02:00Z"),
        ("v3", "2026-11-27T10:03:00Z"),
    ] {
        store
            .upsert_message(&Message {
                chat: chat1.clone(),
                msg_id: id.into(),
                sender_id: "alice".into(),
                sender_name: "Alice".into(),
                from_me: false,
                ts: ts.into(),
                body: String::new(),
                reply_to_id: None,
                reply_broadcast: false,
                media_kind: Some(MediaKind::Voice),
                mime_type: Some("audio/ogg".into()),
                filename: Some(format!("{id}.ogg")),
                file_size: Some(2048),
                reactions: vec![],
                read_status: ReadStatus::None,
                deleted: false,
            })
            .unwrap();
    }

    let mut vm = PostboxViewModel::new_uninitialized(store, Keymap::postbox_defaults(), "0.1.0")
        .with_data_dir(dir.clone())
        .with_deepgram_key_available(true);
    vm.initialize();
    let _ = vm.take_emitted_sync_commands();
    (vm, chat1, chat2)
}

#[test]
fn voice_playback_ctrl_p_speed_resume_and_auto_advance_including_download_wait() {
    let dir = std::env::temp_dir().join(format!("postbox_m4_play_{}", std::process::id()));
    let (mut vm, chat1, _) = seeded_voice_vm(&dir);
    let ogg = sample_ogg_bytes();

    let v1_path = dir.join("v1.ogg");
    let v2_path = dir.join("v2.ogg");
    std::fs::write(&v1_path, &ogg).unwrap();
    std::fs::write(&v2_path, &ogg).unwrap();

    // v1 downloaded; v2 NOT downloaded yet (will test download-triggered auto-advance!).
    vm.apply_sync_event(SyncEvent::MediaDownloaded {
        chat: chat1.clone(),
        msg_id: "v1".into(),
        path: v1_path.clone(),
        mime_type: "audio/ogg".into(),
        sha256: "sha-v1".into(),
    });
    let _ = vm.take_emitted_sync_commands();

    // Select v1 and press Ctrl+P.
    vm.select_message_index(0);
    vm.handle_action(Action::PlayPauseVoice, 100);
    let pb = vm.voice_playback().unwrap();
    assert_eq!(pb.msg_id, "v1");
    assert_eq!(pb.status, VoicePlaybackStatus::Playing);
    assert!(vm.is_voice_played(&chat1, "v1"));
    assert!(
        vm.take_emitted_sync_commands()
            .iter()
            .any(|c| matches!(c, SyncCommand::VoicePlay { msg_id, .. } if msg_id == "v1"))
    );

    // Cycle speed 1x -> 1.5x -> 2x -> 1x and verify persistence.
    vm.cycle_playback_speed();
    assert_eq!(vm.playback_speed(), PlaybackSpeed::X1_5);
    assert_eq!(
        vm.store().load_playback_speed().unwrap(),
        PlaybackSpeed::X1_5
    );
    vm.cycle_playback_speed();
    assert_eq!(vm.playback_speed(), PlaybackSpeed::X2);
    vm.cycle_playback_speed();
    assert_eq!(vm.playback_speed(), PlaybackSpeed::X1);

    // Progress at 2000 / 5000 ms, then pause -> saves resume position.
    vm.apply_sync_event(SyncEvent::VoicePlaybackProgress {
        chat: chat1.clone(),
        msg_id: "v1".into(),
        pos_ms: 2_000,
        dur_ms: 5_000,
        waveform: vec![50; 64],
        playing: true,
    });
    vm.handle_action(Action::PlayPauseVoice, 200);
    assert_eq!(
        vm.voice_playback().unwrap().status,
        VoicePlaybackStatus::Paused
    );
    assert_eq!(vm.voice_resume_position(&chat1, "v1"), Some((2_000, 5_000)));
    assert_eq!(
        vm.store().get_voice_position(&chat1, "v1").unwrap(),
        Some((2_000, 5_000))
    );

    // Resume then end -> clears resume position AND auto-advances to v2 (requesting download).
    vm.handle_action(Action::PlayPauseVoice, 300);
    let _ = vm.take_emitted_sync_commands();
    vm.apply_sync_event(SyncEvent::VoicePlaybackEnded {
        chat: chat1.clone(),
        msg_id: "v1".into(),
    });
    assert!(vm.voice_resume_position(&chat1, "v1").is_none());
    assert!(
        vm.store()
            .get_voice_position(&chat1, "v1")
            .unwrap()
            .is_none()
    );

    // Now v2 finishes downloading -> immediately starts playback via pending_auto_advance!
    vm.apply_sync_event(SyncEvent::MediaDownloaded {
        chat: chat1.clone(),
        msg_id: "v2".into(),
        path: v2_path,
        mime_type: "audio/ogg".into(),
        sha256: "sha-v2".into(),
    });
    let pb2 = vm.voice_playback().unwrap();
    assert_eq!(pb2.msg_id, "v2");
    assert_eq!(pb2.status, VoicePlaybackStatus::Playing);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn voice_recording_lifecycle_outbox_retention_chat_switch_and_guards() {
    let dir = std::env::temp_dir().join(format!("postbox_m4_rec_{}", std::process::id()));
    let (mut vm, chat1, _chat2) = seeded_voice_vm(&dir);
    let ogg = sample_ogg_bytes();

    // Blocked when window is inactive or ThreadComposer is focused.
    assert!(!vm.toggle_voice_recording(false));
    assert_eq!(*vm.voice_record_state(), VoiceRecordState::Idle);
    vm.set_focus(FocusTarget::ThreadComposer);
    assert!(!vm.toggle_voice_recording(true));
    assert_eq!(*vm.voice_record_state(), VoiceRecordState::Idle);

    // Start recording normally.
    vm.set_focus(FocusTarget::Composer);
    assert!(vm.toggle_voice_recording(true));
    assert!(vm.is_voice_recording());
    assert!(vm.take_emitted_sync_commands().iter().any(
        |c| matches!(c, SyncCommand::VoiceRecordStart { origin_chat, .. } if origin_chat == &chat1)
    ));

    // Chat switch while recording cancels cleanly with exact notice.
    vm.select_chat_index(1);
    assert!(!vm.is_voice_recording());
    assert_eq!(
        vm.status_notice(),
        Some("Voice note cancelled: you switched chats")
    );
    assert!(
        vm.take_emitted_sync_commands()
            .iter()
            .any(|c| matches!(c, SyncCommand::VoiceRecordCancel))
    );

    // Switch back to chat1, record, finish -> durable voice outbox record (`ptt: true`, `voice.ogg`).
    vm.select_chat_index(0);
    assert!(vm.toggle_voice_recording(true));
    let gen1 = vm.active_record_generation();
    vm.apply_sync_event(SyncEvent::VoiceRecordTick {
        generation: gen1,
        elapsed_ms: 1_200,
        bars: vec![40, 80, 100],
    });
    assert!(vm.finish_voice_recording());
    let _ = vm.take_emitted_sync_commands();

    vm.apply_sync_event(SyncEvent::VoiceRecordFinished {
        generation: gen1,
        origin_chat: chat1.clone(),
        ogg_bytes: ogg.clone(),
        duration_secs: 2,
        waveform: vec![75; 64],
    });
    assert_eq!(*vm.voice_record_state(), VoiceRecordState::Idle);
    assert_eq!(vm.status_notice(), Some("Voice note saved"));
    let outbox = vm.store().list_outbox_for_chat(&chat1).unwrap();
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].kind, OutboxKind::Voice);
    assert!(outbox[0].ptt);
    assert_eq!(outbox[0].wire_filename, "voice.ogg");
    assert_eq!(outbox[0].voice_seconds, 2);
    assert!(outbox[0].staged_file_path.is_some());

    // Queue-full retention: fill outbox to MAX_PENDING_SENDS, finish another recording -> Retained!
    for i in 1..MAX_PENDING_SENDS {
        let rec = OutboxRecord::new_text(
            format!("{i:032x}"),
            chat1.clone(),
            "filler",
            None,
            false,
            1000 + i as i64,
        );
        vm.store_mut().enqueue_outbox(&rec).unwrap();
    }
    assert!(vm.toggle_voice_recording(true));
    let gen2 = vm.active_record_generation();
    assert!(vm.finish_voice_recording());
    vm.apply_sync_event(SyncEvent::VoiceRecordFinished {
        generation: gen2,
        origin_chat: chat1.clone(),
        ogg_bytes: ogg,
        duration_secs: 1,
        waveform: vec![50; 64],
    });
    assert!(vm.is_voice_retained());
    assert_eq!(
        vm.status_notice(),
        Some("Voice note retained; Enter retries")
    );

    // Esc discards retained voice note cleanly.
    vm.handle_escape();
    assert_eq!(*vm.voice_record_state(), VoiceRecordState::Idle);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn transcript_immutable_cache_retry_once_per_open_and_marked_raw_copy() {
    let dir = std::env::temp_dir().join(format!("postbox_m4_tr_{}", std::process::id()));
    let (mut vm, chat1, _) = seeded_voice_vm(&dir);
    let ogg = sample_ogg_bytes();

    let p1 = dir.join("v1.ogg");
    let p2 = dir.join("v2.ogg");
    let p3 = dir.join("v3.ogg");
    std::fs::write(&p1, &ogg).unwrap();
    std::fs::write(&p2, &ogg).unwrap();
    std::fs::write(&p3, &ogg).unwrap();

    // v1 has a pre-cached SQLite transcript -> MUST NEVER trigger TranscribeAudio!
    vm.store()
        .cache_raw_transcript("sha-v1", "en", "First cached raw transcript.", 100)
        .unwrap();
    // v2 has ONLY a legacy formatted `.txt` sidecar (no `.raw.txt`) -> MUST be refused as raw!
    std::fs::write(transcript_txt_path(&p2), "1. Legacy summary only").unwrap();
    // v3 has no cache yet -> triggers Deepgram TranscribeAudio once.
    vm.apply_sync_event(SyncEvent::MediaDownloaded {
        chat: chat1.clone(),
        msg_id: "v1".into(),
        path: p1.clone(),
        mime_type: "audio/ogg".into(),
        sha256: "sha-v1".into(),
    });
    vm.apply_sync_event(SyncEvent::MediaDownloaded {
        chat: chat1.clone(),
        msg_id: "v2".into(),
        path: p2.clone(),
        mime_type: "audio/ogg".into(),
        sha256: "sha-v2".into(),
    });
    vm.apply_sync_event(SyncEvent::MediaDownloaded {
        chat: chat1.clone(),
        msg_id: "v3".into(),
        path: p3.clone(),
        mime_type: "audio/ogg".into(),
        sha256: "sha-v3".into(),
    });

    let cmds = vm.take_emitted_sync_commands();
    // v1 and v2 are cached (zero Deepgram requests); ONLY v3 requests Deepgram!
    let transcribed_ids: Vec<String> = cmds
        .iter()
        .filter_map(|c| match c {
            SyncCommand::TranscribeAudio { msg_id, .. } => Some(msg_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(transcribed_ids, vec!["v3".to_owned()]);

    // Simulate v3 failing -> 0 retries while staying in chat, then 1 retry upon next chat open!
    vm.apply_sync_event(SyncEvent::TranscriptFailed {
        chat: chat1.clone(),
        msg_id: "v3".into(),
        error: TranscriptError::NetworkFailed,
    });
    assert!(matches!(
        vm.transcript_status(&chat1, "v3"),
        TranscriptStatus::Failed(TranscriptError::NetworkFailed)
    ));
    vm.refresh_messages();
    assert!(
        !vm.take_emitted_sync_commands()
            .iter()
            .any(|c| matches!(c, SyncCommand::TranscribeAudio { msg_id, .. } if msg_id == "v3"))
    );

    // Switch away to chat2 and back to chat1 -> retries failed v3 ONCE, still NEVER retries cached v1!
    vm.select_chat_index(1);
    let _ = vm.take_emitted_sync_commands();
    vm.select_chat_index(0);
    let retry_ids: Vec<String> = vm
        .take_emitted_sync_commands()
        .iter()
        .filter_map(|c| match c {
            SyncCommand::TranscribeAudio { msg_id, .. } => Some(msg_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(retry_ids, vec!["v3".to_owned()]);

    // Deliver v3 transcript (raw starts with "1. " -> genuinely raw in SQLite & .raw.txt!).
    write_transcript_sidecars(&p3, "1. Genuine numbered raw note").unwrap();
    vm.apply_sync_event(SyncEvent::TranscriptReady {
        chat: chat1.clone(),
        msg_id: "v3".into(),
        sha256: "sha-v3".into(),
        raw_text: "1. Genuine numbered raw note".into(),
        formatted_text: Some("1. Formatted summary of v3".into()),
        language: "en".into(),
    });

    // Ctrl+T expands & collapses selected transcript.
    vm.select_message_index(0);
    assert!(!vm.is_transcript_expanded(&chat1, "v1"));
    vm.handle_action(Action::ToggleTranscript, 10);
    assert!(vm.is_transcript_expanded(&chat1, "v1"));
    vm.handle_action(Action::ToggleTranscript, 20);
    assert!(!vm.is_transcript_expanded(&chat1, "v1"));

    // Mark v3, v2, v1 (reverse order) -> Copy all transcripts exports RAW oldest-first `\r\n\r\n`,
    // refusing legacy-formatted v2 and reporting it in Missing!
    vm.toggle_mark_message("v3");
    vm.toggle_mark_message("v2");
    vm.toggle_mark_message("v1");
    assert!(vm.copy_all_marked_transcripts());
    assert_eq!(
        vm.take_pending_clipboard_text().as_deref(),
        Some("First cached raw transcript.\r\n\r\n1. Genuine numbered raw note")
    );
    assert_eq!(
        vm.status_notice(),
        Some("Copied 2 transcripts. Missing: 10:02")
    );

    // Dynamic microphones in Ctrl+K palette.
    vm.apply_sync_event(SyncEvent::MicrophonesUpdated(vec![AudioDeviceInfo {
        id: "mic-1".into(),
        friendly_name: "Shure MV7".into(),
        is_default: false,
    }]));
    vm.open_palette();
    assert!(
        vm.palette_entries()
            .iter()
            .any(|e| matches!(&e.kind, postbox_ui::PaletteEntryKind::Command(PaletteCommand::SelectMicrophone(n)) if n == "Shure MV7"))
    );
    vm.select_microphone(Some("Shure MV7".into()));
    assert_eq!(
        vm.store().load_microphone_preference().unwrap().as_deref(),
        Some("Shure MV7")
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn voice_outbox_ack_replaces_optimistic_row_and_held_surfaces_failed_badge() {
    let dir = std::env::temp_dir().join(format!("postbox_m4_ack_{}", std::process::id()));
    let (mut vm, chat1, _) = seeded_voice_vm(&dir);
    let ogg = sample_ogg_bytes();

    // Archive chat1 and open it in the archived view to verify voice send auto-unarchives it and updates preview.
    let mut archived = vm.store().get_chat(&chat1).unwrap().unwrap();
    archived.archived_local = true;
    vm.store_mut()
        .upsert_chat(&archived, 1_764_237_900)
        .unwrap();
    assert!(vm.select_chat_key(&chat1));
    assert_eq!(vm.selected_chat().map(|c| &c.key), Some(&chat1));

    // Record and finish a voice note.
    assert!(vm.toggle_voice_recording(true));
    let gen_id = vm.active_record_generation();
    assert!(vm.finish_voice_recording());
    vm.apply_sync_event(SyncEvent::VoiceRecordFinished {
        generation: gen_id,
        origin_chat: chat1.clone(),
        ogg_bytes: ogg.clone(),
        duration_secs: 2,
        waveform: vec![60; 64],
    });

    let outbox = vm.store().list_outbox_for_chat(&chat1).unwrap();
    assert_eq!(outbox.len(), 1);
    let client_id = outbox[0].client_id.clone();

    // Optimistic message MUST use msg_id == client_id so ACK replaces it and Held badge matches.
    let msgs = vm.store().list_messages(&chat1, 50, None).unwrap();
    assert!(
        msgs.iter()
            .any(|m| m.msg_id == client_id && m.read_status == ReadStatus::Sending)
    );
    let updated_chat = vm.store().get_chat(&chat1).unwrap().unwrap();
    assert!(!updated_chat.archived_local);
    assert!(updated_chat.last_msg_preview.starts_with("Voice message"));

    // Open unarchived chat1 in inbox view, claim, and mark outbox Held -> OutboxBadge::Failed!
    assert!(vm.select_chat_key(&chat1));
    let claimed = vm
        .store_mut()
        .claim_next_ready_outbox(i64::MAX)
        .unwrap()
        .expect("voice outbox row must be claimable");
    assert_eq!(claimed.client_id, client_id);
    vm.store_mut()
        .mark_outbox_held(
            &claimed.client_id,
            claimed.attempt_serial,
            false,
            "HTTP 400",
        )
        .unwrap();
    vm.refresh_messages();
    let has_failed_badge = vm.timeline_items().iter().any(|item| {
        matches!(
            item,
            postbox_ui::TimelineItem::MessageRow {
                message,
                outbox_status: Some(postbox_ui::OutboxBadge::Failed { .. }),
                ..
            } if message.msg_id == client_id
        )
    });
    assert!(has_failed_badge);

    // Retry held voice send via ViewModel, re-claim, and simulate Hub ACK -> ZERO ghost Sending rows!
    let failed_idx = vm
        .messages()
        .iter()
        .position(|m| m.msg_id == client_id)
        .expect("optimistic voice row in timeline");
    vm.select_message_index(failed_idx);
    vm.handle_action(Action::ResendFailedMessage, 10);
    let claimed2 = vm
        .store_mut()
        .claim_next_ready_outbox(i64::MAX)
        .unwrap()
        .expect("retried voice outbox row must be claimable");
    let confirmed = Message {
        chat: chat1.clone(),
        msg_id: "srv-voice-99".into(),
        sender_id: "me".into(),
        sender_name: "You".into(),
        from_me: true,
        ts: "2026-11-27T10:10:00Z".into(),
        body: "Voice message 0:02".into(),
        reply_to_id: None,
        reply_broadcast: false,
        media_kind: Some(MediaKind::Voice),
        mime_type: Some("audio/ogg".into()),
        filename: Some("voice.ogg".into()),
        file_size: Some(ogg.len() as u64),
        reactions: vec![],
        read_status: ReadStatus::Sent,
        deleted: false,
    };
    assert!(
        vm.store_mut()
            .acknowledge_outbox(&claimed2.client_id, claimed2.attempt_serial)
            .unwrap()
    );
    vm.store_mut()
        .replace_optimistic_message(&chat1, &claimed2.client_id, &confirmed)
        .unwrap();
    vm.refresh_messages();

    let after = vm.store().list_messages(&chat1, 50, None).unwrap();
    assert!(after.iter().any(|m| m.msg_id == "srv-voice-99"));
    assert!(after.iter().all(|m| m.msg_id != client_id));
    assert!(after.iter().all(|m| m.msg_id != claimed2.client_id));
    assert!(after.iter().all(|m| m.read_status != ReadStatus::Sending));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stale_voice_playback_ended_does_not_wipe_current_playback() {
    let dir = std::env::temp_dir().join(format!("postbox_m4_stale_end_{}", std::process::id()));
    let (mut vm, chat1, _) = seeded_voice_vm(&dir);
    let ogg = sample_ogg_bytes();
    let p1 = dir.join("v1.ogg");
    let p2 = dir.join("v2.ogg");
    std::fs::write(&p1, &ogg).unwrap();
    std::fs::write(&p2, &ogg).unwrap();

    for (id, path, sha) in [("v1", p1, "sha-1"), ("v2", p2, "sha-2")] {
        vm.apply_sync_event(SyncEvent::MediaDownloaded {
            chat: chat1.clone(),
            msg_id: id.into(),
            path,
            mime_type: "audio/ogg".into(),
            sha256: sha.into(),
        });
    }

    assert!(vm.start_voice_playback(&chat1, "v2"));
    assert_eq!(vm.voice_playback().unwrap().msg_id, "v2");

    // Stale VoicePlaybackEnded for v1 arrives while v2 is actively playing -> v2 MUST remain untouched.
    vm.apply_sync_event(SyncEvent::VoicePlaybackEnded {
        chat: chat1.clone(),
        msg_id: "v1".into(),
    });
    let pb = vm.voice_playback().expect("v2 playback must remain active");
    assert_eq!(pb.msg_id, "v2");
    assert_eq!(pb.status, VoicePlaybackStatus::Playing);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn paused_waveform_scrub_emits_seek_and_resume() {
    let dir = std::env::temp_dir().join(format!("postbox_m4_scrub_{}", std::process::id()));
    let (mut vm, chat1, _) = seeded_voice_vm(&dir);
    let ogg = sample_ogg_bytes();
    let p1 = dir.join("v1.ogg");
    std::fs::write(&p1, &ogg).unwrap();

    vm.apply_sync_event(SyncEvent::MediaDownloaded {
        chat: chat1.clone(),
        msg_id: "v1".into(),
        path: p1,
        mime_type: "audio/ogg".into(),
        sha256: "sha-1".into(),
    });
    assert!(vm.start_voice_playback(&chat1, "v1"));
    vm.apply_sync_event(SyncEvent::VoicePlaybackProgress {
        chat: chat1.clone(),
        msg_id: "v1".into(),
        pos_ms: 200,
        dur_ms: 4_000,
        waveform: vec![50; 64],
        playing: true,
    });
    assert!(vm.toggle_voice_playback());
    assert_eq!(
        vm.voice_playback().unwrap().status,
        VoicePlaybackStatus::Paused
    );
    let _ = vm.take_emitted_sync_commands();

    // Scrub to 50% while paused -> MUST emit VoiceSeek followed by VoiceResume and stay Playing.
    vm.click_voice_bubble_scrub(&chat1, "v1", 0.5);
    assert_eq!(
        vm.voice_playback().unwrap().status,
        VoicePlaybackStatus::Playing
    );
    let cmds = vm.take_emitted_sync_commands();
    let seek_idx = cmds
        .iter()
        .position(|c| matches!(c, SyncCommand::VoiceSeek { pos_ms: 2_000 }));
    let resume_idx = cmds
        .iter()
        .position(|c| matches!(c, SyncCommand::VoiceResume));
    assert!(seek_idx.is_some(), "expected VoiceSeek command");
    assert!(resume_idx.is_some(), "expected VoiceResume command");
    assert!(seek_idx.unwrap() < resume_idx.unwrap());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn stale_voice_record_events_after_cancel_and_rerecord_are_dropped() {
    let dir = std::env::temp_dir().join(format!("postbox_m4_stale_rec_{}", std::process::id()));
    let (mut vm, chat1, _) = seeded_voice_vm(&dir);
    let ogg = sample_ogg_bytes();

    // Case 1: Start recording, cancel via Esc, then deliver stale VoiceRecordFinished -> 0 files, 0 outbox.
    assert!(vm.toggle_voice_recording(true));
    let gen_cancelled_esc = vm.active_record_generation();
    vm.handle_escape();
    assert_eq!(*vm.voice_record_state(), VoiceRecordState::Idle);
    vm.apply_sync_event(SyncEvent::VoiceRecordFinished {
        generation: gen_cancelled_esc,
        origin_chat: chat1.clone(),
        ogg_bytes: ogg.clone(),
        duration_secs: 2,
        waveform: vec![50; 64],
    });
    assert!(vm.store().list_outbox_for_chat(&chat1).unwrap().is_empty());
    assert!(!dir.join("voice").exists());

    // Case 2: Cancel via chat switch, switch back, immediately re-record in SAME chat,
    // then deliver stale VoiceRecordFinished & VoiceRecordFailed from previous generation -> both dropped!
    assert!(vm.toggle_voice_recording(true));
    let gen_old = vm.active_record_generation();
    vm.select_chat_index(1);
    assert_eq!(*vm.voice_record_state(), VoiceRecordState::Idle);
    vm.select_chat_index(0);
    assert!(vm.toggle_voice_recording(true));
    let gen_new = vm.active_record_generation();
    assert_ne!(gen_old, gen_new);

    vm.apply_sync_event(SyncEvent::VoiceRecordFinished {
        generation: gen_old,
        origin_chat: chat1.clone(),
        ogg_bytes: ogg,
        duration_secs: 2,
        waveform: vec![50; 64],
    });
    vm.apply_sync_event(SyncEvent::VoiceRecordFailed {
        generation: gen_old,
        message: "stale failure".into(),
    });
    assert!(vm.is_voice_recording());
    assert!(vm.store().list_outbox_for_chat(&chat1).unwrap().is_empty());
    assert!(!dir.join("voice").exists());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pending_auto_advance_cancelled_by_manual_stop_chat_switch_download_failure_and_recording() {
    let dir = std::env::temp_dir().join(format!("postbox_m4_advance_{}", std::process::id()));
    let ogg = sample_ogg_bytes();

    // Helper: arms pending_auto_advance on (chat1, "v2") by finishing v1 while v2 is undownloaded.
    let arm_wait = |sub: &str| {
        let sub_dir = dir.join(sub);
        let (mut vm, chat1, chat2) = seeded_voice_vm(&sub_dir);
        let p1 = sub_dir.join("v1.ogg");
        let p2 = sub_dir.join("v2.ogg");
        std::fs::write(&p1, &ogg).unwrap();
        std::fs::write(&p2, &ogg).unwrap();
        vm.apply_sync_event(SyncEvent::MediaDownloaded {
            chat: chat1.clone(),
            msg_id: "v1".into(),
            path: p1,
            mime_type: "audio/ogg".into(),
            sha256: "sha-v1".into(),
        });
        assert!(vm.start_voice_playback(&chat1, "v1"));
        vm.apply_sync_event(SyncEvent::VoicePlaybackEnded {
            chat: chat1.clone(),
            msg_id: "v1".into(),
        });
        assert!(vm.voice_playback().is_none());
        (vm, chat1, chat2, p2)
    };

    // Scenario 1: Manual stop_voice_playback() while waiting for download -> MediaDownloaded MUST NOT hijack.
    {
        let (mut vm, chat1, _, p2) = arm_wait("s1");
        vm.stop_voice_playback();
        vm.apply_sync_event(SyncEvent::MediaDownloaded {
            chat: chat1,
            msg_id: "v2".into(),
            path: p2,
            mime_type: "audio/ogg".into(),
            sha256: "sha-v2".into(),
        });
        assert!(vm.voice_playback().is_none());
    }

    // Scenario 2: Chat switch away and back while waiting -> MediaDownloaded MUST NOT hijack.
    {
        let (mut vm, chat1, _, p2) = arm_wait("s2");
        vm.select_chat_index(1);
        vm.select_chat_index(0);
        vm.apply_sync_event(SyncEvent::MediaDownloaded {
            chat: chat1,
            msg_id: "v2".into(),
            path: p2,
            mime_type: "audio/ogg".into(),
            sha256: "sha-v2".into(),
        });
        assert!(vm.voice_playback().is_none());
    }

    // Scenario 3: MediaStatusChanged(Unavailable | Gone) clears wait -> later retry MediaDownloaded MUST NOT hijack.
    for status in [
        postbox_media::drain::MediaItemStatus::Unavailable,
        postbox_media::drain::MediaItemStatus::Gone,
    ] {
        let sub = format!("s3_{status:?}");
        let (mut vm, chat1, _, p2) = arm_wait(&sub);
        vm.apply_sync_event(SyncEvent::MediaStatusChanged {
            chat: chat1.clone(),
            msg_id: "v2".into(),
            status,
        });
        vm.apply_sync_event(SyncEvent::MediaDownloaded {
            chat: chat1,
            msg_id: "v2".into(),
            path: p2,
            mime_type: "audio/ogg".into(),
            sha256: "sha-v2".into(),
        });
        assert!(vm.voice_playback().is_none());
    }

    // Scenario 4: Starting voice recording while waiting -> MediaDownloaded MUST NOT play over mic!
    {
        let (mut vm, chat1, _, p2) = arm_wait("s4");
        assert!(vm.toggle_voice_recording(true));
        vm.apply_sync_event(SyncEvent::MediaDownloaded {
            chat: chat1,
            msg_id: "v2".into(),
            path: p2,
            mime_type: "audio/ogg".into(),
            sha256: "sha-v2".into(),
        });
        assert!(vm.voice_playback().is_none());
        assert!(vm.is_voice_recording());
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn escape_closes_overlay_modals_before_cancelling_recording_and_thread_composer_toggle_finishes_active_recording()
 {
    let dir = std::env::temp_dir().join(format!("postbox_m4_modal_esc_{}", std::process::id()));
    let (mut vm, chat1, _) = seeded_voice_vm(&dir);
    vm = vm.with_giphy_key_available(true);

    // Seed an image message in chat1 so Lightbox can be opened over an active recording.
    vm.store_mut()
        .upsert_message(&Message {
            chat: chat1.clone(),
            msg_id: "img1".into(),
            sender_id: "alice".into(),
            sender_name: "Alice".into(),
            from_me: false,
            ts: "2026-11-27T10:04:00Z".into(),
            body: String::new(),
            reply_to_id: None,
            reply_broadcast: false,
            media_kind: Some(MediaKind::Image),
            mime_type: Some("image/png".into()),
            filename: Some("photo.png".into()),
            file_size: Some(128),
            reactions: vec![],
            read_status: ReadStatus::None,
            deleted: false,
        })
        .unwrap();
    vm.refresh_messages();

    // Start recording from Composer.
    vm.set_focus(FocusTarget::Composer);
    assert!(vm.toggle_voice_recording(true));
    assert!(vm.is_voice_recording());

    // 1. F1 ShortcutsOverlay -> first Esc closes ONLY shortcuts (restoring Composer) and preserves recording.
    vm.toggle_shortcuts();
    assert!(vm.shortcuts_open());
    vm.handle_escape();
    assert!(!vm.shortcuts_open());
    assert!(vm.is_voice_recording());
    assert_eq!(vm.focus(), FocusTarget::Composer);

    // 2. Command Palette -> first Esc closes ONLY palette and preserves recording.
    vm.open_palette();
    assert!(vm.palette_open());
    vm.handle_escape();
    assert!(!vm.palette_open());
    assert!(vm.is_voice_recording());

    // 3. EmojiPicker -> first Esc closes ONLY emoji picker and preserves recording.
    assert!(vm.open_insert_emoji_picker());
    assert!(vm.emoji_picker().is_some());
    vm.handle_escape();
    assert!(vm.emoji_picker().is_none());
    assert!(vm.is_voice_recording());

    // 4. Lightbox -> first Esc closes ONLY lightbox and preserves recording.
    assert!(vm.open_lightbox_for_message("img1", 1000));
    assert!(vm.lightbox().is_some());
    vm.handle_escape();
    assert!(vm.lightbox().is_none());
    assert!(vm.is_voice_recording());

    // 5. GiphyPicker -> first Esc closes ONLY giphy picker and preserves recording.
    assert!(vm.open_giphy_picker());
    assert!(vm.giphy_picker().is_open());
    vm.handle_escape();
    assert!(!vm.giphy_picker().is_open());
    assert!(vm.is_voice_recording());

    // Once all modals are closed, next Esc cancels the active voice recording cleanly.
    vm.handle_escape();
    assert_eq!(*vm.voice_record_state(), VoiceRecordState::Idle);
    assert_eq!(vm.status_notice(), Some("Voice note cancelled"));

    // ThreadComposer still blocks starting a NEW recording...
    vm.set_focus(FocusTarget::ThreadComposer);
    assert!(!vm.toggle_voice_recording(true));
    assert_eq!(*vm.voice_record_state(), VoiceRecordState::Idle);

    // ...but if focus moves to ThreadComposer mid-recording, Ctrl+Shift+M still finishes the active recording!
    vm.set_focus(FocusTarget::Composer);
    assert!(vm.toggle_voice_recording(true));
    assert!(vm.is_voice_recording());
    let _ = vm.take_emitted_sync_commands();
    vm.set_focus(FocusTarget::ThreadComposer);
    assert!(vm.toggle_voice_recording(true));
    assert!(
        vm.take_emitted_sync_commands()
            .iter()
            .any(|c| matches!(c, SyncCommand::VoiceRecordFinish)),
        "Ctrl+Shift+M must still finish an already-active recording even if ThreadComposer is focused"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
