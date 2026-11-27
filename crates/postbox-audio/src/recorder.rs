//! Voice note recorder (`Ctrl+Shift+M`) driving `AudioBackend::open_capture` + `Resampler`
//! + `encode_voice_note`, enforcing `250 ms` min and `300 s` max duration bounds.

use crate::ogg::OPUS_SAMPLE_RATE;
use crate::voice_note::{
    AudioError, EncodedVoiceNote, Resampler, WAVEFORM_BARS, encode_voice_note,
};
use postbox_core::voice::{VOICE_MAX_DURATION_SECS, VOICE_MIN_DURATION_MS};
use postbox_platform::{AudioBackend, AudioCaptureStream};
use std::collections::VecDeque;
use std::time::Duration;
use thiserror::Error;

pub const RECORDING_POLL_INTERVAL: Duration = Duration::from_millis(10);
pub const RECORDING_TICK_INTERVAL: Duration = Duration::from_millis(100);
pub const MAX_RECORD_SAMPLES: usize =
    (VOICE_MAX_DURATION_SECS as usize) * (OPUS_SAMPLE_RATE as usize);
pub const MIN_RECORD_SAMPLES: usize =
    ((VOICE_MIN_DURATION_MS as usize) * (OPUS_SAMPLE_RATE as usize)) / 1000;

