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

use postbox_core::MediaKind;
use postbox_core::dictation::DictationLanguage;
use postbox_core::voice::{AudioDeviceInfo, PlaybackSpeed, TranscriptError};
use postbox_media::avatar::SlackEmojiIndex;
use postbox_media::drain::MediaItemStatus;
use postbox_media::giphy::{GiphyPage, GiphyTarget};
use postbox_media::unfurl::{ClassifiedUrl, UnfurlCard};
use std::path::PathBuf;

/// Descriptor for a single downloadable media item queued from an open chat (newest-first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaDownloadTarget {
    pub msg_id: String,
    pub filename: Option<String>,
    pub mime_type: Option<String>,
    pub media_kind: Option<MediaKind>,
    pub file_size: Option<u64>,
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
    // --- Milestone 3 commands ---
    /// Auto-downloads all eligible media (`<= 100 MiB`) in an opened chat newest-first.
    DownloadChatMedia {
        chat: ChatKey,
        items: Vec<MediaDownloadTarget>,
    },
    /// User-triggered explicit download (for `> 100 MiB`, retry, or lightbox on-demand).
    DownloadMediaExplicit {
        chat: ChatKey,
        target: MediaDownloadTarget,
    },
    FetchAvatar {
        chat: ChatKey,
    },
    FetchWorkspaceIcon {
        team: String,
    },
    EnsureSlackEmojiIndex {
        team: String,
    },
    FetchSlackEmojiMedia {
        team: String,
        name: String,
        url: String,
    },
    ReactToMessage {
        chat: ChatKey,
        msg_id: String,
        emoji: String,
        client_id: String,
    },
    UnfurlUrl {
        target: ClassifiedUrl,
    },
    SearchGiphy {
        query: String,
        offset: u32,
        generation: u64,
        target: GiphyTarget,
    },
    DownloadGiphyGif {
        gif_id: String,
        url: String,
        index: usize,
        generation: u64,
        download_generation: u64,
        target: GiphyTarget,
    },
    CancelGiphy {
        generation: u64,
    },
    // --- Milestone 4 commands ---
    TranscribeAudio {
        chat: ChatKey,
        msg_id: String,
        path: PathBuf,
        sha256: String,
        mime_type: String,
        from_me: bool,
        contact_name: String,
        language: DictationLanguage,
    },
    FormatTranscript {
        chat: ChatKey,
        msg_id: String,
        sha256: String,
        raw_text: String,
        from_me: bool,
        contact_name: String,
        local_path: Option<PathBuf>,
    },
    VoicePlay {
        chat: ChatKey,
        msg_id: String,
        path: PathBuf,
        speed: PlaybackSpeed,
        start_ms: u64,
    },
    VoicePause,
    VoiceResume,
    VoiceStop,
    VoiceSeek {
        pos_ms: u64,
    },
    VoiceSetSpeed(PlaybackSpeed),
    VoiceRecordStart {
        generation: u64,
        origin_chat: ChatKey,
        preferred_mic: Option<String>,
    },
    VoiceRecordFinish,
    VoiceRecordCancel,
    RefreshMicrophones,
    // --- Milestone 5 commands ---
    DictationStart {
        generation: u64,
        origin_chat: ChatKey,
        language: DictationLanguage,
        preferred_mic: Option<String>,
    },
    DictationStop {
        generation: u64,
    },
    DictationCancel {
        generation: u64,
    },
    SaveDictationLanguage(DictationLanguage),
    SaveMicrophonePreference(Option<String>),
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
    // --- Milestone 3 events ---
    MediaDownloaded {
        chat: ChatKey,
        msg_id: String,
        path: PathBuf,
        mime_type: String,
        sha256: String,
    },
    MediaStatusChanged {
        chat: ChatKey,
        msg_id: String,
        status: MediaItemStatus,
    },
    AvatarReady {
        chat: ChatKey,
        path: Option<PathBuf>,
    },
    WorkspaceIconReady {
        team: String,
        path: Option<PathBuf>,
    },
    SlackEmojiIndexReady {
        team: String,
        index: SlackEmojiIndex,
    },
    SlackEmojiMediaReady {
        team: String,
        name: String,
        path: PathBuf,
    },
    ReactionSettled {
        chat: ChatKey,
        msg_id: String,
        emoji: String,
        ok: bool,
        http_status: Option<u16>,
    },
    UnfurlReady {
        target: ClassifiedUrl,
        card: Option<UnfurlCard>,
    },
    GiphyResults {
        generation: u64,
        target: GiphyTarget,
        page: Option<GiphyPage>,
        error: Option<String>,
    },
    GiphyPreviewReady {
        generation: u64,
        index: usize,
        path: PathBuf,
    },
    GiphyDownloadReady {
        generation: u64,
        download_generation: u64,
        target: GiphyTarget,
        index: usize,
        gif_id: String,
        bytes: Vec<u8>,
    },
    GiphyError {
        generation: u64,
        index: usize,
        download_generation: u64,
        message: String,
    },
    // --- Milestone 4 events ---
    TranscriptReady {
        chat: ChatKey,
        msg_id: String,
        sha256: String,
        raw_text: String,
        formatted_text: Option<String>,
        language: String,
    },
    TranscriptFailed {
        chat: ChatKey,
        msg_id: String,
        error: TranscriptError,
    },
    TranscriptFormatted {
        chat: ChatKey,
        msg_id: String,
        sha256: String,
        formatted_text: String,
    },
    VoicePlaybackProgress {
        chat: ChatKey,
        msg_id: String,
        pos_ms: u64,
        dur_ms: u64,
        waveform: Vec<u8>,
        playing: bool,
    },
    VoicePlaybackEnded {
        chat: ChatKey,
        msg_id: String,
    },
    VoicePlaybackFailed {
        chat: ChatKey,
        msg_id: String,
        message: String,
    },
    VoiceRecordTick {
        generation: u64,
        elapsed_ms: u64,
        bars: Vec<u8>,
    },
    VoiceRecordFinished {
        generation: u64,
        origin_chat: ChatKey,
        ogg_bytes: Vec<u8>,
        duration_secs: u32,
        waveform: Vec<u8>,
    },
    VoiceRecordFailed {
        generation: u64,
        message: String,
    },
    MicrophonesUpdated(Vec<AudioDeviceInfo>),
    // --- Milestone 5 events ---
    DictationConnected {
        generation: u64,
    },
    DictationInterim {
        generation: u64,
        revision: u64,
        text: String,
    },
    DictationLevel {
        generation: u64,
        level: u8,
    },
    DictationDraining {
        generation: u64,
    },
    DictationFinal {
        generation: u64,
        text: String,
    },
    DictationFailed {
        generation: u64,
        message: String,
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
    let (cmd_tx, cmd_rx) = async_channel::bounded(256);
    let (event_tx, event_rx) = async_channel::bounded(512);
    (
        SyncBridge { cmd_tx, event_rx },
        SyncWorkerHandle { cmd_rx, event_tx },
    )
}
