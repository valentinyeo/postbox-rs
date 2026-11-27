use postbox_core::archive::{IncomingProbeMessage, PendingArchiveIntent, unix_seconds};
use postbox_core::outbox::{
    OutboxKind, OutboxPhase, classify_http_outcome, verify_attachment_bytes,
};
use postbox_core::read::select_read_message_id;
use postbox_core::views::{format_account_label, resolve_slack_workspace_name};
use postbox_core::{Account, ChatKey, MediaKind, Message, Provider, ReadStatus};
use postbox_hub::{
    ArchiveOutcome, Endpoint, EventStream, HubClient, HubError, MAX_ROWS, OLDER_PAGE_LIMIT,
    SendFileRequest, SseParser, SseReconnectState, classify_signal_send_failure,
    classify_telegram_send_failure, is_real_message, read_message_id, should_run_safety_poll,
};
use postbox_store::Store;
use postbox_ui::{HubConnectionStatus, SyncCommand, SyncEvent, SyncWorkerHandle};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
/// Hub HTTP/2 + SSE sync, durable SQLite outbox delivery, pending mark-read, and archive batching.
pub fn spawn_sync_engine(
    endpoint: Endpoint,
    token: String,
    store_loc: StoreLocation,
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

                let mut engine = SyncEngine::new(client, store, worker);
                engine.run().await;
            });
        })
        .expect("spawn postbox-hub-sync thread")
}

struct SyncEngine {
    client: HubClient,
    store: Store,
    worker: SyncWorkerHandle,
    open_chat: Option<ChatKey>,
    sse_parser: SseParser,
    reconnect: SseReconnectState,
    last_sync_mono: Instant,
    last_sync_wall: i64,
}

impl SyncEngine {
    fn new(client: HubClient, store: Store, worker: SyncWorkerHandle) -> Self {
        Self {
            client,
            store,
            worker,
            open_chat: None,
            sse_parser: SseParser::new(),
            reconnect: SseReconnectState::new(),
            last_sync_mono: Instant::now(),
            last_sync_wall: now_epoch_secs(),
        }
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
        while let Ok(cmd) = self.worker.cmd_rx.recv().await {
            match cmd {
                SyncCommand::Shutdown => return false,
                SyncCommand::RestartLiveSync => return true,
                other => {
                    self.handle_command(other).await;
                }
            }
        }
        false
    }

    async fn sleep_interruptible(&mut self, dur: Duration) -> bool {
        let sleep = tokio::time::sleep(dur);
        tokio::pin!(sleep);
        loop {
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

            tokio::select! {
                chunk_opt = stream.next_chunk() => {
                    match chunk_opt {
                        Some(Ok(chunk)) => {
                            match self.sse_parser.push_bytes(&chunk) {
                                Ok(events) if !events.is_empty() => {
                                    let is_reset = events.iter().any(|e| e.kind == "reset");
                                    let _ = self.sync_chats(is_reset).await;
                                    if let Some(chat) = self.open_chat.clone() {
                                        let _ = self.sync_chat_messages(&chat, OLDER_PAGE_LIMIT, None).await;
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
            SyncCommand::RestartLiveSync | SyncCommand::Shutdown => {}
        }
    }

    async fn sync_chats(&mut self, reset: bool) -> Result<(), HubError> {
        let hub_chats = self.client.fetch_archived_and_inbox_chats(MAX_ROWS).await?;
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
                        mime_type: None,
                        filename: if claimed.wire_filename.is_empty() {
                            None
                        } else {
                            Some(claimed.wire_filename.clone())
                        },
                        reactions: Vec::new(),
                        read_status: ReadStatus::Sent,
                        deleted: false,
                    };
                    let _ = self.store.replace_optimistic_message(
                        &claimed.chat,
                        &claimed.client_id,
                        &confirmed,
                    );
                    self.emit(SyncEvent::OutboxUpdated {
                        chat: claimed.chat,
                        notice: None,
                    });
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

    #[derive(Default)]
    struct MockState {
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
                        let g = st.lock().unwrap();
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
}
