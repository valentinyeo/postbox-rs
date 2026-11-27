//! Pitch-preserving WSOLA/SOLA time-stretcher matching Zig `src/stretch.zig` verbatim.

pub const WINDOW_FRAMES: usize = 960; // 20 ms @ 48 kHz
pub const OVERLAP_FRAMES: usize = 240; // 5 ms @ 48 kHz
pub const SEARCH_FRAMES: usize = 240; // +-5 ms search window (offsets 0..=480)
pub const MAX_CHANNELS: usize = 2;
pub const HOP_FRAMES: usize = WINDOW_FRAMES - OVERLAP_FRAMES; // 720 (15 ms)
pub const LOOKAHEAD_FRAMES: usize = 2 * SEARCH_FRAMES + WINDOW_FRAMES; // 1440 (30 ms)
pub const MIN_SPEED_PERCENT: u16 = 50;
pub const MAX_SPEED_PERCENT: u16 = 300;

#[derive(Debug, Clone)]
pub struct TimeStretcher {
    channels: usize,
    percent: u16,
    pos_scaled: u64,
    pending: [f32; OVERLAP_FRAMES * MAX_CHANNELS],
    tail: [f32; OVERLAP_FRAMES * MAX_CHANNELS],
    has_tail: bool,
}

impl TimeStretcher {
    pub fn new(channels: usize, percent: u16) -> Self {
        Self {
            channels: channels.clamp(1, MAX_CHANNELS),
            percent: clamp_speed_percent(percent),
            pos_scaled: 0,
            pending: [0.0; OVERLAP_FRAMES * MAX_CHANNELS],
            tail: [0.0; OVERLAP_FRAMES * MAX_CHANNELS],
            has_tail: false,
        }
    }

    pub fn set_speed_percent(&mut self, percent: u16) {
        self.percent = clamp_speed_percent(percent);
    }

