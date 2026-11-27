use futures_util::{SinkExt, StreamExt};
use postbox_audio::{LiveDictationConfig, LiveDictationPhase, LiveDictationSession};
use postbox_core::{Account, Chat, ChatKey, DictationLanguage, Provider};
use postbox_keymap::Keymap;
use postbox_platform::MockAudioBackend;
use postbox_store::Store;
use postbox_ui::{FocusTarget, PostboxViewModel, SyncCommand, SyncEvent};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::Frame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};

fn seeded_vm() -> (PostboxViewModel, ChatKey, ChatKey) {
    let store = Store::open_in_memory().unwrap();
    store
        .upsert_account(&Account {
            provider: Provider::Whatsapp,
            account_id: "test".into(),
            label: "Test".into(),
            state: "connected".into(),
            sort_order: 0,
            last_event_at: None,
        })
        .unwrap();
    let first = ChatKey::new(Provider::Whatsapp, "test", "first");
    let second = ChatKey::new(Provider::Whatsapp, "test", "second");
    for (key, ts) in [(&first, "2026-01-02"), (&second, "2026-01-01")] {
        store
            .upsert_chat(
                &Chat {
                    key: key.clone(),
                    title: key.id.clone(),
                    is_group: false,
                    archived_local: false,
                    archived_remote: false,
                    pinned: false,
                    unread_count: 0,
                    last_msg_ts: ts.into(),
                    last_msg_preview: String::new(),
                    avatar_hash: None,
                },
                1,
            )
            .unwrap();
    }
    let mut vm = PostboxViewModel::new_uninitialized(store, Keymap::postbox_defaults(), "test")
        .with_deepgram_key_available(true);
    vm.initialize();
    assert!(vm.select_chat_key(&first));
    vm.set_focus(FocusTarget::Composer);
    vm.take_emitted_sync_commands();
    (vm, first, second)
}

fn start(vm: &mut PostboxViewModel, text: &str, selection: std::ops::Range<usize>) -> u64 {
    vm.set_composer_text(text);
    vm.set_composer_selection(selection);
    assert!(vm.toggle_dictation(true));
    let generation = vm.live_dictation().unwrap().generation;
    vm.apply_sync_event(SyncEvent::DictationConnected { generation });
    generation
}