#[derive(Debug, Error)]
pub enum RecordError {
    #[error("Voice note too short")]
    TooShort,
    #[error("Could not start the microphone")]
    StartFailed(#[source] postbox_platform::PlatformError),
    #[error("Could not record; check the microphone")]
    CaptureFailed(#[source] postbox_platform::PlatformError),
    #[error("Could not encode voice note")]
    EncodeFailed(#[source] AudioError),
}

impl RecordError {
    pub fn user_message(&self) -> &'static str {
        match self {
            Self::TooShort => "Voice note too short",
            Self::StartFailed(_) => "Could not start the microphone",
            Self::CaptureFailed(_) | Self::EncodeFailed(_) => {
                "Could not record; check the microphone"
            }
        }
    }
}

/// Active voice recording session buffering resampled 48 kHz mono `i16` PCM and a live 64-bar meter.
pub struct VoiceRecorder {
    stream: Box<dyn AudioCaptureStream>,
    resampler: Resampler,
    pcm_48k: Vec<i16>,
    live_bars: VecDeque<u8>,
    reached_max: bool,
}

impl VoiceRecorder {
    pub fn start(
        backend: &dyn AudioBackend,
        preferred_mic_name: Option<&str>,
    ) -> Result<Self, RecordError> {
        let stream = backend
            .open_capture(preferred_mic_name)
            .map_err(RecordError::StartFailed)?;
        let resampler = Resampler::new(stream.format());
        Ok(Self {
            stream,
            resampler,
            pcm_48k: Vec::with_capacity(OPUS_SAMPLE_RATE as usize * 4),
            live_bars: VecDeque::with_capacity(WAVEFORM_BARS),
            reached_max: false,
        })
    }

    pub fn elapsed_ms(&self) -> u64 {
        (self.pcm_48k.len() as u64).saturating_mul(1000) / u64::from(OPUS_SAMPLE_RATE)
    }

    pub fn sample_count(&self) -> usize {
        self.pcm_48k.len()
    }

    pub fn reached_max_duration(&self) -> bool {
        self.reached_max
    }

    pub fn live_bars(&self) -> Vec<u8> {
        self.live_bars.iter().copied().collect()
    }

    /// Drains all currently available capture packets from the microphone stream.
    /// Returns `true` if any new samples were captured.
    pub fn poll_packets(&mut self) -> Result<bool, RecordError> {
        if self.reached_max {
            return Ok(false);
        }
        let before = self.pcm_48k.len();
        while let Some(pkt) = self
            .stream
            .read_packet()
            .map_err(RecordError::CaptureFailed)?
        {
            let start_idx = self.pcm_48k.len();
            if pkt.silent {
                let silent_bytes = if pkt.frames > 0 {
                    pkt.frames
                        .saturating_mul(self.stream.format().block_align())
                } else {
                    pkt.data.len()
                };
                if silent_bytes == 0 {
                    continue;
                }
                let zeros = vec![0u8; silent_bytes];
                self.resampler.push_bytes(&zeros, &mut self.pcm_48k);
            } else {
                if pkt.data.is_empty() {
                    continue;
                }
                self.resampler.push_bytes(&pkt.data, &mut self.pcm_48k);
            }
            if self.pcm_48k.len() >= MAX_RECORD_SAMPLES {
                self.pcm_48k.truncate(MAX_RECORD_SAMPLES);
                self.reached_max = true;
            }
            let new_slice = &self.pcm_48k[start_idx..];
            if !new_slice.is_empty() {
                let peak = new_slice
                    .iter()
                    .map(|&s| i32::from(s).unsigned_abs())
                    .max()
                    .unwrap_or(0);
                // Matches voice_capture.zig:251-253: min(100, peak * 100 / 16384)
                let level = ((peak.saturating_mul(100)) / 16_384).min(100) as u8;
                if self.live_bars.len() >= WAVEFORM_BARS {
                    self.live_bars.pop_front();
                }
                self.live_bars.push_back(level);
            }
            if self.reached_max {
                break;
            }
        }
        Ok(self.pcm_48k.len() > before)
    }

    /// Stops capture cleanly, enforces `>= 250 ms` minimum duration, and encodes Ogg Opus.
    pub fn finish(mut self) -> Result<EncodedVoiceNote, RecordError> {
        let _ = self.poll_packets();
        let _ = self.stream.stop();
        if self.pcm_48k.len() < MIN_RECORD_SAMPLES {
            return Err(RecordError::TooShort);
        }
        encode_voice_note(&self.pcm_48k).map_err(RecordError::EncodeFailed)
    }

    /// Stops capture and discards all recorded audio.
    pub fn cancel(mut self) {
        let _ = self.stream.stop();
        self.pcm_48k.clear();
        self.live_bars.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice_note::decode_ogg_opus;
    use postbox_platform::{CaptureFormat, MockAudioBackend};

    #[test]
    fn recorder_rejects_under_250ms_and_encodes_normal_voice_note() {
        let mock = MockAudioBackend::new();
        mock.set_capture_format(CaptureFormat {
            sample_rate: 48_000,
            channels: 1,
            is_float: true,
            bits_per_sample: 32,
        });

        // 100 ms -> 4800 samples < 12000 -> TooShort ("Voice note too short").
        let short_samples = vec![0.25f32; 4_800];
        mock.push_capture_f32(&short_samples, 1, 960);
        let mut rec_short = VoiceRecorder::start(&mock, None).unwrap();
        rec_short.poll_packets().unwrap();
        assert!(rec_short.elapsed_ms() < VOICE_MIN_DURATION_MS);
        let err = rec_short.finish().unwrap_err();
        assert_eq!(err.user_message(), "Voice note too short");

        // 600 ms -> 28800 samples -> valid Ogg Opus note with 64-bar waveform.
        let mut normal_samples = Vec::with_capacity(28_800);
        for i in 0..28_800 {
            normal_samples
                .push((2.0 * std::f32::consts::PI * 350.0 * (i as f32) / 48_000.0).sin() * 0.5);
        }
        mock.push_capture_f32(&normal_samples, 1, 960);
        let mut rec = VoiceRecorder::start(&mock, Some("USB Studio Mic")).unwrap();
        assert!(rec.poll_packets().unwrap());
        assert_eq!(rec.elapsed_ms(), 600);
        assert!(!rec.live_bars().is_empty());
        assert_eq!(mock.last_capture_mic().as_deref(), Some("USB Studio Mic"));

        let encoded = rec.finish().unwrap();
        assert_eq!(encoded.duration_secs, 1);
        assert_eq!(*encoded.waveform.iter().max().unwrap(), 100);
        let decoded = decode_ogg_opus(&encoded.ogg_bytes).unwrap();
        assert_eq!(decoded.duration_ms, 600);
    }

    #[test]
    fn recorder_honors_silent_flag_with_nonzero_garbage_and_empty_data() {
        let mock = MockAudioBackend::new();
        mock.set_capture_format(CaptureFormat {
            sample_rate: 48_000,
            channels: 1,
            is_float: true,
            bits_per_sample: 32,
        });

        // Case A: silent == true with loud non-zero garbage in pkt.data -> must produce pure silence.
        let mut noisy_bytes = Vec::with_capacity(960 * 4);
        for _ in 0..960 {
            noisy_bytes.extend_from_slice(&0.9f32.to_le_bytes());
        }
        for _ in 0..20 {
            mock.push_capture_packet(postbox_platform::CapturePacket {
                data: noisy_bytes.clone(),
                frames: 960,
                silent: true,
            });
        }
        let mut rec_noisy = VoiceRecorder::start(&mock, None).unwrap();
        assert!(rec_noisy.poll_packets().unwrap());
        assert_eq!(rec_noisy.sample_count(), 19_200);
        assert_eq!(rec_noisy.elapsed_ms(), 400);
        assert!(rec_noisy.live_bars().iter().all(|&b| b == 0));
        let encoded_noisy = rec_noisy.finish().unwrap();
        assert_eq!(*encoded_noisy.waveform.iter().max().unwrap(), 0);

        // Case B: silent == true with empty pkt.data and frames == 960 -> must preserve silent duration.
        for _ in 0..20 {
            mock.push_capture_packet(postbox_platform::CapturePacket {
                data: Vec::new(),
                frames: 960,
                silent: true,
            });
        }
        let mut rec_empty = VoiceRecorder::start(&mock, None).unwrap();
        assert!(rec_empty.poll_packets().unwrap());
        assert_eq!(rec_empty.sample_count(), 19_200);
        assert_eq!(rec_empty.elapsed_ms(), 400);
        assert!(rec_empty.live_bars().iter().all(|&b| b == 0));
        let encoded_empty = rec_empty.finish().unwrap();
        assert_eq!(encoded_empty.duration_ms, 400);
        assert_eq!(*encoded_empty.waveform.iter().max().unwrap(), 0);
    }
}
