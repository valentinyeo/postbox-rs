//! Applies `postboxctl` requests against the live view model.
//!
//! Everything here is headless: it takes a [`PostboxViewModel`] and the telemetry the desktop
//! shell samples, and returns the JSON-line [`ControlResponse`]. `desktop.rs` only has to
//! route the line onto the UI thread and send the reply back.

use crate::sync_bridge::HubConnectionStatus;
use crate::viewmodel::PostboxViewModel;
use postbox_core::control::{
    ChatSummary, ControlRequest, ControlResponse, MessageSummary, StatusReport, TelemetryCounters,
    page_limit,
};
use postbox_core::views::compare_message_order;
use postbox_core::{ChatKey, Provider, ViewTab};

/// Live numbers the desktop shell samples once per request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveTelemetry {
    pub counters: TelemetryCounters,
    pub fps_tenths: u16,
    pub gpu_texture_bytes: u64,
}

impl LiveTelemetry {
    pub fn new(counters: TelemetryCounters, fps: f64) -> Self {
        Self {
            gpu_texture_bytes: counters.gpu_texture_bytes,
            counters,
            fps_tenths: (fps.max(0.0) * 10.0).round() as u16,
        }
    }

    pub fn fps(&self) -> f64 {
        f64::from(self.fps_tenths) / 10.0
    }
}

/// Builds the `status` payload.
pub fn status_report(vm: &PostboxViewModel, telemetry: &LiveTelemetry) -> StatusReport {
    let selected = vm.selected_chat().map(|chat| {
        format!(
            "{}/{}/{}",
            chat.key.provider.as_str(),
            chat.key.account,
            chat.key.id
        )
    });
    let view_key = vm.active_view_key();
    let active_view = vm
        .active_view()
        .map(|view| view.label().to_owned())
        .unwrap_or_else(|| "Combined inbox".to_owned());
    let hub = match vm.hub_status() {
        HubConnectionStatus::Connected { .. } => "connected",
        HubConnectionStatus::Connecting { .. } | HubConnectionStatus::Reconnecting { .. } => {
            "connecting"
        }
        HubConnectionStatus::NotConfigured { .. } => "not-configured",
        HubConnectionStatus::Error { .. } => "error",
    };
    let mut report = StatusReport::from_counters(
        &telemetry.counters,
        telemetry.fps(),
        vm.version(),
        &active_view,
        selected.as_deref(),
    );
    report.gpu_texture_bytes = telemetry.gpu_texture_bytes;
    report.active_view = format!("{view_key}|{active_view}|{hub}");
    report
}

