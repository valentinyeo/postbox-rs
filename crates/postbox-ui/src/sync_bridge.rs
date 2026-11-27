use postbox_core::ChatKey;

/// Live Hub connection/setup status surfaced in the zero-toolbar empty state (`hub-status-card`),
/// `Ctrl+K` command palette, and transient window-title notice when disconnected/errored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubConnectionStatus {
    /// No Hub URL and/or Bearer token discovered yet.
    NotConfigured {
        missing_url: bool,
        missing_token: bool,
        configured_url: Option<String>,
        detail: Option<String>,
    },
    /// Initial handshake / baseline chat fetch in progress.
    Connecting { url: String },
    /// Live SSE stream open & baseline synced.
    Connected {
        url: String,
        last_event_id: Option<u64>,
    },
    /// SSE stream dropped; retrying with exponential backoff.
    Reconnecting {
        url: String,
        attempt: u32,
        retry_in_secs: u64,
        last_error: String,
    },
    /// Fatal auth/config error (HTTP 401/403 or invalid endpoint/token).
    Error {
        url: Option<String>,
        message: String,
    },
}

impl Default for HubConnectionStatus {
    fn default() -> Self {
        Self::NotConfigured {
            missing_url: true,
            missing_token: true,
            configured_url: None,
            detail: None,
        }
    }
}

impl HubConnectionStatus {
    /// Concise `Ctrl+K` command palette label for the Hub status row.
    pub fn palette_label(&self) -> String {
        match self {
            Self::Connected { .. } => "Hub: Connected".to_owned(),
            Self::Connecting { .. } => "Hub: Connecting...".to_owned(),
            Self::Reconnecting { retry_in_secs, .. } => {
                format!("Hub: Reconnecting (retry in {retry_in_secs}s)")
            }
            Self::NotConfigured {
                missing_url,
                missing_token,
                ..
            } => {
                if *missing_url && *missing_token {
                    "Hub: Not configured (set POSTBOX_HUB_URL & POSTBOX_HUB_TOKEN)".to_owned()
                } else if *missing_url {
                    "Hub: Not configured (set POSTBOX_HUB_URL or WAZIG_HUB_URL)".to_owned()
                } else {
                    "Hub: Not configured (set POSTBOX_HUB_TOKEN or WAZIG_HUB_TOKEN)".to_owned()
                }
            }
            Self::Error { message, .. } => {
                let short = if message.len() > 64 {
                    let mut end = 64;
                    while end > 0 && !message.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("{}...", &message[..end])
                } else {
                    message.clone()
                };
                format!("Hub: Error — {short}")
            }
        }
    }

    /// Transient window-title notice shown ONLY when reconnecting or errored (preserving zero-toolbar).
    pub fn title_notice(&self) -> Option<String> {
        match self {
            Self::Reconnecting { .. } => Some("Reconnecting to Hub".to_owned()),
            Self::Error { .. } => Some("Hub error".to_owned()),
            Self::NotConfigured { .. } | Self::Connecting { .. } | Self::Connected { .. } => None,
        }
    }

    pub fn sidebar_empty_label(&self) -> &'static str {
        match self {
            Self::NotConfigured { .. } => "Hub not configured",
            Self::Connecting { .. } => "Connecting to Hub...",
            Self::Connected { .. } => "No chats in this view",
            Self::Reconnecting { .. } => "Reconnecting to Hub...",
            Self::Error { .. } => "Hub connection error",
        }
    }

    pub fn is_configured(&self) -> bool {
        !matches!(self, Self::NotConfigured { .. })
    }
}

/// Commands pushed from UI (`PostboxViewModel`) to the background Tokio Hub sync engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncCommand {
    RefreshAll {
        open_chat: Option<ChatKey>,
    },
    FetchChatMessages {
        chat: ChatKey,
        limit: usize,
    },
    FetchOlderMessages {
        chat: ChatKey,
        before_cursor: String,
        limit: usize,
    },
    WakeOutbox,
    WakeMarkRead,
    WakeArchive,
    RestartLiveSync,
    DeleteMessage {
        chat: ChatKey,
        msg_id: String,
        client_id: String,
    },
    Shutdown,
}

/// Notifications pushed from the background Tokio Hub sync engine back to the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncEvent {
    StatusChanged(HubConnectionStatus),
    ChatsUpdated {
        reset: bool,
    },
    MessagesUpdated {
        chat: ChatKey,
        prepended: usize,
        exhausted: bool,
    },
    OutboxUpdated {
        chat: ChatKey,
        notice: Option<String>,
    },
    Notice(String),
}

/// Runtime-agnostic channel bridge connecting `postbox-ui` (GPUI/smol) and `postbox-app` (Tokio)
/// with 0% idle CPU/GPU wakeups.
pub struct SyncBridge {
    pub cmd_tx: async_channel::Sender<SyncCommand>,
    pub event_rx: async_channel::Receiver<SyncEvent>,
}

/// Worker-side endpoints paired with a `SyncBridge`.
pub struct SyncWorkerHandle {
    pub cmd_rx: async_channel::Receiver<SyncCommand>,
    pub event_tx: async_channel::Sender<SyncEvent>,
}

pub fn create_sync_bridge() -> (SyncBridge, SyncWorkerHandle) {
    let (cmd_tx, cmd_rx) = async_channel::bounded(128);
    let (event_tx, event_rx) = async_channel::bounded(256);
    (
        SyncBridge { cmd_tx, event_rx },
        SyncWorkerHandle { cmd_rx, event_tx },
    )
}
