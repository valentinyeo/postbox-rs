//! Milestone 5 GPUI desktop UI for Feature #32:
//! - Live 8-bar `micPill` waveform + stop square + `m:ss` elapsed clock + cancel button
//! - `120 ms` animation timer running ONLY while `window.is_window_active() && vm.is_dictation_active()`
//!   (`0.0%` idle CPU/GPU otherwise, never stealing focus from open modals).

use crate::desktop::{PostboxShell, RADIUS_CHIP_PX};
use crate::m5::{DICTATION_METER_TICK_MS, DictationPhase};
use crate::theme::ShadcnPalette;
use crate::{FONT_MONO, scaled_px};
use gpui_kit::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, TestSupportExt, Window, div, px, rgb,
};
use postbox_core::voice::format_voice_clock;

impl PostboxShell {
    /// Ensures the `120 ms` dictation meter timer runs ONLY while the window is active AND
    /// live dictation is active (`0.0%` idle CPU/GPU otherwise).
    /// Ticks update `meter_bars` / `elapsed_ms` and call `cx.notify()` WITHOUT stealing focus.
    pub(crate) fn sync_dictation_meter_timer(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let should_run = window.is_window_active() && self.vm.is_dictation_active();
        if !should_run {
            self.dictation_timer_task = None;
            return;
        }
        if self.dictation_timer_task.is_some() {
            return;
        }
        let task = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(DICTATION_METER_TICK_MS))
                    .await;
                let keep_going = this
                    .update_in(cx, |shell, window, cx| {
                        if !window.is_window_active() || !shell.vm.is_dictation_active() {
                            shell.dictation_timer_task = None;
                            return false;
                        }
                        let now_ms = crate::viewmodel::now_epoch_millis_public().max(0) as u64;
                        shell.vm.tick_dictation_meter(now_ms);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        });
        self.dictation_timer_task = Some(task);
    }

    /// Renders the live 8-bar `micPill` above the main composer (`for_thread == false`)
    /// or inside the thread panel footer (`for_thread == true`). Returns `None` when idle.
    pub(crate) fn render_dictation_mic_pill(
        &self,
        for_thread: bool,
        p: &ShadcnPalette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let d = self.vm.live_dictation()?;
        if d.in_thread != for_thread || !d.phase.is_active() {
            return None;
        }

        let scale = self.vm.font_scale().percent();
        let elapsed_label = match d.phase {
            DictationPhase::Draining => {
                format!("{} · Transcribing...", format_voice_clock(d.elapsed_ms))
            }
            _ => format_voice_clock(d.elapsed_ms),
        };

        let mut wave_bars = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            .h(px(20.0));

        for (i, &level) in d.meter_bars.iter().enumerate() {
            let bar_h = 4.0 + (f32::from(level) / 255.0) * 14.0;
            wave_bars = wave_bars.child(
                div()
                    .id(format!("dictation-wave-bar-{i}"))
                    .test_support()
                    .w(px(3.0))
                    .h(px(bar_h))
                    .rounded_full()
                    .bg(rgb(p.foreground)),
            );
        }

        let pill = div()
            .id("dictation-mic-pill")
            .test_support()
            .w_full()
            .px_3()
            .py_1p5()
            .rounded(px(RADIUS_CHIP_PX))
            .bg(rgb(p.secondary))
            .border_1()
            .border_color(rgb(p.ring))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("dictation-stop-btn")
                            .test_support()
                            .w(px(20.0))
                            .h(px(20.0))
                            .rounded(px(RADIUS_CHIP_PX))
                            .bg(rgb(p.muted))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.vm.stop_dictation();
                                this.sync_after_vm_change(window, cx);
                            }))
                            .child(
                                div()
                                    .w(px(10.0))
                                    .h(px(10.0))
                                    .rounded(px(2.0))
                                    .bg(rgb(p.destructive)),
                            ),
                    )
                    .child(
                        div()
                            .id("dictation-elapsed")
                            .test_support()
                            .font_family(FONT_MONO)
                            .text_size(px(scaled_px(12.0, scale)))
                            .text_color(rgb(p.foreground))
                            .child(elapsed_label),
                    )
                    .child(wave_bars),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(scaled_px(11.0, scale)))
                            .text_color(rgb(p.muted_foreground))
                            .child("Ctrl+Enter sends · Esc cancels"),
                    )
                    .child(
                        div()
                            .id("dictation-cancel-btn")
                            .test_support()
                            .px_2()
                            .py_0p5()
                            .rounded(px(RADIUS_CHIP_PX))
                            .bg(rgb(p.muted))
                            .text_size(px(scaled_px(11.0, scale)))
                            .text_color(rgb(p.foreground))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.vm.cancel_dictation();
                                this.sync_after_vm_change(window, cx);
                            }))
                            .child("Cancel"),
                    ),
            );

        Some(pill.into_any_element())
    }
}