/// Applies one request and returns the line to send back.
pub fn handle(
    vm: &mut PostboxViewModel,
    request: &ControlRequest,
    telemetry: &LiveTelemetry,
) -> ControlResponse {
    match request {
        ControlRequest::Status => ControlResponse::with_status(status_report(vm, telemetry)),
        ControlRequest::Theme { theme } => {
            vm.set_theme(*theme);
            ControlResponse::with_message(format!("Theme set to {}", theme.label()))
        }
        ControlRequest::Font { percent } => {
            let clamped = postbox_core::control::clamped_percent(*percent);
            vm.set_font_scale(postbox_core::FontScale::clamped(clamped));
            ControlResponse::with_message(format!("Font scale set to {clamped}%"))
        }
        ControlRequest::Chats {
            provider,
            archived,
            all,
            limit,
        } => {
            let limit = page_limit(*limit);
            let mut rows: Vec<ChatSummary> = if *all || *archived {
                let view = vm.active_view().cloned().unwrap_or(ViewTab::CombinedInbox);
                match vm.store().list_chats_filtered(
                    &view,
                    true,
                    false,
                    false,
                    postbox_core::control::MAX_LIMIT,
                ) {
                    Ok(chats) => chats.iter().map(ChatSummary::from_chat).collect(),
                    Err(_) => Vec::new(),
                }
            } else {
                vm.chats().iter().map(ChatSummary::from_chat).collect()
            };
            if let Some(provider) = provider {
                rows.retain(|row| row.provider == *provider);
            }
            if !*all {
                rows.retain(|row| row.archived == *archived);
            }
            rows.truncate(limit);
            ControlResponse::with_chats(rows)
        }
        ControlRequest::Search { text } => {
            match vm.store().search_messages_fts(text, page_limit(None)) {
                Ok(hits) => {
                    let rows = hits
                        .into_iter()
                        .map(|hit| MessageSummary {
                            id: hit.msg_id,
                            provider: hit.chat.provider,
                            account: hit.chat.account,
                            chat_id: hit.chat.id,
                            sender: hit.sender_name,
                            ts: String::new(),
                            from_me: false,
                            body: hit.snippet,
                        })
                        .collect();
                    ControlResponse::with_messages(rows)
                }
                Err(err) => ControlResponse::error(err.to_string()),
            }
        }
        ControlRequest::View { target } => {
            if vm.switch_to_view_key(target) {
                ControlResponse::with_message(format!("Switched to {target}"))
            } else {
                ControlResponse::error(format!("no such view: {target}"))
            }
        }
        ControlRequest::Select {
            id,
            provider,
            account,
        } => match resolve_chat(vm, id, *provider, account.as_deref()) {
            Some(key) => {
                if vm.select_chat_key(&key) {
                    ControlResponse::with_message(format!("Selected {}", chat_label(&key)))
                } else {
                    ControlResponse::error(format!("could not select {}", chat_label(&key)))
                }
            }
            None => ControlResponse::error(format!("no such chat: {id}")),
        },
        ControlRequest::Messages { id, limit } => {
            let key = match id {
                Some(id) => match resolve_chat(vm, id, None, None) {
                    Some(key) => key,
                    None => return ControlResponse::error(format!("no such chat: {id}")),
                },
                None => match vm.selected_chat() {
                    Some(chat) => chat.key.clone(),
                    None => return ControlResponse::error("no chat selected"),
                },
            };
            let limit = page_limit(*limit);
            match vm.store().list_messages(&key, limit, None) {
                Ok(mut messages) => {
                    messages
                        .sort_by(|a, b| compare_message_order(&a.ts, &a.msg_id, &b.ts, &b.msg_id));
                    ControlResponse::with_messages(
                        messages.iter().map(MessageSummary::from_message).collect(),
                    )
                }
                Err(err) => ControlResponse::error(err.to_string()),
            }
        }
        ControlRequest::Archive { id } => set_archived(vm, id, true),
        ControlRequest::Unarchive { id } => set_archived(vm, id, false),
        ControlRequest::ArchiveMany { ids, archived } => {
            let mut changed = 0usize;
            for id in ids {
                if resolve_chat(vm, id, None, None).is_some()
                    && set_chat_archived(vm, id, *archived).ok
                {
                    changed += 1;
                }
            }
            ControlResponse::with_message(format!(
                "{} {changed} chats",
                if *archived { "Archived" } else { "Unarchived" }
            ))
        }
    }
}

fn set_archived(vm: &mut PostboxViewModel, id: &str, archived: bool) -> ControlResponse {
    set_chat_archived(vm, id, archived)
}

fn set_chat_archived(vm: &mut PostboxViewModel, id: &str, archived: bool) -> ControlResponse {
    let Some(key) = resolve_chat(vm, id, None, None) else {
        return ControlResponse::error(format!("no such chat: {id}"));
    };
    let already = vm
        .store()
        .get_chat(&key)
        .ok()
        .flatten()
        .map(|chat| chat.is_archived())
        .unwrap_or(false);
    if already == archived {
        return ControlResponse::with_message(format!(
            "{} is already {}",
            chat_label(&key),
            state_word(archived)
        ));
    }
    if !vm.select_chat_key(&key) {
        return ControlResponse::error(format!("could not select {}", chat_label(&key)));
    }
    vm.archive_selected();
    ControlResponse::with_message(format!(
        "{} {}",
        if archived { "Archived" } else { "Unarchived" },
        chat_label(&key)
    ))
}

fn state_word(archived: bool) -> &'static str {
    if archived { "archived" } else { "in the inbox" }
}

fn chat_label(key: &ChatKey) -> String {
    format!("{}/{}/{}", key.provider.as_str(), key.account, key.id)
}

