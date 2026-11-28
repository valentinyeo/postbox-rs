use crate::audio_service::{
    AudioWorkerCmd, AudioWorkerHandle, LiveDictationController, persist_dictation_language_pref,
    persist_microphone_pref, persist_transcript_to_store, spawn_format_task,
    spawn_transcription_task,
};
use postbox_audio::{DeepgramRestClient, TranscriptFormatter};
use postbox_core::archive::{IncomingProbeMessage, PendingArchiveIntent, unix_seconds};
use postbox_core::outbox::{
    OutboxKind, OutboxPhase, classify_http_outcome, verify_attachment_bytes,
};
use postbox_core::read::select_read_message_id;
use postbox_core::views::{format_account_label, resolve_slack_workspace_name};
use postbox_core::{Account, ChatKey, MediaKind, Message, Provider, ReadStatus};
use postbox_hub::{
    ArchiveOutcome, AvatarFetchState, Endpoint, EventStream, HubClient, HubError, MAX_ROWS,
    OLDER_PAGE_LIMIT, SendFileRequest, SseEvent, SseParser, SseReconnectState,
    classify_signal_send_failure, classify_telegram_send_failure, is_real_message, read_message_id,
    should_run_safety_poll,
};
use postbox_media::MEDIA_EVICTION_DAYS;
use postbox_media::age::{evict_expired_media_cache, format_utc_rfc3339};
use postbox_media::avatar::{
    AvatarCacheLookup, SlackEmojiIndex, commit_avatar_fetch_result,
    default_wacli_remote_avatars_dir, fetch_or_load_slack_emoji_media,
    fetch_or_load_workspace_icon, resolve_avatar_cache_or_seed,
};
use postbox_media::drain::{
    BACKGROUND_MEDIA_LIMIT, CachedDownloadResult, DownloadOutcome, MAX_BACKGROUND_READ_FAILURES,
    MediaDownloadSpec, MediaDownloader, MediaDrainTracker, MediaItemStatus, atomic_write_file,
    background_chat_due, background_may_start, effective_cached_mime, is_downloadable_message,
    is_oversized, media_cache_key,
};
use postbox_media::giphy::{
    GIPHY_DEFAULT_LIMIT, GIPHY_MSG_NEEDS_KEY, GiphyClient, giphy_error_message,
};
use postbox_media::unfurl::{ClassifiedUrl, UnfurlCard, UnfurlService};
use postbox_platform::{AudioBackend, FormatProviderConfig, NativePlatform, VideoBackend};
use postbox_store::{MediaCacheEntry, Store};
use postbox_ui::{
    HubConnectionStatus, MediaDownloadTarget, SyncCommand, SyncEvent, SyncWorkerHandle,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, watch};

pub(crate) fn sse_event_provider(ev: &SseEvent) -> Option<Provider> {
    if ev.chat.starts_with("slack:") {
        Some(Provider::Slack)
    } else if ev.chat.starts_with("telegram:") {
        Some(Provider::Telegram)
    } else if ev.chat.starts_with("signal:") {
        Some(Provider::Signal)
    } else if !ev.account.is_empty()
        || [
            "@s.whatsapp.net",
            "@g.us",
            "@lid",
            "@broadcast",
            "@newsletter",
            "@c.us",
        ]
        .iter()
        .any(|suffix| ev.chat.ends_with(suffix))
    {
        Some(Provider::Whatsapp)
    } else {
        None
    }
}

pub(crate) fn sse_batch_target_provider(events: &[SseEvent]) -> Option<Provider> {
    if events.iter().any(|e| e.kind == "reset") {
        return None;
    }
    let provider = sse_event_provider(events.first()?)?;
    events
        .iter()
        .all(|e| sse_event_provider(e) == Some(provider))
        .then_some(provider)
}

pub(crate) fn sse_batch_hits_open_chat(events: &[SseEvent], open_chat: &ChatKey) -> bool {
    events
        .iter()
        .any(|e| e.kind != "reset" && !e.chat.is_empty() && e.chat == open_chat.id)
}

#[derive(Debug, Clone)]
pub enum StoreLocation {
    File(PathBuf),
    SharedMemory(String),
}

impl StoreLocation {
    pub fn open(&self) -> Result<Store, postbox_store::StoreError> {
        match self {
            Self::File(path) => Store::open(path),
            Self::SharedMemory(name) => Store::open_shared_memory(name),
        }
    }
}

/// Hub media cache root (video thumbnails live beside the downloaded videos).
fn data_dir_hub_media(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("hub-media")
}

fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_epoch_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Spawns the dedicated background OS thread owning a single-threaded Tokio runtime for live
/// Hub HTTP/2 + SSE sync, durable SQLite outbox delivery, pending mark-read, archive batching,
/// media auto-downloading, avatars/Slack emoji, link unfurls, and cancellable Giphy searches.
#[allow(clippy::too_many_arguments)]
pub fn spawn_sync_engine(
    endpoint: Endpoint,
    token: String,
    store_loc: StoreLocation,
    data_dir: PathBuf,
    giphy_api_key: Option<String>,
    deepgram_api_key: Option<String>,
    format_provider: Option<FormatProviderConfig>,
    audio_backend: Arc<dyn AudioBackend>,
    worker: SyncWorkerHandle,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("postbox-hub-sync".to_owned())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    let _ = worker.event_tx.try_send(SyncEvent::StatusChanged(
                        HubConnectionStatus::Error {
                            url: Some(endpoint.base_url()),
                            message: format!("Failed to initialize async runtime: {err}"),
                        },
                    ));
                    return;
                }
            };
            rt.block_on(async move {
                let mut store = match store_loc.open() {
                    Ok(s) => s,
                    Err(err) => {
                        let _ = worker.event_tx.try_send(SyncEvent::StatusChanged(
                            HubConnectionStatus::Error {
                                url: Some(endpoint.base_url()),
                                message: format!("Failed to open local store for sync: {err}"),
                            },
                        ));
                        return;
                    }
                };

                // Startup 14-day media disk + SQLite cache eviction (`main.zig:7986-8039`).
                let hub_media_dir = data_dir.join("hub-media");
                let _ = evict_expired_media_cache(
                    &store,
                    &hub_media_dir,
                    now_epoch_secs(),
                    MEDIA_EVICTION_DAYS,
                );

                // SendQueue.tla startup crash recovery: any `uncertain` rows from a prior crash
                // become `held` + `ambiguous = true` and NEVER auto-resend.
                if let Ok(recovered) = store.recover_uncertain_on_startup() {
                    for rec in recovered {
                        let _ = worker.event_tx.try_send(SyncEvent::OutboxUpdated {
                            chat: rec.chat,
                            notice: Some(
                                "Previous send unconfirmed after restart — click to retry".into(),
                            ),
                        });
                    }
                }

                let client = match HubClient::new(endpoint.clone(), token) {
                    Ok(c) => c,
                    Err(err) => {
                        let _ = worker.event_tx.try_send(SyncEvent::StatusChanged(
                            HubConnectionStatus::Error {
                                url: Some(endpoint.base_url()),
                                message: format!("Invalid Hub credentials: {err}"),
                            },
                        ));
                        return;
                    }
                };

                let mut engine = SyncEngine::with_m4(
                    client,
                    store,
                    worker,
                    data_dir,
                    giphy_api_key,
                    deepgram_api_key,
                    format_provider,
                    audio_backend,
                );
                // Milestone 6: warm the video thumbnail cache off the UI thread as soon as a
                // video finishes auto-downloading.
                engine.store_loc = Some(store_loc);
                engine.run().await;
            });
        })
        .expect("spawn postbox-hub-sync thread")
}

enum BackgroundTaskResult {
    AvatarFetched {
        chat: ChatKey,
        outcome: Result<AvatarFetchState, HubError>,
    },
    MediaDownloadDone {
        chat: ChatKey,
        target: MediaDownloadTarget,
        automatic: bool,
        is_background: bool,
        result: Result<CachedDownloadResult, postbox_media::MediaError>,
    },
    UnfurlDone {
        target: ClassifiedUrl,
        card: Option<UnfurlCard>,
    },
    BackgroundChatMessagesFetched {
        chat: ChatKey,
        last_msg_ts: String,
        is_group: bool,
        result: Result<Vec<postbox_hub::HubMessage>, HubError>,
    },
}

struct SyncEngine {
    client: Arc<HubClient>,
    store: Store,
    worker: SyncWorkerHandle,
    open_chat: Option<ChatKey>,
    sse_parser: SseParser,
    reconnect: SseReconnectState,
    last_sync_mono: Instant,
    last_sync_wall: i64,
    started_at: Instant,
    #[cfg(test)]
    test_clock_offset_ms: u64,
    data_dir: PathBuf,
    downloader: Arc<MediaDownloader>,
    drain_tracker: MediaDrainTracker,
    unfurl_service: Option<Arc<UnfurlService>>,
    giphy_client: Option<GiphyClient>,
    giphy_api_key: Option<String>,
    giphy_cancel_tx: Option<(u64, watch::Sender<bool>)>,
    slack_emoji_cache: HashMap<String, SlackEmojiIndex>,
    avatar_sem: Arc<tokio::sync::Semaphore>,
    avatar_in_flight: HashSet<ChatKey>,
    background_swept_chats: HashMap<ChatKey, String>,
    bg_tx: mpsc::UnboundedSender<BackgroundTaskResult>,
    bg_rx: mpsc::UnboundedReceiver<BackgroundTaskResult>,
    retry_targets: HashMap<String, (ChatKey, MediaDownloadTarget)>,
    pending_gone_events: Vec<(ChatKey, String)>,
    open_media_in_flight: usize,
    bg_media_in_flight: usize,
    bg_fetch_in_flight: HashSet<ChatKey>,
    bg_read_failures: HashMap<ChatKey, (String, u8)>,
    bg_burst_started_ms: u64,
    bg_burst_files_started: usize,
    bg_drain_pending: bool,
    deepgram_api_key: Arc<std::sync::Mutex<Option<String>>>,
    deepgram_client: DeepgramRestClient,
    format_provider: Option<FormatProviderConfig>,
    formatter: TranscriptFormatter,
    audio_worker: AudioWorkerHandle,
    dictation_controller: LiveDictationController,
    /// Milestone 6: Media Foundation + Intel Arc DXVA thumbnail extraction for downloaded videos.
    video_backend: Arc<dyn VideoBackend>,
    video_thumb_cache: postbox_media::VideoThumbnailCache,
    /// Store location, so the off-thread thumbnail job can open its own SQLite connection.
    store_loc: Option<StoreLocation>,
}

impl Drop for SyncEngine {
    fn drop(&mut self) {
        self.dictation_controller.shutdown();
        self.audio_worker.send(AudioWorkerCmd::Shutdown);
    }
}

impl SyncEngine {
    #[cfg(test)]
    fn new(client: HubClient, store: Store, worker: SyncWorkerHandle) -> Self {
        let data_dir = std::env::temp_dir().join("postbox-sync-test");
        Self::with_m3(client, store, worker, data_dir, None)
    }

