use crate::desktop_m3::{GpuTextureLru, avatar_tex_key, workspace_icon_tex_key};
use crate::geometry::{
    CHAT_COLUMN_MAX_WIDTH_PX, SIDEBAR_WIDTH_PX, TYPE_CHAT_NAME_PX, TYPE_LABEL_PX, TYPE_TIME_PX,
    VIEW_RAIL_WIDTH_PX,
};
use crate::palette::PaletteMode;
use crate::sync_bridge::{HubConnectionStatus, SyncBridge, SyncCommand, SyncEvent};
use crate::theme::{ShadcnPalette, archived_row_color, archived_text_color};
use crate::timeline::{OutboxBadge, ReadTickDisplay, TimelineItem, format_time_hm};
use crate::viewmodel::{FocusTarget, PostboxLaunchContext, PostboxViewModel, SidebarChatRow};
use crate::{BASE_FONT_PX, FONT_MONO, FONT_SANS, scaled_px};
use gpui_kit::assets::Assets;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::base::actions::Cancel;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::command::{Command, CommandGroup, CommandItem, CommandState};
use gpui_kit::component::input::{InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::message_scroller::{MessageScroller, MessageScrollerState};
use gpui_kit::component::{IndexPath, Theme, ThemeMode, VirtualListScrollHandle, v_virtual_list};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Action as GpuiAction, AnyElement, App, AppContext as _, AsyncWindowContext, Context, Entity,
    ExternalPaths, FocusHandle, Focusable, Hsla, InteractiveElement as _, IntoElement, KeyBinding,
    KeyBindingContextPredicate, KeyDownEvent, Keystroke, Modifiers, ModifiersChangedEvent,
    MouseButton, MouseDownEvent, ObjectFit, ParentElement as _, Pixels, Render, ScrollStrategy,
    SharedString, Size, StatefulInteractiveElement as _, Styled as _, StyledImage as _,
    Subscription, Task, TitlebarOptions, WeakEntity, Window, WindowOptions, div, hsla, img, px,
    rgb, size,
};
use postbox_core::threads::replies_line;
use postbox_core::{FontScale, Provider, ThemeChoice};
use postbox_keymap::{Action as PbAction, Keymap, f1_shortcut_groups};
use postbox_media::paste::PasteStageResult;
use postbox_media::unfurl::omit_unfurled_url_from_body;
use postbox_platform::{ClipboardBackend, MemoryPressure, NativePlatform};
use std::borrow::Cow;
use std::rc::Rc;
use std::time::Instant;

gpui_kit::actions!(
    postbox,
    [
        OpenPalette,
        ShowShortcutsHelp,
        SearchChats,
        ClearSearchOrBack,
        ToggleUnread,
        ToggleArchived,
        ArchiveSelected,
        KeepArchivedSelected,
        SelectAllChats,
        Refresh,
        RestartLiveSync,
        Quit,
        NextChat,
        PrevChat,
        FirstChat,
        LastChat,
        PageUpChats,
        PageDownChats,
        OpenChatSlot1,
        OpenChatSlot2,
        OpenChatSlot3,
        OpenChatSlot4,
        OpenChatSlot5,
        OpenChatSlot6,
        OpenChatSlot7,
        OpenChatSlot8,
        OpenChatSlot9,
        SwitchCombinedInbox,
        SwitchAccountView2,
        SwitchAccountView3,
        SwitchAccountView4,
        SwitchAccountView5,
        SwitchAccountView6,
        SwitchAccountView7,
        SwitchAccountView8,
        SwitchAccountView9,
        NextMessage,
        PrevMessage,
        PageUpMessages,
        PageDownMessages,
        JumpLatest,
        ReactToMessage,
        ReplyToMessage,
        OpenThread,
        ResendFailedMessage,
        OpenImageLightbox,
        PasteImage,
        CloseLightbox,
        PrevLightboxImage,
        NextLightboxImage,
        EmojiMoveUp,
        EmojiMoveDown,
        EmojiMoveLeft,
        EmojiMoveRight,
        EmojiActivate,
        CloseEmojiPicker,
        ShortcodePrev,
        ShortcodeNext,
        ShortcodeAccept,
        CloseShortcodePopup,
        CloseThreadPanel,
        CycleThreadFocus,
        ToggleThreadBroadcast,
        OpenLinksAndMedia,
        FocusComposer,
        FontSmaller,
        FontLarger,
        FontReset,
        PlayPauseVoice,
        ToggleTranscript,
        FinishVoiceRecord,
        CancelVoiceRecord,
        ToggleDictation,
        FinishDictation,
        CancelDictation,
        CopyMessageText,
        SmartCopy,
        MoveViewUp,
        MoveViewDown,
        ToggleVideoFullscreen,
        CloseVideoPlayer,
        ToggleVideoPlayPause,
        ToggleVideoMute,
    ]
);

fn gpui_action_for(action: PbAction) -> Option<Box<dyn GpuiAction>> {
    match action {
        PbAction::OpenPalette => Some(Box::new(OpenPalette)),
        PbAction::ShowShortcutsHelp => Some(Box::new(ShowShortcutsHelp)),
        PbAction::SearchChats => Some(Box::new(SearchChats)),
        PbAction::ClearSearchOrBack => Some(Box::new(ClearSearchOrBack)),
        PbAction::ToggleUnread => Some(Box::new(ToggleUnread)),
        PbAction::ToggleArchived => Some(Box::new(ToggleArchived)),
        PbAction::ArchiveSelected => Some(Box::new(ArchiveSelected)),
        PbAction::KeepArchivedSelected => Some(Box::new(KeepArchivedSelected)),
        PbAction::SelectAllChats => Some(Box::new(SelectAllChats)),
        PbAction::Refresh => Some(Box::new(Refresh)),
        PbAction::RestartLiveSync => Some(Box::new(RestartLiveSync)),
        PbAction::Quit => Some(Box::new(Quit)),
        PbAction::NextChat => Some(Box::new(NextChat)),
        PbAction::PrevChat => Some(Box::new(PrevChat)),
        PbAction::FirstChat => Some(Box::new(FirstChat)),
        PbAction::LastChat => Some(Box::new(LastChat)),
        PbAction::PageUpChats => Some(Box::new(PageUpChats)),
        PbAction::PageDownChats => Some(Box::new(PageDownChats)),
        PbAction::OpenChatSlot(1) => Some(Box::new(OpenChatSlot1)),
        PbAction::OpenChatSlot(2) => Some(Box::new(OpenChatSlot2)),
        PbAction::OpenChatSlot(3) => Some(Box::new(OpenChatSlot3)),
        PbAction::OpenChatSlot(4) => Some(Box::new(OpenChatSlot4)),
        PbAction::OpenChatSlot(5) => Some(Box::new(OpenChatSlot5)),
        PbAction::OpenChatSlot(6) => Some(Box::new(OpenChatSlot6)),
        PbAction::OpenChatSlot(7) => Some(Box::new(OpenChatSlot7)),
        PbAction::OpenChatSlot(8) => Some(Box::new(OpenChatSlot8)),
        PbAction::OpenChatSlot(9) => Some(Box::new(OpenChatSlot9)),
        PbAction::SwitchCombinedInbox => Some(Box::new(SwitchCombinedInbox)),
        PbAction::SwitchAccountView(2) => Some(Box::new(SwitchAccountView2)),
        PbAction::SwitchAccountView(3) => Some(Box::new(SwitchAccountView3)),
        PbAction::SwitchAccountView(4) => Some(Box::new(SwitchAccountView4)),
        PbAction::SwitchAccountView(5) => Some(Box::new(SwitchAccountView5)),
        PbAction::SwitchAccountView(6) => Some(Box::new(SwitchAccountView6)),
        PbAction::SwitchAccountView(7) => Some(Box::new(SwitchAccountView7)),
        PbAction::SwitchAccountView(8) => Some(Box::new(SwitchAccountView8)),
        PbAction::SwitchAccountView(9) => Some(Box::new(SwitchAccountView9)),
        PbAction::NextMessage => Some(Box::new(NextMessage)),
        PbAction::PrevMessage => Some(Box::new(PrevMessage)),
        PbAction::PageUpMessages => Some(Box::new(PageUpMessages)),
        PbAction::PageDownMessages => Some(Box::new(PageDownMessages)),
        PbAction::JumpLatest => Some(Box::new(JumpLatest)),
        PbAction::ReactToMessage => Some(Box::new(ReactToMessage)),
        PbAction::ReplyToMessage => Some(Box::new(ReplyToMessage)),
        PbAction::OpenThread => Some(Box::new(OpenThread)),
        PbAction::ResendFailedMessage => Some(Box::new(ResendFailedMessage)),
        PbAction::OpenImageLightbox => Some(Box::new(OpenImageLightbox)),
        PbAction::PasteImage => Some(Box::new(PasteImage)),
        PbAction::CloseLightbox => Some(Box::new(CloseLightbox)),
        PbAction::PrevLightboxImage => Some(Box::new(PrevLightboxImage)),
        PbAction::NextLightboxImage => Some(Box::new(NextLightboxImage)),
        PbAction::EmojiMoveUp => Some(Box::new(EmojiMoveUp)),
        PbAction::EmojiMoveDown => Some(Box::new(EmojiMoveDown)),
        PbAction::EmojiMoveLeft => Some(Box::new(EmojiMoveLeft)),
        PbAction::EmojiMoveRight => Some(Box::new(EmojiMoveRight)),
        PbAction::EmojiActivate => Some(Box::new(EmojiActivate)),
        PbAction::CloseEmojiPicker => Some(Box::new(CloseEmojiPicker)),
        PbAction::ShortcodePrev => Some(Box::new(ShortcodePrev)),
        PbAction::ShortcodeNext => Some(Box::new(ShortcodeNext)),
        PbAction::ShortcodeAccept => Some(Box::new(ShortcodeAccept)),
        PbAction::CloseShortcodePopup => Some(Box::new(CloseShortcodePopup)),
        PbAction::CloseThreadPanel => Some(Box::new(CloseThreadPanel)),
        PbAction::CycleThreadFocus => Some(Box::new(CycleThreadFocus)),
        PbAction::ToggleThreadBroadcast => Some(Box::new(ToggleThreadBroadcast)),
        PbAction::OpenLinksAndMedia => Some(Box::new(OpenLinksAndMedia)),
        PbAction::FocusComposer => Some(Box::new(FocusComposer)),
        PbAction::FontSmaller => Some(Box::new(FontSmaller)),
        PbAction::FontLarger => Some(Box::new(FontLarger)),
        PbAction::FontReset => Some(Box::new(FontReset)),
        PbAction::PlayPauseVoice => Some(Box::new(PlayPauseVoice)),
        PbAction::ToggleTranscript => Some(Box::new(ToggleTranscript)),
        PbAction::FinishVoiceRecord => Some(Box::new(FinishVoiceRecord)),
        PbAction::CancelVoiceRecord => Some(Box::new(CancelVoiceRecord)),
        PbAction::ToggleDictation => Some(Box::new(ToggleDictation)),
        PbAction::FinishDictation => Some(Box::new(FinishDictation)),
        PbAction::CancelDictation => Some(Box::new(CancelDictation)),
        PbAction::CopyMessageText => Some(Box::new(CopyMessageText)),
        PbAction::SmartCopy => Some(Box::new(SmartCopy)),
        PbAction::MoveViewUp => Some(Box::new(MoveViewUp)),
        PbAction::MoveViewDown => Some(Box::new(MoveViewDown)),
        PbAction::ToggleVideoFullscreen => Some(Box::new(ToggleVideoFullscreen)),
        PbAction::CloseVideoPlayer => Some(Box::new(CloseVideoPlayer)),
        _ => None,
    }
}

/// Registers all 6 bundled IBM Plex Sans & Mono TTF fonts with GPUI's text system.
pub fn register_bundled_fonts(cx: &App) {
    let fonts: Vec<Cow<'static, [u8]>> = vec![
        Cow::Borrowed(include_bytes!("../../../assets/IBMPlexSans-Regular.ttf")),
        Cow::Borrowed(include_bytes!("../../../assets/IBMPlexSans-Medium.ttf")),
        Cow::Borrowed(include_bytes!("../../../assets/IBMPlexSans-SemiBold.ttf")),
        Cow::Borrowed(include_bytes!("../../../assets/IBMPlexSans-Bold.ttf")),
        Cow::Borrowed(include_bytes!("../../../assets/IBMPlexSans-Italic.ttf")),
        Cow::Borrowed(include_bytes!("../../../assets/IBMPlexMono-Regular.ttf")),
    ];
    let _ = cx.text_system().add_fonts(fonts);
}

/// Translates `postbox-keymap` bindings (including `keymap.json` overrides) into GPUI `KeyBinding`s.
///
/// Scopes `PostboxComposerEmpty` (`ctrl-a`, `up`, `down`) and `PostboxThreadPanel` (`escape`, `tab`)
/// beneath `gpui-base`'s inner `Input` context (`"PostboxComposerEmpty > Input"`) so they override
/// `Textarea`'s default `Input` bindings while empty, and automatically defer to `Textarea` as soon
/// as text is entered (`key_context` switches to `"PostboxComposer"`).
pub fn bind_postbox_keys(cx: &mut App, keymap: &Keymap) {
    let mut bindings = Vec::new();
    for entry in keymap.bindings() {
        let Some(action) = gpui_action_for(entry.action) else {
            continue;
        };
        // Scope `ToggleThreadBroadcast` ("space") strictly to `PostboxThreadBroadcast` so
        // Space inserts `' '` normally inside `thread_composer` (`Textarea`) and toggles the
        // broadcast checkbox only when focus is on `thread-broadcast-wrap` (via `Tab`).
        if entry.context == postbox_keymap::KeyContext::ThreadPanel
            && entry.action == PbAction::ToggleThreadBroadcast
        {
            let pred = KeyBindingContextPredicate::parse("PostboxThreadBroadcast")
                .ok()
                .map(Rc::new);
            if let Ok(kb) = KeyBinding::load(&entry.stroke, action, pred, false, None, cx) {
                bindings.push(kb);
            }
            continue;
        }

        let extra_descendant_ctx: Option<&'static str> = match entry.context {
            postbox_keymap::KeyContext::ComposerEmpty => Some("PostboxComposerEmpty > Input"),
            postbox_keymap::KeyContext::Composer if entry.action == PbAction::PasteImage => {
                Some("PostboxComposer > Input")
            }
            postbox_keymap::KeyContext::ShortcodePopup => Some("PostboxShortcodePopup > Input"),
            postbox_keymap::KeyContext::EmojiPicker => Some("PostboxEmojiPicker > Input"),
            postbox_keymap::KeyContext::GiphyPicker => Some("PostboxGiphyPicker > Input"),
            postbox_keymap::KeyContext::VoiceRecording => Some("PostboxVoiceRecording > Input"),
            postbox_keymap::KeyContext::Dictating => Some("PostboxDictating > Input"),
            postbox_keymap::KeyContext::ThreadPanel
                if matches!(entry.stroke.as_str(), "escape" | "tab") =>
            {
                Some("PostboxThreadPanel > Input")
            }
            _ => None,
        };

        if let Some(desc_ctx) = extra_descendant_ctx
            && let Some(desc_action) = gpui_action_for(entry.action)
        {
            let pred = KeyBindingContextPredicate::parse(desc_ctx)
                .ok()
                .map(Rc::new);
            if let Ok(kb) = KeyBinding::load(&entry.stroke, desc_action, pred, false, None, cx) {
                bindings.push(kb);
            }
        }

        let predicate = KeyBindingContextPredicate::parse(entry.context.as_gpui_context())
            .ok()
            .map(Rc::new);
        if let Ok(kb) = KeyBinding::load(&entry.stroke, action, predicate, false, None, cx) {
            bindings.push(kb);
        }
    }
    cx.bind_keys(bindings);
}

fn hsla_rgb(hex: u32) -> Hsla {
    Hsla::from(rgb(hex))
}

/// Applies `ShadcnPalette` and `FontScale` to `gpui_kit::component::Theme`.
/// Switches `ThemeMode` via `Theme::change` FIRST so `Theme::update` overrides are never clobbered.
pub fn apply_postbox_theme(cx: &mut App, choice: ThemeChoice, scale: FontScale) {
    let mode = match choice {
        ThemeChoice::Light => ThemeMode::Light,
        ThemeChoice::ZincDark | ThemeChoice::Amoled => ThemeMode::Dark,
    };
    if Theme::global(cx).mode != mode {
        Theme::change(mode, None, cx);
    }

    let p = ShadcnPalette::for_choice(choice);
    let body_px = scaled_px(BASE_FONT_PX, scale.percent());
    let mono_px = scaled_px(13.0, scale.percent());

    Theme::update(cx, |theme| {
        theme.font_family = SharedString::from(FONT_SANS);
        theme.mono_font_family = SharedString::from(FONT_MONO);
        theme.font_size = px(body_px);
        theme.mono_font_size = px(mono_px);
        theme.radius = px(6.0);
        theme.radius_lg = px(10.0);

        let c = &mut theme.colors;
        c.background = hsla_rgb(p.background);
        c.foreground = hsla_rgb(p.foreground);
        c.sidebar = hsla_rgb(p.sidebar);
        c.sidebar_foreground = hsla_rgb(p.foreground);
        c.sidebar_border = hsla_rgb(p.border);
        c.sidebar_accent = hsla_rgb(p.accent);
        c.sidebar_accent_foreground = hsla_rgb(p.foreground);
        c.sidebar_primary = hsla_rgb(p.primary);
        c.sidebar_primary_foreground = hsla_rgb(p.primary_foreground);
        c.popover = hsla_rgb(p.popover);
        c.popover_foreground = hsla_rgb(p.foreground);
        c.primary = hsla_rgb(p.primary);
        c.primary_foreground = hsla_rgb(p.primary_foreground);
        c.secondary = hsla_rgb(p.secondary);
        c.secondary_foreground = hsla_rgb(p.foreground);
        c.muted = hsla_rgb(p.muted);
        c.muted_foreground = hsla_rgb(p.muted_foreground);
        c.accent = hsla_rgb(p.accent);
        c.accent_foreground = hsla_rgb(p.foreground);
        c.border = hsla_rgb(p.border);
        c.input = hsla_rgb(p.border);
        c.ring = hsla_rgb(p.ring);
        c.danger = hsla_rgb(p.destructive);
        c.list = hsla_rgb(p.sidebar);
        c.list_active = hsla_rgb(p.accent);
        c.list_active_border = hsla_rgb(p.border);
        c.list_hover = hsla_rgb(p.muted);
        c.selection = hsla_rgb(p.selection_fill);
        c.caret = hsla_rgb(p.foreground);
    });
}

fn provider_badge(provider: Provider) -> &'static str {
    match provider {
        Provider::Whatsapp => "WA",
        Provider::Slack => "SL",
        Provider::Telegram => "TG",
        Provider::Signal => "SG",
    }
}

fn slot_kbd_badge(slot: u8) -> Kbd {
    Kbd::new(Keystroke {
        modifiers: Modifiers::none(),
        key: slot.to_string(),
        key_char: None,
    })
    .outline()
}

pub struct PostboxShell {
    pub(crate) vm: PostboxViewModel,
    pub(crate) started_at: Instant,
    init_elapsed_ms: u128,
    root_focus: FocusHandle,
    sidebar_focus: FocusHandle,
    message_list_focus: FocusHandle,
    thread_broadcast_focus: FocusHandle,
    lightbox_focus: FocusHandle,
    emoji_picker_focus: FocusHandle,
    giphy_picker_focus: FocusHandle,
    pub(crate) voice_record_focus: FocusHandle,
    sidebar_scroll: VirtualListScrollHandle,
    sidebar_item_sizes: Rc<Vec<Size<Pixels>>>,
    pub(crate) sidebar_rows_cache: Vec<SidebarChatRow>,
    timeline_state: Entity<MessageScrollerState>,
    thread_timeline_state: Entity<MessageScrollerState>,
    composer: Entity<TextareaState>,
    thread_composer: Entity<TextareaState>,
    emoji_search_input: Entity<InputState>,
    giphy_search_input: Entity<InputState>,
    palette_state: Entity<CommandState>,
    last_window_title: String,
    last_applied_theme: ThemeChoice,
    last_applied_scale: FontScale,
    pub(crate) gpu_textures: GpuTextureLru,
    pub(crate) gif_tick: usize,
    pub(crate) gif_timer_task: Option<Task<()>>,
    pub(crate) dictation_timer_task: Option<Task<()>>,
    /// Milestone 6: `16 ms` presentation timer, armed only while a video is playing.
    pub(crate) video_timer_task: Option<Task<()>>,
    video_player_focus: FocusHandle,
    pub(crate) video_backend: Option<std::sync::Arc<dyn postbox_platform::VideoBackend>>,
    pub(crate) audio_backend: Option<std::sync::Arc<dyn postbox_platform::AudioBackend>>,
    pub(crate) video_thumb_cache: Option<postbox_media::VideoThumbnailCache>,
    pub(crate) video_runtime: Option<crate::desktop_m6::VideoRuntime>,
    /// M5.5 RAM diet: chat whose media is currently resident, so a chat switch can evict the
    /// previous chat's inline images/GIF frames before decoding the next chat's.
    pub(crate) resident_chat_id: Option<String>,
    /// Working-set trim + `GetProcessMemoryInfo` telemetry source.
    pub(crate) memory_platform: Option<std::sync::Arc<dyn MemoryPressure>>,
    /// Number of idle/minimize trims performed (test observability).
    pub(crate) memory_trim_count: u32,
    /// Frames painted since launch, reported in `TelemetryCounters::frames_rendered`.
    pub(crate) frames_rendered: u64,
    applying_dictation_sync: bool,
    pub(crate) reaction_retry_task: Option<Task<()>>,
    pub(crate) reaction_retry_deadline_ms: Option<u64>,
    sync_cmd_tx: Option<async_channel::Sender<SyncCommand>>,
    _sync_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
    /// Milestone 7: OS queue for links and `postboxctl` requests, when OS integration is on.
    pub(crate) os_runtime: Option<crate::desktop_m7::OsRuntime>,
    /// Milestone 7: Toast sink (`NativePlatform` on Windows, a mock in tests).
    pub(crate) notifier: Option<std::sync::Arc<dyn postbox_platform::Notifier>>,
    /// Milestone 7: frames-per-second over a one-second window, reported by `postboxctl status`.
    pub(crate) fps_meter: crate::desktop_m7::SharedFpsMeter,
    /// Milestone 7: timer that drains the OS queue into UI work.
    pub(crate) os_pump_task: Option<Task<()>>,
    /// Milestone 7: directory the running executable lives in, where updates are staged.
    pub(crate) install_dir: std::path::PathBuf,
    /// Milestone 7: manifest/archive source, injected by `postbox-app`.
    pub(crate) release_source: Option<std::sync::Arc<dyn postbox_platform::ReleaseSource>>,
    /// Milestone 7: the verified, staged update waiting for the restart click.
    pub(crate) staged_update: Option<postbox_platform::StagedUpdate>,
    /// Milestone 7: deferred first check plus the repeating background update loop.
    pub(crate) update_scheduler_task: Option<Task<()>>,
    /// Milestone 7: how the restart click launches the installed build.
    pub(crate) relauncher: std::sync::Arc<dyn postbox_platform::Relauncher>,
}

impl Drop for PostboxShell {
    fn drop(&mut self) {
        if let Some(tx) = &self.sync_cmd_tx {
            let _ = tx.try_send(SyncCommand::Shutdown);
        }
    }
}

impl PostboxShell {
    pub fn new(ctx: PostboxLaunchContext, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let started_at = ctx.started_at;
        let mut vm = PostboxViewModel::new_uninitialized(ctx.store, ctx.keymap, ctx.version)
            .with_hub_status(ctx.hub_status)
            .with_data_dir(ctx.data_dir)
            .with_giphy_key_available(ctx.giphy_key_available)
            .with_deepgram_key_available(ctx.deepgram_key_available)
            .with_llm_format_key_available(ctx.llm_format_key_available);
        if let Some(bridge) = &ctx.sync_bridge {
            vm.attach_sync_sender(bridge.cmd_tx.clone());
        }
        vm.initialize();
        Self::from_view_model_with_bridge(vm, ctx.sync_bridge, started_at, window, cx)
            .with_memory_platform(ctx.memory_platform)
            .with_video_support(ctx.video_backend, ctx.audio_backend, ctx.video_thumb_cache)
            .with_release_source(ctx.release_source)
    }

    /// Installs the Milestone 6 video backend (Media Foundation + Intel Arc DXVA), the WASAPI
    /// render backend that clocks playback, and the disk thumbnail cache.
    pub fn with_video_support(
        mut self,
        video_backend: Option<std::sync::Arc<dyn postbox_platform::VideoBackend>>,
        audio_backend: Option<std::sync::Arc<dyn postbox_platform::AudioBackend>>,
        video_thumb_cache: Option<postbox_media::VideoThumbnailCache>,
    ) -> Self {
        self.video_backend = video_backend;
        self.audio_backend = audio_backend;
        self.video_thumb_cache = video_thumb_cache;
        self
    }

