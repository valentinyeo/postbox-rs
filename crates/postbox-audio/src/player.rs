//! Voice playback controller driving `AudioBackend::open_render` with Ogg Opus decoding,
//! WSOLA pitch-preserving time-stretch (`1.0x`/`1.5x`/`2.0x`), seek, pause/resume, and EOS drain.

use crate::ogg::OPUS_SAMPLE_RATE;
use crate::stretch::{LOOKAHEAD_FRAMES, TimeStretcher, WINDOW_FRAMES};
use crate::voice_note::{AudioError, DecodedVoiceNote, WAVEFORM_BARS, decode_ogg_opus};
use postbox_core::voice::PlaybackSpeed;
use postbox_platform::{AudioBackend, AudioRenderStream};
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerState {
    Idle,
    Ready,
    Playing,
    Paused,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayerSnapshot {
    pub state: PlayerState,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub speed: PlaybackSpeed,
}

/// Deterministic, pull/pump-driven Ogg Opus voice player suitable for both a dedicated
/// background playback thread (8 ms cadence) and synchronous unit tests via `MockAudioBackend`.
pub struct VoicePlayer {
    decoded: Option<DecodedVoiceNote>,
    stream: Option<Box<dyn AudioRenderStream>>,
    stretcher: TimeStretcher,
    state: PlayerState,
    speed: PlaybackSpeed,
    /// Current position in source 48 kHz frames.
    source_frame_cursor: usize,
    /// Pending stretched interleaved stereo f32 frames waiting to be written to the render stream.
    pending_out: Vec<f32>,
    pending_out_offset: usize,
    /// Total source frames rendered since `seek_base_frame`.
    seek_base_frame: u64,
    rendered_src_frames: u64,
    tail_flushed: bool,
    eos_drain_polls: u32,
}

impl Default for VoicePlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl VoicePlayer {
    pub fn new() -> Self {
        Self {
            decoded: None,
            stream: None,
            stretcher: TimeStretcher::new(2, PlaybackSpeed::X1.percent()),
            state: PlayerState::Idle,
            speed: PlaybackSpeed::X1,
            source_frame_cursor: 0,
            pending_out: Vec::new(),
            pending_out_offset: 0,
            seek_base_frame: 0,
            rendered_src_frames: 0,
            tail_flushed: false,
            eos_drain_polls: 0,
        }
    }

    pub fn load_file(
        &mut self,
        backend: &dyn AudioBackend,
        path: &Path,
        speed: PlaybackSpeed,
        start_ms: u64,
    ) -> Result<[u8; WAVEFORM_BARS], AudioError> {
        let bytes = std::fs::read(path)?;
        self.load_bytes(backend, &bytes, speed, start_ms)
    }

    pub fn load_bytes(
        &mut self,
        backend: &dyn AudioBackend,
        ogg_bytes: &[u8],
        speed: PlaybackSpeed,
        start_ms: u64,
    ) -> Result<[u8; WAVEFORM_BARS], AudioError> {
        self.stop();
        let decoded = decode_ogg_opus(ogg_bytes)?;
        let waveform = decoded.waveform;
        let stream = backend.open_render(OPUS_SAMPLE_RATE, 2)?;

        self.speed = speed;
        self.stretcher = TimeStretcher::new(2, speed.percent());
        self.decoded = Some(decoded);
        self.stream = Some(stream);
        self.state = PlayerState::Playing;
        self.source_frame_cursor = 0;
        self.pending_out.clear();
        self.pending_out_offset = 0;
        self.seek_base_frame = 0;
        self.rendered_src_frames = 0;
        self.tail_flushed = false;
        self.eos_drain_polls = 0;

        if start_ms > 0 {
            self.seek_ms(start_ms)?;
        }

        Ok(waveform)
    }

    pub fn state(&self) -> PlayerState {
        self.state
    }

    pub fn duration_ms(&self) -> u64 {
        self.decoded.as_ref().map_or(0, |d| d.duration_ms)
    }

    pub fn position_ms(&self) -> u64 {
        let dur = self.duration_ms();
        let raw_ms = self
            .seek_base_frame
            .saturating_add(self.rendered_src_frames)
            .saturating_mul(1000)
            / u64::from(OPUS_SAMPLE_RATE);
        if dur > 0 { raw_ms.min(dur) } else { raw_ms }
    }

    pub fn speed(&self) -> PlaybackSpeed {
        self.speed
    }

    pub fn waveform(&self) -> Option<[u8; WAVEFORM_BARS]> {
        self.decoded.as_ref().map(|d| d.waveform)
    }

    pub fn snapshot(&self) -> PlayerSnapshot {
        PlayerSnapshot {
            state: self.state,
            position_ms: self.position_ms(),
            duration_ms: self.duration_ms(),
            speed: self.speed,
        }
    }

    pub fn set_speed(&mut self, speed: PlaybackSpeed) {
        self.speed = speed;
        self.stretcher.set_speed_percent(speed.percent());
    }

    pub fn pause(&mut self) -> Result<(), AudioError> {
        if self.state == PlayerState::Playing {
            if let Some(stream) = self.stream.as_mut() {
                stream.pause()?;
            }
            self.state = PlayerState::Paused;
        }
        Ok(())
    }

    pub fn resume(&mut self) -> Result<(), AudioError> {
        if self.state == PlayerState::Ended {
            self.seek_ms(0)?;
        }
        if matches!(self.state, PlayerState::Paused | PlayerState::Ready) {
            if let Some(stream) = self.stream.as_mut() {
                stream.resume()?;
            }
            self.state = PlayerState::Playing;
        }
        Ok(())
    }

    pub fn seek_ms(&mut self, ms: u64) -> Result<(), AudioError> {
        let Some(decoded) = self.decoded.as_ref() else {
            return Ok(());
        };
        let clamped_ms = if decoded.duration_ms > 0 {
            ms.min(decoded.duration_ms.saturating_sub(1))
        } else {
            0
        };
        let total_frames = decoded.pcm_stereo_f32.len() / 2;
        let target_frame = ((clamped_ms.saturating_mul(u64::from(OPUS_SAMPLE_RATE))) / 1000)
            .min(total_frames as u64);

        if let Some(stream) = self.stream.as_mut() {
            stream.reset()?;
            if self.state == PlayerState::Paused {
                stream.pause()?;
            }
        }

        self.source_frame_cursor = target_frame as usize;
        self.seek_base_frame = target_frame;
        self.rendered_src_frames = 0;
        self.stretcher.reset(target_frame);
        self.pending_out.clear();
        self.pending_out_offset = 0;
        self.tail_flushed = false;
        self.eos_drain_polls = 0;
        if self.state == PlayerState::Ended {
            self.state = PlayerState::Playing;
        }
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(mut stream) = self.stream.take() {
            let _ = stream.stop();
        }
        self.decoded = None;
        self.pending_out.clear();
        self.pending_out_offset = 0;
        self.source_frame_cursor = 0;
        self.seek_base_frame = 0;
        self.rendered_src_frames = 0;
        self.tail_flushed = false;
        self.eos_drain_polls = 0;
        self.state = PlayerState::Idle;
    }

    /// Pumps one playback slice through WSOLA into the active `AudioRenderStream`.
    /// Returns `true` when playback has newly transitioned to `PlayerState::Ended`.
    pub fn pump(&mut self) -> Result<bool, AudioError> {
        if self.state != PlayerState::Playing {
            return Ok(false);
        }
        let Some(decoded) = self.decoded.as_ref() else {
            self.state = PlayerState::Idle;
            return Ok(false);
        };

        let pcm = &decoded.pcm_stereo_f32;
        let total_frames = pcm.len() / 2;

        // Compact already-written pending samples.
        if self.pending_out_offset > 0 && self.pending_out_offset >= self.pending_out.len() {
            self.pending_out.clear();
            self.pending_out_offset = 0;
        } else if self.pending_out_offset >= WINDOW_FRAMES * 8 {
            self.pending_out.drain(..self.pending_out_offset);
            self.pending_out_offset = 0;
        }

        // Refill WSOLA output buffer up to a few blocks ahead.
        let target_pending_samples = WINDOW_FRAMES * 2 * 4;
        while self
            .pending_out
            .len()
            .saturating_sub(self.pending_out_offset)
            < target_pending_samples
            && !self.tail_flushed
        {
            if self.stretcher.can_emit(0, total_frames) {
                let emitted = self.stretcher.emit(pcm, 0, &mut self.pending_out);
                if emitted == 0 {
                    break;
                }
                self.source_frame_cursor =
                    (self.stretcher.source_frames_consumed() as usize).min(total_frames);
            } else {
                let consumed = (self.stretcher.source_frames_consumed() as usize).min(total_frames);
                let remaining = total_frames.saturating_sub(consumed);
                if remaining < LOOKAHEAD_FRAMES || self.stretcher.speed_percent() == 100 {
                    self.stretcher
                        .finish_tail(&pcm[consumed * 2..], &mut self.pending_out);
                    self.source_frame_cursor = total_frames;
                    self.tail_flushed = true;
                }
                break;
            }
        }

        // Write pending stretched frames to the audio render stream.
        if let Some(stream) = self.stream.as_mut() {
            let unread = &self.pending_out[self.pending_out_offset..];
            if !unread.is_empty() {
                let written_frames = stream.write_frames(unread)?;
                if written_frames > 0 {
                    self.pending_out_offset += written_frames * 2;
                    let src_advance = (written_frames as u64)
                        .saturating_mul(u64::from(self.speed.percent()))
                        / 100;
                    self.rendered_src_frames = self.rendered_src_frames.saturating_add(src_advance);
                }
            }

            if self.tail_flushed && self.pending_out_offset >= self.pending_out.len() {
                let padding = stream.padding_frames().unwrap_or(0);
                // Matches audio.zig EOS drain: wait up to 50 polls (~1000 ms @ 20 ms) for padding == 0.
                if padding == 0 || self.eos_drain_polls >= 50 {
                    self.state = PlayerState::Ended;
                    return Ok(true);
                }
                self.eos_drain_polls += 1;
            }
        }

        Ok(false)
    }

    /// Pumps synchronously until `PlayerState::Ended` (useful for deterministic tests).
    pub fn pump_to_end(&mut self) -> Result<usize, AudioError> {
        let mut steps = 0usize;
        while self.state == PlayerState::Playing {
            steps += 1;
            if self.pump()? {
                break;
            }
            if steps > 100_000 {
                break;
            }
        }
        Ok(steps)
    }
}

pub const PLAYBACK_POLL_INTERVAL: Duration = Duration::from_millis(8);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice_note::encode_voice_note;
    use postbox_platform::MockAudioBackend;

    fn sample_ogg(duration_secs: f32) -> Vec<u8> {
        let samples = (48_000.0 * duration_secs) as usize;
        let pcm: Vec<i16> = (0..samples)
            .map(|i| {
                let s = (2.0 * std::f32::consts::PI * 440.0 * (i as f32) / 48_000.0).sin();
                (s * 12_000.0) as i16
            })
            .collect();
        encode_voice_note(&pcm).unwrap().ogg_bytes
    }

    #[test]
    fn voice_player_plays_seeks_pauses_resumes_and_stretches_via_mock_backend() {
        let mock = MockAudioBackend::new();
        let ogg = sample_ogg(1.0);
        let mut player = VoicePlayer::new();

        // 1.0x full playback.
        let wave = player
            .load_bytes(&mock, &ogg, PlaybackSpeed::X1, 0)
            .unwrap();
        assert_eq!(*wave.iter().max().unwrap(), 100);
        assert_eq!(player.state(), PlayerState::Playing);
        assert_eq!(player.duration_ms(), 1000);

        player.pump().unwrap();
        player.pause().unwrap();
        assert_eq!(player.state(), PlayerState::Paused);
        let paused_pos = player.position_ms();
        player.pump().unwrap();
        assert_eq!(player.position_ms(), paused_pos);

        player.seek_ms(500).unwrap();
        assert_eq!(player.position_ms(), 500);
        player.resume().unwrap();
        assert_eq!(player.state(), PlayerState::Playing);
        player.pump_to_end().unwrap();
        assert_eq!(player.state(), PlayerState::Ended);

        // 2.0x full playback renders roughly half as many frames as 1.0x.
        let mock_1x = MockAudioBackend::new();
        let mut p1 = VoicePlayer::new();
        p1.load_bytes(&mock_1x, &ogg, PlaybackSpeed::X1, 0).unwrap();
        p1.pump_to_end().unwrap();
        let samples_1x = mock_1x.total_rendered_samples();

        let mock_2x = MockAudioBackend::new();
        let mut p2 = VoicePlayer::new();
        p2.load_bytes(&mock_2x, &ogg, PlaybackSpeed::X2, 0).unwrap();
        p2.pump_to_end().unwrap();
        let samples_2x = mock_2x.total_rendered_samples();

        let ratio = samples_1x as f64 / samples_2x as f64;
        assert!(
            (ratio - 2.0).abs() < 0.12,
            "expected ~2.0x render sample ratio, got {ratio} ({samples_1x} vs {samples_2x})"
        );
    }
}