    #[cfg(test)]
    fn with_m3(
        client: HubClient,
        store: Store,
        worker: SyncWorkerHandle,
        data_dir: PathBuf,
        giphy_api_key: Option<String>,
    ) -> Self {
        Self::with_m4(
            client,
            store,
            worker,
            data_dir,
            giphy_api_key,
            None,
            None,
            Arc::new(NativePlatform::default()),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn with_m4(
        client: HubClient,
        store: Store,
        worker: SyncWorkerHandle,
        data_dir: PathBuf,
        giphy_api_key: Option<String>,
        deepgram_api_key: Option<String>,
        format_provider: Option<FormatProviderConfig>,
        audio_backend: Arc<dyn AudioBackend>,
    ) -> Self {
        let data_dir_hint = data_dir.clone();
        let downloader = Arc::new(MediaDownloader::new(data_dir.join("hub-media")));
        let unfurl_service = UnfurlService::new(data_dir.join("link-previews"))
            .ok()
            .map(Arc::new);
        let giphy_client = GiphyClient::new().ok();
        let (bg_tx, bg_rx) = mpsc::unbounded_channel();
        let audio_worker =
            AudioWorkerHandle::spawn(Arc::clone(&audio_backend), worker.event_tx.clone());
        let deepgram_api_key = Arc::new(std::sync::Mutex::new(deepgram_api_key));
        let dictation_controller = LiveDictationController::new(
            audio_backend,
            Arc::clone(&deepgram_api_key),
            worker.event_tx.clone(),
        );
        let now = Instant::now();
        Self {
            client: Arc::new(client),
            store,
            worker,
            open_chat: None,
            sse_parser: SseParser::new(),
            reconnect: SseReconnectState::new(),
            last_sync_mono: now,
            last_sync_wall: now_epoch_secs(),
            started_at: now,
            #[cfg(test)]
            test_clock_offset_ms: 0,
            data_dir,
            downloader,
            drain_tracker: MediaDrainTracker::new(),
            unfurl_service,
            giphy_client,
            giphy_api_key,
            giphy_cancel_tx: None,
            slack_emoji_cache: HashMap::new(),
            avatar_sem: Arc::new(tokio::sync::Semaphore::new(4)),
            avatar_in_flight: HashSet::new(),
            background_swept_chats: HashMap::new(),
            bg_tx,
            bg_rx,
            retry_targets: HashMap::new(),
            pending_gone_events: Vec::new(),
            open_media_in_flight: 0,
            bg_media_in_flight: 0,
            bg_fetch_in_flight: HashSet::new(),
            bg_read_failures: HashMap::new(),
            bg_burst_started_ms: 0,
            bg_burst_files_started: 0,
            bg_drain_pending: false,
            deepgram_api_key,
            deepgram_client: DeepgramRestClient::new(),
            format_provider,
            formatter: TranscriptFormatter::new(),
            audio_worker,
            dictation_controller,
            video_backend: Arc::new(NativePlatform::default()),
            video_thumb_cache: postbox_media::VideoThumbnailCache::new(data_dir_hub_media(
                &data_dir_hint,
            )),
            store_loc: None,
        }
    }

    /// Extracts and caches one video thumbnail after a download completed, on the blocking pool
    /// so the sync loop never stalls and the UI thread never decodes a video.
    fn schedule_video_thumbnail(&self, path: &std::path::Path, mime_type: &str, sha256: &str) {
        if !postbox_media::is_video_mime(mime_type) || sha256.is_empty() {
            return;
        }
        let Some(store_loc) = self.store_loc.clone() else {
            return;
        };
        let backend = Arc::clone(&self.video_backend);
        let cache = self.video_thumb_cache.clone();
        let path = path.to_path_buf();
        let sha = sha256.to_owned();
        tokio::task::spawn_blocking(move || {
            let Ok(store) = store_loc.open() else {
                return;
            };
            let _ = cache.ensure(&store, backend.as_ref(), &sha, &path);
        });
    }

    fn now_media_ms(&self) -> u64 {
        let base = self.started_at.elapsed().as_millis() as u64;
        #[cfg(test)]
        {
            base.saturating_add(self.test_clock_offset_ms)
        }
        #[cfg(not(test))]
        {
            base
        }
    }

    fn next_media_wake_duration(&self) -> Option<Duration> {
        let now_ms = self.now_media_ms();
        let earliest = [
            self.drain_tracker.next_retry_due_ms(),
            self.drain_tracker.next_gone_flush_due_ms(),
        ]
        .into_iter()
        .flatten()
        .min()?;
        Some(Duration::from_millis(earliest.saturating_sub(now_ms)))
    }

    fn emit(&self, ev: SyncEvent) {
        let _ = self.worker.event_tx.try_send(ev);
    }

    async fn run(&mut self) {
        let url = self.client.endpoint().base_url();
        self.emit(SyncEvent::StatusChanged(HubConnectionStatus::Connecting {
            url: url.clone(),
        }));

        // Initial baseline chats fetch + drain any pending queues.
        if let Err(err) = self.sync_chats(true).await
            && Self::is_fatal_auth(&err)
        {
            self.emit_auth_error(&url);
            if !self.wait_for_restart_or_shutdown().await {
                return;
            }
        }
        self.drain_outbox().await;
        self.drain_pending_reads().await;
        self.drain_pending_archives().await;

        loop {
            self.sse_parser.clear_buffer();
            let connect_res = self
                .client
                .connect_events(self.sse_parser.last_event_id())
                .await;

            match connect_res {
                Ok(resp) => {
                    self.reconnect.on_connected();
                    self.emit(SyncEvent::StatusChanged(HubConnectionStatus::Connected {
                        url: url.clone(),
                        last_event_id: self.sse_parser.last_event_id_u64(),
                    }));
                    // Synthetic reset on connect (`main.zig:25085-25142`).
                    let _ = self.sync_chats(true).await;
                    if let Some(chat) = self.open_chat.clone() {
                        let _ = self.sync_chat_messages(&chat, OLDER_PAGE_LIMIT, None).await;
                    }

                    let outcome = self.drive_sse_stream(resp).await;
                    match outcome {
                        StreamExit::Shutdown => return,
                        StreamExit::RestartRequested => {
                            self.reconnect.on_connected();
                            continue;
                        }
                        StreamExit::CleanEof => {
                            let wait_ms = self.reconnect.on_clean_eof();
                            self.emit(SyncEvent::StatusChanged(
                                HubConnectionStatus::Reconnecting {
                                    url: url.clone(),
                                    attempt: 1,
                                    retry_in_secs: (wait_ms / 1_000).max(1),
                                    last_error: "stream closed".to_owned(),
                                },
                            ));
                            if !self
                                .sleep_interruptible(Duration::from_millis(wait_ms))
                                .await
                            {
                                return;
                            }
                        }
                        StreamExit::Error(err_msg) => {
                            let wait_ms = self.reconnect.on_error();
                            self.emit(SyncEvent::StatusChanged(
                                HubConnectionStatus::Reconnecting {
                                    url: url.clone(),
                                    attempt: self.reconnect.failures(),
                                    retry_in_secs: (wait_ms / 1_000).max(1),
                                    last_error: err_msg,
                                },
                            ));
                            if !self
                                .sleep_interruptible(Duration::from_millis(wait_ms))
                                .await
                            {
                                return;
                            }
                        }
                    }
                }
                Err(err) if Self::is_fatal_auth(&err) => {
                    self.emit_auth_error(&url);
                    if !self.wait_for_restart_or_shutdown().await {
                        return;
                    }
                }
                Err(err) => {
                    let wait_ms = self.reconnect.on_error();
                    self.emit(SyncEvent::StatusChanged(
                        HubConnectionStatus::Reconnecting {
                            url: url.clone(),
                            attempt: self.reconnect.failures(),
                            retry_in_secs: (wait_ms / 1_000).max(1),
                            last_error: err.to_string(),
                        },
                    ));
                    if !self
                        .sleep_interruptible(Duration::from_millis(wait_ms))
                        .await
                    {
                        return;
                    }
                }
            }
        }
    }

    fn is_fatal_auth(err: &HubError) -> bool {
        matches!(
            err,
            HubError::HttpStatus {
                status: 401 | 403,
                ..
            }
        )
    }

    fn emit_auth_error(&self, url: &str) {
        self.emit(SyncEvent::StatusChanged(HubConnectionStatus::Error {
            url: Some(url.to_owned()),
            message:
                "Hub rejected credentials (HTTP 401/403). Check POSTBOX_HUB_TOKEN / WAZIG_HUB_TOKEN."
                    .to_owned(),
        }));
    }

    async fn wait_for_restart_or_shutdown(&mut self) -> bool {
        loop {
            tokio::select! {
                cmd = self.worker.cmd_rx.recv() => {
                    match cmd {
                        Ok(SyncCommand::Shutdown) | Err(_) => return false,
                        Ok(SyncCommand::RestartLiveSync) => return true,
                        Ok(other) => {
                            self.handle_command(other).await;
                        }
                    }
                }
                Some(bg) = self.bg_rx.recv() => {
                    self.handle_bg_result(bg);
                }
            }
        }
    }

    async fn sleep_interruptible(&mut self, dur: Duration) -> bool {
        let sleep = tokio::time::sleep(dur);
        tokio::pin!(sleep);
        loop {
            let media_wake_opt = self.next_media_wake_duration();
            let media_sleep = async {
                match media_wake_opt {
                    Some(d) => tokio::time::sleep(d).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(media_sleep);

            tokio::select! {
                _ = &mut sleep => return true,
                cmd = self.worker.cmd_rx.recv() => {
                    match cmd {
                        Ok(SyncCommand::Shutdown) | Err(_) => return false,
                        Ok(SyncCommand::RestartLiveSync) => return true,
                        Ok(other) => {
                            self.handle_command(other).await;
                        }
                    }
                }
                Some(bg) = self.bg_rx.recv() => {
                    self.handle_bg_result(bg);
                }
                _ = &mut media_sleep => {
                    self.process_media_timers();
                }
            }
        }
    }

    async fn drive_sse_stream(&mut self, mut stream: EventStream) -> StreamExit {
        let mut safety_interval = tokio::time::interval(Duration::from_secs(10));
        safety_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            let outbox_sleep_dur = self.next_outbox_sleep_duration();
            let outbox_sleep = tokio::time::sleep(outbox_sleep_dur);
            tokio::pin!(outbox_sleep);

            let media_wake_opt = self.next_media_wake_duration();
            let media_sleep = async {
                match media_wake_opt {
                    Some(d) => tokio::time::sleep(d).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(media_sleep);

            tokio::select! {
                chunk_opt = stream.next_chunk() => {
                    match chunk_opt {
                        Some(Ok(chunk)) => {
                            match self.sse_parser.push_bytes(&chunk) {
                                Ok(events) if !events.is_empty() => {
                                    let is_reset = events.iter().any(|e| e.kind == "reset");
                                    if is_reset {
                                        let _ = self.sync_chats_for_provider(true, None).await;
                                        if let Some(chat) = self.open_chat.clone() {
                                            let _ = self.sync_chat_messages(&chat, OLDER_PAGE_LIMIT, None).await;
                                        }
                                    } else {
                                        let target_provider = sse_batch_target_provider(&events);
                                        if let Some(chat) = self.open_chat.clone()
                                            && sse_batch_hits_open_chat(&events, &chat)
                                        {
                                            let _ = self.sync_chat_messages(&chat, OLDER_PAGE_LIMIT, None).await;
                                        }
                                        let _ = self.sync_chats_for_provider(false, target_provider).await;
                                    }
                                    self.emit(SyncEvent::StatusChanged(HubConnectionStatus::Connected {
                                        url: self.client.endpoint().base_url(),
                                        last_event_id: self.sse_parser.last_event_id_u64(),
                                    }));
                                }
                                Ok(_) => {}
                                Err(err) => return StreamExit::Error(err.to_string()),
                            }
                        }
                        Some(Err(err)) => return StreamExit::Error(err.to_string()),
                        None => return StreamExit::CleanEof,
                    }
                }
                cmd_res = self.worker.cmd_rx.recv() => {
                    match cmd_res {
                        Ok(SyncCommand::Shutdown) | Err(_) => return StreamExit::Shutdown,
                        Ok(SyncCommand::RestartLiveSync) => return StreamExit::RestartRequested,
                        Ok(cmd) => {
                            self.handle_command(cmd).await;
                        }
                    }
                }
                Some(bg) = self.bg_rx.recv() => {
                    self.handle_bg_result(bg);
                }
                _ = &mut media_sleep => {
                    self.process_media_timers();
                }
                _ = &mut outbox_sleep => {
                    self.drain_outbox().await;
                    self.drain_pending_reads().await;
                    self.drain_pending_archives().await;
                }
                _ = safety_interval.tick() => {
                    let mono_ms = self.last_sync_mono.elapsed().as_millis() as u64;
                    let wall_secs = now_epoch_secs().saturating_sub(self.last_sync_wall);
                    if should_run_safety_poll(mono_ms, wall_secs) {
                        let _ = self.sync_chats(false).await;
                        if let Some(chat) = self.open_chat.clone() {
                            let _ = self.sync_chat_messages(&chat, OLDER_PAGE_LIMIT, None).await;
                        }
                        self.drain_outbox().await;
                        self.drain_pending_reads().await;
                        self.drain_pending_archives().await;
                    }
                }
            }
        }
    }

    fn next_outbox_sleep_duration(&self) -> Duration {
        let now_ms = now_epoch_millis();
        let candidates = [
            self.store.next_outbox_ready_at_ms().ok().flatten(),
            self.store.next_pending_archive_ready_at_ms().ok().flatten(),
            self.store
                .next_pending_read_ready_at_ms(now_ms)
                .ok()
                .flatten(),
        ];
        match candidates.into_iter().flatten().min() {
            Some(ready_at) => {
                if ready_at <= now_ms {
                    Duration::from_millis(50)
                } else {
                    Duration::from_millis((ready_at - now_ms).min(60_000) as u64)
                }
            }
            None => Duration::from_secs(60),
        }
    }

    async fn handle_command(&mut self, cmd: SyncCommand) {
        match cmd {
            SyncCommand::RefreshAll { open_chat } => {
                if let Some(c) = open_chat {
                    self.open_chat = Some(c);
                }
                let _ = self.sync_chats(true).await;
                if let Some(chat) = self.open_chat.clone() {
                    let _ = self.sync_chat_messages(&chat, OLDER_PAGE_LIMIT, None).await;
                }
                self.drain_outbox().await;
                self.drain_pending_reads().await;
                self.drain_pending_archives().await;
            }
            SyncCommand::FetchChatMessages { chat, limit } => {
                self.open_chat = Some(chat.clone());
                let _ = self.sync_chat_messages(&chat, limit, None).await;
            }
            SyncCommand::FetchOlderMessages {
                chat,
                before_cursor,
                limit,
            } => {
                let _ = self
                    .sync_chat_messages(&chat, limit, Some(before_cursor.as_str()))
                    .await;
            }
            SyncCommand::WakeOutbox => {
                self.drain_outbox().await;
            }
            SyncCommand::WakeMarkRead => {
                self.drain_pending_reads().await;
            }
            SyncCommand::WakeArchive => {
                self.drain_pending_archives().await;
            }
            SyncCommand::DeleteMessage {
                chat,
                msg_id,
                client_id,
            } => {
                match self
                    .client
                    .delete_message(&chat.id, &msg_id, &client_id)
                    .await
                {
                    Ok(()) => {
                        let _ = self.sync_chat_messages(&chat, OLDER_PAGE_LIMIT, None).await;
                        self.emit(SyncEvent::Notice("Message deleted".into()));
                    }
                    Err(err) => {
                        self.emit(SyncEvent::Notice(format!("Delete failed: {err}")));
                    }
                }
            }
            SyncCommand::DownloadChatMedia { chat, items } => {
                self.open_chat = Some(chat.clone());
                self.enqueue_media_targets(&chat, &items, true, false);
                self.maybe_start_background_drain();
            }
            SyncCommand::DownloadMediaExplicit { chat, target } => {
                let key = media_cache_key(&chat, &target.msg_id);
                self.drain_tracker.forget_attempt(&key);
                self.retry_targets.remove(&key);
                self.enqueue_media_targets(&chat, &[target], false, false);
            }
            SyncCommand::FetchAvatar { chat } => {
                let avatar_dir = self.data_dir.join("avatars");
                match resolve_avatar_cache_or_seed(
                    &self.store,
                    &avatar_dir,
                    default_wacli_remote_avatars_dir().as_deref(),
                    &chat,
                    now_epoch_secs(),
                ) {
                    Ok(AvatarCacheLookup::Ready(path)) => {
                        self.emit(SyncEvent::AvatarReady {
                            chat,
                            path: Some(path),
                        });
                    }
                    Ok(AvatarCacheLookup::Missing) => {
                        self.emit(SyncEvent::AvatarReady { chat, path: None });
                    }
                    Ok(AvatarCacheLookup::NeedsFetch { fallback_path }) => {
                        if let Some(path) = fallback_path {
                            self.emit(SyncEvent::AvatarReady {
                                chat: chat.clone(),
                                path: Some(path),
                            });
                        }
                        if self.avatar_in_flight.insert(chat.clone()) {
                            let client = Arc::clone(&self.client);
                            let avatar_sem = Arc::clone(&self.avatar_sem);
                            let bg_tx = self.bg_tx.clone();
                            tokio::spawn(async move {
                                let _permit = avatar_sem
                                    .acquire_owned()
                                    .await
                                    .expect("avatar semaphore is never closed");
                                let mut outcome = Ok(AvatarFetchState::Pending);
                                for attempt in 0..3 {
                                    if attempt > 0 {
                                        tokio::time::sleep(Duration::from_secs(15)).await;
                                    }
                                    outcome = client.fetch_avatar_state(&chat.id).await;
                                    if !matches!(outcome, Ok(AvatarFetchState::Pending)) {
                                        break;
                                    }
                                }
                                let _ = bg_tx
                                    .send(BackgroundTaskResult::AvatarFetched { chat, outcome });
                            });
                        }
                    }
                    Err(_) => {}
                }
            }
            SyncCommand::FetchWorkspaceIcon { team } => {
                let avatar_dir = self.data_dir.join("avatars");
                if let Ok(path) = fetch_or_load_workspace_icon(
                    &self.client,
                    &self.store,
                    &avatar_dir,
                    &team,
                    now_epoch_secs(),
                )
                .await
                {
                    self.emit(SyncEvent::WorkspaceIconReady { team, path });
                }
            }
            SyncCommand::EnsureSlackEmojiIndex { team } => {
                let now = now_epoch_secs();
                if let Some(cached) = self.slack_emoji_cache.get(&team)
                    && cached.is_fresh(now)
                {
                    self.emit(SyncEvent::SlackEmojiIndexReady {
                        team,
                        index: cached.clone(),
                    });
                    return;
                }
                if let Ok(json_val) = self.client.fetch_slack_emoji_index(&team).await {
                    let index = SlackEmojiIndex::from_hub_json(&team, &json_val, now);
                    self.slack_emoji_cache.insert(team.clone(), index.clone());
                    self.emit(SyncEvent::SlackEmojiIndexReady { team, index });
                }
            }
            SyncCommand::FetchSlackEmojiMedia { team, name, url } => {
                let dir = self.data_dir.join("slack-emoji");
                if let Ok(Some(path)) =
                    fetch_or_load_slack_emoji_media(&self.client, &dir, &team, &name, &url).await
                {
                    self.emit(SyncEvent::SlackEmojiMediaReady { team, name, path });
                }
            }
            SyncCommand::ReactToMessage {
                chat,
                msg_id,
                emoji,
                client_id,
            } => {
                let res = self
                    .client
                    .react(&chat.id, &msg_id, &emoji, &client_id)
                    .await;
                let (ok, http_status) = match res {
                    Ok(_) => (true, Some(200)),
                    Err(HubError::HttpStatus { status, .. }) => (false, Some(status)),
                    Err(_) => (false, None),
                };
                self.emit(SyncEvent::ReactionSettled {
                    chat,
                    msg_id,
                    emoji,
                    ok,
                    http_status,
                });
            }
            SyncCommand::UnfurlUrl { target } => {
                let Some(svc) = self.unfurl_service.clone() else {
                    self.emit(SyncEvent::UnfurlReady { target, card: None });
                    return;
                };
                if let Some(cached) = svc.load_from_disk(&target) {
                    self.emit(SyncEvent::UnfurlReady {
                        target,
                        card: Some(cached),
                    });
                    return;
                }
                let bg_tx = self.bg_tx.clone();
                tokio::spawn(async move {
                    let card = svc.fetch_or_load(&target).await.ok();
                    let _ = bg_tx.send(BackgroundTaskResult::UnfurlDone { target, card });
                });
            }
            SyncCommand::SearchGiphy {
                query,
                offset,
                generation,
                target,
            } => {
                // Cancel any prior in-flight GIPHY search.
                if let Some((_prev_gen, prev_tx)) = self.giphy_cancel_tx.take() {
                    let _ = prev_tx.send(true);
                }
                let Some(api_key) = self.giphy_api_key.clone() else {
                    self.emit(SyncEvent::GiphyResults {
                        generation,
                        target,
                        page: None,
                        error: Some(GIPHY_MSG_NEEDS_KEY.to_owned()),
                    });
                    return;
                };
                let Some(giphy) = self.giphy_client.clone() else {
                    return;
                };
                let (cancel_tx, mut cancel_rx) = watch::channel(false);
                self.giphy_cancel_tx = Some((generation, cancel_tx));
                let event_tx = self.worker.event_tx.clone();
                let preview_dir = self.data_dir.join("giphy-previews");
                tokio::spawn(async move {
                    match giphy
                        .search_or_trending(
                            &api_key,
                            &query,
                            GIPHY_DEFAULT_LIMIT,
                            offset,
                            &mut cancel_rx,
                        )
                        .await
                    {
                        Ok(page) => {
                            let preview_urls: Vec<(usize, String, String)> = page
                                .items
                                .iter()
                                .enumerate()
                                .map(|(i, item)| (i, item.id.clone(), item.preview.url.clone()))
                                .collect();
                            let _ = event_tx.try_send(SyncEvent::GiphyResults {
                                generation,
                                target,
                                page: Some(page),
                                error: None,
                            });
                            for (idx, gif_id, url) in preview_urls {
                                if *cancel_rx.borrow() {
                                    break;
                                }
                                if let Ok(bytes) = giphy
                                    .download_gif(&gif_id, &url, true, &mut cancel_rx)
                                    .await
                                {
                                    let p = preview_dir.join(format!("{gif_id}.gif"));
                                    if atomic_write_file(&p, &bytes).is_ok() {
                                        let _ = event_tx.try_send(SyncEvent::GiphyPreviewReady {
                                            generation,
                                            index: idx,
                                            path: p,
                                        });
                                    }
                                }
                            }
                        }
                        Err(postbox_media::MediaError::Cancelled) => {}
                        Err(err) => {
                            let msg = giphy_error_message(&err, false).to_owned();
                            let _ = event_tx.try_send(SyncEvent::GiphyResults {
                                generation,
                                target,
                                page: None,
                                error: Some(msg),
                            });
                        }
                    }
                });
            }
            SyncCommand::DownloadGiphyGif {
                gif_id,
                url,
                index,
                generation,
                download_generation,
                target,
            } => {
                let Some(giphy) = self.giphy_client.clone() else {
                    return;
                };
                let (cancel_tx, mut cancel_rx) = watch::channel(false);
                if let Some((_prev_gen, prev_tx)) =
                    self.giphy_cancel_tx.replace((generation, cancel_tx))
                {
                    let _ = prev_tx.send(true);
                }
                let event_tx = self.worker.event_tx.clone();
                tokio::spawn(async move {
                    match giphy
                        .download_gif(&gif_id, &url, false, &mut cancel_rx)
                        .await
                    {
                        Ok(bytes) => {
                            let _ = event_tx.try_send(SyncEvent::GiphyDownloadReady {
                                generation,
                                download_generation,
                                target,
                                index,
                                gif_id,
                                bytes,
                            });
                        }
                        Err(postbox_media::MediaError::Cancelled) => {}
                        Err(err) => {
                            let msg = giphy_error_message(&err, true).to_owned();
                            let _ = event_tx.try_send(SyncEvent::GiphyError {
                                generation,
                                index,
                                download_generation,
                                message: msg,
                            });
                        }
                    }
                });
            }
            SyncCommand::CancelGiphy { generation } => {
                if let Some((cur_gen, tx)) = self.giphy_cancel_tx.take() {
                    if cur_gen == generation || generation == 0 {
                        let _ = tx.send(true);
                    } else {
                        self.giphy_cancel_tx = Some((cur_gen, tx));
                    }
                }
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
                if !sha256.is_empty()
                    && let Ok(Some(rec)) = self.store.get_transcript_record(&sha256)
                {
                    self.emit(SyncEvent::TranscriptReady {
                        chat,
                        msg_id,
                        sha256,
                        raw_text: rec.raw_text,
                        formatted_text: rec.formatted_text,
                        language: rec.language,
                    });
                } else {
                    spawn_transcription_task(
                        self.deepgram_client.clone(),
                        Arc::clone(&self.deepgram_api_key),
                        chat,
                        msg_id,
                        path,
                        sha256,
                        mime_type,
                        language,
                        self.worker.event_tx.clone(),
                    );
                }
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
                persist_transcript_to_store(&self.store, &sha256, "auto", &raw_text);
                spawn_format_task(
                    self.formatter.clone(),
                    self.format_provider.clone(),
                    self.data_dir.clone(),
                    chat,
                    msg_id,
                    sha256,
                    raw_text,
                    from_me,
                    contact_name,
                    local_path,
                    self.worker.event_tx.clone(),
                );
            }
            SyncCommand::VoicePlay {
                chat,
                msg_id,
                path,
                speed,
                start_ms,
            } => self.audio_worker.send(AudioWorkerCmd::Play {
                chat,
                msg_id,
                path,
                speed,
                start_ms,
            }),
            SyncCommand::VoicePause => self.audio_worker.send(AudioWorkerCmd::Pause),
            SyncCommand::VoiceResume => self.audio_worker.send(AudioWorkerCmd::Resume),
            SyncCommand::VoiceStop => self.audio_worker.send(AudioWorkerCmd::Stop),
            SyncCommand::VoiceSeek { pos_ms } => {
                self.audio_worker.send(AudioWorkerCmd::Seek { pos_ms })
            }
            SyncCommand::VoiceSetSpeed(speed) => {
                self.audio_worker.send(AudioWorkerCmd::SetSpeed(speed))
            }
            SyncCommand::VoiceRecordStart {
                generation,
                origin_chat,
                preferred_mic,
            } => self.audio_worker.send(AudioWorkerCmd::RecordStart {
                generation,
                origin_chat,
                preferred_mic,
            }),
            SyncCommand::VoiceRecordFinish => self.audio_worker.send(AudioWorkerCmd::RecordFinish),
            SyncCommand::VoiceRecordCancel => self.audio_worker.send(AudioWorkerCmd::RecordCancel),
            SyncCommand::RefreshMicrophones => {
                self.audio_worker.send(AudioWorkerCmd::RefreshMicrophones)
            }
            SyncCommand::DictationStart {
                generation,
                language,
                preferred_mic,
                ..
            } => {
                self.dictation_controller
                    .start(generation, language, preferred_mic);
            }
            SyncCommand::DictationStop { generation } => {
                self.dictation_controller.stop(generation);
            }
            SyncCommand::DictationCancel { generation } => {
                self.dictation_controller.cancel(generation);
            }
            SyncCommand::SaveDictationLanguage(lang) => {
                persist_dictation_language_pref(Some(&self.store), lang);
            }
            SyncCommand::SaveMicrophonePreference(mic) => {
                persist_microphone_pref(Some(&self.store), mic.as_deref());
            }
            SyncCommand::Shutdown => {
                self.dictation_controller.shutdown();
                self.audio_worker.send(AudioWorkerCmd::Shutdown);
            }
            SyncCommand::RestartLiveSync => {}
        }
    }

    fn enqueue_media_targets(
        &mut self,
        chat: &ChatKey,
        items: &[MediaDownloadTarget],
        automatic: bool,
        is_background: bool,
    ) -> usize {
        let now_ms = self.now_media_ms();
        let mut spawned = 0usize;
        for target in items {
            if automatic && target.file_size.is_some_and(is_oversized) {
                self.emit(SyncEvent::MediaStatusChanged {
                    chat: chat.clone(),
                    msg_id: target.msg_id.clone(),
                    status: MediaItemStatus::Oversized,
                });
                continue;
            }
            let key = media_cache_key(chat, &target.msg_id);
            if self.drain_tracker.is_in_flight(&key) {
                continue;
            }
            if automatic
                && self
                    .store
                    .is_media_gone(chat, &target.msg_id)
                    .unwrap_or(false)
            {
                if !self.drain_tracker.should_skip_auto(&key, now_ms) {
                    self.handle_gone_outcome(
                        chat.clone(),
                        target.msg_id.clone(),
                        &key,
                        now_ms,
                        false,
                    );
                }
                continue;
            }
            if let Some(hit) =
                self.downloader
                    .resolve_cached_entry(&self.store, chat, &target.msg_id)
            {
                self.drain_tracker.forget_attempt(&key);
                self.retry_targets.remove(&key);
                self.emit(SyncEvent::MediaDownloaded {
                    chat: chat.clone(),
                    msg_id: target.msg_id.clone(),
                    path: hit.path,
                    mime_type: effective_cached_mime(target.mime_type.as_deref(), &hit.mime_type),
                    sha256: hit.sha256,
                });
                continue;
            }
            if automatic && self.drain_tracker.should_skip_auto(&key, now_ms) {
                continue;
            }
            self.spawn_media_download(chat.clone(), target.clone(), key, automatic, is_background);
            spawned += 1;
        }
        spawned
    }

    fn spawn_media_download(
        &mut self,
        chat: ChatKey,
        target: MediaDownloadTarget,
        key: String,
        automatic: bool,
        is_background: bool,
    ) {
        self.drain_tracker.mark_in_flight(key);
        if is_background {
            self.bg_media_in_flight = self.bg_media_in_flight.saturating_add(1);
        } else {
            self.open_media_in_flight = self.open_media_in_flight.saturating_add(1);
        }
        let spec = MediaDownloadSpec {
            chat: chat.clone(),
            msg_id: target.msg_id.clone(),
            filename: target.filename.clone(),
            mime_type: target.mime_type.clone(),
            media_kind: target.media_kind,
        };
        let downloader = self.downloader.clone();
        let client = self.client.clone();
        let bg_tx = self.bg_tx.clone();
        tokio::spawn(async move {
            let result = downloader.download_to_cache_owned(client, None, spec).await;
            let _ = bg_tx.send(BackgroundTaskResult::MediaDownloadDone {
                chat,
                target,
                automatic,
                is_background,
                result,
            });
        });
    }

    fn handle_gone_outcome(
        &mut self,
        chat: ChatKey,
        msg_id: String,
        key: &str,
        now_ms: u64,
        force_emit: bool,
    ) {
        self.drain_tracker.note_gone(key);
        if self.drain_tracker.should_repaint_gone(now_ms) {
            let pending = std::mem::take(&mut self.pending_gone_events);
            let mut emitted_current = false;
            for (p_chat, p_msg_id) in pending {
                if p_chat == chat && p_msg_id == msg_id {
                    emitted_current = true;
                }
                self.emit(SyncEvent::MediaStatusChanged {
                    chat: p_chat,
                    msg_id: p_msg_id,
                    status: MediaItemStatus::Gone,
                });
            }
            if !emitted_current {
                self.emit(SyncEvent::MediaStatusChanged {
                    chat,
                    msg_id,
                    status: MediaItemStatus::Gone,
                });
            }
        } else if force_emit {
            self.emit(SyncEvent::MediaStatusChanged {
                chat,
                msg_id,
                status: MediaItemStatus::Gone,
            });
        } else if !self
            .pending_gone_events
            .iter()
            .any(|(c, m)| c == &chat && m == &msg_id)
        {
            self.pending_gone_events.push((chat, msg_id));
        }
    }

    fn handle_bg_result(&mut self, bg: BackgroundTaskResult) {
        match bg {
            BackgroundTaskResult::AvatarFetched { chat, outcome } => {
                self.avatar_in_flight.remove(&chat);
                if matches!(outcome, Ok(AvatarFetchState::Pending)) {
                    return;
                }
                let avatar_dir = self.data_dir.join("avatars");
                if let Ok(path) = commit_avatar_fetch_result(
                    &self.store,
                    &avatar_dir,
                    &chat,
                    now_epoch_secs(),
                    outcome,
                ) {
                    self.emit(SyncEvent::AvatarReady { chat, path });
                }
            }
            BackgroundTaskResult::MediaDownloadDone {
                chat,
                target,
                automatic,
                is_background,
                result,
            } => {
                if is_background {
                    self.bg_media_in_flight = self.bg_media_in_flight.saturating_sub(1);
                } else {
                    self.open_media_in_flight = self.open_media_in_flight.saturating_sub(1);
                }
                let now_ms = self.now_media_ms();
                let key = media_cache_key(&chat, &target.msg_id);
                let outcome = match result {
                    Ok(cached) => MediaDownloader::commit_download_result(
                        &self.store,
                        &chat,
                        &target.msg_id,
                        cached,
                    ),
                    Err(err) => Err(err),
                };
                match outcome {
                    Ok(DownloadOutcome::Ready {
                        path,
                        mime_type,
                        sha256,
                    }) => {
                        // Milestone 6: cache the thumbnail now, keyed by the video's SHA-256.
                        self.schedule_video_thumbnail(&path, &mime_type, &sha256);
                        self.drain_tracker.note_success(&key);
                        self.retry_targets.remove(&key);
                        self.emit(SyncEvent::MediaDownloaded {
                            chat,
                            msg_id: target.msg_id,
                            path,
                            mime_type,
                            sha256,
                        });
                    }
                    Ok(DownloadOutcome::Gone) => {
                        self.retry_targets.remove(&key);
                        self.handle_gone_outcome(chat, target.msg_id, &key, now_ms, !automatic);
                    }
                    Ok(DownloadOutcome::Unavailable) => {
                        let st = self.drain_tracker.note_failure(&key, 404, now_ms);
                        if matches!(st, MediaItemStatus::RetryPending { .. }) {
                            self.retry_targets
                                .insert(key, (chat.clone(), target.clone()));
                        } else {
                            self.retry_targets.remove(&key);
                        }
                        self.emit(SyncEvent::MediaStatusChanged {
                            chat,
                            msg_id: target.msg_id,
                            status: st,
                        });
                    }
                    Ok(DownloadOutcome::TransientFailure { http_status })
                    | Ok(DownloadOutcome::OtherFailure { http_status }) => {
                        let st = self.drain_tracker.note_failure(&key, http_status, now_ms);
                        if matches!(st, MediaItemStatus::RetryPending { .. }) {
                            self.retry_targets
                                .insert(key, (chat.clone(), target.clone()));
                        } else {
                            self.retry_targets.remove(&key);
                        }
                        self.emit(SyncEvent::MediaStatusChanged {
                            chat,
                            msg_id: target.msg_id,
                            status: st,
                        });
                    }
                    Err(_) => {
                        let st = self.drain_tracker.note_failure(&key, 0, now_ms);
                        if matches!(st, MediaItemStatus::RetryPending { .. }) {
                            self.retry_targets
                                .insert(key, (chat.clone(), target.clone()));
                        } else {
                            self.retry_targets.remove(&key);
                        }
                        self.emit(SyncEvent::MediaStatusChanged {
                            chat,
                            msg_id: target.msg_id,
                            status: st,
                        });
                    }
                }
                if self.bg_drain_pending
                    && self.open_media_in_flight == 0
                    && self.bg_media_in_flight == 0
                {
                    self.maybe_start_background_drain();
                }
            }
            BackgroundTaskResult::UnfurlDone { target, card } => {
                self.emit(SyncEvent::UnfurlReady { target, card });
            }
            BackgroundTaskResult::BackgroundChatMessagesFetched {
                chat,
                last_msg_ts,
                is_group,
                result,
            } => {
                self.bg_fetch_in_flight.remove(&chat);
                match result {
                    Ok(hub_msgs) => {
                        self.bg_read_failures.remove(&chat);
                        let core_msgs: Vec<Message> = hub_msgs
                            .iter()
                            .filter(|m| is_real_message(m))
                            .map(|m| m.to_core_message(&chat, is_group))
                            .collect();
                        let _ = self.store.upsert_messages_batch(&core_msgs);
                        let _ = self.store.shrink_memory();
                        if core_msgs.is_empty() {
                            self.background_swept_chats.insert(chat, last_msg_ts);
                        } else {
                            self.bg_drain_pending = true;
                            self.maybe_start_background_drain();
                        }
                    }
                    Err(_) => {
                        let entry = self
                            .bg_read_failures
                            .entry(chat)
                            .or_insert_with(|| (last_msg_ts.clone(), 0));
                        if entry.0 != last_msg_ts {
                            entry.0 = last_msg_ts;
                            entry.1 = 1;
                        } else {
                            entry.1 = entry.1.saturating_add(1);
                        }
                    }
                }
            }
        }
    }

    fn process_media_timers(&mut self) {
        let now_ms = self.now_media_ms();
        if self.drain_tracker.should_repaint_gone(now_ms) {
            let pending = std::mem::take(&mut self.pending_gone_events);
            for (chat, msg_id) in pending {
                self.emit(SyncEvent::MediaStatusChanged {
                    chat,
                    msg_id,
                    status: MediaItemStatus::Gone,
                });
            }
        }
        for key in self.drain_tracker.take_due_retries(now_ms) {
            let Some((chat, target)) = self.retry_targets.get(&key).cloned() else {
                self.drain_tracker.clear_in_flight(&key);
                self.drain_tracker.forget_attempt(&key);
                continue;
            };
            if self
                .store
                .is_media_gone(&chat, &target.msg_id)
                .unwrap_or(false)
            {
                self.retry_targets.remove(&key);
                self.handle_gone_outcome(chat, target.msg_id, &key, now_ms, false);
                continue;
            }
            if let Some(hit) =
                self.downloader
                    .resolve_cached_entry(&self.store, &chat, &target.msg_id)
            {
                self.drain_tracker.note_success(&key);
                self.drain_tracker.forget_attempt(&key);
                self.retry_targets.remove(&key);
                self.emit(SyncEvent::MediaDownloaded {
                    chat,
                    msg_id: target.msg_id,
                    path: hit.path,
                    mime_type: effective_cached_mime(target.mime_type.as_deref(), &hit.mime_type),
                    sha256: hit.sha256,
                });
                continue;
            }
            let is_background = self.open_chat.as_ref() != Some(&chat);
            self.spawn_media_download(chat, target, key, true, is_background);
        }
    }

    fn has_due_open_chat_retry(&self, now_ms: u64) -> bool {
        let Some(open) = &self.open_chat else {
            return false;
        };
        self.retry_targets.iter().any(|(key, (chat, _))| {
            chat == open && !self.drain_tracker.should_skip_auto(key, now_ms)
        })
    }

    /// Background 14-day recent-chat media drain yielding to the open chat (up to 4 files / 20s
    /// burst), fetching messages from Hub for unopened 0-message recent chats instead of
    /// prematurely marking them swept.
    fn maybe_start_background_drain(&mut self) {
        let now_ms = self.now_media_ms();
        if self.bg_burst_files_started == 0
            || now_ms.saturating_sub(self.bg_burst_started_ms)
                >= postbox_media::drain::BURST_BUDGET_MS
        {
            self.bg_burst_started_ms = now_ms;
            self.bg_burst_files_started = 0;
        }
        let active = self.open_media_in_flight + self.bg_media_in_flight;
        let open_ready = self.has_due_open_chat_retry(now_ms);
        if !background_may_start(
            active,
            open_ready,
            self.bg_burst_files_started,
            now_ms,
            self.bg_burst_started_ms,
        ) {
            if self.open_media_in_flight > 0 || open_ready {
                self.bg_drain_pending = true;
            }
            return;
        }

        let now_s = now_epoch_secs();
        let cutoff = format_utc_rfc3339(now_s - MEDIA_EVICTION_DAYS * 86_400);
        let Ok(recent) = self.store.list_recent_chats_within_days(&cutoff, 8) else {
            self.bg_drain_pending = false;
            return;
        };

        for chat in recent {
            if !background_chat_due(
                self.open_chat.as_ref() == Some(&chat.key),
                &chat.last_msg_ts,
                self.background_swept_chats
                    .get(&chat.key)
                    .map(String::as_str),
                now_s,
            ) {
                continue;
            }
            if self.bg_fetch_in_flight.contains(&chat.key) {
                continue;
            }
            if self.bg_read_failures.get(&chat.key).is_some_and(|(ts, n)| {
                ts == &chat.last_msg_ts && *n >= MAX_BACKGROUND_READ_FAILURES
            }) {
                continue;
            }
            let Ok(msgs) = self
                .store
                .list_messages(&chat.key, BACKGROUND_MEDIA_LIMIT, None)
            else {
                continue;
            };
            if msgs.is_empty() {
                self.bg_fetch_in_flight.insert(chat.key.clone());
                let client = self.client.clone();
                let bg_tx = self.bg_tx.clone();
                let chat_key = chat.key.clone();
                let last_msg_ts = chat.last_msg_ts.clone();
                let is_group = chat.is_group;
                tokio::spawn(async move {
                    let result = client
                        .fetch_messages(&chat_key.id, BACKGROUND_MEDIA_LIMIT, None)
                        .await;
                    let _ = bg_tx.send(BackgroundTaskResult::BackgroundChatMessagesFetched {
                        chat: chat_key,
                        last_msg_ts,
                        is_group,
                        result,
                    });
                });
                return;
            }

            let mut spawned_one = false;
            for msg in msgs.iter().rev() {
                if !is_downloadable_message(msg) || msg.file_size.is_some_and(is_oversized) {
                    continue;
                }
                if self
                    .store
                    .is_media_gone(&chat.key, &msg.msg_id)
                    .unwrap_or(false)
                {
                    continue;
                }
                let key = media_cache_key(&chat.key, &msg.msg_id);
                if self
                    .downloader
                    .resolve_cached_entry(&self.store, &chat.key, &msg.msg_id)
                    .is_some()
                {
                    continue;
                }
                if self.drain_tracker.should_skip_auto(&key, now_ms) {
                    continue;
                }
                let target = MediaDownloadTarget {
                    msg_id: msg.msg_id.clone(),
                    filename: msg.filename.clone(),
                    mime_type: msg.mime_type.clone(),
                    media_kind: msg.media_kind,
                    file_size: msg.file_size,
                };
                self.spawn_media_download(chat.key.clone(), target, key, true, true);
                self.bg_burst_files_started += 1;
                self.bg_drain_pending = true;
                spawned_one = true;
                break;
            }
            if spawned_one {
                return;
            }
            self.background_swept_chats
                .insert(chat.key, chat.last_msg_ts);
        }
        self.bg_drain_pending = false;
    }

    async fn sync_chats(&mut self, reset: bool) -> Result<(), HubError> {
        self.sync_chats_for_provider(reset, None).await
    }

    async fn sync_chats_for_provider(
        &mut self,
        reset: bool,
        provider: Option<Provider>,
    ) -> Result<(), HubError> {
        let provider = if reset { None } else { provider };
        let hub_chats = self
            .client
            .fetch_archived_and_inbox_chats_for_provider(MAX_ROWS, provider)
            .await?;
        let now_s = now_epoch_secs();
        let now_ms = now_epoch_millis();

        let mut archive_table = self.store.load_archive_table().unwrap_or_default();
        let mut read_state = self.store.load_read_state().unwrap_or_default();
        let existing_accounts: HashMap<(Provider, String), Account> = self
            .store
            .list_accounts()
            .unwrap_or_default()
            .into_iter()
            .map(|a| ((a.provider, a.account_id.clone()), a))
            .collect();

        let mut accounts_to_upsert: BTreeMap<(Provider, String), Account> = BTreeMap::new();
        let mut core_chats = Vec::with_capacity(hub_chats.len());
        let mut probe_candidates: Vec<(ChatKey, String)> = Vec::new();
        let mut rearchive_kept: Vec<ChatKey> = Vec::new();

        for hc in &hub_chats {
            let key = hc.chat_key();
            if hc.provider == Provider::Whatsapp {
                if hc.archived {
                    archive_table.observe_provider_archived(&key, &hc.last_message_ts);
                } else {
                    if archive_table.hold_kept(&key, false, &hc.last_message_ts) {
                        rearchive_kept.push(key.clone());
                    } else {
                        archive_table.forget_provider_unarchived(&key);
                    }
                }
                if archive_table.needs_return_probe(&key, &hc.last_message_ts) {
                    probe_candidates.push((key.clone(), hc.last_message_ts.clone()));
                }
            } else {
                archive_table.observe_local(&key, &hc.last_message_ts);
            }

            let last_ts_sec = unix_seconds(&hc.last_message_ts).unwrap_or(0);
            if last_ts_sec > 0 {
                read_state.observe_chat(&key, last_ts_sec, hc.unread, true);
            }

            let effective_archived = archive_table.is_archived(&key, hc.archived);
            let pinned = self.store.is_pinned(&key).unwrap_or(false);
            let mut chat = hc.to_core_chat(effective_archived && !hc.archived, pinned);
            chat.archived_remote = hc.archived;
            if !effective_archived {
                chat.archived_local = false;
                chat.archived_remote = false;
            }
            if last_ts_sec > 0 && read_state.get(&key).read_ts >= last_ts_sec {
                chat.unread_count = 0;
            }
            core_chats.push(chat);

            let acc_key = (hc.provider, hc.view_account.clone());
            let cached_ws = self
                .store
                .get_account_workspace_name(hc.provider, &hc.view_account)
                .ok()
                .flatten()
                .unwrap_or_default();
            let existing_label = existing_accounts.get(&acc_key).map(|a| a.label.as_str());
            let workspace_name = if hc.provider == Provider::Slack {
                let resolved = resolve_slack_workspace_name("", &hc.team_name, &cached_ws, None)
                    .unwrap_or_default();
                if !resolved.is_empty() && resolved != cached_ws {
                    let _ = self.store.set_account_workspace_name(
                        Provider::Slack,
                        &hc.view_account,
                        &resolved,
                    );
                }
                resolved
            } else {
                String::new()
            };
            let label = if !workspace_name.is_empty() {
                format_account_label(hc.provider, &hc.view_account, &workspace_name)
            } else if let Some(prev) = existing_label.filter(|l| !l.is_empty()) {
                prev.to_owned()
            } else {
                format_account_label(hc.provider, &hc.view_account, "")
            };
            accounts_to_upsert
                .entry(acc_key)
                .or_insert_with(|| Account {
                    provider: hc.provider,
                    account_id: hc.view_account.clone(),
                    label,
                    state: "connected".to_owned(),
                    sort_order: existing_accounts
                        .get(&(hc.provider, hc.view_account.clone()))
                        .map(|a| a.sort_order)
                        .unwrap_or(0),
                    last_event_at: Some(hc.last_message_ts.clone()),
                });
        }

        for acc in accounts_to_upsert.values() {
            let _ = self.store.upsert_account(acc);
        }
        let _ = self.store.upsert_chats_batch(&core_chats, now_s);
        let _ = self.store.save_archive_table(&archive_table);
        if read_state.dirty {
            let _ = self.store.save_read_state(&read_state);
        }

        for key in rearchive_kept {
            let _ = self.store.enqueue_pending_archive(
                &key,
                PendingArchiveIntent::ArchivePending,
                now_ms,
            );
        }

        // Run IGSH-902 WhatsApp resurface probes.
        let mut probed_any = false;
        let mut resurfaced_any = false;
        for (key, row_ts) in probe_candidates {
            if let Ok(msgs) = self.client.fetch_messages(&key.id, 20, None).await {
                probed_any = true;
                let probes: Vec<IncomingProbeMessage<'_>> = msgs
                    .iter()
                    .map(|m| IncomingProbeMessage {
                        from_me: m.from_me,
                        is_reaction: !m.reaction_to_id.is_empty(),
                        ts: m.ts.as_str(),
                    })
                    .collect();
                if archive_table.apply_incoming_probe(&key, &row_ts, &probes) {
                    resurfaced_any = true;
                    if let Ok(Some(mut chat)) = self.store.get_chat(&key) {
                        chat.archived_local = false;
                        chat.archived_remote = false;
                        let _ = self.store.upsert_chat(&chat, now_s);
                    }
                    let _ = self.store.enqueue_pending_archive(
                        &key,
                        PendingArchiveIntent::UnarchivePending,
                        now_ms,
                    );
                }
            }
        }
        if probed_any || resurfaced_any {
            let _ = self.store.save_archive_table(&archive_table);
        }

        self.last_sync_mono = Instant::now();
        self.last_sync_wall = now_epoch_secs();
        // M5.5 RAM diet: bulk chat sync touched thousands of pages; hand them back.
        let _ = self.store.shrink_memory();
        self.emit(SyncEvent::ChatsUpdated { reset });
        Ok(())
    }

    async fn sync_chat_messages(
        &mut self,
        chat: &ChatKey,
        limit: usize,
        before: Option<&str>,
    ) -> Result<usize, HubError> {
        let hub_msgs = self.client.fetch_messages(&chat.id, limit, before).await?;
        let is_group = self
            .store
            .get_chat(chat)
            .ok()
            .flatten()
            .is_some_and(|c| c.is_group);
        let core_msgs: Vec<Message> = hub_msgs
            .iter()
            .filter(|m| is_real_message(m))
            .map(|m| m.to_core_message(chat, is_group))
            .collect();
        let added = core_msgs.len();
        let _ = self.store.upsert_messages_batch(&core_msgs);
        // M5.5 RAM diet: release the page cache after every message batch ingest.
        let _ = self.store.shrink_memory();
        let prepended = if before.is_some() { added } else { 0 };
        let exhausted = before.is_some() && added == 0;
        self.emit(SyncEvent::MessagesUpdated {
            chat: chat.clone(),
            prepended,
            exhausted,
        });
        Ok(added)
    }

    async fn drain_outbox(&mut self) {
        loop {
            let now_ms = now_epoch_millis();
            let claimed = match self.store.claim_next_ready_outbox(now_ms) {
                Ok(Some(rec)) => rec,
                _ => break,
            };

            let send_res = match claimed.kind {
                OutboxKind::Text => {
                    self.client
                        .send_text(
                            &claimed.chat.id,
                            &claimed.text_body,
                            claimed.reply_to_id.as_deref(),
                            claimed.reply_broadcast,
                            &claimed.client_id,
                        )
                        .await
                }
                OutboxKind::File | OutboxKind::Voice => {
                    let Some(path) = claimed.staged_file_path.as_deref() else {
                        let _ = self
                            .store
                            .mark_outbox_attachment_unavailable(&claimed.client_id);
                        self.mark_optimistic_failed(&claimed.chat, &claimed.client_id);
                        self.emit(SyncEvent::OutboxUpdated {
                            chat: claimed.chat,
                            notice: Some("Attachment unavailable".into()),
                        });
                        continue;
                    };
                    let bytes = match std::fs::read(path) {
                        Ok(b) => b,
                        Err(_) => {
                            let _ = self
                                .store
                                .mark_outbox_attachment_unavailable(&claimed.client_id);
                            self.mark_optimistic_failed(&claimed.chat, &claimed.client_id);
                            self.emit(SyncEvent::OutboxUpdated {
                                chat: claimed.chat,
                                notice: Some("Attachment unavailable".into()),
                            });
                            continue;
                        }
                    };
                    let sha_ok = claimed.file_sha256.is_some_and(|sha| {
                        verify_attachment_bytes(&bytes, claimed.file_bytes, &sha)
                    });
                    if !sha_ok {
                        let _ = self
                            .store
                            .mark_outbox_attachment_unavailable(&claimed.client_id);
                        self.mark_optimistic_failed(&claimed.chat, &claimed.client_id);
                        self.emit(SyncEvent::OutboxUpdated {
                            chat: claimed.chat,
                            notice: Some("Attachment verification failed".into()),
                        });
                        continue;
                    }
                    self.client
                        .send_file(SendFileRequest {
                            chat: &claimed.chat.id,
                            text: &claimed.text_body,
                            reply_to: claimed.reply_to_id.as_deref().unwrap_or(""),
                            client_id: &claimed.client_id,
                            ptt: claimed.ptt,
                            filename: &claimed.wire_filename,
                            bytes: &bytes,
                        })
                        .await
                }
                OutboxKind::React | OutboxKind::Delete => {
                    let _ = self
                        .store
                        .acknowledge_outbox(&claimed.client_id, claimed.attempt_serial);
                    continue;
                }
            };

            match send_res {
                Ok(reply) => {
                    let _ = self
                        .store
                        .acknowledge_outbox(&claimed.client_id, claimed.attempt_serial);
                    let confirmed = Message {
                        chat: claimed.chat.clone(),
                        msg_id: if reply.msg_id.is_empty() {
                            claimed.client_id.clone()
                        } else {
                            reply.msg_id
                        },
                        sender_id: "me".into(),
                        sender_name: "You".into(),
                        from_me: true,
                        ts: if reply.ts.is_empty() {
                            postbox_core::archive::format_utc_seconds(now_epoch_secs())
                        } else {
                            reply.ts
                        },
                        body: claimed.text_body.clone(),
                        reply_to_id: claimed.reply_to_id.clone(),
                        reply_broadcast: claimed.reply_broadcast,
                        media_kind: match claimed.kind {
                            OutboxKind::Voice => Some(MediaKind::Voice),
                            OutboxKind::File => Some(MediaKind::Document),
                            OutboxKind::Text | OutboxKind::React | OutboxKind::Delete => None,
                        },
                        mime_type: (claimed.kind == OutboxKind::Voice)
                            .then(|| "audio/ogg".to_owned()),
                        filename: if claimed.wire_filename.is_empty() {
                            None
                        } else {
                            Some(claimed.wire_filename.clone())
                        },
                        file_size: (claimed.file_bytes > 0).then_some(claimed.file_bytes),
                        reactions: Vec::new(),
                        read_status: ReadStatus::Sent,
                        deleted: false,
                    };
                    let _ = self.store.replace_optimistic_message(
                        &claimed.chat,
                        &claimed.client_id,
                        &confirmed,
                    );
                    let mut downloaded_event = None;
                    if claimed.kind == OutboxKind::Voice
                        && let Some(staged) = claimed.staged_file_path.as_deref()
                        && let Ok(bytes) = std::fs::read(staged)
                    {
                        let cache_key = media_cache_key(&claimed.chat, &confirmed.msg_id);
                        let rel_path = format!("{cache_key}.bin.ogg");
                        let dest_path = self.downloader.cache_dir().join(&rel_path);
                        if let Some(parent) = dest_path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        if atomic_write_file(&dest_path, &bytes).is_ok() {
                            let sha_hex = claimed
                                .file_sha256
                                .map(|d| d.iter().map(|b| format!("{b:02x}")).collect::<String>())
                                .unwrap_or_default();
                            let entry = MediaCacheEntry {
                                cache_key,
                                sha256: sha_hex.clone(),
                                rel_path,
                                mime_type: "audio/ogg".to_owned(),
                                byte_size: bytes.len() as u64,
                                last_accessed_at: now_epoch_secs(),
                            };
                            let _ = self.store.upsert_media_cache(&entry);
                            downloaded_event = Some(SyncEvent::MediaDownloaded {
                                chat: claimed.chat.clone(),
                                msg_id: confirmed.msg_id.clone(),
                                path: dest_path,
                                mime_type: "audio/ogg".to_owned(),
                                sha256: sha_hex,
                            });
                        }
                    }
                    self.emit(SyncEvent::OutboxUpdated {
                        chat: claimed.chat,
                        notice: None,
                    });
                    if let Some(ev) = downloaded_event {
                        self.emit(ev);
                    }
                }
                Err(err) => {
                    let (status, detail) = match &err {
                        HubError::HttpStatus { status, body } => {
                            let classified = if claimed.chat.provider == Provider::Signal {
                                classify_signal_send_failure(*status, body)
                                    .map(|f| f.reason().to_owned())
                            } else if claimed.chat.provider == Provider::Telegram {
                                classify_telegram_send_failure(*status, body)
                                    .map(|f| f.reason().to_owned())
                            } else {
                                None
                            };
                            (
                                *status,
                                classified.unwrap_or_else(|| format!("HTTP {status}")),
                            )
                        }
                        HubError::SendNotConfirmed => (200, "delivery unconfirmed".to_owned()),
                        _ => (0, err.to_string()),
                    };

                    let classified =
                        classify_http_outcome(claimed.chat.provider, false, status, false, &detail);
                    let updated = match classified.outcome {
                        postbox_core::outbox::SendOutcome::Held => self.store.mark_outbox_held(
                            &claimed.client_id,
                            claimed.attempt_serial,
                            classified.ambiguous,
                            classified.reason.as_deref().unwrap_or("send failed"),
                        ),
                        postbox_core::outbox::SendOutcome::Retryable => {
                            self.store.mark_outbox_retryable(
                                &claimed.client_id,
                                claimed.attempt_serial,
                                classified.ambiguous,
                                classified.reason.as_deref().unwrap_or("send failed"),
                                now_epoch_millis(),
                            )
                        }
                        postbox_core::outbox::SendOutcome::Acknowledged => Ok(None),
                    };

                    let mut notice = None;
                    if let Ok(Some(rec)) = updated
                        && rec.phase == OutboxPhase::Held
                    {
                        self.mark_optimistic_failed(&claimed.chat, &claimed.client_id);
                        notice = rec.failure_reason.clone();
                    }
                    self.emit(SyncEvent::OutboxUpdated {
                        chat: claimed.chat,
                        notice,
                    });
                }
            }
        }
    }

    fn mark_optimistic_failed(&mut self, chat: &ChatKey, client_id: &str) {
        if let Ok(msgs) = self.store.list_messages(chat, 200, None)
            && let Some(mut msg) = msgs.into_iter().find(|m| m.msg_id == client_id)
        {
            msg.read_status = ReadStatus::Failed;
            let _ = self.store.upsert_message(&msg);
        }
    }

    async fn drain_pending_reads(&mut self) {
        let mut queue = match self.store.load_pending_reads() {
            Ok(q) => q,
            Err(_) => return,
        };
        let mut changed = false;
        let max_iters = queue.len();
        for _ in 0..max_iters {
            let now_ms = now_epoch_millis().max(0) as u64;
            let Some(head) = queue.peek_ready(now_ms).cloned() else {
                break;
            };
            let msg_id = match head.chat.provider {
                Provider::Signal | Provider::Telegram => {
                    let msgs = self
                        .store
                        .list_messages(&head.chat, 80, None)
                        .unwrap_or_default();
                    if let Some(id) = select_read_message_id(&msgs) {
                        id.to_owned()
                    } else {
                        match self.client.fetch_messages(&head.chat.id, 20, None).await {
                            Ok(hub_msgs) => match read_message_id(&hub_msgs) {
                                Ok(id) => id,
                                Err(HubError::NoIncomingMessage) => {
                                    queue.complete_head();
                                    changed = true;
                                    continue;
                                }
                                Err(_) => {
                                    queue.requeue_failed_head(now_ms);
                                    changed = true;
                                    break;
                                }
                            },
                            Err(_) => {
                                queue.requeue_failed_head(now_ms);
                                changed = true;
                                break;
                            }
                        }
                    }
                }
                Provider::Whatsapp | Provider::Slack => String::new(),
            };
            match self.client.mark_read(&head.chat.id, &msg_id).await {
                Ok(()) => {
                    queue.complete_head();
                    changed = true;
                }
                Err(_) => {
                    queue.requeue_failed_head(now_ms);
                    changed = true;
                    break;
                }
            }
        }
        if changed {
            let _ = self.store.save_pending_reads(&queue);
        }
    }

    async fn drain_pending_archives(&mut self) {
        let now_ms = now_epoch_millis();
        let rows = match self.store.list_ready_pending_archives(now_ms) {
            Ok(r) if !r.is_empty() => r,
            _ => return,
        };

        let mut archive_table = self.store.load_archive_table().unwrap_or_default();
        let mut table_dirty = false;

        let mut wa_archive = Vec::new();
        let mut wa_unarchive = Vec::new();
        let mut others = Vec::new();
        for row in rows {
            if row.chat.provider == Provider::Whatsapp {
                if row.intent.archived() {
                    wa_archive.push(row);
                } else {
                    wa_unarchive.push(row);
                }
            } else {
                others.push(row);
            }
        }

        for (group, target_archived) in [(wa_archive, true), (wa_unarchive, false)] {
            if group.len() >= 2 {
                let ids: Vec<&str> = group.iter().map(|r| r.chat.id.as_str()).collect();
                match self.client.archive_batch(&ids, target_archived).await {
                    Ok(results) => {
                        for (row, item) in group.iter().zip(results.iter()) {
                            if item.ok {
                                let _ = self.store.delete_pending_archive(&row.chat);
                                if target_archived && archive_table.confirm_archived(&row.chat) {
                                    table_dirty = true;
                                }
                            } else {
                                let _ = self.store.delay_pending_archive(
                                    &row.chat,
                                    now_ms + 3_000,
                                    row.retries.saturating_add(1),
                                );
                            }
                        }
                    }
                    Err(_) => {
                        for row in &group {
                            let _ = self.store.delay_pending_archive(
                                &row.chat,
                                now_ms + 3_000,
                                row.retries.saturating_add(1),
                            );
                        }
                    }
                }
            } else if let Some(row) = group.first() {
                match self.client.archive(&row.chat.id, target_archived).await {
                    Ok(ArchiveOutcome::Completed | ArchiveOutcome::ManagedLocally) => {
                        let _ = self.store.delete_pending_archive(&row.chat);
                        if target_archived && archive_table.confirm_archived(&row.chat) {
                            table_dirty = true;
                        }
                    }
                    Err(_) => {
                        let _ = self.store.delay_pending_archive(
                            &row.chat,
                            now_ms + 3_000,
                            row.retries.saturating_add(1),
                        );
                    }
                }
            }
        }

        for row in others {
            match self
                .client
                .archive(&row.chat.id, row.intent.archived())
                .await
            {
                Ok(ArchiveOutcome::Completed | ArchiveOutcome::ManagedLocally) => {
                    let _ = self.store.delete_pending_archive(&row.chat);
                }
                Err(_) => {
                    let _ = self.store.delay_pending_archive(
                        &row.chat,
                        now_ms + 3_000,
                        row.retries.saturating_add(1),
                    );
                }
            }
        }

        if table_dirty {
            let _ = self.store.save_archive_table(&archive_table);
        }
    }
}

enum StreamExit {
    Shutdown,
    RestartRequested,
    CleanEof,
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::archive::ArchiveTable;
    use postbox_core::outbox::{OutboxRecord, format_client_id, sha256_bytes};
    use postbox_core::read::{PendingReadQueue, ReadWatermark};
    use postbox_ui::create_sync_bridge;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn sse_event(kind: &str, chat: &str, account: &str) -> SseEvent {
        SseEvent {
            id: 1,
            account: account.to_owned(),
            chat: chat.to_owned(),
            ts: String::new(),
            kind: kind.to_owned(),
        }
    }

    #[test]
    fn sse_event_provider_recognizes_prefixes_jids_and_account_fallback() {
        for (chat, provider) in [
            ("slack:T1:C1", Provider::Slack),
            ("telegram:123", Provider::Telegram),
            ("signal:u1", Provider::Signal),
        ] {
            for account in ["", "account"] {
                assert_eq!(
                    sse_event_provider(&sse_event("msg", chat, account)),
                    Some(provider)
                );
            }
        }
        for suffix in [
            "@s.whatsapp.net",
            "@g.us",
            "@lid",
            "@broadcast",
            "@newsletter",
            "@c.us",
        ] {
            for prefix in ["", "personal:"] {
                let chat = format!("{prefix}123{suffix}");
                assert_eq!(
                    sse_event_provider(&sse_event("msg", &chat, "")),
                    Some(Provider::Whatsapp)
                );
            }
        }
        for chat in ["", "unknown", "personal:123@s.whatsapp.net"] {
            assert_eq!(
                sse_event_provider(&sse_event("msg", chat, "personal")),
                Some(Provider::Whatsapp)
            );
        }
        for chat in ["", "unknown", "slack", "123@s.whatsapp.net.invalid"] {
            assert_eq!(sse_event_provider(&sse_event("msg", chat, "")), None);
        }
    }

    #[test]
    fn sse_batch_target_provider_requires_one_known_provider_without_resets() {
        assert_eq!(sse_batch_target_provider(&[]), None);
        for (chat, provider) in [
            ("personal:123@s.whatsapp.net", Provider::Whatsapp),
            ("slack:T1:C1", Provider::Slack),
            ("telegram:123", Provider::Telegram),
            ("signal:u1", Provider::Signal),
        ] {
            let event = sse_event("msg", chat, "");
            assert_eq!(
                sse_batch_target_provider(std::slice::from_ref(&event)),
                Some(provider)
            );
            assert_eq!(
                sse_batch_target_provider(&[event.clone(), sse_event("read", chat, "")]),
                Some(provider)
            );
            for other in [
                sse_event("msg", "unknown", ""),
                sse_event("reset", chat, ""),
            ] {
                assert_eq!(
                    sse_batch_target_provider(&[event.clone(), other.clone()]),
                    None
                );
                assert_eq!(sse_batch_target_provider(&[other, event.clone()]), None);
            }
        }
        assert_eq!(
            sse_batch_target_provider(&[
                sse_event("msg", "personal:123@s.whatsapp.net", "personal"),
                sse_event("msg", "slack:T1:C1", ""),
            ]),
            None
        );
        assert_eq!(
            sse_batch_target_provider(&[sse_event("reset", "", "")]),
            None
        );
        assert_eq!(
            sse_batch_target_provider(&[sse_event("msg", "unknown", "")]),
            None
        );
    }

    #[test]
    fn sse_batch_hits_open_chat_ignores_resets_empty_and_unrelated_chats() {
        let open_chat = ChatKey::new(
            Provider::Whatsapp,
            "personal",
            "personal:123@s.whatsapp.net",
        );
        let unrelated = sse_event("msg", "personal:456@s.whatsapp.net", "personal");
        assert!(!sse_batch_hits_open_chat(&[], &open_chat));
        assert!(!sse_batch_hits_open_chat(
            std::slice::from_ref(&unrelated),
            &open_chat
        ));
        assert!(!sse_batch_hits_open_chat(
            &[sse_event("reset", &open_chat.id, "personal")],
            &open_chat
        ));
        assert!(sse_batch_hits_open_chat(
            &[unrelated, sse_event("msg", &open_chat.id, "personal")],
            &open_chat
        ));
        let empty_chat = ChatKey::new(Provider::Whatsapp, "personal", "");
        assert!(!sse_batch_hits_open_chat(
            &[sse_event("msg", "", "personal")],
            &empty_chat
        ));
    }

    #[derive(Default)]
    struct MockState {
        chat_fetches: Vec<String>,
        chats_archived: String,
        chats_inbox: String,
        messages_by_chat: HashMap<String, String>,
        message_fetches: Vec<String>,
        mark_read_bodies: Vec<String>,
        send_count: usize,
    }

    async fn start_mock_server(
        state: Arc<Mutex<MockState>>,
    ) -> (Endpoint, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let endpoint = Endpoint::parse(&format!("http://127.0.0.1:{}", addr.port()), true).unwrap();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let st = state.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    let header_end;
                    loop {
                        let Ok(n) = sock.read(&mut tmp).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            header_end = pos + 4;
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
                    let content_len: usize = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    while buf.len() < header_end + content_len {
                        let Ok(n) = sock.read(&mut tmp).await else {
                            return;
                        };
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let req_line = head.lines().next().unwrap_or("");
                    let mut parts = req_line.split_whitespace();
                    let method = parts.next().unwrap_or("");
                    let target = parts.next().unwrap_or("");
                    let body = String::from_utf8_lossy(&buf[header_end..]).into_owned();

                    let (status, resp_body) = if method == "GET"
                        && target.starts_with("/v1/wazig/chats?")
                    {
                        let mut g = st.lock().unwrap();
                        g.chat_fetches.push(target.to_owned());
                        if target.contains("archived=true") {
                            (200, g.chats_archived.clone())
                        } else {
                            (200, g.chats_inbox.clone())
                        }
                    } else if method == "GET" && target.starts_with("/v1/wazig/messages?") {
                        let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
                        let chat_raw = query
                            .split('&')
                            .find_map(|kv| kv.strip_prefix("chat="))
                            .unwrap_or("");
                        let chat = chat_raw.replace("%3A", ":").replace("%40", "@");
                        let mut g = st.lock().unwrap();
                        g.message_fetches.push(chat.clone());
                        let payload = g
                            .messages_by_chat
                            .get(&chat)
                            .cloned()
                            .unwrap_or_else(|| "[]".to_owned());
                        (200, payload)
                    } else if method == "POST" && target == "/v1/wazig/mark-read" {
                        st.lock().unwrap().mark_read_bodies.push(body);
                        (200, "{}".to_owned())
                    } else if method == "POST" && target == "/v1/wazig/send" {
                        let mut g = st.lock().unwrap();
                        g.send_count += 1;
                        let seq = g.send_count;
                        // Extract client_id from multipart form-data.
                        let cid = body
                            .split("name=\"client_id\"\r\n\r\n")
                            .nth(1)
                            .and_then(|s| s.split("\r\n").next())
                            .unwrap_or("00000000000000000000000000000000");
                        (
                            200,
                            format!(
                                "{{\"client_id\":\"{cid}\",\"msg_id\":\"srv_{seq}\",\"ts\":\"2026-11-26T12:00:00Z\",\"status\":\"sent\"}}"
                            ),
                        )
                    } else {
                        (404, "{}".to_owned())
                    };

                    let resp = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                        resp_body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        (endpoint, handle)
    }

    #[tokio::test]
    async fn provider_scoped_chat_sync_preserves_other_providers_and_resets_fetch_all() {
        let slack_key = ChatKey::new(Provider::Slack, "T1", "slack:T1:C1");
        let whatsapp_key = ChatKey::new(Provider::Whatsapp, "p", "p:123@s.whatsapp.net");
        let mut store = Store::open_in_memory().unwrap();
        let mut archive_table = ArchiveTable::new();
        archive_table.archive(&slack_key, "2026-11-26T10:00:00Z", false);
        store.save_archive_table(&archive_table).unwrap();
        let state = Arc::new(Mutex::new(MockState {
            chats_archived: "[]".to_owned(),
            chats_inbox: r##"[
                {"jid":"slack:T1:C1","provider":"slack","account":"T1","name":"#general","last_message_ts":"2026-11-26T10:00:00Z","unread":false},
                {"jid":"p:123@s.whatsapp.net","provider":"whatsapp","account":"p","name":"Before","last_message_ts":"2026-11-26T10:00:00Z","unread":false}
            ]"##.to_owned(),
            ..Default::default()
        }));
        let (endpoint, server) = start_mock_server(state.clone()).await;
        let client = HubClient::new(endpoint, "test-token").unwrap();
        let (_bridge, worker) = create_sync_bridge();
        let mut engine = SyncEngine::new(client, store, worker);
        engine.sync_chats(true).await.unwrap();
        let slack_chat = engine.store.get_chat(&slack_key).unwrap().unwrap();
        let slack_archive = engine
            .store
            .load_archive_table()
            .unwrap()
            .get(&slack_key)
            .unwrap()
            .clone();
        let slack_read = engine.store.load_read_state().unwrap().get(&slack_key);
        {
            let mut state = state.lock().unwrap();
            assert_eq!(state.chat_fetches.len(), 2);
            assert!(
                state
                    .chat_fetches
                    .iter()
                    .all(|path| !path.contains("provider="))
            );
            state.chat_fetches.clear();
            state.chats_inbox = r#"[
                {"jid":"p:123@s.whatsapp.net","provider":"whatsapp","account":"p","name":"After","last_message_ts":"2026-11-26T11:00:00Z","unread":false}
            ]"#.to_owned();
        }

        engine
            .sync_chats_for_provider(false, Some(Provider::Whatsapp))
            .await
            .unwrap();

        assert_eq!(
            engine.store.get_chat(&whatsapp_key).unwrap().unwrap().title,
            "After"
        );
        assert_eq!(
            engine.store.get_chat(&slack_key).unwrap().unwrap(),
            slack_chat
        );
        assert_eq!(
            engine.store.load_archive_table().unwrap().get(&slack_key),
            Some(&slack_archive)
        );
        assert_eq!(
            engine.store.load_read_state().unwrap().get(&slack_key),
            slack_read
        );
        {
            let mut state = state.lock().unwrap();
            state.chat_fetches.sort();
            assert_eq!(
                state.chat_fetches,
                vec![
                    "/v1/wazig/chats?archived=false&limit=500&provider=whatsapp",
                    "/v1/wazig/chats?archived=true&limit=500&provider=whatsapp",
                ]
            );
            state.chat_fetches.clear();
        }

        engine
            .sync_chats_for_provider(true, Some(Provider::Whatsapp))
            .await
            .unwrap();

        let requests = state.lock().unwrap().chat_fetches.clone();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|path| !path.contains("provider=")));
        server.abort();
    }

    #[tokio::test]
    async fn sync_chats_suppresses_stale_unread_persists_read_state_local_unarchive_and_probe_checked_unix()
     {
        let stale_key = ChatKey::new(Provider::Whatsapp, "p", "p:stale@s.whatsapp.net");
        let phone_read_key = ChatKey::new(Provider::Whatsapp, "p", "p:phone@s.whatsapp.net");
        let slack_key = ChatKey::new(Provider::Slack, "T1", "slack:T1:C1");
        let probe_key = ChatKey::new(Provider::Whatsapp, "p", "p:probe@s.whatsapp.net");

        let mut store = Store::open_in_memory().unwrap();
        // Local user already marked `stale_key` read at 10:00:00Z.
        let ts_10 = unix_seconds("2026-11-26T10:00:00Z").unwrap();
        store
            .upsert_read_watermark(
                &stale_key,
                &ReadWatermark {
                    read_ts: ts_10,
                    incoming_ts: 0,
                    checked_ts: 0,
                },
            )
            .unwrap();
        // Slack chat locally unarchived despite remote `archived: true`; WA probe chat archived at 10:00Z.
        let mut archive_table = ArchiveTable::new();
        archive_table.set_local_unarchived(&slack_key);
        archive_table.archive(&probe_key, "2026-11-26T10:00:00Z", false);
        store.save_archive_table(&archive_table).unwrap();

        let state = Arc::new(Mutex::new(MockState {
            chats_archived: r##"[
                {"jid":"slack:T1:C1","provider":"slack","account":"T1","name":"#general","last_message_ts":"2026-11-26T10:00:00Z","unread":false,"archived":true},
                {"jid":"p:probe@s.whatsapp.net","provider":"whatsapp","account":"p","name":"Probe","last_message_ts":"2026-11-26T10:05:00Z","unread":false,"archived":true}
            ]"##
            .to_owned(),
            chats_inbox: r#"[
                {"jid":"p:stale@s.whatsapp.net","provider":"whatsapp","account":"p","name":"Stale","last_message_ts":"2026-11-26T10:00:00Z","unread":true,"unread_count":3,"archived":false},
                {"jid":"p:phone@s.whatsapp.net","provider":"whatsapp","account":"p","name":"PhoneRead","last_message_ts":"2026-11-26T11:00:00Z","unread":false,"archived":false}
            ]"#
            .to_owned(),
            messages_by_chat: HashMap::from([(
                "p:probe@s.whatsapp.net".to_owned(),
                r#"[{"MsgID":"own1","ChatJID":"p:probe@s.whatsapp.net","FromMe":true,"Timestamp":"2026-11-26T10:05:00Z","Text":"my own msg"}]"#.to_owned(),
            )]),
            ..Default::default()
        }));

        let (endpoint, server) = start_mock_server(state.clone()).await;
        let client = HubClient::new(endpoint, "test-token").unwrap();
        let (_bridge, worker) = create_sync_bridge();
        let mut engine = SyncEngine::new(client, store, worker);

        engine.sync_chats(true).await.unwrap();

        // 1. Stale Hub `unread: true` suppressed because local read_ts >= last_message_ts!
        let stale_chat = engine.store.get_chat(&stale_key).unwrap().unwrap();
        assert_eq!(stale_chat.unread_count, 0);

        // 2. Remote-read watermark persisted to SQLite!
        let persisted_rs = engine.store.load_read_state().unwrap();
        assert_eq!(
            persisted_rs.get(&phone_read_key).read_ts,
            unix_seconds("2026-11-26T11:00:00Z").unwrap()
        );

        // 3. Locally unarchived Slack chat stays unarchived (both flags cleared)!
        let slack_chat = engine.store.get_chat(&slack_key).unwrap().unwrap();
        assert!(!slack_chat.archived_local);
        assert!(!slack_chat.archived_remote);

        // 4. Non-resurfacing IGSH-902 probe persisted `checked_unix` -> 2nd sync MUST NOT re-probe!
        assert_eq!(state.lock().unwrap().message_fetches.len(), 1);
        let persisted_tbl = engine.store.load_archive_table().unwrap();
        assert_eq!(
            persisted_tbl.get(&probe_key).unwrap().checked_unix,
            unix_seconds("2026-11-26T10:05:00Z").unwrap()
        );

        engine.sync_chats(false).await.unwrap();
        assert_eq!(
            state.lock().unwrap().message_fetches.len(),
            1,
            "checked_unix must prevent infinite re-probing on subsequent syncs"
        );

        server.abort();
    }

    #[tokio::test]
    async fn drain_pending_reads_fetches_signal_msg_id_and_never_posts_empty_id() {
        let sig_ok = ChatKey::new(Provider::Signal, "signal", "signal:u1");
        let sig_none = ChatKey::new(Provider::Signal, "signal", "signal:u2");

        let mut store = Store::open_in_memory().unwrap();
        let mut q = PendingReadQueue::new();
        q.enqueue(sig_ok.clone());
        q.enqueue(sig_none.clone());
        store.save_pending_reads(&q).unwrap();

        let state = Arc::new(Mutex::new(MockState {
            messages_by_chat: HashMap::from([
                (
                    "signal:u1".to_owned(),
                    r#"[
                        {"MsgID":"sig_own","ChatJID":"signal:u1","FromMe":true,"Timestamp":"2026-11-26T10:02:00Z","Text":"me"},
                        {"MsgID":"sig_react","ChatJID":"signal:u1","FromMe":false,"Timestamp":"2026-11-26T10:01:00Z","ReactionToID":"sig_m42","ReactionEmoji":"👍"},
                        {"MsgID":"sig_m42","ChatJID":"signal:u1","FromMe":false,"Timestamp":"2026-11-26T10:00:00Z","Text":"incoming"}
                    ]"#
                    .to_owned(),
                ),
                (
                    "signal:u2".to_owned(),
                    r#"[{"MsgID":"only_me","ChatJID":"signal:u2","FromMe":true,"Timestamp":"2026-11-26T10:00:00Z","Text":"hi"}]"#.to_owned(),
                ),
            ]),
            ..Default::default()
        }));

        let (endpoint, server) = start_mock_server(state.clone()).await;
        let client = HubClient::new(endpoint, "test-token").unwrap();
        let (_bridge, worker) = create_sync_bridge();
        let mut engine = SyncEngine::new(client, store, worker);

        engine.drain_pending_reads().await;

        let calls = state.lock().unwrap().mark_read_bodies.clone();
        assert_eq!(
            calls.len(),
            1,
            "NoIncomingMessage chat must NOT POST empty message_id"
        );
        assert!(calls[0].contains("\"chat\":\"signal:u1\""));
        assert!(calls[0].contains("\"message_id\":\"sig_m42\""));
        assert!(engine.store.load_pending_reads().unwrap().is_empty());

        server.abort();
    }

