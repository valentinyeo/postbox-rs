use crate::theme::ShadcnPalette;
use gpui_kit::*;
use postbox_core::{ThemeChoice, ViewTab};

pub struct PostboxShell {
    theme: ThemeChoice,
    view: ViewTab,
}

impl PostboxShell {
    pub fn new() -> Self {
        Self {
            theme: ThemeChoice::default(),
            view: ViewTab::CombinedInbox,
        }
    }
}

impl Default for PostboxShell {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for PostboxShell {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let palette = ShadcnPalette::for_choice(self.theme);
        let title = self.view.window_title(env!("CARGO_PKG_VERSION"));

        div()
            .flex()
            .size_full()
            .bg(rgb(palette.background))
            .text_color(rgb(palette.foreground))
            .child(
                div()
                    .w(px(300.0))
                    .h_full()
                    .bg(rgb(palette.sidebar))
                    .border_r_1()
                    .border_color(rgb(palette.border))
                    .p_3()
                    .child(title),
            )
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(palette.muted_foreground))
                    .child("Press Ctrl+K to open the Postbox command palette"),
            )
    }
}

pub fn run_desktop() {
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);
        gpui_kit::open_window(WindowOptions::default(), cx, |_window, cx| {
            cx.new(|_cx| PostboxShell::new())
        })
        .expect("failed to open Postbox window");
    });
}