    /// Installs the OS memory-pressure source (working-set trim + `GetProcessMemoryInfo`).
    pub fn with_memory_platform(
        mut self,
        platform: Option<std::sync::Arc<dyn MemoryPressure>>,
    ) -> Self {
        self.memory_platform = platform;
        self
    }

    pub fn from_view_model(
        vm: PostboxViewModel,
        started_at: Instant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::from_view_model_with_bridge(vm, None, started_at, window, cx)
    }

    pub fn from_view_model_with_bridge(
        vm: PostboxViewModel,
        sync_bridge: Option<SyncBridge>,
        started_at: Instant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial_theme = vm.theme();
        let initial_scale = vm.font_scale();
        let initial_title = vm.window_title();
        let initial_timeline_len = vm.timeline_items().len();
        let resident_chat_id = vm.selected_chat().map(|c| c.key.id.clone());
        let memory_platform: Option<std::sync::Arc<dyn MemoryPressure>> = None;
        let sidebar_rows_cache = vm.sidebar_rows();
        let row_h = vm.row_geometry().height;
        let sidebar_item_sizes = Rc::new(vec![
            size(px(SIDEBAR_WIDTH_PX), px(row_h));
            sidebar_rows_cache.len()
        ]);

        let root_focus = cx.focus_handle();
        let sidebar_focus = cx.focus_handle();
        let message_list_focus = cx.focus_handle();
        let thread_broadcast_focus = cx.focus_handle();
        let lightbox_focus = cx.focus_handle();
        let emoji_picker_focus = cx.focus_handle();
        let giphy_picker_focus = cx.focus_handle();
        let voice_record_focus = cx.focus_handle();
        let video_player_focus = cx.focus_handle();
        sidebar_focus.focus(window, cx);

        let timeline_state = cx.new(|cx| MessageScrollerState::new(initial_timeline_len, cx));
        let thread_timeline_state = cx.new(|cx| MessageScrollerState::new(0, cx));
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 6)
                .submit_on_enter(true)
                .placeholder("Write a message... (Ctrl+K for commands)")
        });
        let thread_composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 5)
                .submit_on_enter(true)
                .placeholder("Reply in thread...")
        });
        let emoji_search_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("React: search emoji (thumbs, heart, laugh...)")
        });
        let giphy_search_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search GIPHY"));
        let palette_state = cx.new(|cx| CommandState::new(window, cx));

        let emoji_search_sub = cx.subscribe_in(
            &emoji_search_input,
            window,
            |this: &mut Self, state: &Entity<InputState>, event: &InputEvent, window, cx| {
                match event {
                    InputEvent::Change => {
                        let q = state.read(cx).value().to_string();
                        this.vm.set_emoji_picker_query(q);
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => {
                        this.vm.activate_emoji_picker();
                        this.sync_after_vm_change(window, cx);
                    }
                    _ => {}
                }
            },
        );

        let giphy_search_sub = cx.subscribe_in(
            &giphy_search_input,
            window,
            |this: &mut Self, state: &Entity<InputState>, event: &InputEvent, window, cx| {
                match event {
                    InputEvent::Change => {
                        let q = state.read(cx).value().to_string();
                        this.vm.set_giphy_query(q);
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => {
                        this.vm.activate_giphy_enter();
                        this.sync_after_vm_change(window, cx);
                    }
                    _ => {}
                }
            },
        );

        let composer_sub = cx.subscribe_in(
            &composer,
            window,
            |this: &mut Self, state: &Entity<TextareaState>, event: &InputEvent, window, cx| {
                match event {
                    InputEvent::Change => {
                        if this.applying_dictation_sync {
                            return;
                        }
                        let (value, sel) = {
                            let st = state.read(cx);
                            (st.value().to_string(), st.selected_range())
                        };
                        this.vm.set_composer_selection(sel);
                        this.vm.set_composer_text(value);
                        // Sync back immediately if closed `:shortcode:` auto-replaced!
                        let updated = this.vm.composer_text().to_owned();
                        if state.read(cx).value().as_ref() != updated.as_str() {
                            this.applying_dictation_sync = true;
                            state.update(cx, |input, cx| input.set_value(updated, window, cx));
                            this.applying_dictation_sync = false;
                        }
                        this.sync_sidebar_cache();
                        cx.notify();
                    }
                    InputEvent::PressEnter { shift, .. } if !shift => {
                        if this.vm.has_composer_popup() {
                            this.vm.accept_composer_popup();
                            this.sync_after_vm_change(window, cx);
                        } else if this.vm.submit_composer_message() {
                            state.update(cx, |input, cx| input.clean(window, cx));
                            this.sync_after_vm_change(window, cx);
                        }
                    }
                    InputEvent::Focus => {
                        this.vm.set_composer_focused(true);
                        cx.notify();
                    }
                    InputEvent::Blur => {
                        if !this.vm.voice_record_state().is_active()
                            && !this.vm.is_dictation_active()
                        {
                            this.vm.set_composer_focused(false);
                        }
                        cx.notify();
                    }
                    _ => {}
                }
            },
        );

        let thread_composer_sub = cx.subscribe_in(
            &thread_composer,
            window,
            |this: &mut Self, state: &Entity<TextareaState>, event: &InputEvent, window, cx| {
                match event {
                    InputEvent::Change => {
                        if this.applying_dictation_sync {
                            return;
                        }
                        let (value, sel) = {
                            let st = state.read(cx);
                            (st.value().to_string(), st.selected_range())
                        };
                        this.vm.set_thread_selection(sel);
                        this.vm.set_thread_composer_text(value);
                        cx.notify();
                    }
                    InputEvent::PressEnter { shift, .. }
                        if !shift && this.vm.submit_thread_reply() =>
                    {
                        state.update(cx, |input, cx| input.clean(window, cx));
                        this.sync_after_vm_change(window, cx);
                    }
                    InputEvent::Focus => {
                        this.vm.set_focus(FocusTarget::ThreadComposer);
                        cx.notify();
                    }
                    _ => {}
                }
            },
        );

        let activation_sub = cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                this.gif_timer_task = None;
                this.dictation_timer_task = None;
                // Milestone 6: minimize/occlude pauses video, stops the render stream and
                // drops the presentation timer (0.0% idle CPU/GPU when nothing plays).
                this.pause_video_when_occluded(cx);
                // M5.5 RAM diet: losing focus counts as idle, so drop transient textures,
                // release SQLite pages, and hand reclaimable pages back to Windows.
                this.trim_memory_for_idle(window, cx);
                if this.vm.on_window_blur() {
                    this.sync_sidebar_cache();
                }
                cx.notify();
            } else {
                this.sync_gif_animation_timer(window, cx);
                this.sync_dictation_meter_timer(window, cx);
            }
        });

        let sync_cmd_tx = sync_bridge.as_ref().map(|b| b.cmd_tx.clone());
        let sync_task = sync_bridge.map(|bridge| {
            let rx = bridge.event_rx;
            cx.spawn_in(
                window,
                async move |this: WeakEntity<PostboxShell>, cx: &mut AsyncWindowContext| {
                    // Parked with 0% idle CPU/GPU until background Tokio sync engine sends an event.
                    while let Ok(first) = rx.recv().await {
                        let mut batch = vec![first];
                        while let Ok(next) = rx.try_recv() {
                            batch.push(next);
                        }
                        let alive = this
                            .update_in(cx, |shell, window, cx| {
                                shell.apply_sync_events(batch, window, cx);
                            })
                            .is_ok();
                        if !alive {
                            break;
                        }
                    }
                },
            )
        });

        window.set_window_title(&initial_title);
        let init_elapsed_ms = started_at.elapsed().as_millis();

        let mut shell = Self {
            vm,
            started_at,
            init_elapsed_ms,
            root_focus,
            sidebar_focus,
            message_list_focus,
            thread_broadcast_focus,
            lightbox_focus,
            emoji_picker_focus,
            giphy_picker_focus,
            voice_record_focus,
            sidebar_scroll: VirtualListScrollHandle::new(),
            sidebar_item_sizes,
            sidebar_rows_cache,
            timeline_state,
            thread_timeline_state,
            composer,
            thread_composer,
            emoji_search_input,
            giphy_search_input,
            palette_state,
            last_window_title: initial_title,
            last_applied_theme: initial_theme,
            last_applied_scale: initial_scale,
            gpu_textures: GpuTextureLru::default(),
            gif_tick: 0,
            gif_timer_task: None,
            dictation_timer_task: None,
            video_timer_task: None,
            video_player_focus,
            video_backend: None,
            audio_backend: None,
            video_thumb_cache: None,
            video_runtime: None,
            resident_chat_id,
            memory_platform,
            memory_trim_count: 0,
            frames_rendered: 0,
            applying_dictation_sync: false,
            reaction_retry_task: None,
            reaction_retry_deadline_ms: None,
            sync_cmd_tx,
            _sync_task: sync_task,
            _subscriptions: vec![
                composer_sub,
                thread_composer_sub,
                emoji_search_sub,
                giphy_search_sub,
                activation_sub,
            ],
            os_runtime: None,
            notifier: None,
            fps_meter: crate::desktop_m7::SharedFpsMeter::default(),
            os_pump_task: None,
            install_dir: std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
                .unwrap_or_else(std::env::temp_dir),
            release_source: None,
            staged_update: None,
            update_scheduler_task: None,
            relauncher: std::sync::Arc::new(postbox_platform::ProcessRelauncher),
        };
        shell.refresh_visible_gpu_textures(window, cx);
        shell.sync_reaction_retry_timer(window, cx);
        shell
    }

    /// Arms a single on-demand one-shot GPUI timer ONLY when a reaction has a scheduled retry
    /// (`3s / 10s / 30s`). Drops the task (`0.0%` idle CPU) when no retries are scheduled.
    pub(crate) fn sync_reaction_retry_timer(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(deadline_ms) = self.vm.next_reaction_retry_at_ms() else {
            self.reaction_retry_task = None;
            self.reaction_retry_deadline_ms = None;
            return;
        };
        if self.reaction_retry_task.is_some()
            && self.reaction_retry_deadline_ms == Some(deadline_ms)
        {
            return;
        }
        let now_ms = crate::viewmodel::now_epoch_millis_public() as u64;
        let delay_ms = deadline_ms.saturating_sub(now_ms);
        self.reaction_retry_deadline_ms = Some(deadline_ms);
        let task = cx.spawn_in(
            window,
            async move |this: WeakEntity<PostboxShell>, cx: &mut AsyncWindowContext| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(delay_ms))
                    .await;
                let _ = this.update_in(cx, |shell, window, cx| {
                    shell.reaction_retry_task = None;
                    shell.reaction_retry_deadline_ms = None;
                    // `.max(deadline_ms)` prevents 1ms wall-clock jitter AND makes fake-clock
                    // `cx.background_executor().advance_clock(...)` tests deterministic.
                    let fire_ms =
                        (crate::viewmodel::now_epoch_millis_public() as u64).max(deadline_ms);
                    shell.vm.retry_due_reactions(fire_ms);
                    shell.sync_after_vm_change(window, cx);
                });
            },
        );
        self.reaction_retry_task = Some(task);
    }

    pub fn view_model(&self) -> &PostboxViewModel {
        &self.vm
    }

    pub fn init_elapsed_ms(&self) -> u128 {
        self.init_elapsed_ms
    }

    pub(crate) fn now_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    pub fn gpu_vram_bytes(&self) -> usize {
        self.gpu_textures.total_vram_bytes()
    }

    fn apply_sync_events(
        &mut self,
        events: Vec<SyncEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_selections_to_vm(cx);
        let mut total_prepended = 0usize;
        for ev in events {
            total_prepended += self.vm.apply_sync_event(ev);
        }
        if total_prepended > 0 {
            self.timeline_state.update(cx, |state, cx| {
                state.prepend(total_prepended, cx);
            });
        }
        self.sync_after_vm_change(window, cx);
    }

    fn sync_sidebar_cache(&mut self) {
        self.sidebar_rows_cache = self.vm.sidebar_rows();
        let row_h = self.vm.row_geometry().height;
        self.sidebar_item_sizes = Rc::new(vec![
            size(px(SIDEBAR_WIDTH_PX), px(row_h));
            self.sidebar_rows_cache.len()
        ]);
    }

    fn sync_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.vm.voice_record_state().is_active()
            && !self.vm.palette_open()
            && !self.vm.shortcuts_open()
            && self.vm.lightbox().is_none()
            && self.vm.emoji_picker().is_none()
            && !self.vm.giphy_picker().is_open()
        {
            self.voice_record_focus.focus(window, cx);
            return;
        }
        match self.vm.focus() {
            FocusTarget::ChatList => self.sidebar_focus.focus(window, cx),
            FocusTarget::MessageList => self.message_list_focus.focus(window, cx),
            FocusTarget::Composer => self
                .composer
                .update(cx, |input, cx| input.focus(window, cx)),
            FocusTarget::ThreadComposer => self
                .thread_composer
                .update(cx, |input, cx| input.focus(window, cx)),
            FocusTarget::ThreadBroadcast => self.thread_broadcast_focus.focus(window, cx),
            FocusTarget::Palette => self
                .palette_state
                .update(cx, |state, cx| state.focus(window, cx)),
            FocusTarget::ShortcutsOverlay => self.root_focus.focus(window, cx),
            FocusTarget::Lightbox => self.lightbox_focus.focus(window, cx),
            FocusTarget::EmojiPicker => self
                .emoji_search_input
                .update(cx, |input, cx| input.focus(window, cx)),
            FocusTarget::GiphyPicker => self
                .giphy_search_input
                .update(cx, |input, cx| input.focus(window, cx)),
            FocusTarget::VideoPlayer => self.video_player_focus.focus(window, cx),
        }
    }

    /// M5.5 RAM diet: drops the previous chat's inline media and animated GIF textures when the
    /// selected chat changes, so only the active chat's media stays resident.
    pub(crate) fn evict_previous_chat_media(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        let current = self.vm.selected_chat().map(|c| c.key.id.clone());
        let previous = self.resident_chat_id.clone();
        self.resident_chat_id = current.clone();
        let Some(previous_id) = previous.filter(|prev| current.as_deref() != Some(prev.as_str()))
        else {
            return 0;
        };
        // Milestone 6: leaving a chat stops the player and releases its frame + render stream.
        let active_chat = self.vm.selected_chat().map(|c| c.key.clone());
        self.vm.cleanup_video_player_for_chat(active_chat.as_ref());
        self.release_video_runtime(window, cx);
        self.gpu_textures.evict_chat_media(&previous_id, window, cx)
    }

    /// M5.5 RAM diet idle/minimize trim: purges transient GPU textures, asks SQLite to release
    /// its page cache, and asks Windows to move reclaimable pages out of the working set.
    /// Returns the number of textures released.
    pub fn trim_memory_for_idle(&mut self, window: &mut Window, cx: &mut Context<Self>) -> usize {
        self.gif_timer_task = None;
        self.memory_trim_count = self.memory_trim_count.saturating_add(1);
        let dropped = self.gpu_textures.purge_transient_textures(window, cx);
        let _ = self.vm.shrink_store_memory();
        if let Some(platform) = &self.memory_platform {
            let _ = platform.trim_process_working_set();
        }
        dropped
    }

    /// Working-set and private-commit telemetry, filled from `GetProcessMemoryInfo` on Windows.
    pub fn process_memory_stats(&self) -> postbox_platform::ProcessMemoryStats {
        self.memory_platform
            .as_ref()
            .and_then(|p| p.read_process_memory_stats().ok())
            .unwrap_or_default()
    }

    /// `postboxctl status` telemetry: frames rendered, working set, private commit, GPU texture
    /// bytes, and queued outbox rows for the active chat.
    pub fn telemetry_counters(&self) -> postbox_core::control::TelemetryCounters {
        let mem = self.process_memory_stats();
        postbox_core::control::TelemetryCounters {
            frames_rendered: self.frames_rendered,
            working_set_kb: mem.working_set_kb(),
            private_usage_kb: mem.private_usage_kb(),
            gpu_texture_bytes: self.gpu_textures.total_vram_bytes() as u64,
            pending_outbox: self.vm.outbox_for_chat().len() as u32,
        }
    }

    pub(crate) fn sync_after_vm_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let theme = self.vm.theme();
        let scale = self.vm.font_scale();
        let scale_changed = scale != self.last_applied_scale;
        if theme != self.last_applied_theme || scale_changed {
            apply_postbox_theme(cx, theme, scale);
            self.last_applied_theme = theme;
            self.last_applied_scale = scale;
            if scale_changed {
                self.timeline_state.update(cx, |s, cx| s.remeasure(cx));
                self.thread_timeline_state
                    .update(cx, |s, cx| s.remeasure(cx));
            }
        }

        self.sync_sidebar_cache();
        if let Some(sel) = self.vm.selected_chat_idx() {
            self.sidebar_scroll
                .scroll_to_item(sel, ScrollStrategy::Nearest);
        }

        let new_timeline_len = self.vm.timeline_items().len();
        let scroll_latest = self.vm.take_scroll_to_latest_pending();
        let current_len = self.timeline_state.read(cx).item_count();
        if current_len != new_timeline_len || scroll_latest {
            self.timeline_state.update(cx, |state, cx| {
                state.reset(new_timeline_len, cx);
                if scroll_latest {
                    state.scroll_to_end(cx);
                }
            });
        }

        let new_thread_len = self
            .vm
            .thread_panel()
            .map(|p| p.timeline_items.len())
            .unwrap_or(0);
        if self.thread_timeline_state.read(cx).item_count() != new_thread_len {
            self.thread_timeline_state.update(cx, |state, cx| {
                state.reset(new_thread_len, cx);
                if new_thread_len > 0 {
                    state.scroll_to_end(cx);
                }
            });
        }

        // Sync main & thread composer text and pending UTF-8 selection (e.g., dictation interim/final/Esc restore).
        self.applying_dictation_sync = true;
        let wanted_composer = self.vm.composer_text().to_owned();
        if self.composer.read(cx).value().as_ref() != wanted_composer.as_str() {
            self.composer.update(cx, |input, cx| {
                input.set_value(wanted_composer, window, cx);
            });
        }
        if let Some(sel) = self.vm.take_pending_composer_selection() {
            self.composer.update(cx, |input, cx| {
                input.set_selected_range(sel, cx);
            });
        }

        let wanted_thread_composer = self
            .vm
            .thread_panel()
            .map(|p| p.composer_text.clone())
            .unwrap_or_default();
        if self.thread_composer.read(cx).value().as_ref() != wanted_thread_composer.as_str() {
            self.thread_composer.update(cx, |input, cx| {
                input.set_value(wanted_thread_composer, window, cx);
            });
        }
        if let Some(sel) = self.vm.take_pending_thread_selection() {
            self.thread_composer.update(cx, |input, cx| {
                input.set_selected_range(sel, cx);
            });
        }
        self.applying_dictation_sync = false;

        if let Some(picker) = self.vm.emoji_picker() {
            let wanted = picker.query.as_str();
            if self.emoji_search_input.read(cx).value().as_ref() != wanted {
                let wanted_owned = wanted.to_owned();
                self.emoji_search_input.update(cx, |input, cx| {
                    input.set_value(wanted_owned, window, cx);
                });
            }
        }
        if self.vm.giphy_picker().is_open() {
            let wanted = self.vm.giphy_picker().query.as_str();
            if self.giphy_search_input.read(cx).value().as_ref() != wanted {
                let wanted_owned = wanted.to_owned();
                self.giphy_search_input.update(cx, |input, cx| {
                    input.set_value(wanted_owned, window, cx);
                });
            }
        }

        self.refresh_visible_gpu_textures(window, cx);
        // `Ctrl+K > Play video inline` queues an open request; answer it here.
        if let Some(msg_id) = self.vm.take_pending_video_open() {
            self.open_video_player_for(&msg_id, window, cx);
        }
        self.sync_video_player_timer(window, cx);
        self.sync_dictation_meter_timer(window, cx);
        self.sync_reaction_retry_timer(window, cx);
        self.sync_focus(window, cx);

        let title = self.vm.window_title();
        if title != self.last_window_title {
            window.set_window_title(&title);
            self.last_window_title = title;
        }

        if let Some(text) = self.vm.take_pending_clipboard_text() {
            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text));
        }

        if self.vm.quit_requested() {
            self.gpu_textures.clear_all(window, cx);
            self.record_clean_exit();
            cx.quit();
        }

        // Milestone 7: the `Ctrl+K` update commands, both run off the UI thread.
        if self.vm.take_update_check_request() {
            self.start_update_cycle(cx, true);
        }
        if self.vm.take_update_restart_request() {
            self.request_update_restart(window, cx);
        }

        cx.notify();
    }

    fn open_palette_mode(
        &mut self,
        mode: PaletteMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match mode {
            PaletteMode::Normal => self.vm.open_palette(),
            PaletteMode::SearchChats => self.vm.open_search_palette(),
            PaletteMode::SortViews => self.vm.open_sort_views_palette(),
            PaletteMode::Links => self.vm.open_links_palette(),
        }
        self.palette_state.update(cx, |state, cx| {
            state.set_query("", window, cx);
        });
        self.sync_after_vm_change(window, cx);
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.vm.close_palette();
        self.sync_after_vm_change(window, cx);
    }

    fn sync_selections_to_vm(&mut self, cx: &App) {
        let main_sel = self.composer.read(cx).selected_range();
        let thread_sel = self.thread_composer.read(cx).selected_range();
        self.vm.set_composer_selection(main_sel);
        self.vm.set_thread_selection(thread_sel);
    }

    fn execute_palette_index(
        &mut self,
        index: IndexPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_selections_to_vm(cx);
        self.vm
            .execute_palette_section_row(index.section, index.row);
        if self.vm.palette_open() {
            self.palette_state.update(cx, |state, cx| {
                state.set_query("", window, cx);
            });
        }
        self.sync_after_vm_change(window, cx);
    }

    fn dispatch_vm_action(
        &mut self,
        action: PbAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.vm.reset_chords();
        match action {
            PbAction::OpenPalette => self.open_palette_mode(PaletteMode::Normal, window, cx),
            PbAction::SearchChats => self.open_palette_mode(PaletteMode::SearchChats, window, cx),
            PbAction::OpenLinksAndMedia => self.open_palette_mode(PaletteMode::Links, window, cx),
            PbAction::PageUpMessages => {
                let at_scroll_top = self.timeline_state.read(cx).logical_scroll_top().item_ix == 0;
                if at_scroll_top || self.vm.selected_message_idx() == Some(0) {
                    self.vm.request_older_messages();
                }
                let now = self.now_ms();
                self.vm.handle_action(PbAction::PageUpMessages, now);
                self.sync_after_vm_change(window, cx);
            }
            PbAction::MoveViewUp | PbAction::MoveViewDown => {
                let moved = if matches!(action, PbAction::MoveViewUp) {
                    self.vm.palette_move_view_up()
                } else {
                    self.vm.palette_move_view_down()
                };
                if moved
                    && self.palette_state.read(cx).matched_count() > 0
                    && let Some((section, row)) = self.vm.palette_selected_section_row()
                {
                    let target = IndexPath::new(row).section(section);
                    self.palette_state.update(cx, |state, cx| {
                        state.set_selected_index(Some(target), window, cx);
                    });
                }
                self.sync_after_vm_change(window, cx);
            }
            other => {
                let now = self.now_ms();
                self.vm.handle_action(other, now);
                self.sync_after_vm_change(window, cx);
            }
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let mods = event.keystroke.modifiers;
        let now = self.now_ms();

        // Milestone 6: while the inline player is open, `Space` plays/pauses and `M` mutes,
        // mirroring every desktop video player. `F11`/`Esc` come from the keymap.
        if self.vm.video_player_is_open()
            && self.vm.focus() == FocusTarget::VideoPlayer
            && !mods.control
            && !mods.alt
            && !mods.platform
        {
            match key {
                "space" => {
                    self.toggle_video_play_pause_ui(window, cx);
                    return;
                }
                "m" if !mods.shift => {
                    self.toggle_video_mute_ui(cx);
                    return;
                }
                _ => {}
            }
        }

        // Alt+G, Alt+G jump-to-latest chord
        if mods.alt
            && !mods.control
            && !mods.shift
            && !mods.platform
            && key.eq_ignore_ascii_case("g")
        {
            self.vm.reset_inbox_chord();
            self.vm.handle_alt_g(now, event.is_held);
            self.sync_after_vm_change(window, cx);
            return;
        }

        self.vm.reset_latest_chord();

        let mods_active = mods.control || mods.alt || mods.platform || mods.shift;
        if !mods_active && key.len() == 1 {
            if let Some(ch) = key.chars().next()
                && self
                    .vm
                    .handle_inbox_chord_key(ch, now, event.is_held, false)
            {
                self.sync_after_vm_change(window, cx);
            }
        } else {
            self.vm.reset_inbox_chord();
        }
    }

    fn render_view_rail(&self, p: &ShadcnPalette, cx: &mut Context<Self>) -> AnyElement {
        let active_is_inbox = matches!(
            self.vm.active_view(),
            Some(postbox_core::ViewTab::CombinedInbox)
        );
        let accounts = self.vm.accounts().to_vec();
        let active_view_key = self
            .vm
            .active_view()
            .map(postbox_core::ViewTab::view_key)
            .unwrap_or_default();

        let mut rail = div()
            .id("view-rail")
            .test_support()
            .w(px(VIEW_RAIL_WIDTH_PX))
            .h_full()
            .bg(rgb(p.sidebar))
            .border_r_1()
            .border_color(rgb(p.border))
            .flex()
            .flex_col()
            .items_center()
            .py_3()
            .gap_2()
            .child(
                div()
                    .id("view-tab-inbox")
                    .test_support()
                    .w(px(34.0))
                    .h(px(34.0))
                    .rounded_md()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(TYPE_LABEL_PX))
                    .bg(rgb(if active_is_inbox {
                        p.primary
                    } else {
                        p.secondary
                    }))
                    .text_color(rgb(if active_is_inbox {
                        p.primary_foreground
                    } else {
                        p.foreground
                    }))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.vm.switch_to_combined_inbox();
                        this.sync_after_vm_change(window, cx);
                    }))
                    .child("All"),
            );

        for (idx, account) in accounts.into_iter().enumerate() {
            let key = account.view_key();
            let is_active = key == active_view_key;
            let element_id = SharedString::from(format!("view-tab-{}", idx + 2));

            let decoded_ws_icon = if account.provider == Provider::Slack {
                self.vm
                    .workspace_icon_path_for(&account.account_id)
                    .and_then(|path| {
                        self.gpu_textures
                            .peek(&workspace_icon_tex_key(&account.account_id, path))
                            .and_then(|e| e.frame_at(0))
                    })
            } else {
                None
            };

            if let Some(frame) = decoded_ws_icon {
                rail = rail.child(
                    div()
                        .id(element_id)
                        .test_support()
                        .w(px(34.0))
                        .h(px(34.0))
                        .rounded_md()
                        .overflow_hidden()
                        .border_1()
                        .border_color(rgb(if is_active { p.ring } else { p.border }))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.vm.switch_to_view_key(&key);
                            this.sync_after_vm_change(window, cx);
                        }))
                        .child(
                            div()
                                .id(SharedString::from(format!("view-tab-icon-{}", idx + 2)))
                                .test_support()
                                .w(px(34.0))
                                .h(px(34.0))
                                .child(
                                    img(frame)
                                        .w(px(34.0))
                                        .h(px(34.0))
                                        .object_fit(ObjectFit::Cover)
                                        .rounded_md(),
                                ),
                        ),
                );
            } else {
                let (bg_hex, fg_hex, label) = if account.provider == Provider::Slack {
                    let palette_hex = postbox_media::avatar::SENDER_PALETTE_HEX
                        [postbox_media::avatar::sender_color_index(
                            &account.account_id,
                            &account.label,
                        )];
                    let initial = postbox_media::avatar::avatar_initial(&account.label);
                    if is_active {
                        (p.primary, p.primary_foreground, initial)
                    } else {
                        (palette_hex, 0xFFFFFF, initial)
                    }
                } else {
                    (
                        if is_active { p.primary } else { p.secondary },
                        if is_active {
                            p.primary_foreground
                        } else {
                            p.foreground
                        },
                        provider_badge(account.provider).to_owned(),
                    )
                };
                rail = rail.child(
                    div()
                        .id(element_id)
                        .test_support()
                        .w(px(34.0))
                        .h(px(34.0))
                        .rounded_md()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(TYPE_LABEL_PX))
                        .bg(rgb(bg_hex))
                        .text_color(rgb(fg_hex))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.vm.switch_to_view_key(&key);
                            this.sync_after_vm_change(window, cx);
                        }))
                        .child(label),
                );
            }
        }

        rail.into_any_element()
    }

    fn render_chat_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.sidebar_rows_cache.get(ix) else {
            return div().into_any_element();
        };
        let p = ShadcnPalette::for_choice(self.vm.theme());
        let geom = self.vm.row_geometry();

        let row_bg = if row.active { p.accent } else { p.sidebar };
        let title_color = if row.chat.is_archived() {
            archived_row_color(p.foreground, row_bg)
        } else {
            p.foreground
        };
        let preview_color = if row.draft_preview.is_some() {
            p.draft_color(row_bg)
        } else if row.chat.is_archived() {
            archived_text_color(p.muted_foreground, row_bg)
        } else {
            p.muted_foreground
        };
        let preview_text = row
            .draft_preview
            .clone()
            .unwrap_or_else(|| row.chat.last_msg_preview.clone());

        let mut top_line = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_1()
            .w_full()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .flex_1()
                    .overflow_hidden()
                    .when_some(row.slot_badge, |el, slot| {
                        el.child(
                            div()
                                .id(SharedString::from(format!("ctrl-badge-{slot}")))
                                .test_support()
                                .child(slot_kbd_badge(slot)),
                        )
                    })
                    .child(
                        div()
                            .text_size(px(geom.name_font_px))
                            .text_color(rgb(title_color))
                            .truncate()
                            .child(row.chat.title.clone()),
                    ),
            )
            .child(
                div()
                    .text_size(px(geom.time_font_px))
                    .text_color(rgb(p.muted_foreground))
                    .child(format_time_hm(&row.chat.last_msg_ts)),
            );

        if row.chat.pinned {
            top_line = top_line.child(
                div()
                    .text_size(px(TYPE_LABEL_PX))
                    .text_color(rgb(p.muted_foreground))
                    .child("Pin"),
            );
        }

        let mut bottom_line = div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .w_full()
            .child(
                div()
                    .flex_1()
                    .text_size(px(geom.preview_font_px))
                    .text_color(rgb(preview_color))
                    .truncate()
                    .child(preview_text),
            );

        if let Some(archive_label) = row.archive_status.label() {
            bottom_line = bottom_line.child(
                div()
                    .px_1p5()
                    .py_0p5()
                    .rounded_sm()
                    .bg(rgb(p.muted))
                    .text_size(px(TYPE_LABEL_PX))
                    .text_color(rgb(p.muted_foreground))
                    .child(archive_label),
            );
        }

        if row.chat.unread_count > 0 {
            bottom_line = bottom_line.child(
                div()
                    .px_1p5()
                    .py_0p5()
                    .rounded_full()
                    .bg(rgb(p.primary))
                    .text_size(px(TYPE_LABEL_PX))
                    .text_color(rgb(p.primary_foreground))
                    .child(row.chat.unread_count.to_string()),
            );
        }

        let avatar_px = (geom.height - geom.padding * 2.0).clamp(28.0, 42.0);
        let avatar_el = if let Some(path) = self.vm.avatar_path_for(&row.chat.key)
            && let Some(entry) = self.gpu_textures.peek(&avatar_tex_key(&row.chat.key, path))
            && let Some(frame) = entry.frame_at(0)
        {
            div()
                .id(SharedString::from(format!("chat-avatar-img-{ix}")))
                .test_support()
                .w(px(avatar_px))
                .h(px(avatar_px))
                .flex_shrink_0()
                .rounded_full()
                .overflow_hidden()
                .child(
                    img(frame)
                        .w(px(avatar_px))
                        .h(px(avatar_px))
                        .object_fit(ObjectFit::Cover)
                        .rounded_full(),
                )
                .into_any_element()
        } else {
            let color_hex = postbox_media::avatar::SENDER_PALETTE_HEX
                [postbox_media::avatar::sender_color_index(&row.chat.key.id, &row.chat.title)];
            let initial = postbox_media::avatar::avatar_initial(&row.chat.title);
            div()
                .id(SharedString::from(format!("chat-avatar-fallback-{ix}")))
                .test_support()
                .w(px(avatar_px))
                .h(px(avatar_px))
                .flex_shrink_0()
                .rounded_full()
                .bg(rgb(color_hex))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px((avatar_px * 0.44).clamp(11.0, 16.0)))
                .text_color(rgb(0xFFFFFF))
                .child(initial)
                .into_any_element()
        };

        div()
            .id(SharedString::from(format!("chat-row-{ix}")))
            .test_support()
            .w_full()
            .h(px(geom.height))
            .px_3()
            .py(px(geom.padding))
            .flex()
            .flex_row()
            .items_center()
            .gap_2p5()
            .bg(rgb(row_bg))
            .border_b_1()
            .border_color(rgb(if row.bulk_selected { p.ring } else { p.border }))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| {
                this.vm.select_chat_index(ix);
                this.vm.set_focus(FocusTarget::ChatList);
                this.sync_after_vm_change(window, cx);
            }))
            .child(avatar_el)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .child(top_line)
                    .child(bottom_line),
            )
            .into_any_element()
    }

    fn render_timeline_item(
        &self,
        item: &TimelineItem,
        id_prefix: &str,
        ix: usize,
        shell: Entity<Self>,
    ) -> AnyElement {
        let p = ShadcnPalette::for_choice(self.vm.theme());
        let scale = self.vm.font_scale().percent();
        let body_px = scaled_px(BASE_FONT_PX, scale);
        let time_px = scaled_px(TYPE_TIME_PX, scale);
        let label_px = scaled_px(TYPE_LABEL_PX, scale);

        match item {
            TimelineItem::DateDivider { date_label } => div()
                .w_full()
                .flex()
                .justify_center()
                .py_2()
                .child(
                    div()
                        .px_2p5()
                        .py_0p5()
                        .rounded_full()
                        .bg(rgb(p.muted))
                        .border_1()
                        .border_color(rgb(p.border))
                        .text_size(px(label_px))
                        .text_color(rgb(p.muted_foreground))
                        .child(date_label.clone()),
                )
                .into_any_element(),

            TimelineItem::MessageRow {
                message,
                grouped_with_prev,
                selected,
                marked,
                thread_reply_count,
                outbox_status,
                read_ticks,
                ..
            } => {
                let bubble_bg = if message.from_me {
                    p.bubble_outgoing
                } else {
                    p.bubble_incoming
                };
                let bubble_fg = if message.from_me {
                    p.bubble_outgoing_foreground
                } else {
                    p.foreground
                };
                let bubble_meta = if message.from_me {
                    p.bubble_outgoing_muted
                } else {
                    p.muted_foreground
                };
                let is_failed = matches!(outbox_status, Some(OutboxBadge::Failed { .. }));
                let border_col = if is_failed {
                    p.destructive
                } else if *selected {
                    p.ring
                } else {
                    p.border
                };

                let msg_id_dbl = message.msg_id.clone();
                let shell_dbl = shell.clone();
                let mut bubble = div()
                    .id(SharedString::from(format!("{id_prefix}-bubble-{ix}")))
                    .test_support()
                    .max_w(px(620.0))
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .bg(rgb(bubble_bg))
                    .border_1()
                    .border_color(rgb(border_col))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .on_mouse_down(
                        MouseButton::Left,
                        move |event: &MouseDownEvent, window, cx| {
                            if event.click_count == 2 {
                                shell_dbl.update(cx, |this, cx| {
                                    // Videos open the inline player; other bubbles keep the
                                    // double-click thumbs-up reaction.
                                    if this.vm.is_video_message_id(&msg_id_dbl) {
                                        this.open_video_player_for(&msg_id_dbl, window, cx);
                                    } else if this.vm.double_click_message_bubble(&msg_id_dbl) {
                                        this.sync_after_vm_change(window, cx);
                                    }
                                });
                            } else if event.click_count == 1 {
                                shell_dbl.update(cx, |this, cx| {
                                    if event.modifiers.shift {
                                        if let Some(idx) = this
                                            .vm
                                            .messages
                                            .iter()
                                            .position(|m| m.msg_id == msg_id_dbl)
                                        {
                                            this.vm.range_mark_messages_to(
                                                idx,
                                                event.modifiers.control,
                                            );
                                            this.sync_after_vm_change(window, cx);
                                        }
                                    } else if event.modifiers.control {
                                        this.vm.toggle_mark_message(&msg_id_dbl);
                                        this.sync_after_vm_change(window, cx);
                                    } else if let Some(idx) =
                                        this.vm.messages.iter().position(|m| m.msg_id == msg_id_dbl)
                                    {
                                        this.vm.select_message_index(idx);
                                        this.sync_after_vm_change(window, cx);
                                    }
                                });
                            }
                        },
                    );

                if !grouped_with_prev && !message.from_me {
                    bubble = bubble.child(
                        div()
                            .text_size(px(label_px))
                            .text_color(rgb(bubble_meta))
                            .child(message.sender_name.clone()),
                    );
                }

                if let Some(reply_id) = &message.reply_to_id {
                    let quote_label = if message.reply_broadcast {
                        format!("Broadcast reply to {reply_id}")
                    } else {
                        format!("Replying to {reply_id}")
                    };
                    bubble = bubble.child(
                        div()
                            .px_2()
                            .py_0p5()
                            .rounded_sm()
                            .bg(rgb(p.secondary))
                            .text_size(px(label_px))
                            .text_color(rgb(p.muted_foreground))
                            .child(quote_label),
                    );
                }

                let unfurl_match = self.vm.unfurl_card_for_message(message);
                let body_text = if message.deleted {
                    "Message deleted".to_owned()
                } else {
                    let base = self.vm.render_message_text_with_slack_emoji(message);
                    if let Some((_, ref cls)) = unfurl_match {
                        omit_unfurled_url_from_body(&base, cls)
                    } else {
                        base
                    }
                };

                if let Some(media_el) =
                    self.render_inline_media_element(message, &p, ix, id_prefix, &shell)
                {
                    bubble = bubble.child(media_el);
                }

                let mut meta_row = div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .text_size(px(time_px))
                    .text_color(rgb(bubble_meta))
                    .when(*marked, |el| el.child("Marked"))
                    .child(format_time_hm(&message.ts));

                if *read_ticks != ReadTickDisplay::None {
                    let tick_color = if *read_ticks == ReadTickDisplay::DoubleRead {
                        p.ring
                    } else {
                        bubble_meta
                    };
                    meta_row = meta_row.child(
                        div()
                            .text_size(px(label_px))
                            .text_color(rgb(tick_color))
                            .child(read_ticks.symbol()),
                    );
                }

                if !body_text.is_empty() {
                    bubble = bubble.child(self.render_message_body_element(
                        message, body_text, body_px, bubble_fg, ix, id_prefix,
                    ));
                }
                if let Some((card, _)) = unfurl_match {
                    bubble = bubble.child(self.render_unfurl_card(card, &p, ix, id_prefix, &shell));
                }
                bubble = bubble.child(meta_row);

                if let Some(chips) = self.render_reaction_chips(message, &p, ix, id_prefix, &shell)
                {
                    bubble = bubble.child(chips);
                }

                if let Some(badge) = outbox_status {
                    match badge {
                        OutboxBadge::Sending => {
                            bubble = bubble.child(
                                div()
                                    .text_size(px(label_px))
                                    .text_color(rgb(bubble_meta))
                                    .child("Sending..."),
                            );
                        }
                        OutboxBadge::Failed { reason, ambiguous } => {
                            let label = if *ambiguous {
                                "Delivery unconfirmed — click or press Enter to retry".to_owned()
                            } else {
                                format!("Failed: {reason} — click or press Enter to retry")
                            };
                            let retry_id = message.msg_id.clone();
                            let shell_retry = shell.clone();
                            bubble = bubble.child(
                                div()
                                    .id(SharedString::from(format!("{id_prefix}-retry-{ix}")))
                                    .test_support()
                                    .text_size(px(label_px))
                                    .text_color(rgb(p.destructive))
                                    .cursor_pointer()
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation();
                                    })
                                    .on_click(move |_, window, cx| {
                                        cx.stop_propagation();
                                        shell_retry.update(cx, |this, cx| {
                                            this.vm.retry_failed_message(&retry_id);
                                            this.sync_after_vm_change(window, cx);
                                        });
                                    })
                                    .child(label),
                            );
                        }
                    }
                }

                if *thread_reply_count > 0 {
                    let root_id = message.msg_id.clone();
                    let label = replies_line(*thread_reply_count);
                    let shell_thread = shell.clone();
                    bubble = bubble.child(
                        div()
                            .id(SharedString::from(format!("{id_prefix}-thread-pill-{ix}")))
                            .test_support()
                            .mt_1()
                            .px_2()
                            .py_0p5()
                            .rounded_sm()
                            .bg(rgb(p.secondary))
                            .border_1()
                            .border_color(rgb(p.border))
                            .text_size(px(label_px))
                            .text_color(rgb(p.foreground))
                            .cursor_pointer()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                shell_thread.update(cx, |this, cx| {
                                    this.vm.open_thread(&root_id);
                                    this.sync_after_vm_change(window, cx);
                                });
                            })
                            .child(label),
                    );
                }

                div()
                    .w_full()
                    .max_w(px(CHAT_COLUMN_MAX_WIDTH_PX))
                    .mx_auto()
                    .px_4()
                    .pt(px(if *grouped_with_prev { 2.0 } else { 8.0 }))
                    .pb(px(2.0))
                    .flex()
                    .when(message.from_me, |el| el.justify_end())
                    .when(!message.from_me, |el| el.justify_start())
                    .child(bubble)
                    .into_any_element()
            }
        }
    }

    fn render_timeline_row(&self, ix: usize, shell: Entity<Self>) -> AnyElement {
        let Some(item) = self.vm.timeline_items().get(ix) else {
            return div().into_any_element();
        };
        self.render_timeline_item(item, "msg", ix, shell)
    }

    fn render_thread_timeline_row(&self, ix: usize, shell: Entity<Self>) -> AnyElement {
        let Some(item) = self
            .vm
            .thread_panel()
            .and_then(|p| p.timeline_items.get(ix))
        else {
            return div().into_any_element();
        };
        self.render_timeline_item(item, "thread", ix, shell)
    }

    fn render_hub_status_card(&self, p: &ShadcnPalette, cx: &mut Context<Self>) -> AnyElement {
        let mut card = div()
            .id("hub-status-card")
            .test_support()
            .max_w(px(540.0))
            .w_full()
            .p_5()
            .rounded_lg()
            .bg(rgb(p.card))
            .border_1()
            .border_color(rgb(
                if matches!(self.vm.hub_status(), HubConnectionStatus::Error { .. }) {
                    p.destructive
                } else {
                    p.border
                },
            ))
            .flex()
            .flex_col()
            .gap_3();

        match self.vm.hub_status() {
            HubConnectionStatus::NotConfigured {
                missing_url,
                missing_token,
                configured_url,
                detail,
            } => {
                let subtitle = if *missing_url && *missing_token {
                    "No Hub URL or token detected yet.".to_owned()
                } else if *missing_url {
                    "Missing Hub URL.".to_owned()
                } else if let Some(u) = configured_url {
                    format!("Missing Hub token for {u}.")
                } else {
                    "Missing Hub token.".to_owned()
                };

                card = card
                    .child(
                        div()
                            .text_size(px(18.0))
                            .text_color(rgb(p.foreground))
                            .child("Connect Postbox to your Hub"),
                    )
                    .child(
                        div()
                            .text_size(px(TYPE_CHAT_NAME_PX))
                            .text_color(rgb(p.muted_foreground))
                            .child(subtitle),
                    );
                if let Some(d) = detail {
                    card = card.child(
                        div()
                            .text_size(px(TYPE_TIME_PX))
                            .text_color(rgb(p.destructive))
                            .child(d.clone()),
                    );
                }
                card = card
                    .child(
                        div()
                            .p_3()
                            .rounded_md()
                            .bg(rgb(p.secondary))
                            .border_1()
                            .border_color(rgb(p.border))
                            .flex()
                            .flex_col()
                            .gap_1p5()
                            .font_family(FONT_MONO)
                            .text_size(px(TYPE_TIME_PX))
                            .text_color(rgb(p.foreground))
                            .child("POSTBOX_HUB_URL (or WAZIG_HUB_URL) = https://your-hub.example")
                            .child("POSTBOX_HUB_TOKEN (or WAZIG_HUB_TOKEN) = <your-bearer-token>"),
                    )
                    .child(
                        div()
                            .text_size(px(TYPE_TIME_PX))
                            .text_color(rgb(p.muted_foreground))
                            .child(
                                "On Windows, Postbox automatically reads HKCU\\Environment and reuses existing Zig Postbox DPAPI credentials from HKCU\\Software\\Messages\\HubToken. Restart after setting environment variables, or press Ctrl+K for commands.",
                            ),
                    );
            }
            HubConnectionStatus::Connecting { url } => {
                card = card
                    .child(
                        div()
                            .text_size(px(18.0))
                            .text_color(rgb(p.foreground))
                            .child("Connecting to Hub..."),
                    )
                    .child(
                        div()
                            .text_size(px(TYPE_CHAT_NAME_PX))
                            .text_color(rgb(p.muted_foreground))
                            .child(format!("Fetching chats from {url}")),
                    );
            }
            HubConnectionStatus::Connected { url, .. } => {
                let body = if self.vm.unread_only() {
                    "No unread chats in this view. Press U to show all chats or Esc to reset."
                        .to_owned()
                } else if !self.vm.search_filter().is_empty() {
                    "No matching chats. Press Esc to clear search.".to_owned()
                } else {
                    format!(
                        "Connected to {url}. No chats in this view — press R to refresh, Esc to clear filters, or Ctrl+K for commands."
                    )
                };
                card = card
                    .child(
                        div()
                            .text_size(px(18.0))
                            .text_color(rgb(p.foreground))
                            .child("Connected to Hub"),
                    )
                    .child(
                        div()
                            .text_size(px(TYPE_CHAT_NAME_PX))
                            .text_color(rgb(p.muted_foreground))
                            .child(body),
                    );
            }
            HubConnectionStatus::Reconnecting {
                url,
                attempt,
                retry_in_secs,
                last_error,
            } => {
                card = card
                    .child(
                        div()
                            .text_size(px(18.0))
                            .text_color(rgb(p.foreground))
                            .child("Reconnecting to Hub..."),
                    )
                    .child(
                        div()
                            .text_size(px(TYPE_CHAT_NAME_PX))
                            .text_color(rgb(p.muted_foreground))
                            .child(format!(
                                "{url} — attempt #{attempt}, retrying in {retry_in_secs}s ({last_error})"
                            )),
                    )
                    .child(
                        div()
                            .id("hub-retry-now-btn")
                            .test_support()
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .bg(rgb(p.primary))
                            .text_size(px(TYPE_TIME_PX))
                            .text_color(rgb(p.primary_foreground))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.vm.request_restart_live_sync();
                                this.sync_after_vm_change(window, cx);
                            }))
                            .child("Retry now (S)"),
                    );
            }
            HubConnectionStatus::Error { url, message } => {
                let url_hint = url
                    .as_deref()
                    .map(|u| format!(" ({u})"))
                    .unwrap_or_default();
                card = card
                    .child(
                        div()
                            .text_size(px(18.0))
                            .text_color(rgb(p.destructive))
                            .child(format!("Hub connection error{url_hint}")),
                    )
                    .child(
                        div()
                            .text_size(px(TYPE_CHAT_NAME_PX))
                            .text_color(rgb(p.foreground))
                            .child(message.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(TYPE_TIME_PX))
                            .text_color(rgb(p.muted_foreground))
                            .child(
                                "Verify POSTBOX_HUB_URL / WAZIG_HUB_URL and POSTBOX_HUB_TOKEN / WAZIG_HUB_TOKEN, then press S to retry.",
                            ),
                    );
            }
        }

        div()
            .id("message-timeline-empty")
            .test_support()
            .flex_1()
            .w_full()
            .flex()
            .items_center()
            .justify_center()
            .p_6()
            .child(card)
            .into_any_element()
    }

    fn render_thread_panel(&self, p: &ShadcnPalette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let panel = self.vm.thread_panel()?;
        let chat_title = self
            .vm
            .selected_chat()
            .map(|c| c.title.clone())
            .unwrap_or_else(|| "chat".to_owned());
        let is_slack = panel.chat.provider == Provider::Slack;
        let reply_count = panel.messages.len().saturating_sub(1);
        let broadcast_checked = panel.broadcast;
        let shell_for_thread = cx.entity();

        let header = div()
            .w_full()
            .px_4()
            .py_2p5()
            .border_b_1()
            .border_color(rgb(p.border))
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(TYPE_CHAT_NAME_PX))
                            .text_color(rgb(p.foreground))
                            .child("Thread"),
                    )
                    .child(
                        div()
                            .text_size(px(TYPE_LABEL_PX))
                            .text_color(rgb(p.muted_foreground))
                            .child(replies_line(reply_count)),
                    ),
            )
            .child(
                div()
                    .id("thread-close-btn")
                    .test_support()
                    .px_2()
                    .py_0p5()
                    .rounded_sm()
                    .bg(rgb(p.secondary))
                    .text_size(px(TYPE_LABEL_PX))
                    .text_color(rgb(p.muted_foreground))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.vm.close_thread_panel();
                        this.sync_after_vm_change(window, cx);
                    }))
                    .child("Esc"),
            );

        let body = MessageScroller::new(
            "thread-message-timeline",
            self.thread_timeline_state.clone(),
            move |ix, _window, cx| {
                shell_for_thread
                    .read(cx)
                    .render_thread_timeline_row(ix, shell_for_thread.clone())
            },
        )
        .flex_1()
        .w_full()
        .with_bottom_fade(rgb(p.background));

        let mut footer = div()
            .w_full()
            .p_3()
            .border_t_1()
            .border_color(rgb(p.border))
            .bg(rgb(p.card))
            .flex()
            .flex_col()
            .gap_2();

        if is_slack {
            footer = footer.child(
                div()
                    .id("thread-broadcast-wrap")
                    .test_support()
                    .key_context("PostboxThreadBroadcast")
                    .track_focus(&self.thread_broadcast_focus)
                    .child(
                        Checkbox::new("thread-broadcast-checkbox")
                            .label(format!("Also send to {chat_title}"))
                            .checked(broadcast_checked)
                            .on_change(cx.listener(|this, checked: &bool, window, cx| {
                                this.vm.set_thread_broadcast(*checked);
                                this.sync_after_vm_change(window, cx);
                            })),
                    ),
            );
        }

        if let Some(pill) = self.render_dictation_mic_pill(true, p, cx) {
            footer = footer.child(pill);
        }

        footer = footer.child(
            Textarea::new(&self.thread_composer)
                .aria_label("Thread reply composer")
                .bordered(true),
        );

        let thread_key_ctx = if self
            .vm
            .live_dictation()
            .is_some_and(|d| d.in_thread && d.phase.is_active())
        {
            "PostboxThreadPanel PostboxDictating"
        } else {
            "PostboxThreadPanel"
        };

        Some(
            div()
                .id("thread-panel")
                .test_support()
                .key_context(thread_key_ctx)
                .w(px(420.0))
                .max_w(px(620.0))
                .h_full()
                .bg(rgb(p.background))
                .border_l_1()
                .border_color(rgb(p.border))
                .flex()
                .flex_col()
                .child(header)
                .child(body)
                .child(footer)
                .into_any_element(),
        )
    }

    fn render_palette_overlay(&self, p: &ShadcnPalette, cx: &mut Context<Self>) -> AnyElement {
        let shell_query = cx.entity();
        let shell_select = cx.entity();
        let shell_confirm = cx.entity();

        let mut cmd = Command::new(&self.palette_state)
            .filterable(false)
            .placeholder(self.vm.palette_placeholder())
            .max_h(px(380.0))
            .on_query(move |query, _window, cx| {
                shell_query.update(cx, |this, cx| {
                    this.vm.set_palette_query(query);
                    this.sync_sidebar_cache();
                    cx.notify();
                });
            })
            .on_select(move |index: IndexPath, _window, cx| {
                shell_select.update(cx, |this, _cx| {
                    this.vm.select_palette_section_row(index.section, index.row);
                });
            })
            .on_confirm(move |index: IndexPath, window, cx| {
                shell_confirm.update(cx, |this, cx| {
                    this.execute_palette_index(index, window, cx);
                });
            });

        for section in self.vm.palette_sections() {
            let mut group = CommandGroup::new().label(section.heading);
            for entry in section.items {
                let label = if let Some(shortcut) = &entry.shortcut {
                    format!("{}  [{}]", entry.label, shortcut)
                } else {
                    entry.label
                };
                group = group.item(CommandItem::new().label(label));
            }
            cmd = cmd.group(group);
        }

        div()
            .id("command-palette-backdrop")
            .test_support()
            .key_context("PostboxPalette")
            .absolute()
            .inset_0()
            .bg(hsla(0.0, 0.0, 0.0, 0.55))
            .flex()
            .items_start()
            .justify_center()
            .pt(px(72.0))
            .on_action(cx.listener(|this, _: &Cancel, window, cx| {
                this.close_palette(window, cx);
            }))
            .on_click(cx.listener(|this, _, window, cx| {
                this.close_palette(window, cx);
            }))
            .child(
                div()
                    .id("command-palette-overlay")
                    .test_support()
                    .w(px(580.0))
                    .rounded_lg()
                    .bg(rgb(p.popover))
                    .border_1()
                    .border_color(rgb(p.border))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .child(cmd),
            )
            .into_any_element()
    }

    fn render_shortcuts_overlay(&self, p: &ShadcnPalette, cx: &mut Context<Self>) -> AnyElement {
        let groups = f1_shortcut_groups();
        let left_groups = &groups[..3];
        let right_groups = &groups[3..];

        let render_column = |col_groups: &[postbox_keymap::ShortcutGroup]| {
            let mut col = div().flex_1().flex().flex_col().gap_4();
            for group in col_groups {
                let mut card = div().flex().flex_col().gap_1p5().child(
                    div()
                        .text_size(px(TYPE_CHAT_NAME_PX))
                        .text_color(rgb(p.foreground))
                        .pb_1()
                        .border_b_1()
                        .border_color(rgb(p.border))
                        .child(group.name),
                );
                for item in group.items {
                    card = card.child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .py_0p5()
                            .child(
                                div()
                                    .text_size(px(TYPE_TIME_PX))
                                    .text_color(rgb(p.muted_foreground))
                                    .child(item.desc),
                            )
                            .child(
                                div()
                                    .px_1p5()
                                    .py_0p5()
                                    .rounded_sm()
                                    .bg(rgb(p.muted))
                                    .border_1()
                                    .border_color(rgb(p.border))
                                    .font_family(FONT_MONO)
                                    .text_size(px(TYPE_LABEL_PX))
                                    .text_color(rgb(p.foreground))
                                    .child(item.key),
                            ),
                    );
                }
                col = col.child(card);
            }
            col
        };

        div()
            .id("f1-shortcuts-backdrop")
            .test_support()
            .absolute()
            .inset_0()
            .bg(hsla(0.0, 0.0, 0.0, 0.55))
            .flex()
            .items_center()
            .justify_center()
            .p_6()
            .on_click(cx.listener(|this, _, window, cx| {
                this.vm.close_shortcuts();
                this.sync_after_vm_change(window, cx);
            }))
            .child(
                div()
                    .id("f1-shortcuts-modal")
                    .test_support()
                    .w(px(860.0))
                    .max_h(px(640.0))
                    .rounded_lg()
                    .bg(rgb(p.popover))
                    .border_1()
                    .border_color(rgb(p.border))
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(18.0))
                                    .text_color(rgb(p.foreground))
                                    .child("Keyboard Shortcuts"),
                            )
                            .child(
                                div()
                                    .text_size(px(TYPE_TIME_PX))
                                    .text_color(rgb(p.muted_foreground))
                                    .child("Press Esc or F1 to close"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_6()
                            .child(render_column(left_groups))
                            .child(render_column(right_groups)),
                    ),
            )
            .into_any_element()
    }
}

