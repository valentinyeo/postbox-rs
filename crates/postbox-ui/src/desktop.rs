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
    Action as GpuiAction, AnyElement, App, AppContext as _, AsyncWindowContext, Bounds, Context,
    Entity, ExternalPaths, FocusHandle, Focusable, Hsla, InteractiveElement as _, IntoElement,
    KeyBinding, KeyBindingContextPredicate, KeyDownEvent, Keystroke, Modifiers,
    ModifiersChangedEvent, MouseButton, MouseDownEvent, ObjectFit, ParentElement as _, Pixels,
    Point, Render, ScrollStrategy, SharedString, Size, StatefulInteractiveElement as _,
    Styled as _, StyledImage as _, Subscription, Task, TitlebarOptions, WeakEntity, Window,
    WindowBounds, WindowOptions, div, hsla, img, px, rgb, size,
};
use postbox_core::threads::replies_line;
use postbox_core::{FontScale, Provider, ThemeChoice};
use postbox_keymap::{Action as PbAction, Keymap, f1_shortcut_groups};
use postbox_media::paste::PasteStageResult;
use postbox_media::unfurl::omit_unfurled_url_from_body;
use postbox_platform::{ClipboardBackend, NativePlatform};
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
        MoveViewUp,
        MoveViewDown,
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
        PbAction::MoveViewUp => Some(Box::new(MoveViewUp)),
        PbAction::MoveViewDown => Some(Box::new(MoveViewDown)),
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
    started_at: Instant,
    init_elapsed_ms: u128,
    root_focus: FocusHandle,
    sidebar_focus: FocusHandle,
    message_list_focus: FocusHandle,
    thread_broadcast_focus: FocusHandle,
    lightbox_focus: FocusHandle,
    emoji_picker_focus: FocusHandle,
    giphy_picker_focus: FocusHandle,
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
    pub(crate) reaction_retry_task: Option<Task<()>>,
    pub(crate) reaction_retry_deadline_ms: Option<u64>,
    sync_cmd_tx: Option<async_channel::Sender<SyncCommand>>,
    _sync_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
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
            .with_giphy_key_available(ctx.giphy_key_available);
        if let Some(bridge) = &ctx.sync_bridge {
            vm.attach_sync_sender(bridge.cmd_tx.clone());
        }
        vm.initialize();
        Self::from_view_model_with_bridge(vm, ctx.sync_bridge, started_at, window, cx)
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
                        let value = state.read(cx).value().to_string();
                        this.vm.set_composer_text(value);
                        // Sync back immediately if closed `:shortcode:` auto-replaced!
                        let updated = this.vm.composer_text().to_owned();
                        if state.read(cx).value().as_ref() != updated.as_str() {
                            state.update(cx, |input, cx| input.set_value(updated, window, cx));
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
                        this.vm.set_composer_focused(false);
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
                        let value = state.read(cx).value().to_string();
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
                if this.vm.on_window_blur() {
                    this.sync_sidebar_cache();
                }
                cx.notify();
            } else {
                this.sync_gif_animation_timer(window, cx);
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

        // Sync main composer text if chat switched
        let wanted_composer = self.vm.composer_text().to_owned();
        if self.composer.read(cx).value().as_ref() != wanted_composer.as_str() {
            self.composer.update(cx, |input, cx| {
                input.set_value(wanted_composer, window, cx);
            });
        }

        // Sync thread composer text
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
        self.sync_reaction_retry_timer(window, cx);
        self.sync_focus(window, cx);

        let title = self.vm.window_title();
        if title != self.last_window_title {
            window.set_window_title(&title);
            self.last_window_title = title;
        }

        if self.vm.quit_requested() {
            self.gpu_textures.clear_all(window, cx);
            cx.quit();
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

    fn execute_palette_index(
        &mut self,
        index: IndexPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
                                    if this.vm.double_click_message_bubble(&msg_id_dbl) {
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

        footer = footer.child(
            Textarea::new(&self.thread_composer)
                .aria_label("Thread reply composer")
                .bordered(true),
        );

        Some(
            div()
                .id("thread-panel")
                .test_support()
                .key_context("PostboxThreadPanel")
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
                ) {
                    (true, true) => "PostboxComposerEmpty PostboxShortcodePopup",
                    (true, false) => "PostboxComposerEmpty",
                    (false, true) => "PostboxComposer PostboxShortcodePopup",
                    (false, false) => "PostboxComposer",
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

        let main_column = div()
            .id("chat-main-column")
            .test_support()
            .track_focus(&self.message_list_focus)
            .key_context("PostboxMessageList")
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(p.background))
            .child(timeline_pane)
            .child(
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
            .on_action(cx.listener(|this, _: &MoveViewUp, window, cx| {
                this.dispatch_vm_action(PbAction::MoveViewUp, window, cx);
            }))
            .on_action(cx.listener(|this, _: &MoveViewDown, window, cx| {
                this.dispatch_vm_action(PbAction::MoveViewDown, window, cx);
            }))
            .child(self.render_view_rail(&p, cx))
            .child(sidebar_pane)
            .child(main_column);

        if let Some(thread_panel) = self.render_thread_panel(&p, cx) {
            root = root.child(thread_panel);
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
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(1240.0), px(820.0)),
                })),
                app_id: Some("postbox".to_owned()),
                ..Default::default()
            };

            gpui_kit::open_window(options, cx, move |window, cx| {
                window.set_window_title(&initial_title);
                cx.new(|cx| PostboxShell::new(launch_ctx, window, cx))
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
}