    #[tokio::test]
    async fn drain_outbox_sets_media_kind_for_voice_and_file_and_sleep_timer_tracks_archive_retries()
     {
        let chat = ChatKey::new(Provider::Whatsapp, "p", "p:m@s.whatsapp.net");
        let mut store = Store::open_in_memory().unwrap();

        let voice_path =
            std::env::temp_dir().join(format!("postbox_sync_voice_{}.ogg", std::process::id()));
        let file_path =
            std::env::temp_dir().join(format!("postbox_sync_doc_{}.pdf", std::process::id()));
        let voice_bytes = b"OggS-test-voice";
        let file_bytes = b"%PDF-test-doc";
        std::fs::write(&voice_path, voice_bytes).unwrap();
        std::fs::write(&file_path, file_bytes).unwrap();

        let cid_v = format_client_id([0x11; 16]);
        let cid_f = format_client_id([0x22; 16]);
        let voice_rec = OutboxRecord::new_voice(
            cid_v.clone(),
            chat.clone(),
            None,
            voice_path.to_string_lossy(),
            sha256_bytes(voice_bytes),
            voice_bytes.len() as u64,
            2,
            1_000,
        );
        let file_rec = OutboxRecord::new_file(
            cid_f.clone(),
            chat.clone(),
            "",
            None,
            file_path.to_string_lossy(),
            "report.pdf",
            sha256_bytes(file_bytes),
            file_bytes.len() as u64,
            1_001,
        );
        store.enqueue_outbox(&voice_rec).unwrap();
        store.enqueue_outbox(&file_rec).unwrap();

        let state = Arc::new(Mutex::new(MockState::default()));
        let (endpoint, server) = start_mock_server(state.clone()).await;
        let client = HubClient::new(endpoint, "test-token").unwrap();
        let (_bridge, worker) = create_sync_bridge();
        let mut engine = SyncEngine::new(client, store, worker);

        engine.drain_outbox().await;

        let msgs = engine.store.list_messages(&chat, 10, None).unwrap();
        assert_eq!(msgs.len(), 2);
        let kinds: Vec<Option<MediaKind>> = msgs.iter().map(|m| m.media_kind).collect();
        assert!(kinds.contains(&Some(MediaKind::Voice)));
        assert!(kinds.contains(&Some(MediaKind::Document)));

        // Outbox is now empty -> adding a delayed pending archive (+3s) must wake in <= 3.5s, NOT 60s!
        let now_ms = now_epoch_millis();
        engine
            .store
            .enqueue_pending_archive(&chat, PendingArchiveIntent::ArchivePending, now_ms + 3_000)
            .unwrap();
        let sleep_dur = engine.next_outbox_sleep_duration();
        assert!(
            sleep_dur <= Duration::from_millis(3_500),
            "expected archive retry wake <= 3.5s, got {sleep_dur:?}"
        );

        let _ = std::fs::remove_file(&voice_path);
        let _ = std::fs::remove_file(&file_path);
        server.abort();
    }

