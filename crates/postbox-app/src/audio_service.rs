//! Dedicated M4 audio playback/recording worker thread (0% idle when stopped) and
//! offline-capable bridge so voice playback, recording, mic enumeration, and Deepgram
//! transcription work identically whether Hub is connected or unconfigured.

use postbox_audio::{
    DeepgramRestClient, FormatCache, FormatCacheKey, PLAYBACK_POLL_INTERVAL, PlayerState,
    RECORDING_POLL_INTERVAL, RECORDING_TICK_INTERVAL, TranscriptFormatter, VoicePlayer,
    VoiceRecorder, write_formatted_txt_sidecar, write_transcript_sidecars,
};
use postbox_core::ChatKey;
use postbox_core::dictation::DictationLanguage;
use postbox_core::voice::{PlaybackSpeed, TranscriptError};
use postbox_platform::{
    AudioBackend, FormatProviderConfig, NativePlatform,
    refresh_deepgram_api_key_if_changed_from_system,
};
use postbox_store::Store;
use postbox_ui::{SyncCommand, SyncEvent, SyncWorkerHandle};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub enum AudioWorkerCmd {
    Play {
        chat: ChatKey,
        msg_id: String,
        path: PathBuf,
        speed: PlaybackSpeed,
        start_ms: u64,
    },
    Pause,
    Resume,
    Stop,
    Seek {
        pos_ms: u64,
    },
    SetSpeed(PlaybackSpeed),
    RecordStart {
        generation: u64,
        origin_chat: ChatKey,
        preferred_mic: Option<String>,
    },
    RecordFinish,
    RecordCancel,
    RefreshMicrophones,
    Shutdown,
}

#[derive(Clone)]
pub struct AudioWorkerHandle {
    tx: Sender<AudioWorkerCmd>,
}

impl AudioWorkerHandle {
    pub fn spawn(
        backend: Arc<dyn AudioBackend>,
        event_tx: async_channel::Sender<SyncEvent>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("postbox-audio-worker".into())
            .spawn(move || run_audio_worker(backend, rx, event_tx));
        Self { tx }
    }

    pub fn send(&self, cmd: AudioWorkerCmd) {
        let _ = self.tx.send(cmd);
    }
}