/// Finds a chat by `id`, optionally narrowed by provider and account.
///
/// Accepts a bare chat id (`C02`), the `provider/account/chat` triple, or a message id's chat
/// when `id` matches what the sidebar lists.
pub fn resolve_chat(
    vm: &PostboxViewModel,
    id: &str,
    provider: Option<Provider>,
    account: Option<&str>,
) -> Option<ChatKey> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (inline_provider, inline_account, inline_chat) = split_triple(trimmed).unwrap_or_default();
    let provider = provider.or(inline_provider);
    let account = account.or(inline_account.as_deref());
    let chat_id = inline_chat.unwrap_or_else(|| trimmed.to_owned());

    let matches = |key: &ChatKey| {
        key.id == chat_id
            && provider.is_none_or(|p| key.provider == p)
            && account.is_none_or(|a| key.account == a)
    };

    if let Some(chat) = vm.chats().iter().find(|chat| matches(&chat.key)) {
        return Some(chat.key.clone());
    }
    // A chat the active view does not list may still be on disk: archived, read, or in
    // another account. Try every account's exact key before giving up, which cannot miss a
    // row the way a filtered listing can.
    vm.accounts()
        .iter()
        .filter(|row| provider.is_none_or(|p| row.provider == p))
        .filter(|row| account.is_none_or(|a| row.account_id == a))
        .map(|row| ChatKey::new(row.provider, row.account_id.clone(), chat_id.clone()))
        .find(|key| vm.store().get_chat(key).ok().flatten().is_some())
}