impl Focusable for PostboxShell {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.root_focus.clone()
    }
}

impl Render for PostboxShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.frames_rendered = self.frames_rendered.saturating_add(1);
        let now_ms = crate::viewmodel::now_epoch_millis_public() as u64;
        self.fps_meter.on_frame(now_ms, self.frames_rendered);
        // Milestone 7: an old status card folds itself away on the next painted frame.
        if self.vm.tick_status_card(now_ms) {
            cx.notify();
        }
        let title = self.vm.window_title();
        if title != self.last_window_title {
            window.set_window_title(&title);
            self.last_window_title = title;
        }

        let p = ShadcnPalette::for_choice(self.vm.theme());
        let shell_for_timeline = cx.entity();
        let bulk_count = self.vm.bulk_selected_count();

        let mut sidebar_pane = div()
            .id("chat-sidebar")
            .test_support()
            .track_focus(&self.sidebar_focus)
            .key_context("PostboxChatList")
            .w(px(SIDEBAR_WIDTH_PX))
            .h_full()
            .bg(rgb(p.sidebar))
            .border_r_1()
            .border_color(rgb(p.border))
            .flex()
            .flex_col();

        // Compact inline bulk-archive banner shown ONLY while Ctrl+A selection is active (zero permanent toolbar).
        if bulk_count > 0 {
            sidebar_pane = sidebar_pane.child(
                div()
                    .id("bulk-selection-banner")
                    .test_support()
                    .px_3()
                    .py_2()
                    .bg(rgb(p.secondary))
                    .border_b_1()
                    .border_color(rgb(p.border))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(TYPE_LABEL_PX))
                            .text_color(rgb(p.foreground))
                            .child(format!("{bulk_count} selected")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .child(
                                div()
                                    .id("bulk-archive-btn")
                                    .test_support()
                                    .px_2()
                                    .py_0p5()
                                    .rounded_sm()
                                    .bg(rgb(p.primary))
                                    .text_size(px(TYPE_LABEL_PX))
                                    .text_color(rgb(p.primary_foreground))
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.vm.archive_selected();
                                        this.sync_after_vm_change(window, cx);
                                    }))
                                    .child("Archive"),
                            )
                            .child(
                                div()
                                    .id("bulk-cancel-btn")
                                    .test_support()
                                    .px_2()
                                    .py_0p5()
                                    .rounded_sm()
                                    .bg(rgb(p.muted))
                                    .text_size(px(TYPE_LABEL_PX))
                                    .text_color(rgb(p.muted_foreground))
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.vm.cancel_bulk_selection();
                                        this.sync_after_vm_change(window, cx);
                                    }))
                                    .child("Cancel"),
                            ),
                    ),
            );
        }

        sidebar_pane = sidebar_pane.child(if self.sidebar_rows_cache.is_empty() {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .text_size(px(TYPE_TIME_PX))
                .text_color(rgb(p.muted_foreground))
                .child(self.vm.hub_status().sidebar_empty_label())
                .into_any_element()
        } else {
            v_virtual_list(
                cx.entity(),
                "chat-sidebar-list",
                self.sidebar_item_sizes.clone(),
                |this, visible_range, _window, cx| {
                    visible_range
                        .map(|ix| this.render_chat_row(ix, cx))
                        .collect()
                },
            )
            .track_scroll(&self.sidebar_scroll)
            .flex_1()
            .w_full()
            .into_any_element()
        });

        let timeline_pane = if self.vm.timeline_items().is_empty() {
            if self.vm.chats().is_empty() || !self.vm.hub_status().is_configured() {
                self.render_hub_status_card(&p, cx)
            } else {
                div()
                    .id("message-timeline-empty")
                    .test_support()
                    .flex_1()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .p_6()
                    .child(
                        div()
                            .text_size(px(TYPE_CHAT_NAME_PX))
                            .text_color(rgb(p.muted_foreground))
                            .child("No messages yet"),
                    )
                    .into_any_element()
            }
        } else {
            MessageScroller::new(
                "message-timeline",
                self.timeline_state.clone(),
                move |ix, _window, cx| {
                    shell_for_timeline
                        .read(cx)
                        .render_timeline_row(ix, shell_for_timeline.clone())
                },
            )
            .flex_1()
            .w_full()
            .with_bottom_fade(rgb(p.background))
            .into_any_element()
        };

        let mut composer_section = div()
            .id("composer-container")
            .test_support()
            .key_context(
                match (
                    self.vm.composer_text().is_empty(),
                    self.vm.has_composer_popup(),
                    self.vm
                        .live_dictation()
                        .is_some_and(|d| !d.in_thread && d.phase.is_active()),
                ) {
                    (true, true, true) => {
                        "PostboxComposerEmpty PostboxShortcodePopup PostboxDictating"
                    }
                    (true, true, false) => "PostboxComposerEmpty PostboxShortcodePopup",
                    (true, false, true) => "PostboxComposerEmpty PostboxDictating",
                    (true, false, false) => "PostboxComposerEmpty",
                    (false, true, true) => "PostboxComposer PostboxShortcodePopup PostboxDictating",
                    (false, true, false) => "PostboxComposer PostboxShortcodePopup",
                    (false, false, true) => "PostboxComposer PostboxDictating",
                    (false, false, false) => "PostboxComposer",
                },
            )
            .w_full()
            .max_w(px(CHAT_COLUMN_MAX_WIDTH_PX))
            .mx_auto()
            .px_4()
            .py_3()
            .flex()
            .flex_col()
            .gap_1p5();

        if let Some(reply_id) = self.vm.reply_to_msg_id() {
            composer_section = composer_section.child(
                div()
                    .px_2p5()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(p.secondary))
                    .border_1()
                    .border_color(rgb(p.border))
                    .text_size(px(TYPE_LABEL_PX))
                    .text_color(rgb(p.muted_foreground))
                    .child(format!("Replying to {reply_id} (Esc to cancel)")),
            );
        }

        if let Some(popup) = self.render_composer_popups(&p, cx) {
            composer_section = composer_section.child(popup);
        }

        if let Some(strip) = self.render_staged_attachments_strip(&p, cx) {
            composer_section = composer_section.child(strip);
        }

        if let Some(rec_strip) = self.render_voice_recording_strip(&p, cx) {
            composer_section = composer_section.child(rec_strip);
        }

        if let Some(mic_pill) = self.render_dictation_mic_pill(false, &p, cx) {
            composer_section = composer_section.child(mic_pill);
        }

        composer_section = composer_section
            .on_action(cx.listener(|this, _: &PasteImage, window, cx| {
                let plat = NativePlatform::default();
                let payload = plat
                    .read_paste_payload()
                    .unwrap_or(postbox_platform::ClipboardPayload::Empty);
                match this.vm.handle_paste_clipboard_payload(payload) {
                    PasteStageResult::PropagateTextPaste => {
                        // Let Textarea handle standard text paste cleanly!
                        cx.propagate();
                    }
                    _ => {
                        this.sync_after_vm_change(window, cx);
                    }
                }
            }))
            .child(
                Textarea::new(&self.composer)
                    .aria_label("Message composer")
                    .bordered(true),
            );

        let mut main_column = div()
            .id("chat-main-column")
            .test_support()
            .track_focus(&self.message_list_focus)
            .key_context("PostboxMessageList")
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(p.background));
        if let Some(voice_bar) = self.render_voice_playback_bar(&p, cx) {
            main_column = main_column.child(voice_bar);
        }
        // Milestone 7: a dismissible, non-modal card sits above the timeline and never blocks
        // the composer, the sidebar or the palette.
        let mut main_column = main_column;
        let update_card = self.render_update_card(&p, cx);
        let status_card = self.render_status_card(&p, cx);
        if update_card.is_some() || status_card.is_some() {
            main_column = main_column.child(
                div()
                    .w_full()
                    .max_w(px(CHAT_COLUMN_MAX_WIDTH_PX))
                    .mx_auto()
                    .px_4()
                    .pt_2()
                    .flex()
                    .flex_col()
                    .children(update_card)
                    .children(status_card),
            );
        }
        let main_column = main_column.child(timeline_pane).child(
            div()
                .w_full()
                .border_t_1()
                .border_color(rgb(p.border))
                .bg(rgb(p.card))
                .child(composer_section),
        );

        // ZERO TOOLBAR: Root container holds only View Rail + Virtualized Sidebar + Timeline/Composer + Optional Thread Panel + Overlays.
        let mut root = div()
            .id("postbox-root")
            .test_support()
            .track_focus(&self.root_focus)
            .key_context("PostboxWorkspace")
            .relative()
            .flex()
            .flex_row()
            .size_full()
            .bg(rgb(p.background))
            .text_color(rgb(p.foreground))
            .font_family(FONT_SANS)
            .on_drop::<ExternalPaths>(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.vm.stage_dropped_files(paths.paths());
                this.sync_after_vm_change(window, cx);
            }))
            .on_modifiers_changed(cx.listener(
                |this, event: &ModifiersChangedEvent, _window, cx| {
                    let ctrl_only = event.modifiers.control
                        && !event.modifiers.alt
                        && !event.modifiers.shift
                        && !event.modifiers.platform;
                    let changed = if ctrl_only {
                        this.vm.on_ctrl_down()
                    } else {
                        this.vm.on_ctrl_up()
                    };
                    if changed {
                        this.sync_sidebar_cache();
                        cx.notify();
                    }
                },
            ))
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match postbox_keymap::voice_key(
                    &event.keystroke.key,
                    event.keystroke.modifiers.control,
                    event.keystroke.modifiers.shift,
                    event.keystroke.modifiers.alt,
                    event.is_held,
                ) {
                    postbox_keymap::VoiceKeyOutcome::Ignore => {}
                    postbox_keymap::VoiceKeyOutcome::ConsumeRepeat => {
                        cx.stop_propagation();
                        return;
                    }
                    postbox_keymap::VoiceKeyOutcome::StartOrFinish => {
                        cx.stop_propagation();
                        if this.vm.toggle_voice_recording(window.is_window_active()) {
                            this.sync_after_vm_change(window, cx);
                        }
                        return;
                    }
                }

                let modal_open = this.vm.palette_open()
                    || this.vm.shortcuts_open()
                    || this.vm.lightbox().is_some()
                    || this.vm.emoji_picker().is_some()
                    || this.vm.giphy_picker().is_open()
                    || this.vm.has_composer_popup();
                if !modal_open {
                    match postbox_keymap::dictation_key(
                        &event.keystroke.key,
                        event.keystroke.modifiers.control,
                        event.keystroke.modifiers.shift,
                        event.keystroke.modifiers.alt,
                        event.is_held,
                        this.vm.is_dictation_active(),
                    ) {
                        postbox_keymap::DictationKeyOutcome::Ignore => {}
                        postbox_keymap::DictationKeyOutcome::ConsumeRepeat => {
                            cx.stop_propagation();
                        }
                        postbox_keymap::DictationKeyOutcome::Toggle => {
                            cx.stop_propagation();
                            this.sync_selections_to_vm(cx);
                            if this.vm.toggle_dictation(window.is_window_active()) {
                                this.sync_after_vm_change(window, cx);
                            }
                        }
                        postbox_keymap::DictationKeyOutcome::FinishAndSend => {
                            cx.stop_propagation();
                            this.sync_selections_to_vm(cx);
                            if this.vm.finish_dictation_and_auto_send() {
                                this.sync_after_vm_change(window, cx);
                            }
                        }
                        postbox_keymap::DictationKeyOutcome::Cancel => {
                            cx.stop_propagation();
                            if this.vm.cancel_dictation() {
                                this.sync_after_vm_change(window, cx);
                            }
                        }
                    }
                }
            }))
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_action(cx.listener(|this, _: &OpenPalette, window, cx| {
                this.dispatch_vm_action(PbAction::OpenPalette, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ShowShortcutsHelp, window, cx| {
                this.dispatch_vm_action(PbAction::ShowShortcutsHelp, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SearchChats, window, cx| {
                this.dispatch_vm_action(PbAction::SearchChats, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ClearSearchOrBack, window, cx| {
                this.dispatch_vm_action(PbAction::ClearSearchOrBack, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleUnread, window, cx| {
                this.dispatch_vm_action(PbAction::ToggleUnread, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleArchived, window, cx| {
                this.dispatch_vm_action(PbAction::ToggleArchived, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ArchiveSelected, window, cx| {
                this.dispatch_vm_action(PbAction::ArchiveSelected, window, cx);
            }))
            .on_action(cx.listener(|this, _: &KeepArchivedSelected, window, cx| {
                this.dispatch_vm_action(PbAction::KeepArchivedSelected, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectAllChats, window, cx| {
                this.dispatch_vm_action(PbAction::SelectAllChats, window, cx);
            }))
            .on_action(cx.listener(|this, _: &Refresh, window, cx| {
                this.dispatch_vm_action(PbAction::Refresh, window, cx);
            }))
            .on_action(cx.listener(|this, _: &RestartLiveSync, window, cx| {
                this.dispatch_vm_action(PbAction::RestartLiveSync, window, cx);
            }))
            .on_action(cx.listener(|this, _: &Quit, window, cx| {
                this.dispatch_vm_action(PbAction::Quit, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NextChat, window, cx| {
                this.dispatch_vm_action(PbAction::NextChat, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PrevChat, window, cx| {
                this.dispatch_vm_action(PbAction::PrevChat, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FirstChat, window, cx| {
                this.dispatch_vm_action(PbAction::FirstChat, window, cx);
            }))
            .on_action(cx.listener(|this, _: &LastChat, window, cx| {
                this.dispatch_vm_action(PbAction::LastChat, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PageUpChats, window, cx| {
                this.dispatch_vm_action(PbAction::PageUpChats, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PageDownChats, window, cx| {
                this.dispatch_vm_action(PbAction::PageDownChats, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenChatSlot1, window, cx| {
                this.dispatch_vm_action(PbAction::OpenChatSlot(1), window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenChatSlot2, window, cx| {
                this.dispatch_vm_action(PbAction::OpenChatSlot(2), window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenChatSlot3, window, cx| {
                this.dispatch_vm_action(PbAction::OpenChatSlot(3), window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenChatSlot4, window, cx| {
                this.dispatch_vm_action(PbAction::OpenChatSlot(4), window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenChatSlot5, window, cx| {
                this.dispatch_vm_action(PbAction::OpenChatSlot(5), window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenChatSlot6, window, cx| {
                this.dispatch_vm_action(PbAction::OpenChatSlot(6), window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenChatSlot7, window, cx| {
                this.dispatch_vm_action(PbAction::OpenChatSlot(7), window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenChatSlot8, window, cx| {
                this.dispatch_vm_action(PbAction::OpenChatSlot(8), window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenChatSlot9, window, cx| {
                this.dispatch_vm_action(PbAction::OpenChatSlot(9), window, cx);
            }))
            .on_action(cx.listener(|this, _: &SwitchCombinedInbox, window, cx| {
                this.dispatch_vm_action(PbAction::SwitchCombinedInbox, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SwitchAccountView2, window, cx| {
                this.dispatch_vm_action(PbAction::SwitchAccountView(2), window, cx);
            }))
            .on_action(cx.listener(|this, _: &SwitchAccountView3, window, cx| {
                this.dispatch_vm_action(PbAction::SwitchAccountView(3), window, cx);
            }))
            .on_action(cx.listener(|this, _: &SwitchAccountView4, window, cx| {
                this.dispatch_vm_action(PbAction::SwitchAccountView(4), window, cx);
            }))
            .on_action(cx.listener(|this, _: &SwitchAccountView5, window, cx| {
                this.dispatch_vm_action(PbAction::SwitchAccountView(5), window, cx);
            }))
            .on_action(cx.listener(|this, _: &SwitchAccountView6, window, cx| {
                this.dispatch_vm_action(PbAction::SwitchAccountView(6), window, cx);
            }))
            .on_action(cx.listener(|this, _: &SwitchAccountView7, window, cx| {
                this.dispatch_vm_action(PbAction::SwitchAccountView(7), window, cx);
            }))
            .on_action(cx.listener(|this, _: &SwitchAccountView8, window, cx| {
                this.dispatch_vm_action(PbAction::SwitchAccountView(8), window, cx);
            }))
            .on_action(cx.listener(|this, _: &SwitchAccountView9, window, cx| {
                this.dispatch_vm_action(PbAction::SwitchAccountView(9), window, cx);
            }))
            .on_action(cx.listener(|this, _: &NextMessage, window, cx| {
                this.dispatch_vm_action(PbAction::NextMessage, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PrevMessage, window, cx| {
                this.dispatch_vm_action(PbAction::PrevMessage, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PageUpMessages, window, cx| {
                this.dispatch_vm_action(PbAction::PageUpMessages, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PageDownMessages, window, cx| {
                this.dispatch_vm_action(PbAction::PageDownMessages, window, cx);
            }))
            .on_action(cx.listener(|this, _: &JumpLatest, window, cx| {
                this.dispatch_vm_action(PbAction::JumpLatest, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ReplyToMessage, window, cx| {
                this.dispatch_vm_action(PbAction::ReplyToMessage, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenThread, window, cx| {
                this.dispatch_vm_action(PbAction::OpenThread, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ResendFailedMessage, window, cx| {
                this.dispatch_vm_action(PbAction::ResendFailedMessage, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseThreadPanel, window, cx| {
                this.dispatch_vm_action(PbAction::CloseThreadPanel, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CycleThreadFocus, window, cx| {
                this.dispatch_vm_action(PbAction::CycleThreadFocus, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleThreadBroadcast, window, cx| {
                this.dispatch_vm_action(PbAction::ToggleThreadBroadcast, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenLinksAndMedia, window, cx| {
                this.dispatch_vm_action(PbAction::OpenLinksAndMedia, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FocusComposer, window, cx| {
                this.dispatch_vm_action(PbAction::FocusComposer, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FontSmaller, window, cx| {
                this.dispatch_vm_action(PbAction::FontSmaller, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FontLarger, window, cx| {
                this.dispatch_vm_action(PbAction::FontLarger, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FontReset, window, cx| {
                this.dispatch_vm_action(PbAction::FontReset, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ReactToMessage, window, cx| {
                this.dispatch_vm_action(PbAction::ReactToMessage, window, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenImageLightbox, window, cx| {
                this.dispatch_vm_action(PbAction::OpenImageLightbox, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseLightbox, window, cx| {
                this.dispatch_vm_action(PbAction::CloseLightbox, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PrevLightboxImage, window, cx| {
                this.dispatch_vm_action(PbAction::PrevLightboxImage, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NextLightboxImage, window, cx| {
                this.dispatch_vm_action(PbAction::NextLightboxImage, window, cx);
            }))
            .on_action(cx.listener(|this, _: &EmojiMoveUp, window, cx| {
                this.dispatch_vm_action(PbAction::EmojiMoveUp, window, cx);
            }))
            .on_action(cx.listener(|this, _: &EmojiMoveDown, window, cx| {
                this.dispatch_vm_action(PbAction::EmojiMoveDown, window, cx);
            }))
            .on_action(cx.listener(|this, _: &EmojiMoveLeft, window, cx| {
                this.dispatch_vm_action(PbAction::EmojiMoveLeft, window, cx);
            }))
            .on_action(cx.listener(|this, _: &EmojiMoveRight, window, cx| {
                this.dispatch_vm_action(PbAction::EmojiMoveRight, window, cx);
            }))
            .on_action(cx.listener(|this, _: &EmojiActivate, window, cx| {
                this.dispatch_vm_action(PbAction::EmojiActivate, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseEmojiPicker, window, cx| {
                this.dispatch_vm_action(PbAction::CloseEmojiPicker, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ShortcodePrev, window, cx| {
                this.dispatch_vm_action(PbAction::ShortcodePrev, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ShortcodeNext, window, cx| {
                this.dispatch_vm_action(PbAction::ShortcodeNext, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ShortcodeAccept, window, cx| {
                this.dispatch_vm_action(PbAction::ShortcodeAccept, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseShortcodePopup, window, cx| {
                this.dispatch_vm_action(PbAction::CloseShortcodePopup, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PlayPauseVoice, window, cx| {
                this.dispatch_vm_action(PbAction::PlayPauseVoice, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleTranscript, window, cx| {
                this.dispatch_vm_action(PbAction::ToggleTranscript, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FinishVoiceRecord, window, cx| {
                this.dispatch_vm_action(PbAction::FinishVoiceRecord, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CancelVoiceRecord, window, cx| {
                this.dispatch_vm_action(PbAction::CancelVoiceRecord, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleDictation, window, cx| {
                this.sync_selections_to_vm(cx);
                this.dispatch_vm_action(PbAction::ToggleDictation, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FinishDictation, window, cx| {
                this.sync_selections_to_vm(cx);
                this.dispatch_vm_action(PbAction::FinishDictation, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CancelDictation, window, cx| {
                this.dispatch_vm_action(PbAction::CancelDictation, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CopyMessageText, window, cx| {
                this.dispatch_vm_action(PbAction::CopyMessageText, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SmartCopy, window, cx| {
                this.dispatch_vm_action(PbAction::SmartCopy, window, cx);
            }))
            .on_action(cx.listener(|this, _: &MoveViewUp, window, cx| {
                this.dispatch_vm_action(PbAction::MoveViewUp, window, cx);
            }))
            .on_action(cx.listener(|this, _: &MoveViewDown, window, cx| {
                this.dispatch_vm_action(PbAction::MoveViewDown, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleVideoFullscreen, _, cx| {
                this.toggle_video_fullscreen_ui(cx);
            }))
            .on_action(cx.listener(|this, _: &CloseVideoPlayer, window, cx| {
                this.handle_video_escape(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleVideoPlayPause, window, cx| {
                this.toggle_video_play_pause_ui(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleVideoMute, _, cx| {
                this.toggle_video_mute_ui(cx);
            }))
            .child(self.render_view_rail(&p, cx))
            .child(sidebar_pane)
            .child(main_column);

        if let Some(thread_panel) = self.render_thread_panel(&p, cx) {
            root = root.child(thread_panel);
        }

        if let Some(player) = self.render_video_player_overlay(&p, &self.video_player_focus, cx) {
            root = root.child(player);
        }

        if let Some(lightbox) = self.render_lightbox_overlay(&p, &self.lightbox_focus, cx) {
            root = root.child(lightbox);
        } else if let Some(emoji_picker) = self.render_emoji_picker_overlay(
            &p,
            &self.emoji_picker_focus,
            &self.emoji_search_input,
            cx,
        ) {
            root = root.child(emoji_picker);
        } else if let Some(giphy_picker) = self.render_giphy_picker_overlay(
            &p,
            &self.giphy_picker_focus,
            &self.giphy_search_input,
            cx,
        ) {
            root = root.child(giphy_picker);
        } else if self.vm.palette_open() {
            root = root.child(self.render_palette_overlay(&p, cx));
        } else if self.vm.shortcuts_open() {
            root = root.child(self.render_shortcuts_overlay(&p, cx));
        }

        root
    }
}

/// Launches the real GPUI + `gpui-kit 0.11.0` desktop window with the provided launch context.
pub fn run_desktop(launch_ctx: PostboxLaunchContext) {
    let settings = launch_ctx.store.load_ui_settings().unwrap_or_default();
    let initial_title = postbox_core::ViewTab::CombinedInbox.window_title(&launch_ctx.version);
    // The updater relaunch passes the geometry it left, so the new build opens maximized on
    // the same monitor instead of snapping back to the default rectangle.
    let initial_bounds = crate::desktop_m7::initial_window_bounds(launch_ctx.restore_window);

    // Milestone 7: start the control server and the link pipe before the window opens, so a
    // `postboxctl status` or a second launch's URL lands as soon as the shell is live.
    let os_runtime = if launch_ctx.os_integration {
        let user = postbox_core::os_integration::current_user();
        Some(crate::desktop_m7::OsRuntime::start(
            &launch_ctx.data_dir,
            &user,
        ))
    } else {
        None
    };
    let notifier: Option<std::sync::Arc<dyn postbox_platform::Notifier>> =
        if launch_ctx.os_integration {
            Some(std::sync::Arc::new(
                postbox_platform::NativePlatform::default(),
            ))
        } else {
            None
        };

    gpui_kit::application()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            register_bundled_fonts(cx);
            bind_postbox_keys(cx, &launch_ctx.keymap);
            apply_postbox_theme(cx, settings.theme, settings.font_scale);

            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some(initial_title.clone().into()),
                    ..Default::default()
                }),
                window_bounds: Some(initial_bounds),
                app_id: Some("postbox".to_owned()),
                ..Default::default()
            };

            let startup_link = launch_ctx.startup_link.clone();
            let os_runtime = os_runtime;
            let notifier = notifier;
            gpui_kit::open_window(options, cx, move |window, cx| {
                window.set_window_title(&initial_title);
                cx.new(|cx| {
                    let mut shell = PostboxShell::new(launch_ctx, window, cx);
                    if let Some(os) = os_runtime {
                        shell = shell.with_os_integration(os, notifier);
                    }

                    // Milestone 7: the launch may already carry a `postbox://` URL.
                    if let Some(link) = startup_link.as_deref() {
                        shell.apply_startup_link(link);
                    }
                    if shell.os_runtime.is_some() {
                        shell.os_pump_task = shell.start_os_pump(window, cx);
                    }
                    // Warms nothing at cold start: the first check waits past the first frame.
                    let _ = shell.start_update_scheduler(window, cx);
                    shell
                })
            })
            .expect("failed to open Postbox window");
        });
}

#[cfg(all(test, feature = "desktop-test"))]
mod desktop_tests {
    use super::*;
    use crate::sync_bridge::create_sync_bridge;
    use gpui_kit::base::Root;
    use gpui_kit::test::{TestEventExt as _, TestWindowExt as _};
    use gpui_kit::{Bounds, Point, WindowBounds};
    use gpui_kit::{TestAppContext, WindowHandle};
    use postbox_core::{Account, Chat, ChatKey, Message, ReadStatus};
    use postbox_store::Store;

    fn seeded_launch_ctx() -> PostboxLaunchContext {
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
        store
            .upsert_account(&Account {
                provider: Provider::Slack,
                account_id: "T1".into(),
                label: "Acme Slack".into(),
                state: "connected".into(),
                sort_order: 2,
                last_event_at: None,
            })
            .unwrap();
        let alice = Chat {
            key: ChatKey::new(Provider::Whatsapp, "main", "alice@s.whatsapp.net"),
            title: "Alice".into(),
            is_group: false,
            archived_local: false,
            archived_remote: false,
            pinned: false,
            unread_count: 1,
            last_msg_ts: "2025-01-15T12:00:00Z".into(),
            last_msg_preview: "Hello from Alice".into(),
            avatar_hash: None,
        };
        let bob = Chat {
            key: ChatKey::new(Provider::Slack, "T1", "slack:T1:C-bob"),
            title: "Bob".into(),
            is_group: false,
            archived_local: false,
            archived_remote: false,
            pinned: false,
            unread_count: 0,
            last_msg_ts: "2025-01-15T11:00:00Z".into(),
            last_msg_preview: "Hello from Bob".into(),
            avatar_hash: None,
        };
        store.upsert_chat(&alice, 100).unwrap();
        store.upsert_chat(&bob, 90).unwrap();
        store
            .upsert_message(&Message {
                chat: alice.key,
                msg_id: "m1".into(),
                sender_id: "alice".into(),
                sender_name: "Alice".into(),
                from_me: false,
                ts: "2025-01-15T12:00:00Z".into(),
                body: "Hello from Alice".into(),
                reply_to_id: None,
                reply_broadcast: false,
                media_kind: None,
                mime_type: None,
                filename: None,
                file_size: None,
                reactions: vec![],
                read_status: ReadStatus::Read,
                deleted: false,
            })
            .unwrap();
        store
            .upsert_message(&Message {
                chat: bob.key.clone(),
                msg_id: "1700.1".into(),
                sender_id: "U1".into(),
                sender_name: "Bob".into(),
                from_me: false,
                ts: "2025-01-15T11:00:00Z".into(),
                body: "Thread root".into(),
                reply_to_id: None,
                reply_broadcast: false,
                media_kind: None,
                mime_type: None,
                filename: None,
                file_size: None,
                reactions: vec![],
                read_status: ReadStatus::None,
                deleted: false,
            })
            .unwrap();
        store
            .upsert_message(&Message {
                chat: bob.key,
                msg_id: "1700.2".into(),
                sender_id: "U2".into(),
                sender_name: "Carol".into(),
                from_me: false,
                ts: "2025-01-15T11:01:00Z".into(),
                body: "First folded reply".into(),
                reply_to_id: Some("1700.1".into()),
                reply_broadcast: false,
                media_kind: None,
                mime_type: None,
                filename: None,
                file_size: None,
                reactions: vec![],
                read_status: ReadStatus::None,
                deleted: false,
            })
            .unwrap();

        PostboxLaunchContext {
            store,
            keymap: Keymap::postbox_defaults(),
            version: "0.1.0".into(),
            started_at: Instant::now(),
            hub_status: HubConnectionStatus::Connected {
                url: "https://hub.example".into(),
                last_event_id: Some(1),
            },
            sync_bridge: None,
            data_dir: std::env::temp_dir().join("postbox_desktop_test"),
            giphy_key_available: true,
            deepgram_key_available: true,
            llm_format_key_available: false,
            memory_platform: None,
            video_backend: None,
            audio_backend: None,
            video_thumb_cache: None,
            startup_link: None,
            last_exit_record: None,
            os_integration: false,
            release_source: None,
            restore_window: None,
        }
    }

    fn open_shell_with_ctx(
        cx: &mut TestAppContext,
        launch_ctx: PostboxLaunchContext,
    ) -> (WindowHandle<Root>, Entity<PostboxShell>) {
        let keymap = launch_ctx.keymap.clone();
        cx.update(|cx| {
            gpui_kit::init(cx);
            register_bundled_fonts(cx);
            bind_postbox_keys(cx, &keymap);
            apply_postbox_theme(cx, ThemeChoice::ZincDark, FontScale::default());
        });

        cx.update(|cx| {
            let bounds = Bounds {
                origin: Point::default(),
                size: size(px(1200.0), px(800.0)),
            };
            let (window, shell) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                cx,
                move |window, cx| cx.new(|cx| PostboxShell::new(launch_ctx, window, cx)),
            )
            .expect("open Postbox test window");
            (window.downcast::<Root>().expect("Base Root"), shell)
        })
    }

    fn open_shell(cx: &mut TestAppContext) -> (WindowHandle<Root>, Entity<PostboxShell>) {
        open_shell_with_ctx(cx, seeded_launch_ctx())
    }

    #[gpui_kit::test]
    fn m5_real_composer_cursor_interims_final_and_meter_lifecycle(cx: &mut TestAppContext) {
        let (handle, shell) = open_shell(cx);
        cx.update_window(handle.into(), |_, window, _| window.activate_window())
            .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            shell.update(cx, |this, cx| {
                this.vm.set_composer_text("links rechts");
                this.vm.set_focus(FocusTarget::Composer);
                this.sync_after_vm_change(window, cx);
                this.composer
                    .update(cx, |input, cx| input.set_selected_range(6..6, cx));
            });
            window.render_frame(cx);
            window.press("ctrl-d", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            let generation = shell.read(cx).vm.live_dictation().unwrap().generation;
            shell.update(cx, |this, cx| {
                this.apply_sync_events(
                    vec![
                        SyncEvent::DictationConnected { generation },
                        SyncEvent::DictationInterim {
                            generation,
                            revision: 1,
                            text: "Grüße".into(),
                        },
                        SyncEvent::DictationLevel {
                            generation,
                            level: 200,
                        },
                    ],
                    window,
                    cx,
                );
            });
            window.render_frame(cx);
            assert_eq!(
                shell.read(cx).composer.read(cx).value().as_ref(),
                "links Grüße rechts"
            );
            let caret = "links Grüße ".len();
            assert_eq!(
                shell.read(cx).composer.read(cx).selected_range(),
                caret..caret
            );
            assert!(!shell.read(cx).vm.live_dictation().unwrap().live_detached);
            assert!(shell.read(cx).dictation_timer_task.is_some());
            assert!(window.try_find("dictation-mic-pill").is_some());
            for i in 0..8 {
                assert!(window.try_find(format!("dictation-wave-bar-{i}")).is_some());
            }
        })
        .unwrap();
        cx.run_until_parked();
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(120));
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(
                shell.read(cx).vm.live_dictation().unwrap().meter_bars[7],
                200
            );
        });
        let mut visual = gpui_kit::VisualTestContext::from_window(handle.into(), cx);
        visual.deactivate_window();
        cx.run_until_parked();
        cx.update(|cx| assert!(shell.read(cx).dictation_timer_task.is_none()));
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(240));
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            assert!(!window.is_window_active());
            assert_eq!(shell.read(cx).vm.live_dictation().unwrap().meter_bars[6], 0);
            window.activate_window();
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            assert!(shell.read(cx).dictation_timer_task.is_some());
            window.render_frame(cx);
            window.press("ctrl-d", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            let generation = shell.read(cx).vm.live_dictation().unwrap().generation;
            shell.update(cx, |this, cx| {
                this.apply_sync_events(
                    vec![SyncEvent::DictationFinal {
                        generation,
                        text: "Grüße!".into(),
                    }],
                    window,
                    cx,
                );
            });
            window.render_frame(cx);
            assert_eq!(
                shell.read(cx).composer.read(cx).value().as_ref(),
                "links Grüße! rechts"
            );
            let caret = "links Grüße! ".len();
            assert_eq!(
                shell.read(cx).composer.read(cx).selected_range(),
                caret..caret
            );
            assert!(shell.read(cx).dictation_timer_task.is_none());
            assert!(window.try_find("dictation-mic-pill").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn m5_cursor_move_during_close_stream_disarms_auto_send(cx: &mut TestAppContext) {
        let (handle, shell) = open_shell(cx);
        cx.update_window(handle.into(), |_, window, _| window.activate_window())
            .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            shell.update(cx, |this, cx| {
                this.vm.set_composer_text("draft");
                this.vm.set_focus(FocusTarget::Composer);
                this.sync_after_vm_change(window, cx);
                this.composer
                    .update(cx, |input, cx| input.set_selected_range(5..5, cx));
            });
            window.render_frame(cx);
            window.press("ctrl-d", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            let generation = shell.read(cx).vm.live_dictation().unwrap().generation;
            shell.update(cx, |this, cx| {
                this.apply_sync_events(
                    vec![SyncEvent::DictationConnected { generation }],
                    window,
                    cx,
                );
            });
            window.render_frame(cx);
            window.press("ctrl-enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            assert!(shell.read(cx).vm.live_dictation().unwrap().auto_send_armed);
            shell.update(cx, |this, cx| {
                this.composer
                    .update(cx, |input, cx| input.set_selected_range(0..0, cx));
                let generation = this.vm.live_dictation().unwrap().generation;
                this.apply_sync_events(
                    vec![SyncEvent::DictationFinal {
                        generation,
                        text: "dictated".into(),
                    }],
                    window,
                    cx,
                );
                assert_eq!(this.vm.composer_text(), "draft dictated");
                assert!(this.vm.store().list_outbox().unwrap().is_empty());
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn renders_shell_with_zero_toolbar_and_window_title(cx: &mut TestAppContext) {
        let (handle, shell) = open_shell(cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("postbox-root").is_some());
            assert!(window.try_find("view-rail").is_some());
            assert!(window.try_find("chat-sidebar").is_some());
            assert!(window.try_find("chat-main-column").is_some());
            assert!(window.try_find("composer-container").is_some());
            assert!(window.try_find("toolbar").is_none());
        })
        .unwrap();
        cx.update(|cx| {
            assert_eq!(
                shell.read(cx).view_model().window_title(),
                "Combined inbox - Postbox v0.1.0"
            );
        });
    }

    #[gpui_kit::test]
    fn hold_ctrl_shows_and_clears_slot_badges(cx: &mut TestAppContext) {
        let (handle, _shell) = open_shell(cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("ctrl-badge-1").is_none());
            window.change_modifiers(Modifiers::control(), cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("ctrl-badge-1").is_some());
            window.change_modifiers(Modifiers::none(), cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("ctrl-badge-1").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn ctrl_k_opens_palette_and_switches_theme(cx: &mut TestAppContext) {
        let (handle, shell) = open_shell(cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("ctrl-k", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("command-palette-overlay").is_some());
            window.input("Theme: Light", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("command-palette-overlay").is_none());
        })
        .unwrap();

        cx.update(|cx| {
            assert_eq!(shell.read(cx).view_model().theme(), ThemeChoice::Light);
        });
    }

    #[gpui_kit::test]
    fn f1_toggles_shortcuts_overlay_and_esc_closes(cx: &mut TestAppContext) {
        let (handle, _shell) = open_shell(cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("f1", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("f1-shortcuts-modal").is_some());
            window.press("escape", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("f1-shortcuts-modal").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn font_scaling_shortcuts_update_scale_and_persist(cx: &mut TestAppContext) {
        let (handle, shell) = open_shell(cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("ctrl-=", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("ctrl-=", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update(|cx| {
            let vm = shell.read(cx).view_model();
            assert_eq!(vm.font_scale().percent(), 120);
            assert_eq!(
                vm.store().load_ui_settings().unwrap().font_scale.percent(),
                120
            );
        });

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("ctrl-0", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(shell.read(cx).view_model().font_scale().percent(), 100);
        });
    }

    #[gpui_kit::test]
    fn chat_list_shortcuts_fire_in_desktop_mode(cx: &mut TestAppContext) {
        let (handle, shell) = open_shell(cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("j", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(
                shell.read(cx).view_model().selected_chat().unwrap().title,
                "Bob"
            );
        });

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("k", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(
                shell.read(cx).view_model().selected_chat().unwrap().title,
                "Alice"
            );
        });

        // Note: clicking/navigating to Alice marked her read (user_viewed = true).
        // Mark Alice unread again to test `u` unread filter cleanly.
        cx.update(|cx| {
            shell.update(cx, |this, cx| {
                let mut alice = this.vm.selected_chat().unwrap().clone();
                alice.unread_count = 1;
                this.vm.store().upsert_chat(&alice, 200).unwrap();
                this.vm.refresh_chats();
                cx.notify();
            });
        });
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("u", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update(|cx| {
            let vm = shell.read(cx).view_model();
            assert!(vm.unread_only());
            assert_eq!(vm.chats().len(), 1);
            assert_eq!(vm.chats()[0].title, "Alice");
        });
    }

    #[gpui_kit::test]
    fn esc_closing_palette_does_not_clear_underlying_marked_message(cx: &mut TestAppContext) {
        let (handle, shell) = open_shell(cx);
        cx.update(|cx| {
            shell.update(cx, |this, cx| {
                this.vm.toggle_mark_message("m1");
                cx.notify();
            });
        });
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("ctrl-k", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("command-palette-overlay").is_some());
            window.press("escape", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("command-palette-overlay").is_none());
        })
        .unwrap();

        cx.update(|cx| {
            let vm = shell.read(cx).view_model();
            assert!(!vm.palette_open());
            assert_eq!(vm.marked_messages_count(), 1);
        });

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("escape", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(shell.read(cx).view_model().marked_messages_count(), 0);
        });
    }

    #[gpui_kit::test]
    fn sort_views_palette_syncs_selection_and_reorders_views(cx: &mut TestAppContext) {
        let (handle, shell) = open_shell(cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("ctrl-k", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.input("Sort views", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("down", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update(|cx| {
            let vm = shell.read(cx).view_model();
            assert_eq!(vm.palette_mode(), PaletteMode::SortViews);
            assert_eq!(vm.palette_selected_index(), 1);
            assert_eq!(vm.accounts()[0].label, "WhatsApp");
            assert_eq!(vm.accounts()[1].label, "Acme Slack");
        });

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("ctrl-up", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
        })
        .unwrap();

        cx.update(|cx| {
            let shell_ref = shell.read(cx);
            let vm = shell_ref.view_model();
            assert_eq!(vm.accounts()[0].label, "Acme Slack");
            assert_eq!(vm.accounts()[1].label, "WhatsApp");
            assert_eq!(vm.palette_selected_index(), 0);
            assert_eq!(
                shell_ref.palette_state.read(cx).selected_index(),
                Some(IndexPath::new(0).section(0))
            );
        });
    }

    #[gpui_kit::test]
    fn composer_empty_ctrl_a_up_down_override_textarea_and_nonempty_restores_input(
        cx: &mut TestAppContext,
    ) {
        let mut ctx = seeded_launch_ctx();
        // Seed a second message in Alice's chat so `up`/`down` selection is directly verifiable.
        ctx.store
            .upsert_message(&Message {
                chat: ChatKey::new(Provider::Whatsapp, "main", "alice@s.whatsapp.net"),
                msg_id: "m2".into(),
                sender_id: "alice".into(),
                sender_name: "Alice".into(),
                from_me: false,
                ts: "2025-01-15T12:01:00Z".into(),
                body: "Second message from Alice".into(),
                reply_to_id: None,
                reply_broadcast: false,
                media_kind: None,
                mime_type: None,
                filename: None,
                file_size: None,
                reactions: vec![],
                read_status: ReadStatus::Read,
                deleted: false,
            })
            .unwrap();
        let (handle, shell) = open_shell_with_ctx(cx, ctx);
        // Focus empty composer (`c`), press `up` / `down` -> selects previous/next message!
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("c", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(
                shell
                    .read(cx)
                    .view_model()
                    .selected_message()
                    .map(|m| m.msg_id.as_str()),
                Some("m2")
            );
        });

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("up", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(
                shell
                    .read(cx)
                    .view_model()
                    .selected_message()
                    .map(|m| m.msg_id.as_str()),
                Some("m1")
            );
        });

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("down", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(
                shell
                    .read(cx)
                    .view_model()
                    .selected_message()
                    .map(|m| m.msg_id.as_str()),
                Some("m2")
            );
        });

        // Press `ctrl-a` while composer is empty -> selects all visible chats!
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("ctrl-a", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(shell.read(cx).view_model().bulk_selected_count(), 2);
        });

        // Clear bulk selection, type into composer so it is non-empty, press `ctrl-a`:
        // should NOT select chats (Textarea keeps Input select-all).
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("escape", cx);
            window.input("draft text", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("ctrl-a", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update(|cx| {
            assert_eq!(shell.read(cx).view_model().bulk_selected_count(), 0);
            assert_eq!(shell.read(cx).view_model().composer_text(), "draft text");
        });
    }

    #[gpui_kit::test]
    fn hub_not_configured_shows_setup_card_and_palette_status(cx: &mut TestAppContext) {
        let ctx = PostboxLaunchContext {
            store: Store::open_in_memory().unwrap(),
            keymap: Keymap::postbox_defaults(),
            version: "0.1.0".into(),
            started_at: Instant::now(),
            hub_status: HubConnectionStatus::NotConfigured {
                missing_url: true,
                missing_token: true,
                configured_url: None,
                detail: None,
            },
            sync_bridge: None,
            data_dir: std::env::temp_dir().join("postbox_desktop_test"),
            giphy_key_available: false,
            deepgram_key_available: false,
            llm_format_key_available: false,
            memory_platform: None,
            video_backend: None,
            audio_backend: None,
            video_thumb_cache: None,
            startup_link: None,
            last_exit_record: None,
            os_integration: false,
            release_source: None,
            restore_window: None,
        };
        let (handle, shell) = open_shell_with_ctx(cx, ctx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("hub-status-card").is_some());
            assert!(window.try_find("toolbar").is_none());
            window.press("ctrl-k", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update(|cx| {
            let entries = shell.read(cx).view_model().palette_entries();
            assert!(
                entries
                    .iter()
                    .any(|e| e.label.contains("Hub: Not configured"))
            );
        });
    }

    #[gpui_kit::test]
    fn slack_thread_panel_opens_toggles_broadcast_and_resets_on_send_and_esc(
        cx: &mut TestAppContext,
    ) {
        let (handle, shell) = open_shell(cx);
        // Select Bob (Slack chat with folded thread) and open thread panel.
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("j", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            shell.update(cx, |this, cx| {
                assert!(this.vm.open_thread("1700.1"));
                this.sync_after_vm_change(window, cx);
            });
            window.render_frame(cx);
            assert!(window.try_find("thread-panel").is_some());
            assert!(window.try_find("thread-broadcast-wrap").is_some());
            // Typing Space inside `thread_composer` MUST insert `' '` and MUST NOT flip `broadcast`!
            window.input("Broadcast", cx);
            window.press("space", cx);
            window.input("update", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update(|cx| {
            let panel = shell.read(cx).view_model().thread_panel().unwrap();
            assert_eq!(panel.composer_text, "Broadcast update");
            assert!(!panel.broadcast);
        });

        // `Tab` moves focus to `thread-broadcast-wrap`, then `Space` toggles `broadcast`!
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("tab", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("space", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update(|cx| {
            let vm = shell.read(cx).view_model();
            assert_eq!(vm.focus(), FocusTarget::ThreadBroadcast);
            assert!(vm.thread_panel().unwrap().broadcast);
        });

        cx.update(|cx| {
            shell.update(cx, |this, cx| {
                assert!(this.vm.submit_thread_reply());
                let panel = this.vm.thread_panel().unwrap();
                assert!(!panel.broadcast); // IGSH-1898 auto-reset!
                this.vm.set_thread_broadcast(true);
                this.vm.handle_escape();
                assert!(this.vm.thread_panel().is_none());
                cx.notify();
            });
        });
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("thread-panel").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn sync_bridge_event_updates_sidebar_and_timeline_with_zero_polling(cx: &mut TestAppContext) {
        let (bridge, worker) = create_sync_bridge();
        let store = Store::open_shared_memory("desktop_sync_bridge_test").unwrap();
        let writer_store = Store::open_shared_memory("desktop_sync_bridge_test").unwrap();
        let ctx = PostboxLaunchContext {
            store,
            keymap: Keymap::postbox_defaults(),
            version: "0.1.0".into(),
            started_at: Instant::now(),
            hub_status: HubConnectionStatus::Connecting {
                url: "http://127.0.0.1:9999".into(),
            },
            sync_bridge: Some(bridge),
            data_dir: std::env::temp_dir().join("postbox_desktop_test"),
            giphy_key_available: false,
            deepgram_key_available: false,
            llm_format_key_available: false,
            memory_platform: None,
            video_backend: None,
            audio_backend: None,
            video_thumb_cache: None,
            startup_link: None,
            last_exit_record: None,
            os_integration: false,
            release_source: None,
            restore_window: None,
        };
        let (handle, shell) = open_shell_with_ctx(cx, ctx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("hub-status-card").is_some());
        })
        .unwrap();

        // Background writer inserts a chat and notifies via SyncBridge.
        let chat = Chat {
            key: ChatKey::new(Provider::Whatsapp, "personal", "live@s.whatsapp.net"),
            title: "Live Sync Chat".into(),
            is_group: false,
            archived_local: false,
            archived_remote: false,
            pinned: false,
            unread_count: 1,
            last_msg_ts: "2026-11-26T12:00:00Z".into(),
            last_msg_preview: "Arrived via SSE".into(),
            avatar_hash: None,
        };
        writer_store.upsert_chat(&chat, 1_000).unwrap();
        worker
            .event_tx
            .try_send(SyncEvent::StatusChanged(HubConnectionStatus::Connected {
                url: "http://127.0.0.1:9999".into(),
                last_event_id: Some(7),
            }))
            .unwrap();
        worker
            .event_tx
            .try_send(SyncEvent::ChatsUpdated { reset: true })
            .unwrap();

        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("chat-row-0").is_some());
        })
        .unwrap();
        cx.update(|cx| {
            let vm = shell.read(cx).view_model();
            assert_eq!(vm.chats().len(), 1);
            assert_eq!(vm.chats()[0].title, "Live Sync Chat");
            assert!(matches!(
                vm.hub_status(),
                HubConnectionStatus::Connected {
                    last_event_id: Some(7),
                    ..
                }
            ));
        });
    }

    #[gpui_kit::test]
    fn m3_overlays_reactions_lightbox_mentions_staged_strip_and_giphy_render_cleanly(
        cx: &mut TestAppContext,
    ) {
        let mut ctx = seeded_launch_ctx();
        let alice_key = ChatKey::new(Provider::Whatsapp, "main", "alice@s.whatsapp.net");
        ctx.store
            .upsert_message(&Message {
                chat: alice_key.clone(),
                msg_id: "img_m2".into(),
                sender_id: "alice".into(),
                sender_name: "Alice".into(),
                from_me: false,
                ts: "2025-01-15T12:02:00Z".into(),
                body: "Photo".into(),
                reply_to_id: None,
                reply_broadcast: false,
                media_kind: Some(postbox_core::MediaKind::Image),
                mime_type: Some("image/png".into()),
                filename: Some("photo.png".into()),
                file_size: Some(2048),
                reactions: vec![],
                read_status: ReadStatus::Read,
                deleted: false,
            })
            .unwrap();
        let (handle, shell) = open_shell_with_ctx(cx, ctx);

        // 1. Ctrl+R opens 8x6 reaction picker overlay and Esc dismisses it
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("ctrl-r", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("emoji-picker-overlay").is_some());
            window.press("escape", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("emoji-picker-overlay").is_none());
        })
        .unwrap();

        // 2. Lightbox overlay renders with counter and closes on Esc
        cx.update_window(handle.into(), |_, window, cx| {
            shell.update(cx, |this, cx| {
                let now = this.now_ms();
                assert!(this.vm.open_lightbox_for_message("img_m2", now));
                this.sync_after_vm_change(window, cx);
            });
            window.render_frame(cx);
            assert!(window.try_find("lightbox-overlay").is_some());
            assert!(window.try_find("lightbox-counter").is_some());
            window.press("escape", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("lightbox-overlay").is_none());
        })
        .unwrap();

        // 3. Staged attachment chip strip + @mention popup + Giphy picker overlay render cleanly
        let png = postbox_media::paste::encode_png(64, 48, &vec![200u8; 64 * 48 * 4]).unwrap();
        cx.update_window(handle.into(), |_, window, cx| {
            shell.update(cx, |this, cx| {
                this.vm.handle_paste_clipboard_payload(
                    postbox_platform::ClipboardPayload::PngBytes(png),
                );
                this.vm.set_composer_text("hi @ali");
                this.sync_after_vm_change(window, cx);
            });
            window.render_frame(cx);
            assert!(window.try_find("staged-attachments-strip").is_some());
            assert!(window.try_find("mention-popup").is_some());
            assert!(shell.read(cx).gpu_vram_bytes() <= postbox_media::VRAM_TEXTURE_BUDGET_BYTES);
        })
        .unwrap();

        cx.update_window(handle.into(), |_, window, cx| {
            shell.update(cx, |this, cx| {
                assert!(this.vm.open_giphy_picker());
                this.sync_after_vm_change(window, cx);
            });
            window.render_frame(cx);
            assert!(window.try_find("giphy-picker-overlay").is_some());
        })
        .unwrap();
    }

    fn unique_desktop_test_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("postbox_ui_{tag}_{}_{nanos}", std::process::id()))
    }

    #[gpui_kit::test]
    fn startup_auto_downloads_initial_chat_media_via_sync_bridge(cx: &mut TestAppContext) {
        let data_dir = unique_desktop_test_dir("startup_dl");
        std::fs::create_dir_all(&data_dir).unwrap();
        let (bridge, worker) = create_sync_bridge();
        let mut ctx = seeded_launch_ctx();
        ctx.data_dir = data_dir.clone();
        ctx.sync_bridge = Some(bridge);
        let alice_key = ChatKey::new(Provider::Whatsapp, "main", "alice@s.whatsapp.net");
        ctx.store
            .upsert_message(&Message {
                chat: alice_key.clone(),
                msg_id: "img_m2".into(),
                sender_id: "alice".into(),
                sender_name: "Alice".into(),
                from_me: false,
                ts: "2025-01-15T12:02:00Z".into(),
                body: "Photo".into(),
                reply_to_id: None,
                reply_broadcast: false,
                media_kind: Some(postbox_core::MediaKind::Image),
                mime_type: Some("image/png".into()),
                filename: Some("photo.png".into()),
                file_size: Some(2048),
                reactions: vec![],
                read_status: ReadStatus::Read,
                deleted: false,
            })
            .unwrap();

        let (_handle, _shell) = open_shell_with_ctx(cx, ctx);

        let mut cmds = Vec::new();
        while let Ok(cmd) = worker.cmd_rx.try_recv() {
            cmds.push(cmd);
        }
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                SyncCommand::DownloadChatMedia { chat, items }
                    if chat == &alice_key && items.iter().any(|i| i.msg_id == "img_m2")
            )),
            "expected startup DownloadChatMedia for img_m2, got {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, SyncCommand::FetchAvatar { chat } if chat == &alice_key)),
            "expected startup FetchAvatar for Alice, got {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, SyncCommand::FetchWorkspaceIcon { team } if team == "T1")),
            "expected startup FetchWorkspaceIcon for Slack T1, got {cmds:?}"
        );

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[gpui_kit::test]
    fn off_ui_thread_decode_loads_inline_media_avatar_workspace_icon_slack_emoji_and_unfurl_thumb(
        cx: &mut TestAppContext,
    ) {
        let data_dir = unique_desktop_test_dir("async_decode");
        std::fs::create_dir_all(&data_dir).unwrap();
        let png_bytes =
            postbox_media::paste::encode_png(32, 32, &vec![180u8; 32 * 32 * 4]).unwrap();
        let inline_path = data_dir.join("inline.png");
        let avatar_path = data_dir.join("avatar.png");
        let ws_icon_path = data_dir.join("ws.png");
        let emoji_path = data_dir.join("shipit.png");
        let unfurl_path = data_dir.join("unfurl.png");
        for p in [
            &inline_path,
            &avatar_path,
            &ws_icon_path,
            &emoji_path,
            &unfurl_path,
        ] {
            std::fs::write(p, &png_bytes).unwrap();
        }

        let mut ctx = seeded_launch_ctx();
        ctx.data_dir = data_dir.clone();
        let alice_key = ChatKey::new(Provider::Whatsapp, "main", "alice@s.whatsapp.net");
        let bob_key = ChatKey::new(Provider::Slack, "T1", "slack:T1:C-bob");
        ctx.store
            .upsert_message(&Message {
                chat: alice_key.clone(),
                msg_id: "img_m2".into(),
                sender_id: "alice".into(),
                sender_name: "Alice".into(),
                from_me: false,
                ts: "2025-01-15T12:02:00Z".into(),
                body: "Check https://example.com/guide".into(),
                reply_to_id: None,
                reply_broadcast: false,
                media_kind: Some(postbox_core::MediaKind::Image),
                mime_type: Some("image/png".into()),
                filename: Some("photo.png".into()),
                file_size: Some(2048),
                reactions: vec![],
                read_status: ReadStatus::Read,
                deleted: false,
            })
            .unwrap();
        ctx.store
            .upsert_message(&Message {
                chat: alice_key.clone(),
                msg_id: "link_m3".into(),
                sender_id: "alice".into(),
                sender_name: "Alice".into(),
                from_me: false,
                ts: "2025-01-15T12:03:00Z".into(),
                body: "Read https://example.com/guide".into(),
                reply_to_id: None,
                reply_broadcast: false,
                media_kind: None,
                mime_type: None,
                filename: None,
                file_size: None,
                reactions: vec![],
                read_status: ReadStatus::Read,
                deleted: false,
            })
            .unwrap();
        ctx.store
            .upsert_message(&Message {
                chat: bob_key.clone(),
                msg_id: "1700.3".into(),
                sender_id: "U1".into(),
                sender_name: "Bob".into(),
                from_me: false,
                ts: "2025-01-15T11:02:00Z".into(),
                body: "Übung :shipit: ready!".into(),
                reply_to_id: None,
                reply_broadcast: false,
                media_kind: None,
                mime_type: None,
                filename: None,
                file_size: None,
                reactions: vec![],
                read_status: ReadStatus::None,
                deleted: false,
            })
            .unwrap();

        let (handle, shell) = open_shell_with_ctx(cx, ctx);
        let cls = postbox_media::unfurl::classify_url("https://example.com/guide").unwrap();

        // Deliver MediaDownloaded, AvatarReady, WorkspaceIconReady, UnfurlReady
        cx.update_window(handle.into(), |_, window, cx| {
            shell.update(cx, |this, cx| {
                this.vm.apply_sync_event(SyncEvent::MediaDownloaded {
                    chat: alice_key.clone(),
                    msg_id: "img_m2".into(),
                    path: inline_path.clone(),
                    mime_type: "image/png".into(),
                    sha256: "test-sha-img-m2".into(),
                });
                this.vm.apply_sync_event(SyncEvent::AvatarReady {
                    chat: alice_key.clone(),
                    path: Some(avatar_path.clone()),
                });
                this.vm.apply_sync_event(SyncEvent::WorkspaceIconReady {
                    team: "T1".into(),
                    path: Some(ws_icon_path.clone()),
                });
                this.vm.apply_sync_event(SyncEvent::UnfurlReady {
                    target: cls.clone(),
                    card: Some(postbox_media::unfurl::UnfurlCard {
                        provider: postbox_media::unfurl::UnfurlProvider::Page,
                        id: cls.id.clone(),
                        canonical: cls.canonical.clone(),
                        site: "example.com".into(),
                        title: "Guide".into(),
                        author: "example.com".into(),
                        description: "Docs".into(),
                        play_url: None,
                        thumb_path: Some(unfurl_path.clone()),
                    }),
                });
                this.sync_after_vm_change(window, cx);
                // Decode jobs MUST be queued off-UI-thread (in_flight > 0 immediately before parking!)
                assert!(
                    this.gpu_textures.in_flight_count() > 0,
                    "expected off-UI-thread decode jobs in flight"
                );
            });
        })
        .unwrap();

        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window.try_find("chat-avatar-img-0").is_some(),
                "expected circular chat avatar image rendered"
            );
            assert!(
                window.try_find("view-tab-icon-3").is_some(),
                "expected Slack workspace icon rendered in view rail"
            );
            assert!(
                window.try_find("msg-media-2").is_some(),
                "expected decoded inline message image rendered"
            );
            assert!(
                window.try_find("msg-unfurl-thumb-3").is_some(),
                "expected decoded unfurl card thumbnail rendered"
            );
            assert!(shell.read(cx).gpu_vram_bytes() <= postbox_media::VRAM_TEXTURE_BUDGET_BYTES);
        })
        .unwrap();

        // Switch to Slack chat Bob and deliver Slack custom emoji index + image
        cx.update_window(handle.into(), |_, window, cx| {
            shell.update(cx, |this, cx| {
                assert!(this.vm.select_chat_key(&bob_key));
                let idx = postbox_media::avatar::SlackEmojiIndex {
                    team: "T1".into(),
                    fetched_at: 1000,
                    entries: std::collections::HashMap::from([(
                        "shipit".into(),
                        "https://cdn.example.invalid/shipit.png".into(),
                    )]),
                };
                this.vm.apply_sync_event(SyncEvent::SlackEmojiIndexReady {
                    team: "T1".into(),
                    index: idx,
                });
                this.vm.apply_sync_event(SyncEvent::SlackEmojiMediaReady {
                    team: "T1".into(),
                    name: "shipit".into(),
                    path: emoji_path.clone(),
                });
                this.sync_after_vm_change(window, cx);
            });
        })
        .unwrap();

        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            // Find any rendered Slack custom emoji element across the message rows
            let found_emoji = (0..6).any(|row| {
                window
                    .try_find(SharedString::from(format!("msg-slack-emoji-{row}-0")))
                    .is_some()
            });
            assert!(
                found_emoji,
                "expected inline Slack custom emoji image rendered"
            );
        })
        .unwrap();

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[gpui_kit::test]
    fn emoji_picker_keyboard_navigation_and_stale_query_clear_from_ctrl_r_and_palette(
        cx: &mut TestAppContext,
    ) {
        let (handle, shell) = open_shell_with_ctx(cx, seeded_launch_ctx());

        // 1. Open via Ctrl+R, set query to "fire", close with Escape
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.press("ctrl-r", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            shell.update(cx, |this, cx| {
                this.emoji_search_input
                    .update(cx, |inp, cx| inp.set_value("fire", window, cx));
            });
            window.render_frame(cx);
            window.press("escape", cx);
        })
        .unwrap();
        cx.run_until_parked();

        // 2. Reopen via Ctrl+R -> stale query MUST be cleared!
        cx.update_window(handle.into(), |_, window, cx| {
            window.press("ctrl-r", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                shell.read(cx).emoji_search_input.read(cx).value().as_ref(),
                ""
            );
            // Arrow navigation while emoji_search_input is focused MUST override Input bindings!
            window.press("down", cx);
            window.press("right", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            let sel = shell.read(cx).vm.emoji_picker().unwrap().selected;
            assert_eq!(sel, 9, "down (+8) + right (+1) must select index 9");
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(shell.read(cx).vm.emoji_picker().is_none());
            assert!(
                !shell.read(cx).vm.messages()[0].reactions.is_empty(),
                "Enter must activate selected emoji reaction"
            );
        })
        .unwrap();

        // 3. Open via Ctrl+K palette ("Insert emoji") after dirtying input -> MUST also clear!
        cx.update_window(handle.into(), |_, window, cx| {
            shell.update(cx, |this, cx| {
                this.emoji_search_input
                    .update(cx, |inp, cx| inp.set_value("stale", window, cx));
                assert!(this.vm.open_insert_emoji_picker());
                this.sync_after_vm_change(window, cx);
            });
            window.render_frame(cx);
            assert_eq!(
                shell.read(cx).emoji_search_input.read(cx).value().as_ref(),
                ""
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn giphy_picker_keyboard_navigation_window_scroll_and_escape(cx: &mut TestAppContext) {
        let mut ctx = seeded_launch_ctx();
        ctx.giphy_key_available = true;
        let (handle, shell) = open_shell_with_ctx(cx, ctx);

        cx.update_window(handle.into(), |_, window, cx| {
            shell.update(cx, |this, cx| {
                this.giphy_search_input
                    .update(cx, |inp, cx| inp.set_value("stale", window, cx));
                assert!(this.vm.open_giphy_picker());
                let gen_id = this.vm.giphy_picker().generation;
                let target = this.vm.giphy_picker().target.clone().unwrap();
                let items: Vec<postbox_media::giphy::GiphyItem> = (0..20)
                    .map(|i| postbox_media::giphy::GiphyItem {
                        id: format!("Gif{i}"),
                        title: format!("Title {i}"),
                        preview: postbox_media::giphy::GiphyRendition {
                            url: format!("https://media1.giphy.com/media/Gif{i}/200w.gif"),
                            width: 200,
                            height: 150,
                            size: 1000,
                        },
                        original: postbox_media::giphy::GiphyRendition {
                            url: format!("https://media1.giphy.com/media/Gif{i}/giphy.gif"),
                            width: 400,
                            height: 300,
                            size: 2000,
                        },
                    })
                    .collect();
                this.vm.apply_sync_event(SyncEvent::GiphyResults {
                    generation: gen_id,
                    target,
                    page: Some(postbox_media::giphy::GiphyPage {
                        items,
                        total_count: 20,
                        offset: 0,
                    }),
                    error: None,
                });
                this.sync_after_vm_change(window, cx);
            });
            window.render_frame(cx);
            assert_eq!(
                shell.read(cx).giphy_search_input.read(cx).value().as_ref(),
                ""
            );
            // Navigate while giphy_search_input has focus: right (1), down x3 (5, 9, 13 -> row 3!)
            window.press("right", cx);
            window.press("down", cx);
            window.press("down", cx);
            window.press("down", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(shell.read(cx).vm.giphy_picker().selected, 13);
            assert!(
                window.try_find("giphy-tile-13").is_some(),
                "visible_slice window must scroll to render giphy-tile-13"
            );
            window.press("escape", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("giphy-picker-overlay").is_none());
            assert!(!shell.read(cx).vm.giphy_picker().is_open());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn composer_mention_and_shortcode_popup_arrow_tab_escape_override_input(
        cx: &mut TestAppContext,
    ) {
        let (handle, shell) = open_shell_with_ctx(cx, seeded_launch_ctx());

        // 1. Mention popup: Tab accepts mention instead of inserting a literal tab character
        cx.update_window(handle.into(), |_, window, cx| {
            shell.update(cx, |this, cx| {
                this.vm.set_focus(FocusTarget::Composer);
                this.vm.set_composer_text("hi @ali");
                this.sync_after_vm_change(window, cx);
            });
            window.render_frame(cx);
            assert!(window.try_find("mention-popup").is_some());
            window.press("down", cx);
            window.press("tab", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("mention-popup").is_none());
            let txt = shell.read(cx).vm.composer_text().to_owned();
            assert!(!txt.contains('\t'), "Tab must accept popup, not insert \\t");
            assert!(
                txt.contains("@alice"),
                "expected accepted mention, got {txt:?}"
            );
        })
        .unwrap();

        // 2. Shortcode popup: Escape closes popup cleanly without blurring composer
        cx.update_window(handle.into(), |_, window, cx| {
            shell.update(cx, |this, cx| {
                this.vm.set_composer_text("wow :thu");
                this.sync_after_vm_change(window, cx);
            });
            window.render_frame(cx);
            assert!(window.try_find("shortcode-popup").is_some());
            window.press("escape", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("shortcode-popup").is_none());
            assert_eq!(shell.read(cx).vm.focus(), FocusTarget::Composer);
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn wa_received_bubble_double_click_toggles_thumbs_up_and_chip_click_stops_propagation(
        cx: &mut TestAppContext,
    ) {
        let (bridge, worker) = create_sync_bridge();
        let mut ctx = seeded_launch_ctx();
        ctx.sync_bridge = Some(bridge);
        let (handle, shell) = open_shell_with_ctx(cx, ctx);
        // Drain startup commands
        while worker.cmd_rx.try_recv().is_ok() {}

        // Timeline row 0 is DateSeparator, row 1 is Alice's received WhatsApp message (`msg-bubble-1`)
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("msg-bubble-1").is_some());
            window.double_click("msg-bubble-1", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let msg = &shell.read(cx).vm.messages()[0];
            assert_eq!(msg.reactions.len(), 1);
            assert_eq!(msg.reactions[0].emoji, "👍");
            assert!(window.try_find("msg-react-1-0").is_some());
        })
        .unwrap();

        let mut saw_add = false;
        while let Ok(cmd) = worker.cmd_rx.try_recv() {
            if matches!(cmd, SyncCommand::ReactToMessage { ref emoji, .. } if emoji == "👍") {
                saw_add = true;
            }
        }
        assert!(saw_add, "double-click must emit ReactToMessage 👍");

        // Double-click again -> removes 👍
        cx.update_window(handle.into(), |_, window, cx| {
            window.double_click("msg-bubble-1", cx);
        })
        .unwrap();
        cx.run_until_parked();
        let mut saw_remove = false;
        while let Ok(cmd) = worker.cmd_rx.try_recv() {
            if matches!(cmd, SyncCommand::ReactToMessage { ref emoji, .. } if emoji.is_empty()) {
                saw_remove = true;
            }
        }
        assert!(saw_remove, "second double-click must emit removal");

        // Re-add a non-thumbs-up reaction (🔥) and double-click the chip: stop_propagation must
        // keep bubble 👍 toggle from firing!
        cx.update_window(handle.into(), |_, window, cx| {
            shell.update(cx, |this, cx| {
                let chat = this.vm.selected_chat().unwrap().key.clone();
                this.vm.react_to_message_with_chip(&chat, "m1", "🔥", false);
                this.sync_after_vm_change(window, cx);
            });
            window.render_frame(cx);
        })
        .unwrap();
        while worker.cmd_rx.try_recv().is_ok() {}

        cx.update_window(handle.into(), |_, window, cx| {
            assert!(window.try_find("msg-react-1-0").is_some());
            window.double_click("msg-react-1-0", cx);
        })
        .unwrap();
        cx.run_until_parked();
        let mut saw_thumbs_up_leak = false;
        while let Ok(cmd) = worker.cmd_rx.try_recv() {
            if matches!(cmd, SyncCommand::ReactToMessage { ref emoji, .. } if emoji == "👍") {
                saw_thumbs_up_leak = true;
            }
        }
        assert!(
            !saw_thumbs_up_leak,
            "clicking/double-clicking a reaction chip must stop propagation to bubble 👍 toggle"
        );
    }

    #[gpui_kit::test]
    fn reaction_retry_timer_fires_at_3s_10s_30s_and_parks_at_zero_when_idle(
        cx: &mut TestAppContext,
    ) {
        let (bridge, worker) = create_sync_bridge();
        let mut ctx = seeded_launch_ctx();
        ctx.sync_bridge = Some(bridge);
        let alice_key = ChatKey::new(Provider::Whatsapp, "main", "alice@s.whatsapp.net");
        let (handle, shell) = open_shell_with_ctx(cx, ctx);
        while worker.cmd_rx.try_recv().is_ok() {}

        // Trigger reaction and record initial client_id
        cx.update_window(handle.into(), |_, window, cx| {
            shell.update(cx, |this, cx| {
                assert!(
                    this.vm
                        .react_to_message_with_chip(&alice_key, "m1", "👍", false)
                );
                this.sync_after_vm_change(window, cx);
            });
        })
        .unwrap();
        let initial_client_id = match worker.cmd_rx.try_recv().unwrap() {
            SyncCommand::ReactToMessage { client_id, .. } => client_id,
            other => panic!("expected ReactToMessage, got {other:?}"),
        };

        // Deliver transient 503 failure -> arms one-shot 3s GPUI retry timer
        cx.update_window(handle.into(), |_, window, cx| {
            shell.update(cx, |this, cx| {
                this.vm.apply_sync_event(SyncEvent::ReactionSettled {
                    chat: alice_key.clone(),
                    msg_id: "m1".into(),
                    emoji: "👍".into(),
                    ok: false,
                    http_status: Some(503),
                });
                this.sync_after_vm_change(window, cx);
                assert!(
                    this.reaction_retry_task.is_some(),
                    "transient reaction failure must arm on-demand retry timer"
                );
            });
        })
        .unwrap();

        // Advance GPUI fake clock by 3s -> timer fires and retries with exact same client_id!
        cx.background_executor
            .advance_clock(std::time::Duration::from_secs(3));
        cx.run_until_parked();

        let retry_client_id = match worker.cmd_rx.try_recv().unwrap() {
            SyncCommand::ReactToMessage { client_id, .. } => client_id,
            other => panic!("expected retry ReactToMessage, got {other:?}"),
        };
        assert_eq!(retry_client_id, initial_client_id);

        // Confirm reaction -> timer parks at None (0% idle CPU!)
        cx.update_window(handle.into(), |_, window, cx| {
            shell.update(cx, |this, cx| {
                this.vm.apply_sync_event(SyncEvent::ReactionSettled {
                    chat: alice_key.clone(),
                    msg_id: "m1".into(),
                    emoji: "👍".into(),
                    ok: true,
                    http_status: Some(200),
                });
                this.sync_after_vm_change(window, cx);
                assert!(
                    this.reaction_retry_task.is_none(),
                    "idle reaction queue must park timer at None (0% idle CPU)"
                );
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn drag_and_drop_external_paths_stages_file_in_composer(cx: &mut TestAppContext) {
        let data_dir = unique_desktop_test_dir("dnd");
        std::fs::create_dir_all(&data_dir).unwrap();
        let png_path = data_dir.join("dropped.png");
        let png_bytes =
            postbox_media::paste::encode_png(48, 48, &vec![120u8; 48 * 48 * 4]).unwrap();
        std::fs::write(&png_path, &png_bytes).unwrap();

        let mut ctx = seeded_launch_ctx();
        ctx.data_dir = data_dir.clone();
        let (handle, shell) = open_shell_with_ctx(cx, ctx);

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let pos = Point::new(px(400.0), px(400.0));
            let _ = window.dispatch_event(
                gpui_kit::PlatformInput::FileDrop(gpui_kit::FileDropEvent::Entered {
                    position: pos,
                    paths: ExternalPaths(smallvec::smallvec![png_path.clone()]),
                }),
                cx,
            );
            let _ = window.dispatch_event(
                gpui_kit::PlatformInput::FileDrop(gpui_kit::FileDropEvent::Submit {
                    position: pos,
                }),
                cx,
            );
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(shell.read(cx).vm.staged_attachments().len(), 1);
            assert!(
                window.try_find("staged-attachments-strip").is_some(),
                "drag-and-drop must render staged-attachments-strip"
            );
        })
        .unwrap();

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[gpui_kit::test]
    fn m4_voice_pill_top_bar_ctrl_p_and_ctrl_t_toggle(cx: &mut TestAppContext) {
        let data_dir = unique_desktop_test_dir("m4_voice_pill");
        std::fs::create_dir_all(&data_dir).unwrap();
        let pcm: Vec<i16> = (0..24_000)
            .map(|i| {
                ((2.0 * std::f32::consts::PI * 440.0 * (i as f32) / 48_000.0).sin() * 12_000.0)
                    as i16
            })
            .collect();
        let ogg_bytes = postbox_audio::encode_voice_note(&pcm).unwrap().ogg_bytes;
        let voice_path = data_dir.join("note1.ogg");
        std::fs::write(&voice_path, &ogg_bytes).unwrap();

        let mut ctx = seeded_launch_ctx();
        ctx.data_dir = data_dir.clone();
        let alice_key = ChatKey::new(Provider::Whatsapp, "main", "alice@s.whatsapp.net");
        ctx.store
            .upsert_message(&Message {
                chat: alice_key.clone(),
                msg_id: "voice_m1".into(),
                sender_id: "alice".into(),
                sender_name: "Alice".into(),
                from_me: false,
                ts: "2026-11-27T11:00:00Z".into(),
                body: String::new(),
                reply_to_id: None,
                reply_broadcast: false,
                media_kind: Some(postbox_core::MediaKind::Voice),
                mime_type: Some("audio/ogg".into()),
                filename: Some("note1.ogg".into()),
                file_size: Some(ogg_bytes.len() as u64),
                reactions: vec![],
                read_status: ReadStatus::None,
                deleted: false,
            })
            .unwrap();

        let (handle, shell) = open_shell_with_ctx(cx, ctx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            // 0% idle: top playback bar is NOT rendered before playback starts.
            assert!(window.try_find("voice-playback-bar").is_none());
            assert!(window.try_find("voice-recording-strip").is_none());

            shell.update(cx, |this, cx| {
                this.vm.apply_sync_event(SyncEvent::MediaDownloaded {
                    chat: alice_key.clone(),
                    msg_id: "voice_m1".into(),
                    path: voice_path.clone(),
                    mime_type: "audio/ogg".into(),
                    sha256: "sha-voice-m1".into(),
                });
                this.vm.apply_sync_event(SyncEvent::TranscriptReady {
                    chat: alice_key.clone(),
                    msg_id: "voice_m1".into(),
                    sha256: "sha-voice-m1".into(),
                    raw_text: "Hello from Alice voice note.".into(),
                    formatted_text: None,
                    language: "en".into(),
                });
                let idx = this
                    .vm
                    .messages
                    .iter()
                    .position(|m| m.msg_id == "voice_m1")
                    .unwrap();
                this.vm.select_message_index(idx);
                this.sync_after_vm_change(window, cx);
            });
            window.render_frame(cx);
            assert!(window.try_find("voice-playback-bar").is_none());

            // Ctrl+P starts voice playback -> persistent top bar appears!
            window.press("ctrl-p", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window.try_find("voice-playback-bar").is_some(),
                "voice-playback-bar must render while active"
            );
            assert!(shell.read(cx).vm.is_voice_playing());
            assert!(shell.read(cx).vm.is_voice_played(&alice_key, "voice_m1"));

            // Ctrl+T toggles transcript expansion on selected voice message.
            assert!(
                !shell
                    .read(cx)
                    .vm
                    .is_transcript_expanded(&alice_key, "voice_m1")
            );
            window.press("ctrl-t", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(
                shell
                    .read(cx)
                    .vm
                    .is_transcript_expanded(&alice_key, "voice_m1")
            );

            // Stop playback -> persistent top bar disappears cleanly (0% idle).
            shell.update(cx, |this, cx| {
                this.vm.stop_voice_playback();
                this.sync_after_vm_change(window, cx);
            });
            window.render_frame(cx);
            assert!(window.try_find("voice-playback-bar").is_none());
        })
        .unwrap();

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[gpui_kit::test]
    fn m4_recording_strip_repeat_guard_enter_esc_and_thread_composer_block(
        cx: &mut TestAppContext,
    ) {
        let data_dir = unique_desktop_test_dir("m4_rec");
        std::fs::create_dir_all(&data_dir).unwrap();
        let mut ctx = seeded_launch_ctx();
        ctx.data_dir = data_dir.clone();
        let alice_key = ChatKey::new(Provider::Whatsapp, "main", "alice@s.whatsapp.net");
        let (handle, shell) = open_shell_with_ctx(cx, ctx);

        // 0. Blocked while window is inactive (`TestWindow::is_active` starts false).
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(!window.is_window_active());
            shell.update(cx, |this, cx| {
                this.vm.set_focus(FocusTarget::Composer);
                this.sync_after_vm_change(window, cx);
            });
            window.press("ctrl-shift-m", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(
                !shell.read(cx).vm.is_voice_recording(),
                "Ctrl+Shift+M must not start recording while window is inactive"
            );
            assert!(window.try_find("voice-recording-strip").is_none());
            // Bring headless test window to foreground so active-window recording paths run.
            window.activate_window();
        })
        .unwrap();
        cx.run_until_parked();

        // 1. Blocked when ThreadComposer is focused inside an active window.
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.is_window_active());
            shell.update(cx, |this, cx| {
                assert!(this.vm.select_chat_index(1));
                assert!(this.vm.open_thread("1700.1"));
                this.sync_after_vm_change(window, cx);
            });
            window.render_frame(cx);
            assert_eq!(shell.read(cx).vm.focus(), FocusTarget::ThreadComposer);
            assert!(window.try_find("thread-panel").is_some());
            window.press("ctrl-shift-m", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(!shell.read(cx).vm.is_voice_recording());
            assert!(window.try_find("voice-recording-strip").is_none());

            // 2. Close thread panel, focus main Composer, and press Ctrl+Shift+M -> starts recording strip.
            shell.update(cx, |this, cx| {
                this.vm.close_thread_panel();
                assert!(this.vm.select_chat_index(0));
                this.vm.set_focus(FocusTarget::Composer);
                this.sync_after_vm_change(window, cx);
            });
            window.render_frame(cx);
            assert!(
                shell
                    .read(cx)
                    .composer
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window),
                "Composer must have GPUI focus before Ctrl+Shift+M"
            );
            window.press("ctrl-shift-m", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(shell.read(cx).vm.is_voice_recording());
            assert_eq!(
                shell.read(cx).vm.focus(),
                FocusTarget::Composer,
                "starting voice recording from Composer must not clobber vm.focus on composer blur"
            );
            assert!(
                shell.read(cx).voice_record_focus.is_focused(window),
                "voice_record_focus must hold GPUI focus while recording"
            );
            assert!(
                window.try_find("voice-recording-strip").is_some(),
                "voice-recording-strip must render while recording"
            );

            // 2b. Opening an M3 modal (EmojiPicker) while recording must NOT lose focus on 100ms VoiceRecordTick.
            let generation = shell.read(cx).vm.active_record_generation();
            shell.update(cx, |this, cx| {
                assert!(this.vm.open_insert_emoji_picker());
                this.sync_after_vm_change(window, cx);
                this.apply_sync_events(
                    vec![SyncEvent::VoiceRecordTick {
                        generation,
                        elapsed_ms: 400,
                        bars: vec![40, 80],
                    }],
                    window,
                    cx,
                );
            });
            window.render_frame(cx);
            assert!(
                shell
                    .read(cx)
                    .emoji_search_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window),
                "VoiceRecordTick must not steal GPUI focus from open EmojiPicker modal"
            );
            assert!(!shell.read(cx).voice_record_focus.is_focused(window));

            // Closing EmojiPicker via real Esc must close ONLY the picker and return GPUI focus to voice_record_focus.
            window.press("escape", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(
                shell.read(cx).vm.emoji_picker().is_none(),
                "Esc must close EmojiPicker overlay"
            );
            assert!(
                shell.read(cx).vm.is_voice_recording(),
                "Esc closing EmojiPicker must NOT cancel active voice recording"
            );
            assert!(shell.read(cx).voice_record_focus.is_focused(window));
            assert_eq!(shell.read(cx).vm.focus(), FocusTarget::Composer);

            // 2c. Opening F1 ShortcutsOverlay while recording and pressing real Esc must close ONLY the modal.
            window.press("f1", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(
                shell.read(cx).vm.shortcuts_open(),
                "F1 must open ShortcutsOverlay while recording"
            );
            assert!(window.try_find("f1-shortcuts-modal").is_some());
            assert!(shell.read(cx).vm.is_voice_recording());
            assert!(
                !shell.read(cx).voice_record_focus.is_focused(window),
                "ShortcutsOverlay must hold focus instead of voice_record_focus while open"
            );
            window.press("escape", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(
                !shell.read(cx).vm.shortcuts_open(),
                "Esc must close F1 ShortcutsOverlay"
            );
            assert!(window.try_find("f1-shortcuts-modal").is_none());
            assert!(
                shell.read(cx).vm.is_voice_recording(),
                "Esc closing F1 ShortcutsOverlay must NOT cancel active voice recording"
            );
            assert!(
                shell.read(cx).voice_record_focus.is_focused(window),
                "voice_record_focus must regain GPUI focus after closing F1 ShortcutsOverlay"
            );
            assert_eq!(
                shell.read(cx).vm.focus(),
                FocusTarget::Composer,
                "closing F1 ShortcutsOverlay must preserve Composer focus target"
            );

            // 3. Held key-repeat of Ctrl+Shift+M MUST be consumed without finishing!
            let _ = window.dispatch_event(
                gpui_kit::PlatformInput::KeyDown(gpui_kit::KeyDownEvent {
                    keystroke: gpui_kit::Keystroke::parse("ctrl-shift-m").unwrap(),
                    is_held: true,
                    prefer_character_input: false,
                }),
                cx,
            );
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(
                shell.read(cx).vm.is_voice_recording(),
                "held repeat of Ctrl+Shift+M must NOT finish recording"
            );
            // 4. Esc cancels recording and removes recording strip.
            window.press("escape", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(!shell.read(cx).vm.is_voice_recording());
            assert!(window.try_find("voice-recording-strip").is_none());
            assert_eq!(
                shell.read(cx).vm.focus(),
                FocusTarget::Composer,
                "Esc cancelling recording must preserve FocusTarget::Composer"
            );
            assert!(
                shell
                    .read(cx)
                    .composer
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window),
                "Composer must regain GPUI focus after Esc cancels recording"
            );
            assert!(
                !shell.read(cx).sidebar_focus.is_focused(window),
                "sidebar_focus must NOT gain focus after Esc cancels recording"
            );

            // 5. Start again from Composer and press Enter -> routes via voice_record_focus to VoiceRecordFinish!
            window.press("ctrl-shift-m", cx);
        })
        .unwrap();
        cx.run_until_parked();

        let mut finish_generation = 0u64;
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(shell.read(cx).vm.is_voice_recording());
            assert_eq!(shell.read(cx).vm.focus(), FocusTarget::Composer);
            assert!(shell.read(cx).voice_record_focus.is_focused(window));
            finish_generation = shell.read(cx).vm.active_record_generation();
            let _ = shell.update(cx, |this, _| this.vm.take_emitted_sync_commands());
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            let cmds = shell.update(cx, |this, _| this.vm.take_emitted_sync_commands());
            assert!(
                cmds.iter()
                    .any(|c| matches!(c, SyncCommand::VoiceRecordFinish)),
                "Enter while recording must emit VoiceRecordFinish via voice_record_focus"
            );
            // 6. Completing VoiceRecordFinished returns recording to Idle and restores Composer focus!
            shell.update(cx, |this, cx| {
                this.apply_sync_events(
                    vec![SyncEvent::VoiceRecordFinished {
                        generation: finish_generation,
                        origin_chat: alice_key.clone(),
                        ogg_bytes: vec![b'O', b'g', b'g', b'S'],
                        duration_secs: 2,
                        waveform: vec![50; 32],
                    }],
                    window,
                    cx,
                );
            });
            window.render_frame(cx);
            assert!(!shell.read(cx).vm.voice_record_state().is_active());
            assert!(window.try_find("voice-recording-strip").is_none());
            assert_eq!(
                shell.read(cx).vm.focus(),
                FocusTarget::Composer,
                "VoiceRecordFinished returning to Idle must keep FocusTarget::Composer"
            );
            assert!(
                shell
                    .read(cx)
                    .composer
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window),
                "Composer must regain GPUI focus after VoiceRecordFinished"
            );
            assert!(
                !shell.read(cx).sidebar_focus.is_focused(window),
                "sidebar_focus must NOT gain focus after VoiceRecordFinished"
            );
        })
        .unwrap();

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[gpui_kit::test]
    fn m4_copy_transcript_and_copy_all_marked_writes_clipboard(cx: &mut TestAppContext) {
        let data_dir = unique_desktop_test_dir("m4_clip");
        std::fs::create_dir_all(&data_dir).unwrap();
        let mut ctx = seeded_launch_ctx();
        ctx.data_dir = data_dir.clone();
        let alice_key = ChatKey::new(Provider::Whatsapp, "main", "alice@s.whatsapp.net");
        for (id, ts) in [
            ("vc_1", "2026-11-27T11:01:00Z"),
            ("vc_2", "2026-11-27T11:02:00Z"),
        ] {
            ctx.store
                .upsert_message(&Message {
                    chat: alice_key.clone(),
                    msg_id: id.into(),
                    sender_id: "alice".into(),
                    sender_name: "Alice".into(),
                    from_me: false,
                    ts: ts.into(),
                    body: String::new(),
                    reply_to_id: None,
                    reply_broadcast: false,
                    media_kind: Some(postbox_core::MediaKind::Voice),
                    mime_type: Some("audio/ogg".into()),
                    filename: Some(format!("{id}.ogg")),
                    file_size: Some(1024),
                    reactions: vec![],
                    read_status: ReadStatus::None,
                    deleted: false,
                })
                .unwrap();
        }

        let (handle, shell) = open_shell_with_ctx(cx, ctx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            shell.update(cx, |this, cx| {
                this.vm.apply_sync_event(SyncEvent::TranscriptReady {
                    chat: alice_key.clone(),
                    msg_id: "vc_1".into(),
                    sha256: "sha-vc1".into(),
                    raw_text: "Older raw note".into(),
                    formatted_text: Some("1. Formatted older".into()),
                    language: "en".into(),
                });
                this.vm.apply_sync_event(SyncEvent::TranscriptReady {
                    chat: alice_key.clone(),
                    msg_id: "vc_2".into(),
                    sha256: "sha-vc2".into(),
                    raw_text: "Newer raw note".into(),
                    formatted_text: None,
                    language: "en".into(),
                });
                // 1. Single Copy transcript writes display text (formatted when available).
                let vc1_idx = this
                    .vm
                    .messages
                    .iter()
                    .position(|m| m.msg_id == "vc_1")
                    .unwrap();
                this.vm.select_message_index(vc1_idx);
                assert!(this.vm.copy_selected_transcript());
                this.sync_after_vm_change(window, cx);
            });
        })
        .unwrap();
        cx.run_until_parked();

        cx.update(|cx| {
            let clip = cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default();
            assert_eq!(clip, "1. Formatted older");
        });

        cx.update_window(handle.into(), |_, window, cx| {
            shell.update(cx, |this, cx| {
                // 2. Mark in reverse order to verify oldest-first `\r\n\r\n` raw clipboard export!
                this.vm.toggle_mark_message("vc_2");
                this.vm.toggle_mark_message("vc_1");
                assert!(this.vm.copy_all_marked_transcripts());
                this.sync_after_vm_change(window, cx);
            });
        })
        .unwrap();
        cx.run_until_parked();

        cx.update(|cx| {
            let clip = cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .unwrap_or_default();
            assert_eq!(clip, "Older raw note\r\n\r\nNewer raw note");
        });

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    // -------------------------------------------------------------------------
    // M5.5 RAM diet: active-chat-only timeline, chat-switch eviction, idle trim.
    // -------------------------------------------------------------------------

    /// Records every working-set trim and reports fixed memory counters.
    #[derive(Default)]
    struct TestMemoryPlatform {
        trims: std::sync::atomic::AtomicUsize,
        stats: postbox_platform::ProcessMemoryStats,
    }

    impl postbox_platform::MemoryPressure for TestMemoryPlatform {
        fn trim_process_working_set(&self) -> Result<(), postbox_platform::PlatformError> {
            self.trims
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }

        fn read_process_memory_stats(
            &self,
        ) -> Result<postbox_platform::ProcessMemoryStats, postbox_platform::PlatformError> {
            Ok(self.stats)
        }
    }

    #[gpui_kit::test]
    fn idle_and_minimize_trim_purges_transient_textures_and_reports_memory_counters(
        cx: &mut TestAppContext,
    ) {
        let mut launch_ctx = seeded_launch_ctx();
        let platform = std::sync::Arc::new(TestMemoryPlatform {
            trims: std::sync::atomic::AtomicUsize::new(0),
            stats: postbox_platform::ProcessMemoryStats {
                working_set_bytes: 61 * 1024 * 1024,
                private_usage_bytes: 54 * 1024 * 1024,
            },
        });
        launch_ctx.memory_platform = Some(platform.clone());
        let (handle, shell) = open_shell_with_ctx(cx, launch_ctx);
        let alice_key = ChatKey::new(Provider::Whatsapp, "main", "alice@s.whatsapp.net");

        // Populate the texture LRU with two transient inline images so the idle trim has
        // something to purge and we can count exactly what it released.
        let data_dir = unique_desktop_test_dir("ram_diet_trim");
        std::fs::create_dir_all(&data_dir).unwrap();
        let png = postbox_media::paste::encode_png(16, 16, &vec![200u8; 16 * 16 * 4]).unwrap();
        let inline_one = data_dir.join("inline_one.png");
        let inline_two = data_dir.join("inline_two.png");
        std::fs::write(&inline_one, &png).unwrap();
        std::fs::write(&inline_two, &png).unwrap();

        cx.update_window(handle.into(), |_, window, cx| {
            shell.update(cx, |this, cx| {
                for (msg_id, path, sha) in [
                    ("img_m2", &inline_one, "ram-diet-inline-1"),
                    ("img_m3", &inline_two, "ram-diet-inline-2"),
                ] {
                    this.vm.apply_sync_event(SyncEvent::MediaDownloaded {
                        chat: alice_key.clone(),
                        msg_id: msg_id.into(),
                        path: path.clone(),
                        mime_type: "image/png".into(),
                        sha256: sha.into(),
                    });
                }
                this.sync_after_vm_change(window, cx);
            });
        })
        .unwrap();
        cx.run_until_parked();
        // Feed the decoded textures straight into the LRU: this test is about the trim hook, and
        // driving the real decode queue would make it depend on the off-thread scheduler.
        let decoded = cx.update(|_| {
            postbox_media::decode::decode_image_bytes(&png, 64, 64, false)
                .expect("test PNG decodes")
        });
        cx.update_window(handle.into(), |_, window, cx| {
            shell.update(cx, |this, cx| {
                for (msg_id, path) in [("img_m2", &inline_one), ("img_m3", &inline_two)] {
                    let key = crate::desktop_m3::inline_media_tex_key(&alice_key.id, msg_id, path);
                    assert!(
                        this.gpu_textures
                            .ensure_decoded_inserted(&key, &decoded, window, cx)
                            .is_some()
                    );
                }
            });
        })
        .unwrap();

        let before = cx.update(|cx| shell.read(cx).gpu_textures.resident_count());
        assert!(
            before >= 2,
            "expected resident inline textures, got {before}"
        );

        // The idle/minimize hook drops transient media, keeps small persistent icons, releases
        // SQLite pages, and asks the OS to trim the working set.
        let dropped = cx
            .update_window(handle.into(), |_, window, cx| {
                shell.update(cx, |this, cx| this.trim_memory_for_idle(window, cx))
            })
            .unwrap();
        assert!(dropped >= 1, "the transient inline images must be purged");
        assert_eq!(platform.trims.load(std::sync::atomic::Ordering::Relaxed), 1);

        cx.update(|cx| {
            let this = shell.read(cx);
            assert_eq!(this.memory_trim_count, 1);
            assert!(
                this.gpu_textures
                    .resident_keys()
                    .iter()
                    .all(|k| !k.starts_with(crate::desktop_m3::INLINE_MEDIA_KEY_PREFIX)),
                "inline media must be purged on idle"
            );
            let counters = this.telemetry_counters();
            assert_eq!(counters.working_set_kb, 61 * 1024);
            assert_eq!(counters.private_usage_kb, 54 * 1024);
            assert_eq!(counters.gpu_texture_bytes, this.gpu_vram_bytes() as u64);
        });

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[gpui_kit::test]
    fn switching_chats_evicts_previous_chat_media_and_keeps_only_active_timeline(
        cx: &mut TestAppContext,
    ) {
        let data_dir = unique_desktop_test_dir("ram_diet_chat_switch");
        std::fs::create_dir_all(&data_dir).unwrap();
        let mut launch_ctx = seeded_launch_ctx();
        launch_ctx.data_dir = data_dir.clone();

        let alice_key = ChatKey::new(Provider::Whatsapp, "main", "alice@s.whatsapp.net");
        let bob_key = ChatKey::new(Provider::Slack, "T1", "slack:T1:C-bob");
        let png = postbox_media::paste::encode_png(20, 20, &vec![90u8; 20 * 20 * 4]).unwrap();
        let alice_img = data_dir.join("alice.png");
        let bob_img = data_dir.join("bob.png");
        std::fs::write(&alice_img, &png).unwrap();
        std::fs::write(&bob_img, &png).unwrap();

        let mut media_seeds: Vec<(ChatKey, String, std::path::PathBuf)> = Vec::new();
        for (chat, msg_id, path) in [
            (&alice_key, "alice_img", &alice_img),
            (&bob_key, "bob_img", &bob_img),
        ] {
            launch_ctx
                .store
                .upsert_message(&Message {
                    chat: chat.clone(),
                    msg_id: msg_id.into(),
                    sender_id: "u1".into(),
                    sender_name: "Peer".into(),
                    from_me: false,
                    ts: "2025-01-15T12:05:00Z".into(),
                    body: String::new(),
                    reply_to_id: None,
                    reply_broadcast: false,
                    media_kind: Some(postbox_core::MediaKind::Image),
                    mime_type: Some("image/png".into()),
                    filename: Some(format!("{msg_id}.png")),
                    file_size: Some(1024),
                    reactions: vec![],
                    read_status: ReadStatus::Read,
                    deleted: false,
                })
                .unwrap();
            media_seeds.push((chat.clone(), msg_id.to_string(), path.clone()));
        }

        let (handle, shell) = open_shell_with_ctx(cx, launch_ctx);

        // Load Alice (chat 0), decode her inline image.
        cx.update_window(handle.into(), |_, window, cx| {
            shell.update(cx, |this, cx| {
                for (chat, msg_id, path) in media_seeds.iter() {
                    this.vm.apply_sync_event(SyncEvent::MediaDownloaded {
                        chat: chat.clone(),
                        msg_id: msg_id.clone(),
                        path: path.clone(),
                        mime_type: "image/png".into(),
                        sha256: format!("sha-{msg_id}"),
                    });
                }
                assert!(this.vm.select_chat_index(0));
                this.vm.refresh_messages();
                this.sync_after_vm_change(window, cx);
            });
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            shell.update(cx, |this, cx| this.refresh_visible_gpu_textures(window, cx));
        })
        .unwrap();
        cx.run_until_parked();

        let alice_inline: Vec<String> = cx.update(|cx| {
            shell
                .read(cx)
                .gpu_textures
                .resident_keys()
                .into_iter()
                .filter(|k| k.starts_with(crate::desktop_m3::INLINE_MEDIA_KEY_PREFIX))
                .collect()
        });
        assert!(
            alice_inline.iter().any(|k| k.contains("alice_img")),
            "Alice's inline image should be resident: {alice_inline:?}"
        );

        // Switch to Bob: Alice's media goes, Bob's timeline replaces hers in RAM.
        let alice_msgs_before = cx.update(|cx| shell.read(cx).vm.messages().len());
        let dropped = cx
            .update_window(handle.into(), |_, window, cx| {
                shell.update(cx, |this, cx| {
                    assert!(this.vm.select_chat_index(1));
                    this.vm.refresh_messages();
                    let dropped = this.evict_previous_chat_media(window, cx);
                    this.sync_after_vm_change(window, cx);
                    dropped
                })
            })
            .unwrap();
        assert_eq!(
            dropped, 1,
            "Alice's inline image must be evicted exactly once"
        );
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            shell.update(cx, |this, cx| this.refresh_visible_gpu_textures(window, cx));
        })
        .unwrap();
        cx.run_until_parked();

        cx.update(|cx| {
            let this = shell.read(cx);
            let keys = this.gpu_textures.resident_keys();
            assert!(
                !keys.iter().any(|k| k.contains("alice_img")),
                "Alice's media must be gone after switching to Bob: {keys:?}"
            );
            // Active-chat-only timeline: Bob's messages replaced Alice's, no accumulation.
            assert!(alice_msgs_before >= 1);
            let bob_msgs = this.vm.messages();
            assert!(!bob_msgs.is_empty());
            assert!(bob_msgs.iter().all(|m| m.chat == bob_key));
            assert!(!bob_msgs.iter().any(|m| m.chat == alice_key));
            assert_eq!(this.resident_chat_id.as_deref(), Some(bob_key.id.as_str()));
        });

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    // -----------------------------------------------------------------------
    // Milestone 7: automatic update check, background staging and the restart card
    // -----------------------------------------------------------------------

    /// Records the relaunch instead of spawning, so no test starts a real Postbox.
    #[derive(Debug, Default)]
    struct RecordingRelauncher {
        calls: std::sync::Mutex<Vec<(std::path::PathBuf, Vec<String>)>>,
    }

    impl RecordingRelauncher {
        fn calls(&self) -> Vec<(std::path::PathBuf, Vec<String>)> {
            self.calls
                .lock()
                .map(|calls| calls.clone())
                .unwrap_or_default()
        }
    }

    impl postbox_platform::Relauncher for RecordingRelauncher {
        fn relaunch(
            &self,
            exe: &std::path::Path,
            args: &[String],
        ) -> Result<(), postbox_platform::PlatformError> {
            if let Ok(mut calls) = self.calls.lock() {
                calls.push((exe.to_path_buf(), args.to_vec()));
            }
            Ok(())
        }
    }

    /// A release source that answers with a fixed manifest and archive, and counts requests.
    struct FixedRelease {
        manifest: postbox_core::update::UpdateManifest,
        archive: Vec<u8>,
        downloads: std::sync::atomic::AtomicUsize,
    }

    impl postbox_platform::ReleaseSource for FixedRelease {
        fn fetch_manifest(
            &self,
        ) -> Result<postbox_core::update::UpdateManifest, postbox_platform::UpdateError> {
            Ok(self.manifest.clone())
        }

        fn download(
            &self,
            asset: &postbox_core::update::UpdateAsset,
        ) -> Result<Vec<u8>, postbox_platform::UpdateError> {
            self.downloads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            postbox_platform::verify_download(&self.archive, asset)?;
            Ok(self.archive.clone())
        }
    }

    /// A source that always fails, like an offline machine.
    struct OfflineRelease {
        attempts: std::sync::atomic::AtomicUsize,
    }

    impl postbox_platform::ReleaseSource for OfflineRelease {
        fn fetch_manifest(
            &self,
        ) -> Result<postbox_core::update::UpdateManifest, postbox_platform::UpdateError> {
            self.attempts
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Err(postbox_platform::UpdateError::Io(
                "dns lookup failed".to_owned(),
            ))
        }

        fn download(
            &self,
            _asset: &postbox_core::update::UpdateAsset,
        ) -> Result<Vec<u8>, postbox_platform::UpdateError> {
            Err(postbox_platform::UpdateError::Io(
                "dns lookup failed".to_owned(),
            ))
        }
    }

    fn release_manifest(tag: &str, archive: &[u8]) -> postbox_core::update::UpdateManifest {
        let body = format!(
            r#"{{"tag":"{tag}","asset":"postbox-windows-x64.zip","sha256":"{}","size":{},"body":"- Restarting keeps your window where it is","assets":[{{"name":"postbox-windows-x64.zip","browser_download_url":"https://example.invalid/postbox-windows-x64.zip","size":{}}}]}}"#,
            postbox_core::update::digest_hex(archive),
            archive.len(),
            archive.len()
        );
        postbox_core::update::UpdateManifest::parse(&body).expect("manifest parses")
    }

    fn update_launch_ctx(
        data_dir: &std::path::Path,
        source: std::sync::Arc<dyn postbox_platform::ReleaseSource>,
    ) -> PostboxLaunchContext {
        let mut ctx = seeded_launch_ctx();
        ctx.data_dir = data_dir.to_path_buf();
        ctx.release_source = Some(source);
        ctx
    }

    /// Opens a shell whose install directory is a temp folder holding a fake live binary.
    fn open_update_shell(
        cx: &mut TestAppContext,
        launch_ctx: PostboxLaunchContext,
        install_dir: std::path::PathBuf,
    ) -> (
        WindowHandle<Root>,
        Entity<PostboxShell>,
        std::sync::Arc<RecordingRelauncher>,
    ) {
        let keymap = launch_ctx.keymap.clone();
        cx.update(|cx| {
            gpui_kit::init(cx);
            register_bundled_fonts(cx);
            bind_postbox_keys(cx, &keymap);
            apply_postbox_theme(cx, ThemeChoice::ZincDark, FontScale::default());
        });
        cx.update(|cx| {
            let bounds = Bounds {
                origin: Point::default(),
                size: size(px(1200.0), px(800.0)),
            };
            let relauncher = std::sync::Arc::new(RecordingRelauncher::default());
            let recorder = relauncher.clone();
            let (window, shell) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                cx,
                move |window, cx| {
                    cx.new(|cx| {
                        PostboxShell::new(launch_ctx, window, cx)
                            .with_install_dir(install_dir)
                            .with_relauncher(relauncher)
                    })
                },
            )
            .expect("open Postbox test window");
            (
                window.downcast::<Root>().expect("Base Root"),
                shell,
                recorder,
            )
        })
    }

    fn install_fixture(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = unique_desktop_test_dir(tag);
        std::fs::create_dir_all(&dir).unwrap();
        let live = dir.join(postbox_platform::updater::EXE_ASSET_NAME);
        std::fs::write(&live, b"installed build").unwrap();
        (dir, live)
    }

    /// Runs the executor until the updater leaves its in-flight phases.
    ///
    /// The manifest check, the download and the staging run on a background thread, so one
    /// `run_until_parked()` can return before that thread's result reaches the shell. Polling
    /// the phase instead of sleeping a fixed time keeps the wait bounded and gives the same
    /// answer on a fast Linux disk and a slow Windows CI disk.
    fn settle_update_cycle(cx: &mut TestAppContext, shell: &Entity<PostboxShell>) {
        for _ in 0..600 {
            cx.run_until_parked();
            let settled = cx.update(|cx| !shell.read(cx).vm.m7().busy());
            if settled {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        cx.update(|cx| {
            panic!(
                "the update cycle never settled, still {:?}: {:?}",
                shell.read(cx).vm.m7().update_phase,
                shell.read(cx).vm.m7().update_note
            )
        });
    }

    #[gpui_kit::test]
    fn auto_check_stages_the_download_and_offers_the_restart_card(cx: &mut TestAppContext) {
        let (install_dir, live) = install_fixture("update_auto");
        let data_dir = unique_desktop_test_dir("update_auto_data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let archive = b"new release archive".to_vec();
        let source = std::sync::Arc::new(FixedRelease {
            manifest: release_manifest("v0.2.0", &archive),
            archive: archive.clone(),
            downloads: std::sync::atomic::AtomicUsize::new(0),
        });
        let launch_ctx = update_launch_ctx(&data_dir, source.clone());
        let (handle, shell, _recorder) = open_update_shell(cx, launch_ctx, install_dir.clone());
        cx.run_until_parked();

        // One automatic cycle, exactly as the deferred scheduler runs it.
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            shell.update(cx, |this, cx| this.start_update_cycle(cx, false));
        })
        .unwrap();
        settle_update_cycle(cx, &shell);

        cx.update(|cx| {
            let this = shell.read(cx);
            assert_eq!(
                this.vm.m7().update_phase,
                crate::m7::UpdatePhase::Ready,
                "the auto check stages and offers"
            );
            assert_eq!(
                this.vm.font_scale().percent(),
                100,
                "the card obeys the same font scale contract"
            );
            let prompt = this
                .vm
                .update_prompt()
                .expect("the restart card is offered");
            assert_eq!(prompt.headline(), "Postbox v0.2.0 is ready");
            assert!(prompt.notes.contains("window"), "{}", prompt.notes);
            assert_eq!(this.vm.update_command_label(), "Check for updates now");
            assert_eq!(
                this.vm.m7().restart_label(),
                Some("Restart to install update"),
                "Ctrl+K offers the restart while a staged update waits"
            );
        });
        // Staged beside the live binary, never swapped without the click.
        assert_eq!(
            std::fs::read(&live).unwrap(),
            b"installed build",
            "the running binary is untouched until the restart click"
        );
        assert!(
            std::fs::read(install_dir.join("postbox-windows-x64.zip.staged")).unwrap() == archive,
            "the verified release archive waits beside the live binary"
        );
        assert_eq!(
            source.downloads.load(std::sync::atomic::Ordering::Relaxed),
            1
        );

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window.try_find("update-ready-card").is_some(),
                "the non-modal card is painted"
            );
            assert!(window.try_find("update-ready-restart").is_some());
            assert!(window.try_find("update-ready-later").is_some());
        })
        .unwrap();

        let _ = std::fs::remove_dir_all(&install_dir);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[gpui_kit::test]
    fn later_hides_the_card_for_that_version_and_a_newer_one_re_prompts(cx: &mut TestAppContext) {
        let (install_dir, live) = install_fixture("update_later");
        let data_dir = unique_desktop_test_dir("update_later_data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let archive = b"release archive v2".to_vec();
        let source = std::sync::Arc::new(FixedRelease {
            manifest: release_manifest("v0.2.0", &archive),
            archive: archive.clone(),
            downloads: std::sync::atomic::AtomicUsize::new(0),
        });
        let launch_ctx = update_launch_ctx(&data_dir, source);
        let (handle, shell, _recorder) = open_update_shell(cx, launch_ctx, install_dir.clone());
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            shell.update(cx, |this, cx| this.start_update_cycle(cx, false));
        })
        .unwrap();
        settle_update_cycle(cx, &shell);

        // Clicking Later hides the card and changes nothing on disk.
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("update-ready-later", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window.try_find("update-ready-card").is_none(),
                "Later hides the card"
            );
        })
        .unwrap();
        cx.update(|cx| {
            let this = shell.read(cx);
            assert!(this.vm.update_prompt().is_none());
            assert_eq!(this.vm.m7().staged_tag(), Some("v0.2.0"), "still staged");
            assert!(this.staged_update.is_some());
        });
        assert_eq!(std::fs::read(&live).unwrap(), b"installed build");

        let archive_before = install_dir.join("postbox-windows-x64.zip.staged");
        assert!(archive_before.exists(), "the first download is staged");

        // A newer release re-prompts on its own, with no extra click.
        let newer = b"release archive v3".to_vec();
        cx.update(|cx| {
            shell.update(cx, |this, cx| {
                let asset = release_manifest("v0.3.0", &newer)
                    .select_asset()
                    .expect("the newer asset");
                assert!(this.vm.m7_mut().offer(asset, false));
                this.vm.m7_mut().mark_staged("v0.3.0", "- Newer notes", 10);
                cx.notify();
            });
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window.try_find("update-ready-card").is_some(),
                "a newer version brings the card back"
            );
        })
        .unwrap();

        let _ = std::fs::remove_dir_all(&install_dir);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[gpui_kit::test]
    fn restart_to_update_swaps_the_binary_atomically_and_relaunches(cx: &mut TestAppContext) {
        let (install_dir, live) = install_fixture("update_restart");
        let data_dir = unique_desktop_test_dir("update_restart_data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let archive = b"brand new release".to_vec();
        let source = std::sync::Arc::new(FixedRelease {
            manifest: release_manifest("v0.2.0", &archive),
            archive: archive.clone(),
            downloads: std::sync::atomic::AtomicUsize::new(0),
        });
        let launch_ctx = update_launch_ctx(&data_dir, source);
        let (handle, shell, recorder) = open_update_shell(cx, launch_ctx, install_dir.clone());
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            shell.update(cx, |this, cx| this.start_update_cycle(cx, false));
        })
        .unwrap();
        settle_update_cycle(cx, &shell);

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("update-ready-restart", cx);
        })
        .unwrap();
        cx.run_until_parked();

        assert_eq!(
            std::fs::read(&live).unwrap(),
            archive,
            "the staged build is installed in one atomic replace"
        );
        assert!(
            install_dir.join("postbox.exe.backup.exe").exists(),
            "the previous build is kept for a rollback"
        );
        assert!(
            !install_dir.join("postbox.exe.staged.exe").exists(),
            "the pending executable was moved, not copied"
        );
        cx.update(|cx| {
            let this = shell.read(cx);
            assert_eq!(this.vm.m7().update_phase, crate::m7::UpdatePhase::UpToDate);
            assert!(this.vm.update_prompt().is_none());
            let _ = this;
            let calls = recorder.calls();
            assert_eq!(calls.len(), 1, "the new build is launched exactly once");
            assert_eq!(calls[0].0, live, "the installed binary is relaunched");
            assert!(
                calls[0].1.iter().any(|arg| arg == "--update-relaunch"),
                "the relaunch flags itself: {:?}",
                calls[0].1
            );
            assert!(
                calls[0].1.iter().any(|arg| arg == "--window-state"),
                "the relaunch carries the window geometry: {:?}",
                calls[0].1
            );
        });

        let _ = std::fs::remove_dir_all(&install_dir);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[gpui_kit::test]
    fn a_bad_sha256_fails_closed_without_showing_the_ready_card(cx: &mut TestAppContext) {
        let (install_dir, live) = install_fixture("update_badhash");
        let data_dir = unique_desktop_test_dir("update_badhash_data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let real = b"the real release bytes".to_vec();
        let mut manifest = release_manifest("v0.2.0", &real);
        // The published digest no longer matches the served archive.
        manifest.sha256 = Some(postbox_core::update::digest_hex(b"something else entirely"));
        let source = std::sync::Arc::new(FixedRelease {
            manifest,
            archive: b"tampered release byte".to_vec(),
            downloads: std::sync::atomic::AtomicUsize::new(0),
        });
        let launch_ctx = update_launch_ctx(&data_dir, source);
        let (handle, shell, _recorder) = open_update_shell(cx, launch_ctx, install_dir.clone());
        cx.run_until_parked();

        // A manual check reports the failure; the card never appears.
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            shell.update(cx, |this, cx| this.start_update_cycle(cx, true));
        })
        .unwrap();
        settle_update_cycle(cx, &shell);

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window.try_find("update-ready-card").is_none(),
                "a bad digest never offers a restart"
            );
        })
        .unwrap();
        cx.update(|cx| {
            let this = shell.read(cx);
            assert_eq!(this.vm.m7().update_phase, crate::m7::UpdatePhase::Failed);
            assert!(this.vm.update_prompt().is_none());
            assert!(this.staged_update.is_none(), "nothing was staged");
            let note = this.vm.m7().update_note.clone().unwrap_or_default();
            assert!(note.contains("untouched"), "{note}");
        });
        assert_eq!(std::fs::read(&live).unwrap(), b"installed build");
        assert!(
            !install_dir.join("postbox.exe.staged.exe").exists(),
            "no pending executable was written"
        );

        let _ = std::fs::remove_dir_all(&install_dir);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[gpui_kit::test]
    fn an_offline_background_check_fails_quietly(cx: &mut TestAppContext) {
        let (install_dir, live) = install_fixture("update_offline");
        let data_dir = unique_desktop_test_dir("update_offline_data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let source = std::sync::Arc::new(OfflineRelease {
            attempts: std::sync::atomic::AtomicUsize::new(0),
        });
        let launch_ctx = update_launch_ctx(&data_dir, source.clone());
        let (handle, shell, _recorder) = open_update_shell(cx, launch_ctx, install_dir.clone());
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            shell.update(cx, |this, cx| this.start_update_cycle(cx, false));
        })
        .unwrap();
        settle_update_cycle(cx, &shell);

        cx.update(|cx| {
            let this = shell.read(cx);
            assert_eq!(
                this.vm.m7().update_phase,
                crate::m7::UpdatePhase::Idle,
                "a background failure leaves the updater idle, ready for the next interval"
            );
            assert!(this.vm.m7().update_note.is_none(), "no nagging status line");
            assert!(this.vm.status_notice().is_none(), "and no toast either");
            assert_eq!(
                source.attempts.load(std::sync::atomic::Ordering::Relaxed),
                1
            );
        });
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("update-ready-card").is_none());
        })
        .unwrap();
        assert_eq!(std::fs::read(&live).unwrap(), b"installed build");

        let _ = std::fs::remove_dir_all(&install_dir);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[gpui_kit::test]
    fn the_scheduler_checks_after_the_first_frame_and_not_before(cx: &mut TestAppContext) {
        let (install_dir, _live) = install_fixture("update_schedule");
        let data_dir = unique_desktop_test_dir("update_schedule_data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let source = std::sync::Arc::new(OfflineRelease {
            attempts: std::sync::atomic::AtomicUsize::new(0),
        });
        let launch_ctx = update_launch_ctx(&data_dir, source.clone());
        let (handle, shell, _recorder) = open_update_shell(cx, launch_ctx, install_dir.clone());
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            shell.update(cx, |this, cx| {
                assert!(
                    this.start_update_scheduler(window, cx).is_some(),
                    "the scheduler arms when a release source exists"
                );
            });
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(
            source.attempts.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "cold start touches no network"
        );

        // Past the deferred first check: exactly one cycle runs, then the loop waits.
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(
                crate::desktop_m7::UPDATE_FIRST_CHECK_DELAY_MS + 1_000,
            ));
        cx.run_until_parked();
        assert_eq!(
            source.attempts.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "one automatic check after the first frame"
        );
        // It failed quietly, so the loop is idle-ready rather than spamming.
        cx.update(|cx| {
            let this = shell.read(cx);
            assert_eq!(this.vm.m7().update_phase, crate::m7::UpdatePhase::Idle);
            assert!(this.vm.status_notice().is_none());
        });

        let _ = std::fs::remove_dir_all(&install_dir);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[gpui_kit::test]
    fn a_manual_check_of_a_staged_update_reuses_the_download(cx: &mut TestAppContext) {
        let (install_dir, _live) = install_fixture("update_manual");
        let data_dir = unique_desktop_test_dir("update_manual_data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let archive = b"release to stage once".to_vec();
        let source = std::sync::Arc::new(FixedRelease {
            manifest: release_manifest("v0.2.0", &archive),
            archive: archive.clone(),
            downloads: std::sync::atomic::AtomicUsize::new(0),
        });
        let launch_ctx = update_launch_ctx(&data_dir, source.clone());
        let (handle, shell, _recorder) = open_update_shell(cx, launch_ctx, install_dir.clone());
        cx.run_until_parked();

        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            shell.update(cx, |this, cx| this.start_update_cycle(cx, false));
        })
        .unwrap();
        settle_update_cycle(cx, &shell);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("update-ready-later", cx);
        })
        .unwrap();
        cx.run_until_parked();

        // `Ctrl+K` asks for a check: the staged version is surfaced, not downloaded again.
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            shell.update(cx, |this, cx| this.start_update_cycle(cx, true));
        })
        .unwrap();
        settle_update_cycle(cx, &shell);

        cx.update(|cx| {
            let this = shell.read(cx);
            assert!(this.vm.update_prompt().is_some(), "the card is back");
            assert_eq!(this.vm.m7().update_phase, crate::m7::UpdatePhase::Ready);
        });
        assert_eq!(
            source.downloads.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the staged bytes are reused instead of downloaded twice"
        );

        let _ = std::fs::remove_dir_all(&install_dir);
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