#[tokio::test]
async fn websocket_fragments_replace_cursor_span_and_close_stream_commits_once_in_all_modes() {
    tokio::time::timeout(Duration::from_secs(15), async {
        for language in [
            DictationLanguage::Automatic,
            DictationLanguage::English,
            DictationLanguage::German,
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("ws://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                let result = |text: &str, is_final: bool| {
                    serde_json::json!({
                        "type": "Results", "start": 0.0, "duration": 0.5, "is_final": is_final,
                        "channel": { "alternatives": [{ "transcript": text,
                            "words": [{ "word": "Grüße", "language": "de" }] }] }
                    })
                    .to_string()
                };
                let interim = result("Grüße", false);
                // Split inside ü, not at a UTF-8 boundary, to exercise WebSocket reassembly.
                let split = interim.find('ü').unwrap() + 1;
                ws.send(Message::Frame(Frame::message(
                    interim.as_bytes()[..split].to_vec(),
                    OpCode::Data(Data::Text),
                    false,
                )))
                .await
                .unwrap();
                ws.send(Message::Frame(Frame::message(
                    interim.as_bytes()[split..].to_vec(),
                    OpCode::Data(Data::Continue),
                    true,
                )))
                .await
                .unwrap();
                let mut closes = 0;
                while let Some(msg) = ws.next().await {
                    if let Message::Text(text) = msg.unwrap()
                        && text == r#"{"type":"CloseStream"}"#
                    {
                        closes += 1;
                        let final_text = result("Grüße!", true);
                        ws.send(Message::Text(final_text.clone().into()))
                            .await
                            .unwrap();
                        ws.send(Message::Text(final_text.into())).await.unwrap();
                        ws.send(Message::Close(None)).await.unwrap();
                        break;
                    }
                }
                assert_eq!(closes, 1);
            });
            let (mut vm, first, _) = seeded_vm();
            vm.set_dictation_language(language);
            let generation = start(&mut vm, "links rechts", 6..6);
            let mut config = LiveDictationConfig::new("loopback-test-key", language);
            config.ws_base_url = url;
            let session =
                LiveDictationSession::start(Arc::new(MockAudioBackend::new()), config).unwrap();
            let mut rx = session.subscribe();
            loop {
                let snapshot = rx.borrow().clone();
                if snapshot.text == "Grüße" {
                    vm.apply_sync_event(SyncEvent::DictationInterim {
                        generation,
                        revision: snapshot.revision,
                        text: snapshot.text,
                    });
                    break;
                }
                assert!(!snapshot.phase.is_terminal(), "{snapshot:?}");
                rx.changed().await.unwrap();
            }
            assert_eq!(vm.composer_text(), "links Grüße rechts");
            assert_eq!(vm.composer_selection(), 13..13);
            assert!(vm.stop_dictation());
            session.request_stop();
            let final_snapshot = session.wait_for_terminal().await;
            assert_eq!(final_snapshot.phase, LiveDictationPhase::Ready);
            for _ in 0..2 {
                vm.apply_sync_event(SyncEvent::DictationFinal {
                    generation,
                    text: final_snapshot.text.clone(),
                });
            }
            assert_eq!(vm.composer_text(), "links Grüße! rechts");
            assert_eq!(vm.composer_selection(), 14..14);
            assert_eq!(
                vm.store().load_draft(&first).unwrap().unwrap().text_body,
                "links Grüße! rechts"
            );
            assert!(!vm.is_dictation_active());
            server.await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[test]
fn shrinking_interims_cancel_and_user_edits_preserve_the_original_span() {
    let (mut vm, _, _) = seeded_vm();
    let generation = start(&mut vm, "before selected after", 7..15);
    for (revision, text) in [(1, "long interim"), (2, "short"), (3, "")] {
        vm.apply_sync_event(SyncEvent::DictationInterim {
            generation,
            revision,
            text: text.into(),
        });
        let expected = if text.is_empty() {
            "before selected after".to_owned()
        } else {
            format!("before {text} after")
        };
        assert_eq!(vm.composer_text(), expected);
    }
    vm.apply_sync_event(SyncEvent::DictationInterim {
        generation,
        revision: 4,
        text: "draft".into(),
    });
    assert!(vm.cancel_dictation());
    assert_eq!(vm.composer_text(), "before selected after");
    assert_eq!(vm.composer_selection(), 7..15);
    let generation = start(&mut vm, "original", 8..8);
    vm.set_composer_text("manual edit");
    vm.apply_sync_event(SyncEvent::DictationFinal {
        generation,
        text: "must not replace".into(),
    });
    assert_eq!(vm.composer_text(), "manual edit");
}

#[test]
fn switched_chat_edits_do_not_detach_the_origin_draft() {
    let (mut vm, first, second) = seeded_vm();
    let generation = start(&mut vm, "origin", 6..6);
    assert!(vm.select_chat_key(&second));
    vm.set_composer_text("other chat draft");
    vm.apply_sync_event(SyncEvent::DictationFinal {
        generation,
        text: "dictated".into(),
    });
    assert_eq!(vm.composer_text(), "other chat draft");
    assert_eq!(
        vm.store().load_draft(&first).unwrap().unwrap().text_body,
        "origin dictated"
    );
}

#[test]
fn cursor_drift_disarms_auto_send_and_language_and_meter_state_are_persisted() {
    let (mut vm, _, _) = seeded_vm();
    for language in [
        DictationLanguage::Automatic,
        DictationLanguage::English,
        DictationLanguage::German,
    ] {
        vm.set_dictation_language(language);
        assert_eq!(vm.store().load_dictation_language().unwrap(), language);
    }
    let generation = start(&mut vm, "draft", 5..5);
    vm.apply_sync_event(SyncEvent::DictationLevel {
        generation,
        level: 200,
    });
    let started = vm.live_dictation().unwrap().started_at_ms;
    assert!(vm.tick_dictation_meter(started + 120));
    assert_eq!(
        vm.live_dictation().unwrap().meter_bars,
        [0, 0, 0, 0, 0, 0, 0, 200]
    );
    assert!(vm.finish_dictation_and_auto_send());
    vm.set_composer_selection(0..0);
    vm.take_emitted_sync_commands();
    vm.apply_sync_event(SyncEvent::DictationFinal {
        generation,
        text: "dictated".into(),
    });
    assert_eq!(vm.composer_text(), "draft dictated");
    assert!(vm.store().list_outbox().unwrap().is_empty());
    assert!(
        !vm.take_emitted_sync_commands()
            .iter()
            .any(|cmd| matches!(cmd, SyncCommand::WakeOutbox))
    );
    assert!(!vm.tick_dictation_meter(started + 240));
}