    pub fn speed_percent(&self) -> u16 {
        self.percent
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn has_tail(&self) -> bool {
        self.has_tail
    }

    pub fn reset(&mut self, start_frame: u64) {
        self.pos_scaled = start_frame.saturating_mul(100);
        self.pending = [0.0; OVERLAP_FRAMES * MAX_CHANNELS];
        self.tail = [0.0; OVERLAP_FRAMES * MAX_CHANNELS];
        self.has_tail = false;
    }

    pub fn source_frames_consumed(&self) -> u64 {
        self.pos_scaled / 100
    }

    pub fn rel_frames(&self, in_base: u64) -> usize {
        self.source_frames_consumed().saturating_sub(in_base) as usize
    }

    pub fn can_emit(&self, in_base: u64, in_frames: usize) -> bool {
        if self.percent == 100 {
            self.rel_frames(in_base).saturating_add(WINDOW_FRAMES) <= in_frames
        } else {
            self.rel_frames(in_base).saturating_add(LOOKAHEAD_FRAMES) <= in_frames
        }
    }

    /// Emits one block of interleaved f32 frames (`WINDOW_FRAMES` at 1.0x direct bypass,
    /// `HOP_FRAMES` at stretched speeds) into `out`. Returns number of output frames written.
    pub fn emit(&mut self, in_pcm: &[f32], in_base: u64, out: &mut Vec<f32>) -> usize {
        let ch = self.channels;
        let in_frames = in_pcm.len() / ch;
        if !self.can_emit(in_base, in_frames) {
            return 0;
        }

        if self.percent == 100 {
            self.has_tail = false;
            let rel = self.rel_frames(in_base);
            let start = rel * ch;
            let end = (rel + WINDOW_FRAMES) * ch;
            out.extend_from_slice(&in_pcm[start..end]);
            self.pos_scaled = self.pos_scaled.saturating_add((WINDOW_FRAMES as u64) * 100);
            return WINDOW_FRAMES;
        }

        let rel_pos = self
            .rel_frames(in_base)
            .min(in_frames.saturating_sub(LOOKAHEAD_FRAMES));

        let mut best_off = 0usize;
        if self.has_tail {
            let mut best_err = f64::INFINITY;
            for off in 0..=(2 * SEARCH_FRAMES) {
                let start = rel_pos + off;
                let mut err = 0.0f64;
                let mut i = 0usize;
                while i < OVERLAP_FRAMES {
                    let d = f64::from(in_pcm[(start + i) * ch] - self.tail[i * ch]);
                    err += d * d;
                    i += 4;
                }
                if err < best_err {
                    best_err = err;
                    best_off = off;
                }
            }
        }

        let src = rel_pos + best_off;
        let inv = 1.0f32 / OVERLAP_FRAMES as f32;
        out.reserve(HOP_FRAMES * ch);

        if self.has_tail {
            for i in 0..OVERLAP_FRAMES {
                let t = i as f32 * inv;
                for c in 0..ch {
                    let sample = self.pending[i * ch + c] + in_pcm[(src + i) * ch + c] * t;
                    out.push(sample);
                }
            }
        } else {
            self.has_tail = true;
            let start = src * ch;
            let end = (src + OVERLAP_FRAMES) * ch;
            out.extend_from_slice(&in_pcm[start..end]);
        }

        // Middle non-overlapped frames: OVERLAP_FRAMES..HOP_FRAMES
        let mid_start = (src + OVERLAP_FRAMES) * ch;
        let mid_end = (src + HOP_FRAMES) * ch;
        out.extend_from_slice(&in_pcm[mid_start..mid_end]);

        // Save next overlap tail + pre-multiplied fade-out pending buffer.
        let tail_src = (src + HOP_FRAMES) * ch;
        for i in 0..OVERLAP_FRAMES {
            let fade_out = 1.0f32 - i as f32 * inv;
            for c in 0..ch {
                let sample = in_pcm[tail_src + i * ch + c];
                self.tail[i * ch + c] = sample;
                self.pending[i * ch + c] = sample * fade_out;
            }
        }

        self.pos_scaled = self
            .pos_scaled
            .saturating_add((HOP_FRAMES as u64) * u64::from(self.percent));
        HOP_FRAMES
    }

    /// Flushes remaining end-of-stream samples with a linear crossfade from `pending`
    /// so the end of a voice note never clips or clicks.
    pub fn finish_tail(&mut self, remaining_pcm: &[f32], out: &mut Vec<f32>) -> usize {
        let ch = self.channels;
        let rem_frames = remaining_pcm.len() / ch;
        if rem_frames == 0 {
            if self.has_tail {
                out.extend_from_slice(&self.pending[..OVERLAP_FRAMES * ch]);
                self.has_tail = false;
                return OVERLAP_FRAMES;
            }
            return 0;
        }

        let inv = 1.0f32 / OVERLAP_FRAMES as f32;
        out.reserve(rem_frames * ch);
        if self.has_tail {
            let fade = rem_frames.min(OVERLAP_FRAMES);
            for i in 0..fade {
                let t = i as f32 * inv;
                for c in 0..ch {
                    out.push(self.pending[i * ch + c] + remaining_pcm[i * ch + c] * t);
                }
            }
            if rem_frames > fade {
                out.extend_from_slice(&remaining_pcm[fade * ch..rem_frames * ch]);
            }
            self.has_tail = false;
        } else {
            out.extend_from_slice(&remaining_pcm[..rem_frames * ch]);
        }
        self.pos_scaled = self.pos_scaled.saturating_add((rem_frames as u64) * 100);
        rem_frames
    }
}

pub const fn clamp_speed_percent(percent: u16) -> u16 {
    if percent < MIN_SPEED_PERCENT {
        MIN_SPEED_PERCENT
    } else if percent > MAX_SPEED_PERCENT {
        MAX_SPEED_PERCENT
    } else {
        percent
    }
}

/// Convenience helper: time-stretches a full interleaved f32 buffer from start to end.
pub fn stretch_buffer(pcm: &[f32], channels: usize, speed_percent: u16) -> Vec<f32> {
    let ch = channels.clamp(1, MAX_CHANNELS);
    let mut stretcher = TimeStretcher::new(ch, speed_percent);
    let mut out = Vec::new();
    let total_frames = pcm.len() / ch;
    while stretcher.can_emit(0, total_frames) {
        stretcher.emit(pcm, 0, &mut out);
    }
    let consumed = (stretcher.source_frames_consumed() as usize).min(total_frames);
    stretcher.finish_tail(&pcm[consumed * ch..], &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_stereo_48k(duration_secs: f32, freq_hz: f32) -> Vec<f32> {
        let frames = (48_000.0 * duration_secs) as usize;
        let mut out = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let s = (2.0 * std::f32::consts::PI * freq_hz * (i as f32) / 48_000.0).sin() * 0.5;
            out.push(s);
            out.push(s);
        }
        out
    }

    #[test]
    fn wsola_1x_bypass_is_bit_identical() {
        let input = sine_stereo_48k(1.0, 440.0);
        let output = stretch_buffer(&input, 2, 100);
        assert_eq!(output, input);
    }

    #[test]
    fn wsola_1_5x_and_2x_produce_expected_durations_without_clipping() {
        let input = sine_stereo_48k(2.0, 300.0);
        let in_frames = input.len() / 2;

        let out_150 = stretch_buffer(&input, 2, 150);
        let frames_150 = out_150.len() / 2;
        let ratio_150 = in_frames as f64 / frames_150 as f64;
        assert!(
            (ratio_150 - 1.5).abs() < 0.05,
            "expected ~1.5x ratio, got {ratio_150}"
        );
        assert!(out_150.iter().all(|&s| s.is_finite() && s.abs() <= 1.05));

        let out_200 = stretch_buffer(&input, 2, 200);
        let frames_200 = out_200.len() / 2;
        let ratio_200 = in_frames as f64 / frames_200 as f64;
        assert!(
            (ratio_200 - 2.0).abs() < 0.05,
            "expected ~2.0x ratio, got {ratio_200}"
        );
        assert!(out_200.iter().all(|&s| s.is_finite() && s.abs() <= 1.05));
    }

    #[test]
    fn wsola_tail_crossfade_avoids_discontinuity_click() {
        let input = sine_stereo_48k(0.5, 440.0);
        let mut stretcher = TimeStretcher::new(2, 150);
        let mut out = Vec::new();
        let total_frames = input.len() / 2;
        while stretcher.can_emit(0, total_frames) {
            stretcher.emit(&input, 0, &mut out);
        }
        assert!(stretcher.has_tail());
        let before_tail_len = out.len();
        let consumed = (stretcher.source_frames_consumed() as usize).min(total_frames);
        stretcher.finish_tail(&input[consumed * 2..], &mut out);
        assert!(!stretcher.has_tail());
        assert!(out.len() > before_tail_len);
        // Transition across the tail splice should stay smooth.
        let prev = out[before_tail_len - 2];
        let next = out[before_tail_len];
        assert!(
            (next - prev).abs() < 0.25,
            "tail jump too large: {prev} -> {next}"
        );
    }
}