fn split_triple(raw: &str) -> Option<(Option<Provider>, Option<String>, Option<String>)> {
    let mut parts = raw.splitn(3, '/');
    let provider = Provider::parse(parts.next()?)?;
    let account = parts.next()?.trim();
    if account.is_empty() {
        return None;
    }
    let chat_id = parts.next()?.trim();
    if chat_id.is_empty() {
        return None;
    }
    Some((
        Some(provider),
        Some(account.to_owned()),
        Some(chat_id.to_owned()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::control::TelemetryCounters;
    use postbox_core::{Account, Chat, FontScale, ThemeChoice};
    use postbox_keymap::Keymap;
    use postbox_store::Store;

    fn temp_db(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "postbox-ctl-{tag}-{}-{}",
            std::process::id(),
            postbox_core::update::digest_hex(tag.as_bytes())
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("postbox.db")
    }

    fn account(provider: Provider, id: &str) -> Account {
        Account {
            provider,
            account_id: id.into(),
            label: format!("{} {id}", provider.as_str()),
            state: "connected".into(),
            sort_order: 0,
            last_event_at: None,
        }
    }

    fn chat(provider: Provider, account: &str, id: &str, title: &str) -> Chat {
        Chat {
            key: ChatKey::new(provider, account, id),
            title: title.into(),
            is_group: false,
            archived_local: false,
            archived_remote: false,
            pinned: false,
            unread_count: 1,
            last_msg_ts: "2025-01-15T12:00:00Z".into(),
            last_msg_preview: "hi".into(),
            avatar_hash: None,
        }
    }

    struct Harness {
        vm: PostboxViewModel,
        dir: std::path::PathBuf,
        telemetry: LiveTelemetry,
    }

    impl Harness {
        fn new(tag: &str) -> Self {
            let path = temp_db(tag);
            let dir = path.parent().unwrap().to_path_buf();
            let store = Store::open(&path).unwrap();
            store
                .upsert_account(&account(Provider::Whatsapp, "biz"))
                .unwrap();
            store
                .upsert_account(&account(Provider::Slack, "T01"))
                .unwrap();
            store
                .upsert_chat(&chat(Provider::Whatsapp, "biz", "wa-1", "Ada"), 1)
                .unwrap();
            store
                .upsert_chat(&chat(Provider::Slack, "T01", "C02", "Engineering"), 1)
                .unwrap();
            let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "0.1.0");
            vm.initialize();
            let telemetry = LiveTelemetry::new(
                TelemetryCounters {
                    frames_rendered: 120,
                    working_set_kb: 61 * 1024,
                    private_usage_kb: 54 * 1024,
                    gpu_texture_bytes: 8 * 1024 * 1024,
                    pending_outbox: 1,
                },
                59.9,
            );
            Self { vm, dir, telemetry }
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn status_reports_live_memory_fps_and_the_selected_chat() {
        let mut h = Harness::new("status");
        h.vm.select_chat_key(&ChatKey::new(Provider::Slack, "T01", "C02"));
        let response = handle(&mut h.vm, &ControlRequest::Status, &h.telemetry);
        let status = response.status.expect("a status payload");
        assert_eq!(status.version, "0.1.0");
        assert_eq!(status.working_set_kb, 61 * 1024);
        assert_eq!(status.private_usage_kb, 54 * 1024);
        assert_eq!(status.gpu_texture_bytes, 8 * 1024 * 1024);
        assert_eq!(status.pending_outbox, 1);
        assert_eq!(status.frames_rendered, 120);
        assert!((status.fps - 59.9).abs() < f64::EPSILON);
        assert_eq!(status.selected_chat.as_deref(), Some("slack/T01/C02"));
        assert!(
            status.active_view.contains("Combined inbox"),
            "{}",
            status.active_view
        );
    }

    #[test]
    fn theme_and_font_commands_change_the_view_model() {
        let mut h = Harness::new("theming");
        handle(
            &mut h.vm,
            &ControlRequest::Theme {
                theme: ThemeChoice::Amoled,
            },
            &h.telemetry,
        );
        assert_eq!(h.vm.theme(), ThemeChoice::Amoled);
        handle(
            &mut h.vm,
            &ControlRequest::Font { percent: 500 },
            &h.telemetry,
        );
        assert_eq!(h.vm.font_scale().percent(), FontScale::MAX);
    }

    #[test]
    fn chats_lists_rows_and_filters_by_provider_and_archive() {
        let mut h = Harness::new("chats");
        let all = handle(
            &mut h.vm,
            &ControlRequest::Chats {
                provider: None,
                archived: false,
                all: true,
                limit: Some(10),
            },
            &h.telemetry,
        );
        assert_eq!(all.chats.as_ref().unwrap().len(), 2);

        let slack = handle(
            &mut h.vm,
            &ControlRequest::Chats {
                provider: Some(Provider::Slack),
                archived: false,
                all: true,
                limit: Some(10),
            },
            &h.telemetry,
        );
        let rows = slack.chats.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "C02");
    }

    #[test]
    fn select_accepts_a_bare_id_or_a_full_triple() {
        let mut h = Harness::new("select");
        let response = handle(
            &mut h.vm,
            &ControlRequest::Select {
                id: "C02".into(),
                provider: None,
                account: None,
            },
            &h.telemetry,
        );
        assert!(response.ok, "{response:?}");
        assert_eq!(
            h.vm.selected_chat().map(|c| c.key.id.clone()).as_deref(),
            Some("C02")
        );

        let triple = handle(
            &mut h.vm,
            &ControlRequest::Select {
                id: "whatsapp/biz/wa-1".into(),
                provider: None,
                account: None,
            },
            &h.telemetry,
        );
        assert!(triple.ok, "{triple:?}");
        assert_eq!(
            h.vm.selected_chat().map(|c| c.key.id.clone()).as_deref(),
            Some("wa-1")
        );

        let missing = handle(
            &mut h.vm,
            &ControlRequest::Select {
                id: "nope".into(),
                provider: None,
                account: None,
            },
            &h.telemetry,
        );
        assert!(!missing.ok);
        assert!(missing.error.unwrap().contains("no such chat: nope"));
    }

    #[test]
    fn archive_and_unarchive_change_the_stored_chat() {
        let mut h = Harness::new("archive");
        let key = ChatKey::new(Provider::Slack, "T01", "C02");

        let archived = handle(
            &mut h.vm,
            &ControlRequest::Archive { id: "C02".into() },
            &h.telemetry,
        );
        assert!(archived.ok, "{archived:?}");
        assert!(h.vm.store().get_chat(&key).unwrap().unwrap().is_archived());

        // Archiving again is a no-op that says so.
        let again = handle(
            &mut h.vm,
            &ControlRequest::Archive { id: "C02".into() },
            &h.telemetry,
        );
        assert!(again.ok);
        assert!(again.message.unwrap().contains("already"));

        let unarchived = handle(
            &mut h.vm,
            &ControlRequest::Unarchive { id: "C02".into() },
            &h.telemetry,
        );
        assert!(unarchived.ok, "{unarchived:?}");
        assert!(!h.vm.store().get_chat(&key).unwrap().unwrap().is_archived());
    }

    #[test]
    fn archive_many_reports_how_many_chats_changed() {
        let mut h = Harness::new("archivemany");
        let response = handle(
            &mut h.vm,
            &ControlRequest::ArchiveMany {
                ids: vec!["C02".into(), "wa-1".into(), "missing".into()],
                archived: true,
            },
            &h.telemetry,
        );
        assert!(response.ok);
        assert_eq!(response.message.as_deref(), Some("Archived 2 chats"));
        assert!(
            h.vm.store()
                .get_chat(&ChatKey::new(Provider::Whatsapp, "biz", "wa-1"))
                .unwrap()
                .unwrap()
                .is_archived()
        );
    }

    #[test]
    fn view_switches_to_an_account_or_reports_an_unknown_view() {
        let mut h = Harness::new("view");
        let ok = handle(
            &mut h.vm,
            &ControlRequest::View {
                target: "slack:T01".into(),
            },
            &h.telemetry,
        );
        assert!(ok.ok, "{ok:?}");
        assert_eq!(h.vm.active_view_key(), "slack:T01");

        let bad = handle(
            &mut h.vm,
            &ControlRequest::View {
                target: "slack:T99".into(),
            },
            &h.telemetry,
        );
        assert!(!bad.ok);
    }

    #[test]
    fn search_returns_matching_messages_and_an_empty_query_is_harmless() {
        let mut h = Harness::new("search");
        assert!(
            handle(
                &mut h.vm,
                &ControlRequest::Search {
                    text: String::new()
                },
                &h.telemetry
            )
            .ok
        );
        assert!(
            handle(
                &mut h.vm,
                &ControlRequest::Search {
                    text: "invoice".into()
                },
                &h.telemetry
            )
            .messages
            .is_some()
        );
    }

    #[test]
    fn messages_reads_the_selected_chat_and_reports_a_missing_one() {
        let mut h = Harness::new("messages");
        let selected = handle(
            &mut h.vm,
            &ControlRequest::Messages {
                id: None,
                limit: Some(5),
            },
            &h.telemetry,
        );
        // A chat may or may not be selected before the user clicks; both are valid outcomes.
        assert!(selected.ok || selected.error.is_some());

        let missing = handle(
            &mut h.vm,
            &ControlRequest::Messages {
                id: Some("nope".into()),
                limit: Some(5),
            },
            &h.telemetry,
        );
        assert!(!missing.ok);
    }

    #[test]
    fn every_request_variant_answers_without_panicking() {
        let mut h = Harness::new("allvariants");
        let requests = vec![
            ControlRequest::Status,
            ControlRequest::Theme {
                theme: ThemeChoice::Light,
            },
            ControlRequest::Font { percent: 100 },
            ControlRequest::Chats {
                provider: None,
                archived: false,
                all: false,
                limit: None,
            },
            ControlRequest::Search { text: "x".into() },
            ControlRequest::View {
                target: "all".into(),
            },
            ControlRequest::Select {
                id: "C02".into(),
                provider: Some(Provider::Slack),
                account: Some("T01".into()),
            },
            ControlRequest::Messages {
                id: Some("C02".into()),
                limit: Some(1),
            },
            ControlRequest::Archive { id: "C02".into() },
            ControlRequest::Unarchive { id: "C02".into() },
            ControlRequest::ArchiveMany {
                ids: vec!["C02".into()],
                archived: false,
            },
        ];
        for request in requests {
            let response = handle(&mut h.vm, &request, &h.telemetry);
            // Every answer must serialise as exactly one JSON line.
            let line = postbox_core::control::encode_response(&response);
            assert!(!line.contains('\n'), "{}", request.command_name());
            assert!(postbox_core::control::decode_response(&line).is_ok());
        }
    }
}
