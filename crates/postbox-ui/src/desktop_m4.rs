//! Milestone 4 GPUI + `gpui-kit` desktop views:
//! - Persistent top voice playback bar (`voice-playback-bar`, rendered ONLY while active -> 0% idle)
//! - Voice-note waveform pill (play/pause zone, `1x`/`1.5x`/`2x` speed zone, clickable waveform scrubber, `m:ss` clock, `"✓ played"`, inline `Ctrl+T` transcript toggle & body)
//! - `Ctrl+Shift+M` live 64-bar recording strip (`voice-recording-strip`, `PostboxVoiceRecording` key context)

use crate::desktop::PostboxShell;
use crate::geometry::TYPE_LABEL_PX;
use crate::theme::ShadcnPalette;
use crate::{BASE_FONT_PX, scaled_px};
use gpui_kit::{
    AnyElement, Context, Entity, InteractiveElement, IntoElement, MouseButton, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, TestSupportExt, div, px, rgb,
};
use postbox_core::Message;
use postbox_core::voice::{
    TranscriptStatus, VoicePlaybackStatus, VoiceRecordState, format_voice_clock,
};

const PILL_WAVE_BUCKETS: usize = 32;
const TRANSCRIPT_PREVIEW_MAX_CHARS: usize = 400;

fn safe_voice_bar_label(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains('@') {
        "Voice message".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn truncate_preview_chars(text: &str, max_chars: usize) -> (String, bool) {
    let mut chars = text.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    let truncated = chars.next().is_some();
    (preview, truncated)
}

impl PostboxShell {
    /// Persistent top voice playback bar rendered ONLY when playback is active (0% idle).
    pub(crate) fn render_voice_playback_bar(
        &self,
        p: &ShadcnPalette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let pb = self.vm.voice_playback()?.clone();
        let chat_title = self
            .vm
            .sidebar_rows()
            .into_iter()
            .find(|r| r.chat.key == pb.chat)
            .map(|r| safe_voice_bar_label(&r.chat.title))
            .unwrap_or_else(|| "Voice message".to_owned());

        let is_playing = pb.status == VoicePlaybackStatus::Playing;
        let toggle_glyph = if is_playing { "||" } else { ">" };
        let clock = if pb.dur_ms > 0 {
            format!(
                "{} / {}",
                format_voice_clock(pb.pos_ms.min(pb.dur_ms)),
                format_voice_clock(pb.dur_ms)
            )
        } else {
            format_voice_clock(pb.pos_ms)
        };
        let speed_label = pb.speed.label();
        let scale = self.vm.font_scale().percent();
        let label_px = scaled_px(TYPE_LABEL_PX, scale);

        Some(
            div()
                .id("voice-playback-bar")
                .test_support()
                .w_full()
                .h(px(36.0))
                .px_3()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .bg(rgb(p.card))
                .border_b_1()
                .border_color(rgb(p.border))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .id("voice-bar-toggle")
                                .test_support()
                                .px_2()
                                .py_0p5()
                                .rounded_sm()
                                .bg(rgb(p.primary))
                                .text_color(rgb(p.primary_foreground))
                                .text_size(px(label_px))
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.vm.toggle_voice_playback();
                                    this.sync_after_vm_change(window, cx);
                                }))
                                .child(toggle_glyph),
                        )
                        .child(
                            div()
                                .id("voice-bar-jump")
                                .test_support()
                                .text_size(px(label_px))
                                .text_color(rgb(p.foreground))
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.vm.jump_to_playing_chat();
                                    this.sync_after_vm_change(window, cx);
                                }))
                                .child(chat_title),
                        )
                        .child(
                            div()
                                .text_size(px(label_px))
                                .text_color(rgb(p.muted_foreground))
                                .child(clock),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1p5()
                        .child(
                            div()
                                .id("voice-bar-speed")
                                .test_support()
                                .px_2()
                                .py_0p5()
                                .rounded_sm()
                                .bg(rgb(p.secondary))
                                .border_1()
                                .border_color(rgb(p.border))
                                .text_size(px(label_px))
                                .text_color(rgb(p.foreground))
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.vm.cycle_playback_speed();
                                    this.sync_after_vm_change(window, cx);
                                }))
                                .child(speed_label),
                        )
                        .child(
                            div()
                                .id("voice-bar-stop")
                                .test_support()
                                .px_2()
                                .py_0p5()
                                .rounded_sm()
                                .bg(rgb(p.secondary))
                                .border_1()
                                .border_color(rgb(p.border))
                                .text_size(px(label_px))
                                .text_color(rgb(p.muted_foreground))
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.vm.stop_voice_playback();
                                    this.sync_after_vm_change(window, cx);
                                }))
                                .child("x"),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Voice-note waveform pill with play/pause zone, speed cycle zone, clickable waveform scrubber,
    /// `m:ss` clock, `"✓ played"` indicator, and collapsible inline transcript (`Ctrl+T`).
    pub(crate) fn render_voice_note_pill(
        &self,
        msg: &Message,
        p: &ShadcnPalette,
        ix: usize,
        id_prefix: &str,
        shell: &Entity<Self>,
    ) -> AnyElement {
        let scale = self.vm.font_scale().percent();
        let body_px = scaled_px(BASE_FONT_PX, scale);
        let label_px = scaled_px(TYPE_LABEL_PX, scale);
        let active_pb = self
            .vm
            .voice_playback()
            .filter(|pb| pb.chat == msg.chat && pb.msg_id == msg.msg_id);
        let is_active = active_pb.is_some();
        let is_playing = active_pb.is_some_and(|pb| pb.status == VoicePlaybackStatus::Playing);
        let has_local = self.vm.media_path_for(&msg.chat, &msg.msg_id).is_some();
        let played = self.vm.is_voice_played(&msg.chat, &msg.msg_id);
        let saved_pos = self.vm.voice_resume_position(&msg.chat, &msg.msg_id);

        let (pos_ms, dur_ms) = if let Some(pb) = active_pb {
            (pb.pos_ms, pb.dur_ms)
        } else if let Some((pos, dur)) = saved_pos {
            (pos, dur)
        } else {
            (0, 0)
        };
        let progress = if dur_ms > 0 {
            (pos_ms as f32 / dur_ms as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };

        let speed_label = self.vm.playback_speed().label();
        let play_glyph = if !has_local {
            "DL"
        } else if is_playing {
            "||"
        } else {
            ">"
        };

        let clock_text = if dur_ms > 0 {
            format!(
                "{} / {}",
                format_voice_clock(pos_ms.min(dur_ms)),
                format_voice_clock(dur_ms)
            )
        } else if !has_local {
            self.vm.attachment_display_label(msg)
        } else {
            "0:00".to_owned()
        };

        let raw_wave: Vec<u8> = active_pb
            .map(|pb| pb.waveform.clone())
            .or_else(|| {
                self.vm
                    .voice_waveform(&msg.chat, &msg.msg_id)
                    .map(|s| s.to_vec())
            })
            .unwrap_or_else(|| vec![35u8; PILL_WAVE_BUCKETS]);

        let mut bar_elements = Vec::with_capacity(PILL_WAVE_BUCKETS);
        for bucket in 0..PILL_WAVE_BUCKETS {
            let src_idx = if raw_wave.is_empty() {
                0
            } else {
                (bucket * raw_wave.len()) / PILL_WAVE_BUCKETS
            };
            let level = raw_wave.get(src_idx).copied().unwrap_or(35);
            let bar_h = (4.0 + (f32::from(level) / 100.0) * 18.0).clamp(4.0, 22.0);
            let frac = (bucket as f32 + 0.5) / PILL_WAVE_BUCKETS as f32;
            let bar_color = if frac <= progress {
                p.primary
            } else {
                p.muted_foreground
            };

            let shell_scrub = shell.clone();
            let chat_scrub = msg.chat.clone();
            let msg_scrub = msg.msg_id.clone();
            bar_elements.push(
                div()
                    .id(SharedString::from(format!(
                        "{id_prefix}-voice-bar-{ix}-{bucket}"
                    )))
                    .test_support()
                    .w(px(3.0))
                    .h(px(bar_h))
                    .rounded_sm()
                    .bg(rgb(bar_color))
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        shell_scrub.update(cx, |this, cx| {
                            this.vm
                                .click_voice_bubble_scrub(&chat_scrub, &msg_scrub, frac);
                            this.sync_after_vm_change(window, cx);
                        });
                    }),
            );
        }

        let shell_play = shell.clone();
        let chat_play = msg.chat.clone();
        let msg_play = msg.msg_id.clone();

        let shell_speed = shell.clone();

        let mut footer_row = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .text_size(px(label_px))
            .text_color(rgb(p.muted_foreground))
            .child(clock_text);

        if !is_active && played {
            footer_row = footer_row.child("\u{2713} played");
        }

        let mut container = div()
            .id(SharedString::from(format!("{id_prefix}-voice-{ix}")))
            .test_support()
            .w(px(320.0))
            .px_2p5()
            .py_2()
            .rounded_md()
            .bg(rgb(p.secondary))
            .border_1()
            .border_color(rgb(if is_active { p.primary } else { p.border }))
            .flex()
            .flex_col()
            .gap_1p5()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id(SharedString::from(format!("{id_prefix}-voice-play-{ix}")))
                            .test_support()
                            .w(px(30.0))
                            .h(px(28.0))
                            .rounded_md()
                            .bg(rgb(p.primary))
                            .text_color(rgb(p.primary_foreground))
                            .text_size(px(label_px))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                shell_play.update(cx, |this, cx| {
                                    this.vm.click_voice_bubble_play(&chat_play, &msg_play);
                                    this.sync_after_vm_change(window, cx);
                                });
                            })
                            .child(play_glyph),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("{id_prefix}-voice-speed-{ix}")))
                            .test_support()
                            .px_1p5()
                            .py_0p5()
                            .rounded_sm()
                            .bg(rgb(p.card))
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
                                shell_speed.update(cx, |this, cx| {
                                    this.vm.cycle_playback_speed();
                                    this.sync_after_vm_change(window, cx);
                                });
                            })
                            .child(speed_label),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("{id_prefix}-voice-wave-{ix}")))
                            .test_support()
                            .flex_1()
                            .h(px(26.0))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(2.0))
                            .children(bar_elements),
                    ),
            )
            .child(footer_row);

        // Inline transcript display (`Ctrl+T` expand/collapse, status, or error).
        match self.vm.transcript_status(&msg.chat, &msg.msg_id) {
            TranscriptStatus::None => {}
            TranscriptStatus::Transcribing => {
                container = container.child(
                    div()
                        .id(SharedString::from(format!(
                            "{id_prefix}-transcript-body-{ix}"
                        )))
                        .test_support()
                        .pt_1()
                        .border_t_1()
                        .border_color(rgb(p.border))
                        .text_size(px(label_px))
                        .text_color(rgb(p.muted_foreground))
                        .child("Transcribing\u{2026}"),
                );
            }
            TranscriptStatus::Failed(err) => {
                container = container.child(
                    div()
                        .id(SharedString::from(format!(
                            "{id_prefix}-transcript-body-{ix}"
                        )))
                        .test_support()
                        .pt_1()
                        .border_t_1()
                        .border_color(rgb(p.border))
                        .text_size(px(label_px))
                        .text_color(rgb(p.destructive))
                        .child(err.user_message()),
                );
            }
            TranscriptStatus::Ready(rec) => {
                let full_text = rec.display_text();
                if !full_text.trim().is_empty() {
                    let expanded = self.vm.is_transcript_expanded(&msg.chat, &msg.msg_id);
                    let (shown_text, was_truncated) = if expanded {
                        (full_text.to_owned(), false)
                    } else {
                        truncate_preview_chars(full_text, TRANSCRIPT_PREVIEW_MAX_CHARS)
                    };
                    let toggle_label = if expanded {
                        "Show less"
                    } else if was_truncated {
                        "Show more"
                    } else {
                        "Hide/Expand (Ctrl+T)"
                    };
                    let shell_t = shell.clone();
                    let chat_t = msg.chat.clone();
                    let msg_t = msg.msg_id.clone();

                    container = container.child(
                        div()
                            .pt_1()
                            .border_t_1()
                            .border_color(rgb(p.border))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .id(SharedString::from(format!(
                                        "{id_prefix}-transcript-body-{ix}"
                                    )))
                                    .test_support()
                                    .text_size(px(body_px))
                                    .text_color(rgb(p.foreground))
                                    .child(shown_text),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!(
                                        "{id_prefix}-transcript-toggle-{ix}"
                                    )))
                                    .test_support()
                                    .text_size(px(label_px))
                                    .text_color(rgb(p.primary))
                                    .cursor_pointer()
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation();
                                    })
                                    .on_click(move |_, window, cx| {
                                        cx.stop_propagation();
                                        shell_t.update(cx, |this, cx| {
                                            this.vm.toggle_transcript_for_message(&chat_t, &msg_t);
                                            this.sync_after_vm_change(window, cx);
                                        });
                                    })
                                    .child(toggle_label),
                            ),
                    );
                }
            }
        }

        container.into_any_element()
    }

    /// Live `Ctrl+Shift+M` 64-bar recording strip rendered ONLY while recording or retained (0% idle).
    pub(crate) fn render_voice_recording_strip(
        &self,
        p: &ShadcnPalette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let state = self.vm.voice_record_state();
        let (label, bars) = match state {
            VoiceRecordState::Idle => return None,
            VoiceRecordState::Recording {
                elapsed_ms,
                live_bars,
                ..
            } => (
                format!(
                    "REC {} \u{00B7} Enter sends \u{00B7} Esc cancels",
                    format_voice_clock(*elapsed_ms)
                ),
                live_bars.as_slice(),
            ),
            VoiceRecordState::Retained {
                duration_secs,
                waveform,
                ..
            } => (
                format!(
                    "Voice note retained ({}) \u{00B7} Enter retries \u{00B7} Esc cancels",
                    format_voice_clock(u64::from(*duration_secs) * 1000)
                ),
                waveform.as_slice(),
            ),
        };

        let scale = self.vm.font_scale().percent();
        let label_px = scaled_px(TYPE_LABEL_PX, scale);
        let mut meter_bars = Vec::with_capacity(64);
        for i in 0..64usize {
            let level = bars.get(i).copied().unwrap_or(8);
            let h = (3.0 + (f32::from(level) / 100.0) * 17.0).clamp(3.0, 20.0);
            meter_bars.push(
                div()
                    .w(px(2.0))
                    .h(px(h))
                    .rounded_sm()
                    .bg(rgb(p.destructive)),
            );
        }

        Some(
            div()
                .id("voice-recording-strip")
                .test_support()
                .track_focus(&self.voice_record_focus)
                .key_context("PostboxVoiceRecording")
                .w_full()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(rgb(p.secondary))
                .border_1()
                .border_color(rgb(p.destructive))
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
                                .w(px(8.0))
                                .h(px(8.0))
                                .rounded_full()
                                .bg(rgb(p.destructive)),
                        )
                        .child(
                            div()
                                .text_size(px(label_px))
                                .text_color(rgb(p.foreground))
                                .child(label),
                        ),
                )
                .child(
                    div()
                        .h(px(22.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(2.0))
                        .children(meter_bars),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1p5()
                        .child(
                            div()
                                .id("voice-record-send")
                                .test_support()
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(rgb(p.primary))
                                .text_color(rgb(p.primary_foreground))
                                .text_size(px(label_px))
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.vm.finish_voice_recording();
                                    this.sync_after_vm_change(window, cx);
                                }))
                                .child("Send"),
                        )
                        .child(
                            div()
                                .id("voice-record-cancel")
                                .test_support()
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .bg(rgb(p.card))
                                .border_1()
                                .border_color(rgb(p.border))
                                .text_color(rgb(p.muted_foreground))
                                .text_size(px(label_px))
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.vm.cancel_voice_recording("Voice note cancelled");
                                    this.sync_after_vm_change(window, cx);
                                }))
                                .child("Cancel"),
                        ),
                )
                .into_any_element(),
        )
    }
}