    fn unique_sync_test_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("postbox_sync_{tag}_{}_{nanos}", std::process::id()))
    }

    #[tokio::test]
    async fn avatar_fetches_do_not_block_commands_and_deduplicate_in_flight() {
        let state = Arc::new(Mutex::new(MockState::default()));
        let (endpoint, server) = start_mock_server(state.clone()).await;
        let client = HubClient::new(endpoint, "test-token").unwrap();
        let data_dir = unique_sync_test_dir("avatars");
        let (bridge, worker) = create_sync_bridge();
        let mut engine = SyncEngine::with_m3(
            client,
            Store::open_in_memory().unwrap(),
            worker,
            data_dir.clone(),
            None,
        );
        let permits = engine.avatar_sem.clone().try_acquire_many_owned(4).unwrap();
        let chats: Vec<_> = (0..5)
            .map(|i| {
                ChatKey::new(
                    Provider::Telegram,
                    "telegram",
                    format!("telegram:avatar-{i}"),
                )
            })
            .collect();
        for chat in chats.iter().chain(chats.iter()) {
            tokio::time::timeout(
                Duration::from_secs(1),
                engine.handle_command(SyncCommand::FetchAvatar { chat: chat.clone() }),
            )
            .await
            .expect("avatar command must not wait for a download permit");
        }
        assert_eq!(engine.avatar_in_flight.len(), 5);
        while let Ok(event) = bridge.event_rx.try_recv() {
            assert!(!matches!(event, SyncEvent::AvatarReady { .. }));
        }
        tokio::time::timeout(
            Duration::from_secs(1),
            engine.handle_command(SyncCommand::FetchChatMessages {
                chat: chats[0].clone(),
                limit: 10,
            }),
        )
        .await
        .expect("message sync must run while avatars wait");
        assert_eq!(state.lock().unwrap().message_fetches.len(), 1);

        drop(permits);
        for _ in &chats {
            let result = tokio::time::timeout(Duration::from_secs(2), engine.bg_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(result, BackgroundTaskResult::AvatarFetched { .. }));
            engine.handle_bg_result(result);
        }
        assert!(engine.avatar_in_flight.is_empty());
        assert_eq!(engine.avatar_sem.available_permits(), 4);
        server.abort();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[tokio::test]
    async fn pending_avatar_result_does_not_emit_missing_or_change_cache() {
        let endpoint = Endpoint::parse("http://127.0.0.1:9", true).unwrap();
        let client = HubClient::new(endpoint, "test-token").unwrap();
        let data_dir = unique_sync_test_dir("avatar-pending");
        let (bridge, worker) = create_sync_bridge();
        let mut engine = SyncEngine::with_m3(
            client,
            Store::open_in_memory().unwrap(),
            worker,
            data_dir.clone(),
            None,
        );
        let chat = ChatKey::new(Provider::Telegram, "telegram", "telegram:avatar");
        let marker_key = "avatar:telegram:telegram:telegram:avatar";
        engine.avatar_in_flight.insert(chat.clone());
        engine.handle_bg_result(BackgroundTaskResult::AvatarFetched {
            chat: chat.clone(),
            outcome: Ok(AvatarFetchState::Pending),
        });
        assert!(!engine.avatar_in_flight.contains(&chat));
        while let Ok(event) = bridge.event_rx.try_recv() {
            assert!(!matches!(event, SyncEvent::AvatarReady { .. }));
        }
        assert!(engine.store.get_fetch_marker(marker_key).unwrap().is_none());

        let avatar_dir = data_dir.join("avatars");
        let path = commit_avatar_fetch_result(
            &engine.store,
            &avatar_dir,
            &chat,
            100,
            Ok(AvatarFetchState::Ready(b"CACHED-AVATAR".to_vec())),
        )
        .unwrap()
        .unwrap();
        let before = engine.store.get_fetch_marker(marker_key).unwrap();
        engine.avatar_in_flight.insert(chat.clone());
        engine.handle_bg_result(BackgroundTaskResult::AvatarFetched {
            chat: chat.clone(),
            outcome: Ok(AvatarFetchState::Pending),
        });
        assert!(!engine.avatar_in_flight.contains(&chat));
        while let Ok(event) = bridge.event_rx.try_recv() {
            assert!(!matches!(event, SyncEvent::AvatarReady { .. }));
        }
        assert_eq!(engine.store.get_fetch_marker(marker_key).unwrap(), before);
        assert_eq!(std::fs::read(path).unwrap(), b"CACHED-AVATAR");
        std::fs::remove_dir_all(data_dir).unwrap();
    }

    #[tokio::test]
    async fn concurrent_media_downloads_run_3_in_parallel_without_blocking_commands_or_shutdown() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Barrier;

        let current = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(3));

        let cur_srv = current.clone();
        let peak_srv = peak.clone();
        let bar_srv = barrier.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let cur = cur_srv.clone();
                let pk = peak_srv.clone();
                let bar = bar_srv.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let first_line = req.lines().next().unwrap_or("");
                    let mut parts = first_line.split_whitespace();
                    let method = parts.next().unwrap_or("");
                    let path = parts.next().unwrap_or("");

                    if method == "GET" && path.starts_with("/v1/wazig/events") {
                        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\n: keepalive\n\n";
                        let _ = sock.write_all(head.as_bytes()).await;
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        return;
                    }
                    if method == "GET" && path.starts_with("/v1/wazig/chats") {
                        let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]";
                        let _ = sock.write_all(resp.as_bytes()).await;
                        return;
                    }
                    if method == "POST" && path == "/v1/wazig/react" {
                        let body = r#"{"ok":true}"#;
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                        return;
                    }
                    if method == "GET" && path.starts_with("/v1/wazig/media/") {
                        let active = cur.fetch_add(1, Ordering::SeqCst) + 1;
                        pk.fetch_max(active, Ordering::SeqCst);
                        // First 3 requests must arrive concurrently to pass the barrier!
                        if path.ends_with("/m0") || path.ends_with("/m1") || path.ends_with("/m2") {
                            let _ = tokio::time::timeout(Duration::from_secs(2), bar.wait()).await;
                        }
                        tokio::time::sleep(Duration::from_millis(40)).await;
                        cur.fetch_sub(1, Ordering::SeqCst);
                        let resp = "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 4\r\nConnection: close\r\n\r\nPNG!";
                        let _ = sock.write_all(resp.as_bytes()).await;
                        return;
                    }
                    let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        let data_dir = unique_sync_test_dir("concurrent3");
        let endpoint = Endpoint::parse(&format!("http://127.0.0.1:{port}"), true).unwrap();
        let client = HubClient::new(endpoint, "test-token").unwrap();
        let store = Store::open_in_memory().unwrap();
        let (bridge, worker) = create_sync_bridge();
        let mut engine = SyncEngine::with_m3(client, store, worker, data_dir.clone(), None);

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let engine_task = tokio::task::spawn_local(async move {
                    engine.run().await;
                });

                // Wait until SSE is Connected
                loop {
                    let ev = tokio::time::timeout(Duration::from_secs(3), bridge.event_rx.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    if matches!(
                        ev,
                        SyncEvent::StatusChanged(HubConnectionStatus::Connected { .. })
                    ) {
                        break;
                    }
                }

                let chat = ChatKey::new(Provider::Whatsapp, "main", "conc@s.whatsapp.net");
                let items: Vec<MediaDownloadTarget> = (0..5)
                    .map(|i| MediaDownloadTarget {
                        msg_id: format!("m{i}"),
                        filename: Some(format!("pic{i}.png")),
                        mime_type: Some("image/png".into()),
                        media_kind: Some(MediaKind::Image),
                        file_size: Some(1024),
                    })
                    .collect();
                bridge
                    .cmd_tx
                    .send(SyncCommand::DownloadChatMedia {
                        chat: chat.clone(),
                        items,
                    })
                    .await
                    .unwrap();
                bridge
                    .cmd_tx
                    .send(SyncCommand::ReactToMessage {
                        chat: chat.clone(),
                        msg_id: "m0".into(),
                        emoji: "👍".into(),
                        client_id: "cid_react".into(),
                    })
                    .await
                    .unwrap();

                let mut downloaded = 0usize;
                let mut reaction_settled_before_all_downloads = false;
                let mut reaction_settled = false;
                while downloaded < 5 || !reaction_settled {
                    let ev = tokio::time::timeout(Duration::from_secs(4), bridge.event_rx.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    match ev {
                        SyncEvent::MediaDownloaded { .. } => downloaded += 1,
                        SyncEvent::ReactionSettled { ok: true, .. } => {
                            reaction_settled = true;
                            if downloaded < 5 {
                                reaction_settled_before_all_downloads = true;
                            }
                        }
                        _ => {}
                    }
                }

                assert!(
                    reaction_settled_before_all_downloads,
                    "ReactToMessage must complete without waiting for all 5 media downloads"
                );
                assert_eq!(peak.load(Ordering::SeqCst), 3);

                // Shutdown must terminate promptly (< 500ms)
                let shutdown_start = Instant::now();
                bridge.cmd_tx.send(SyncCommand::Shutdown).await.unwrap();
                tokio::time::timeout(Duration::from_millis(500), engine_task)
                    .await
                    .expect("SyncEngine must shut down within 500ms")
                    .unwrap();
                assert!(shutdown_start.elapsed() < Duration::from_millis(500));
            })
            .await;

        server.abort();
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    async fn drain_tracker_suppresses_backoff_and_exhaustion_and_auto_retries_when_due() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let hits_transient = Arc::new(AtomicUsize::new(0));
        let hits_missing = Arc::new(AtomicUsize::new(0));
        let ht = hits_transient.clone();
        let hm = hits_missing.clone();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let ht = ht.clone();
                let hm = hm.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req
                        .lines()
                        .next()
                        .unwrap_or("")
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("");
                    let resp = if path.contains("m_transient") {
                        let hit = ht.fetch_add(1, Ordering::SeqCst) + 1;
                        if hit < 3 {
                            "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        } else {
                            "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 3\r\nConnection: close\r\n\r\nIMG"
                        }
                    } else if path.contains("m_missing") {
                        hm.fetch_add(1, Ordering::SeqCst);
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    } else {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    };
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        let data_dir = unique_sync_test_dir("backoff");
        let endpoint = Endpoint::parse(&format!("http://127.0.0.1:{port}"), true).unwrap();
        let client = HubClient::new(endpoint, "test-token").unwrap();
        let store = Store::open_in_memory().unwrap();
        let (bridge, worker) = create_sync_bridge();
        let mut engine = SyncEngine::with_m3(client, store, worker, data_dir.clone(), None);

        let chat = ChatKey::new(Provider::Whatsapp, "main", "retry@s.whatsapp.net");
        let items = vec![
            MediaDownloadTarget {
                msg_id: "m_transient".into(),
                filename: Some("a.png".into()),
                mime_type: Some("image/png".into()),
                media_kind: Some(MediaKind::Image),
                file_size: Some(100),
            },
            MediaDownloadTarget {
                msg_id: "m_missing".into(),
                filename: Some("b.png".into()),
                mime_type: Some("image/png".into()),
                media_kind: Some(MediaKind::Image),
                file_size: Some(100),
            },
        ];

        engine.enqueue_media_targets(&chat, &items, true, false);
        for _ in 0..2 {
            let bg = engine.bg_rx.recv().await.unwrap();
            engine.handle_bg_result(bg);
        }
        assert_eq!(hits_transient.load(Ordering::SeqCst), 1);
        assert_eq!(hits_missing.load(Ordering::SeqCst), 1);

        // Immediate re-enqueue MUST be suppressed by should_skip_auto!
        assert_eq!(engine.enqueue_media_targets(&chat, &items, true, false), 0);
        assert_eq!(hits_transient.load(Ordering::SeqCst), 1);
        assert_eq!(hits_missing.load(Ordering::SeqCst), 1);

        // +5s -> m_transient retries (attempt 2, 429 -> 10s delay); m_missing still suppressed!
        engine.test_clock_offset_ms += 5_000;
        engine.process_media_timers();
        let bg = engine.bg_rx.recv().await.unwrap();
        engine.handle_bg_result(bg);
        assert_eq!(hits_transient.load(Ordering::SeqCst), 2);
        assert_eq!(hits_missing.load(Ordering::SeqCst), 1);

        // +10s -> m_transient hits 3 and succeeds (MediaDownloaded)!
        engine.test_clock_offset_ms += 10_000;
        engine.process_media_timers();
        let bg = engine.bg_rx.recv().await.unwrap();
        engine.handle_bg_result(bg);
        assert_eq!(hits_transient.load(Ordering::SeqCst), 3);

        // +15s (30s total) -> m_missing attempt 2 (404 -> 60s delay)
        engine.test_clock_offset_ms += 15_000;
        engine.process_media_timers();
        let bg = engine.bg_rx.recv().await.unwrap();
        engine.handle_bg_result(bg);
        assert_eq!(hits_missing.load(Ordering::SeqCst), 2);

        // +60s -> m_missing attempt 3 -> exhausted into Unavailable!
        engine.test_clock_offset_ms += 60_000;
        engine.process_media_timers();
        let bg = engine.bg_rx.recv().await.unwrap();
        engine.handle_bg_result(bg);
        assert_eq!(hits_missing.load(Ordering::SeqCst), 3);
        assert_eq!(engine.drain_tracker.next_retry_due_ms(), None);

        // Further automatic downloads remain suppressed after exhaustion!
        assert_eq!(engine.enqueue_media_targets(&chat, &items, true, false), 0);
        assert_eq!(hits_missing.load(Ordering::SeqCst), 3);

        let mut saw_downloaded = false;
        let mut saw_unavailable = false;
        while let Ok(ev) = bridge.event_rx.try_recv() {
            match ev {
                SyncEvent::MediaDownloaded { msg_id, .. } if msg_id == "m_transient" => {
                    saw_downloaded = true;
                }
                SyncEvent::MediaStatusChanged {
                    msg_id,
                    status: MediaItemStatus::Unavailable,
                    ..
                } if msg_id == "m_missing" => {
                    saw_unavailable = true;
                }
                _ => {}
            }
        }
        assert!(saw_downloaded);
        assert!(saw_unavailable);

        server.abort();
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    async fn gone_410_burst_respects_20s_repaint_throttle_and_flushes_pending() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_srv = hits.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let h = hits_srv.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    h.fetch_add(1, Ordering::SeqCst);
                    let resp =
                        "HTTP/1.1 410 Gone\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });

        let data_dir = unique_sync_test_dir("gone410");
        let endpoint = Endpoint::parse(&format!("http://127.0.0.1:{port}"), true).unwrap();
        let client = HubClient::new(endpoint, "test-token").unwrap();
        let store = Store::open_in_memory().unwrap();
        let (bridge, worker) = create_sync_bridge();
        let mut engine = SyncEngine::with_m3(client, store, worker, data_dir.clone(), None);

        let chat = ChatKey::new(Provider::Whatsapp, "main", "gone@s.whatsapp.net");
        let items: Vec<MediaDownloadTarget> = (1..=5)
            .map(|i| MediaDownloadTarget {
                msg_id: format!("g{i}"),
                filename: Some(format!("g{i}.jpg")),
                mime_type: Some("image/jpeg".into()),
                media_kind: Some(MediaKind::Image),
                file_size: Some(500),
            })
            .collect();

        engine.enqueue_media_targets(&chat, &items, true, false);
        for _ in 0..5 {
            let bg = engine.bg_rx.recv().await.unwrap();
            engine.handle_bg_result(bg);
        }

        // All 5 persisted immediately in SQLite `gone_media`
        for i in 1..=5 {
            assert!(engine.store.is_media_gone(&chat, &format!("g{i}")).unwrap());
        }

        // Exactly 1 Gone event emitted immediately; 4 buffered behind 20s throttle!
        let mut immediate_gone = 0usize;
        while let Ok(ev) = bridge.event_rx.try_recv() {
            if matches!(
                ev,
                SyncEvent::MediaStatusChanged {
                    status: MediaItemStatus::Gone,
                    ..
                }
            ) {
                immediate_gone += 1;
            }
        }
        assert_eq!(immediate_gone, 1);
        assert_eq!(engine.pending_gone_events.len(), 4);
        assert!(engine.drain_tracker.next_gone_flush_due_ms().is_some());

        // Re-sending causes 0 additional Hub hits
        let before_hits = hits.load(Ordering::SeqCst);
        assert_eq!(engine.enqueue_media_targets(&chat, &items, true, false), 0);
        assert_eq!(hits.load(Ordering::SeqCst), before_hits);

        // Advance +20s -> flushes remaining 4 Gone events and clears flush timer
        engine.test_clock_offset_ms += 20_000;
        engine.process_media_timers();
        let mut flushed_gone = 0usize;
        while let Ok(ev) = bridge.event_rx.try_recv() {
            if matches!(
                ev,
                SyncEvent::MediaStatusChanged {
                    status: MediaItemStatus::Gone,
                    ..
                }
            ) {
                flushed_gone += 1;
            }
        }
        assert_eq!(flushed_gone, 4);
        assert_eq!(engine.drain_tracker.next_gone_flush_due_ms(), None);

        server.abort();
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    async fn background_drain_yields_to_open_chat_and_fetches_unopened_zero_message_chats() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let msg_fetches = Arc::new(AtomicUsize::new(0));
        let media_fetches = Arc::new(AtomicUsize::new(0));
        let mf = msg_fetches.clone();
        let mef = media_fetches.clone();

        let recent_ts = format_utc_rfc3339(now_epoch_secs() - 60);
        let msg_json = format!(
            r#"[{{"MsgID":"bg_m1","ChatJID":"bg@s.whatsapp.net","FromMe":false,"Timestamp":"{recent_ts}","Text":"","MediaType":"image","MimeType":"image/png","Filename":"bg.png"}}]"#
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mf = mf.clone();
                let mef = mef.clone();
                let mj = msg_json.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req
                        .lines()
                        .next()
                        .unwrap_or("")
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("");
                    if path.starts_with("/v1/wazig/messages") {
                        mf.fetch_add(1, Ordering::SeqCst);
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{mj}",
                            mj.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                    } else if path.starts_with("/v1/wazig/media/") {
                        mef.fetch_add(1, Ordering::SeqCst);
                        let resp = "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 4\r\nConnection: close\r\n\r\nDATA";
                        let _ = sock.write_all(resp.as_bytes()).await;
                    } else {
                        let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        let _ = sock.write_all(resp.as_bytes()).await;
                    }
                });
            }
        });

        let data_dir = unique_sync_test_dir("bgdrain");
        let endpoint = Endpoint::parse(&format!("http://127.0.0.1:{port}"), true).unwrap();
        let client = HubClient::new(endpoint, "test-token").unwrap();
        let store = Store::open_in_memory().unwrap();
        let open_chat = ChatKey::new(Provider::Whatsapp, "main", "open@s.whatsapp.net");
        let bg_chat = ChatKey::new(Provider::Whatsapp, "main", "bg@s.whatsapp.net");
        let now_s = now_epoch_secs();
        for (k, title) in [(&open_chat, "Open"), (&bg_chat, "Unopened")] {
            store
                .upsert_chat(
                    &postbox_core::Chat {
                        key: k.clone(),
                        title: title.into(),
                        is_group: false,
                        archived_local: false,
                        archived_remote: false,
                        pinned: false,
                        unread_count: 0,
                        last_msg_ts: recent_ts.clone(),
                        last_msg_preview: "img".into(),
                        avatar_hash: None,
                    },
                    now_s,
                )
                .unwrap();
        }

        let (_bridge, worker) = create_sync_bridge();
        let mut engine = SyncEngine::with_m3(client, store, worker, data_dir.clone(), None);
        engine.open_chat = Some(open_chat.clone());

        // 1. While open-chat download is active, background drain MUST yield!
        engine.open_media_in_flight = 1;
        engine.maybe_start_background_drain();
        assert_eq!(msg_fetches.load(Ordering::SeqCst), 0);
        assert!(engine.bg_drain_pending);
        assert!(!engine.background_swept_chats.contains_key(&bg_chat));

        // 2. Open-chat finishes -> background drain fetches Hub messages for 0-message bg_chat!
        engine.open_media_in_flight = 0;
        engine.maybe_start_background_drain();
        let bg_msgs_res = engine.bg_rx.recv().await.unwrap();
        engine.handle_bg_result(bg_msgs_res);
        assert_eq!(msg_fetches.load(Ordering::SeqCst), 1);
        assert!(!engine.background_swept_chats.contains_key(&bg_chat));

        // 3. Message persisted and its media download spawned -> completes & marks bg_chat swept!
        let bg_dl_res = engine.bg_rx.recv().await.unwrap();
        engine.handle_bg_result(bg_dl_res);
        assert_eq!(media_fetches.load(Ordering::SeqCst), 1);
        assert!(engine.background_swept_chats.contains_key(&bg_chat));

        server.abort();
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