fn run_audio_worker(
    backend: Arc<dyn AudioBackend>,
    rx: mpsc::Receiver<AudioWorkerCmd>,
    event_tx: async_channel::Sender<SyncEvent>,
) {
    // Emit initial microphone list on startup.
    if let Ok(devices) = backend.list_capture_devices() {
        let _ = event_tx.try_send(SyncEvent::MicrophonesUpdated(devices));
    }

    let mut player = VoicePlayer::new();
    let mut active_playback: Option<(ChatKey, String)> = None;
    let mut last_progress_emit = Instant::now();

    let mut recorder: Option<(u64, ChatKey, VoiceRecorder)> = None;
    let mut last_record_tick = Instant::now();

    loop {
        let is_playing = player.state() == PlayerState::Playing;
        let is_recording = recorder.is_some();

        let next_cmd = if is_playing || is_recording {
            let timeout = if is_playing {
                PLAYBACK_POLL_INTERVAL
            } else {
                RECORDING_POLL_INTERVAL
            };
            match rx.recv_timeout(timeout) {
                Ok(cmd) => Some(cmd),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            // Strictly 0% CPU parked when neither playing nor recording!
            match rx.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => break,
            }
        };

        if let Some(cmd) = next_cmd {
            match cmd {
                AudioWorkerCmd::Play {
                    chat,
                    msg_id,
                    path,
                    speed,
                    start_ms,
                } => match player.load_file(backend.as_ref(), &path, speed, start_ms) {
                    Ok(waveform) => {
                        let snap = player.snapshot();
                        active_playback = Some((chat.clone(), msg_id.clone()));
                        last_progress_emit = Instant::now();
                        let _ = event_tx.try_send(SyncEvent::VoicePlaybackProgress {
                            chat,
                            msg_id,
                            pos_ms: snap.position_ms,
                            dur_ms: snap.duration_ms,
                            waveform: waveform.to_vec(),
                            playing: true,
                        });
                    }
                    Err(_) => {
                        active_playback = None;
                        let _ = event_tx.try_send(SyncEvent::VoicePlaybackFailed {
                            chat,
                            msg_id,
                            message: "Windows cannot play this audio format".to_owned(),
                        });
                    }
                },
                AudioWorkerCmd::Pause => {
                    let _ = player.pause();
                    if let Some((ref chat, ref msg_id)) = active_playback {
                        let snap = player.snapshot();
                        let _ = event_tx.try_send(SyncEvent::VoicePlaybackProgress {
                            chat: chat.clone(),
                            msg_id: msg_id.clone(),
                            pos_ms: snap.position_ms,
                            dur_ms: snap.duration_ms,
                            waveform: Vec::new(),
                            playing: false,
                        });
                    }
                }
                AudioWorkerCmd::Resume => {
                    let _ = player.resume();
                    last_progress_emit = Instant::now();
                    if let Some((ref chat, ref msg_id)) = active_playback {
                        let snap = player.snapshot();
                        let _ = event_tx.try_send(SyncEvent::VoicePlaybackProgress {
                            chat: chat.clone(),
                            msg_id: msg_id.clone(),
                            pos_ms: snap.position_ms,
                            dur_ms: snap.duration_ms,
                            waveform: Vec::new(),
                            playing: true,
                        });
                    }
                }
                AudioWorkerCmd::Stop => {
                    player.stop();
                    active_playback = None;
                }
                AudioWorkerCmd::Seek { pos_ms } => {
                    let _ = player.seek_ms(pos_ms);
                    if let Some((ref chat, ref msg_id)) = active_playback {
                        let snap = player.snapshot();
                        let _ = event_tx.try_send(SyncEvent::VoicePlaybackProgress {
                            chat: chat.clone(),
                            msg_id: msg_id.clone(),
                            pos_ms: snap.position_ms,
                            dur_ms: snap.duration_ms,
                            waveform: Vec::new(),
                            playing: snap.state == PlayerState::Playing,
                        });
                    }
                }
                AudioWorkerCmd::SetSpeed(speed) => {
                    player.set_speed(speed);
                }
                AudioWorkerCmd::RecordStart {
                    generation,
                    origin_chat,
                    preferred_mic,
                } => {
                    if let Some((_, _, prev)) = recorder.take() {
                        prev.cancel();
                    }
                    player.stop();
                    active_playback = None;
                    match VoiceRecorder::start(backend.as_ref(), preferred_mic.as_deref()) {
                        Ok(rec) => {
                            last_record_tick = Instant::now();
                            recorder = Some((generation, origin_chat, rec));
                        }
                        Err(err) => {
                            let _ = event_tx.try_send(SyncEvent::VoiceRecordFailed {
                                generation,
                                message: err.user_message().to_owned(),
                            });
                        }
                    }
                }
                AudioWorkerCmd::RecordFinish => {
                    if let Some((generation, origin_chat, rec)) = recorder.take() {
                        match rec.finish() {
                            Ok(encoded) => {
                                let _ = event_tx.try_send(SyncEvent::VoiceRecordFinished {
                                    generation,
                                    origin_chat,
                                    ogg_bytes: encoded.ogg_bytes,
                                    duration_secs: encoded.duration_secs,
                                    waveform: encoded.waveform.to_vec(),
                                });
                            }
                            Err(err) => {
                                let _ = event_tx.try_send(SyncEvent::VoiceRecordFailed {
                                    generation,
                                    message: err.user_message().to_owned(),
                                });
                            }
                        }
                    }
                }
                AudioWorkerCmd::RecordCancel => {
                    if let Some((_, _, rec)) = recorder.take() {
                        rec.cancel();
                    }
                }
                AudioWorkerCmd::RefreshMicrophones => {
                    if let Ok(devices) = backend.list_capture_devices() {
                        let _ = event_tx.try_send(SyncEvent::MicrophonesUpdated(devices));
                    }
                }
                AudioWorkerCmd::Shutdown => {
                    player.stop();
                    if let Some((_, _, rec)) = recorder.take() {
                        rec.cancel();
                    }
                    break;
                }
            }
        }

        // Pump active playback stream.
        if player.state() == PlayerState::Playing {
            match player.pump() {
                Ok(true) => {
                    if let Some((chat, msg_id)) = active_playback.take() {
                        let _ = event_tx.try_send(SyncEvent::VoicePlaybackEnded { chat, msg_id });
                    }
                }
                Ok(false) => {
                    if last_progress_emit.elapsed() >= Duration::from_millis(100)
                        && let Some((ref chat, ref msg_id)) = active_playback
                    {
                        last_progress_emit = Instant::now();
                        let snap = player.snapshot();
                        let _ = event_tx.try_send(SyncEvent::VoicePlaybackProgress {
                            chat: chat.clone(),
                            msg_id: msg_id.clone(),
                            pos_ms: snap.position_ms,
                            dur_ms: snap.duration_ms,
                            waveform: Vec::new(),
                            playing: true,
                        });
                    }
                }
                Err(_) => {
                    player.stop();
                    if let Some((chat, msg_id)) = active_playback.take() {
                        let _ = event_tx.try_send(SyncEvent::VoicePlaybackFailed {
                            chat,
                            msg_id,
                            message: "Windows cannot play this audio format".to_owned(),
                        });
                    }
                }
            }
        }

        // Poll active recording stream & emit 100 ms live meter ticks ONLY while recording.
        if let Some((generation, origin_chat, mut rec)) = recorder.take() {
            match rec.poll_packets() {
                Ok(_) => {
                    if rec.reached_max_duration() {
                        match rec.finish() {
                            Ok(encoded) => {
                                let _ = event_tx.try_send(SyncEvent::VoiceRecordFinished {
                                    generation,
                                    origin_chat,
                                    ogg_bytes: encoded.ogg_bytes,
                                    duration_secs: encoded.duration_secs,
                                    waveform: encoded.waveform.to_vec(),
                                });
                            }
                            Err(err) => {
                                let _ = event_tx.try_send(SyncEvent::VoiceRecordFailed {
                                    generation,
                                    message: err.user_message().to_owned(),
                                });
                            }
                        }
                    } else {
                        if last_record_tick.elapsed() >= RECORDING_TICK_INTERVAL {
                            last_record_tick = Instant::now();
                            let _ = event_tx.try_send(SyncEvent::VoiceRecordTick {
                                generation,
                                elapsed_ms: rec.elapsed_ms(),
                                bars: rec.live_bars(),
                            });
                        }
                        recorder = Some((generation, origin_chat, rec));
                    }
                }
                Err(err) => {
                    rec.cancel();
                    let _ = event_tx.try_send(SyncEvent::VoiceRecordFailed {
                        generation,
                        message: err.user_message().to_owned(),
                    });
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_transcription_task(
    deepgram_client: DeepgramRestClient,
    deepgram_api_key: Arc<Mutex<Option<String>>>,
    chat: ChatKey,
    msg_id: String,
    path: PathBuf,
    sha256: String,
    mime_type: String,
    language: DictationLanguage,
    event_tx: async_channel::Sender<SyncEvent>,
) {
    tokio::spawn(async move {
        let current_key = deepgram_api_key
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .filter(|k| !k.trim().is_empty());
        let Some(api_key) = current_key else {
            let _ = event_tx.try_send(SyncEvent::TranscriptFailed {
                chat,
                msg_id,
                error: TranscriptError::MissingKey,
            });
            return;
        };

        let key_slot = Arc::clone(&deepgram_api_key);
        let result = deepgram_client
            .transcribe_file(&path, &mime_type, language, &api_key, move |curr| {
                let plat = NativePlatform::default();
                let refreshed = refresh_deepgram_api_key_if_changed_from_system(curr, &plat, &plat);
                if let Some(ref new_key) = refreshed
                    && let Ok(mut guard) = key_slot.lock()
                {
                    *guard = Some(new_key.clone());
                }
                refreshed
            })
            .await;

        match result {
            Ok(res) => {
                let _ = write_transcript_sidecars(&path, &res.text);
                let _ = event_tx.try_send(SyncEvent::TranscriptReady {
                    chat,
                    msg_id,
                    sha256,
                    raw_text: res.text,
                    formatted_text: None,
                    language: res.language,
                });
            }
            Err(error) => {
                let _ = event_tx.try_send(SyncEvent::TranscriptFailed {
                    chat,
                    msg_id,
                    error,
                });
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_format_task(
    formatter: TranscriptFormatter,
    provider_config: Option<FormatProviderConfig>,
    data_dir: PathBuf,
    chat: ChatKey,
    msg_id: String,
    sha256: String,
    raw_text: String,
    from_me: bool,
    contact_name: String,
    local_path: Option<PathBuf>,
    event_tx: async_channel::Sender<SyncEvent>,
) {
    let Some(config) = provider_config else {
        return;
    };
    tokio::spawn(async move {
        let cache_key = FormatCacheKey::new(chat.account.clone(), chat.id.clone(), msg_id.clone());
        if let Some(cached) = FormatCache::load_from_disk(&data_dir, &cache_key) {
            let _ = event_tx.try_send(SyncEvent::TranscriptFormatted {
                chat,
                msg_id,
                sha256,
                formatted_text: cached,
            });
            return;
        }
        if let Ok(formatted) = formatter
            .format_transcript(&config, &raw_text, from_me, &contact_name)
            .await
        {
            let _ = FormatCache::save_to_disk(&data_dir, &cache_key, &formatted);
            if let Some(ref p) = local_path {
                let _ = write_formatted_txt_sidecar(p, &formatted);
            }
            let _ = event_tx.try_send(SyncEvent::TranscriptFormatted {
                chat,
                msg_id,
                sha256,
                formatted_text: formatted,
            });
        }
    });
}

/// Standalone audio + transcription worker spawned when Hub is unconfigured/offline so
/// voice playback, recording, microphone listing, and transcription still work seamlessly.
pub fn spawn_offline_audio_worker(
    store_loc: crate::sync::StoreLocation,
    data_dir: PathBuf,
    deepgram_api_key: Option<String>,
    format_provider: Option<FormatProviderConfig>,
    backend: Arc<dyn AudioBackend>,
    worker: SyncWorkerHandle,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("postbox-offline-audio".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async move {
                let store = store_loc.open().ok();
                let audio_handle = AudioWorkerHandle::spawn(backend, worker.event_tx.clone());
                let dg_client = DeepgramRestClient::new();
                let deepgram_api_key = Arc::new(Mutex::new(deepgram_api_key));
                let formatter = TranscriptFormatter::new();

                while let Ok(cmd) = worker.cmd_rx.recv().await {
                    match cmd {
                        SyncCommand::VoicePlay {
                            chat,
                            msg_id,
                            path,
                            speed,
                            start_ms,
                        } => audio_handle.send(AudioWorkerCmd::Play {
                            chat,
                            msg_id,
                            path,
                            speed,
                            start_ms,
                        }),
                        SyncCommand::VoicePause => audio_handle.send(AudioWorkerCmd::Pause),
                        SyncCommand::VoiceResume => audio_handle.send(AudioWorkerCmd::Resume),
                        SyncCommand::VoiceStop => audio_handle.send(AudioWorkerCmd::Stop),
                        SyncCommand::VoiceSeek { pos_ms } => {
                            audio_handle.send(AudioWorkerCmd::Seek { pos_ms })
                        }
                        SyncCommand::VoiceSetSpeed(speed) => {
                            audio_handle.send(AudioWorkerCmd::SetSpeed(speed))
                        }
                        SyncCommand::VoiceRecordStart {
                            generation,
                            origin_chat,
                            preferred_mic,
                        } => audio_handle.send(AudioWorkerCmd::RecordStart {
                            generation,
                            origin_chat,
                            preferred_mic,
                        }),
                        SyncCommand::VoiceRecordFinish => {
                            audio_handle.send(AudioWorkerCmd::RecordFinish)
                        }
                        SyncCommand::VoiceRecordCancel => {
                            audio_handle.send(AudioWorkerCmd::RecordCancel)
                        }
                        SyncCommand::RefreshMicrophones => {
                            audio_handle.send(AudioWorkerCmd::RefreshMicrophones)
                        }
                        SyncCommand::TranscribeAudio {
                            chat,
                            msg_id,
                            path,
                            sha256,
                            mime_type,
                            language,
                            ..
                        } => {
                            if let Some(ref s) = store
                                && !sha256.is_empty()
                                && let Ok(Some(rec)) = s.get_transcript_record(&sha256)
                            {
                                let _ = worker.event_tx.try_send(SyncEvent::TranscriptReady {
                                    chat,
                                    msg_id,
                                    sha256,
                                    raw_text: rec.raw_text,
                                    formatted_text: rec.formatted_text,
                                    language: rec.language,
                                });
                                continue;
                            }
                            spawn_transcription_task(
                                dg_client.clone(),
                                Arc::clone(&deepgram_api_key),
                                chat,
                                msg_id,
                                path,
                                sha256,
                                mime_type,
                                language,
                                worker.event_tx.clone(),
                            );
                        }
                        SyncCommand::FormatTranscript {
                            chat,
                            msg_id,
                            sha256,
                            raw_text,
                            from_me,
                            contact_name,
                            local_path,
                        } => {
                            spawn_format_task(
                                formatter.clone(),
                                format_provider.clone(),
                                data_dir.clone(),
                                chat,
                                msg_id,
                                sha256,
                                raw_text,
                                from_me,
                                contact_name,
                                local_path,
                                worker.event_tx.clone(),
                            );
                        }
                        SyncCommand::Shutdown => {
                            audio_handle.send(AudioWorkerCmd::Shutdown);
                            break;
                        }
                        _ => {}
                    }
                }
            });
        })
        .expect("spawn postbox-offline-audio thread")
}

pub fn persist_transcript_to_store(store: &Store, sha256: &str, language: &str, raw_text: &str) {
    if sha256.is_empty() || raw_text.trim().is_empty() {
        return;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let _ = store.cache_raw_transcript(sha256, language, raw_text, now);
}
