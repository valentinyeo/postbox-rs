//! Voice note resampler, Opus encoder/decoder, 64-bar waveform extraction, and sent-voice echo FIFO.

use crate::ogg::{
    DEFAULT_VENDOR, OPUS_FRAME_SAMPLES, OPUS_SAMPLE_RATE, OggError, OggOpusMuxer, parse_ogg_opus,
};
use opus_pure::{
    Application, MAX_PACKET_BYTES, MAX_PACKET_SAMPLES, OpusDecoder, OpusEncoder, RateControl,
    Signal,
};
use postbox_core::ChatKey;
use postbox_core::voice::VOICE_SENT_FIFO_CAP;
use postbox_platform::CaptureFormat;
use std::collections::VecDeque;
use std::path::PathBuf;
use thiserror::Error;

pub const VOICE_BITRATE_BPS: i32 = 24_000;
pub const VOICE_COMPLEXITY: i32 = 5;
pub const WAVEFORM_BARS: usize = 64;

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("empty audio input")]
    Empty,
    #[error("ogg container error: {0}")]
    Ogg(#[from] OggError),
    #[error("opus codec error: {0}")]
    Opus(String),
    #[error("platform audio error: {0}")]
    Platform(#[from] postbox_platform::PlatformError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedVoiceNote {
    pub ogg_bytes: Vec<u8>,
    pub duration_secs: u32,
    pub duration_ms: u64,
    pub waveform: [u8; WAVEFORM_BARS],
    pub pre_skip: u16,
    pub pcm_samples: usize,
}

#[derive(Debug, Clone)]
pub struct DecodedVoiceNote {
    pub pcm_stereo_f32: Vec<f32>,
    pub duration_ms: u64,
    pub waveform: [u8; WAVEFORM_BARS],
    pub channels: u8,
    pub pre_skip: u16,
}

/// Linear-interpolation resampler converting arbitrary capture format (f32 or PCM16, N channels,
/// arbitrary sample rate) to mono `i16` at a configurable output sample rate (48 kHz for Opus
/// voice notes, 16 kHz for live Deepgram dictation).
#[derive(Debug, Clone)]
pub struct Resampler {
    in_rate: u32,
    out_rate: u32,
    channels: usize,
    is_float: bool,
    bits_per_sample: u16,
    position: f64,
    previous: f32,
}

impl Resampler {
    pub fn new(format: CaptureFormat) -> Self {
        Self::with_output_rate(format, OPUS_SAMPLE_RATE)
    }

    pub fn with_output_rate(format: CaptureFormat, out_rate: u32) -> Self {
        Self {
            in_rate: format.sample_rate.max(1),
            out_rate: out_rate.max(1),
            channels: usize::from(format.channels.max(1)),
            is_float: format.is_float,
            bits_per_sample: format.bits_per_sample,
            position: 0.0,
            previous: 0.0,
        }
    }

    pub fn push_bytes(&mut self, data: &[u8], out: &mut Vec<i16>) {
        let bytes_per_sample = usize::from(self.bits_per_sample / 8).max(1);
        let frame_bytes = bytes_per_sample * self.channels;
        if frame_bytes == 0 {
            return;
        }
        let frames = data.len() / frame_bytes;
        if frames == 0 {
            return;
        }
        if self.in_rate == self.out_rate
            && self.channels == 1
            && !self.is_float
            && self.bits_per_sample == 16
        {
            out.reserve(frames);
            for frame in 0..frames {
                let off = frame * 2;
                let sample = i16::from_le_bytes(data[off..off + 2].try_into().unwrap());
                out.push(sample);
            }
            let last_off = (frames - 1) * 2;
            let last = i16::from_le_bytes(data[last_off..last_off + 2].try_into().unwrap());
            self.previous = (f64::from(last) / 32768.0).clamp(-1.0, 1.0) as f32;
            self.position = 0.0;
            return;
        }
        let step = f64::from(self.in_rate) / f64::from(self.out_rate);
        while self.position < frames as f64 {
            let whole = self.position.floor() as usize;
            let frac = (self.position - whole as f64) as f32;
            let a = if whole == 0 {
                self.previous
            } else {
                self.mono_at(data, frame_bytes, whole - 1)
            };
            let b = self.mono_at(data, frame_bytes, whole);
            let value = a + (b - a) * frac;
            out.push(float_to_i16(value));
            self.position += step;
        }
        self.previous = self.mono_at(data, frame_bytes, frames - 1);
        self.position -= frames as f64;
    }

    fn mono_at(&self, data: &[u8], frame_bytes: usize, frame: usize) -> f32 {
        let base = frame * frame_bytes;
        let mut sum = 0.0f64;
        if self.is_float && self.bits_per_sample == 32 {
            for c in 0..self.channels {
                let off = base + c * 4;
                let sample = f32::from_le_bytes(data[off..off + 4].try_into().unwrap());
                if sample.is_finite() {
                    sum += f64::from(sample).clamp(-1.0, 1.0);
                }
            }
        } else if !self.is_float && self.bits_per_sample == 16 {
            for c in 0..self.channels {
                let off = base + c * 2;
                let sample = i16::from_le_bytes(data[off..off + 2].try_into().unwrap());
                sum += (f64::from(sample) / 32768.0).clamp(-1.0, 1.0);
            }
        }
        (sum / self.channels as f64) as f32
    }
}

fn float_to_i16(value: f32) -> i16 {
    let clamped = value.clamp(-1.0, 1.0);
    (clamped * 32767.0).round() as i16
}

/// Ceiling seconds with minimum 1s (`10 -> 1`, `48000 -> 1`, `48001 -> 2`).
pub const fn duration_seconds(samples_48k: usize) -> u32 {
    if samples_48k == 0 {
        return 1;
    }
    let secs = samples_48k.div_ceil(OPUS_SAMPLE_RATE as usize);
    if secs == 0 { 1 } else { secs as u32 }
}

/// Peak-normalized 64-bar waveform (`0..=100`, loudest bar = `100`) from mono i16 PCM.
pub fn waveform_i16(pcm: &[i16]) -> [u8; WAVEFORM_BARS] {
    if pcm.is_empty() {
        return [0u8; WAVEFORM_BARS];
    }
    let mut levels = [0.0f64; WAVEFORM_BARS];
    let mut peak = 0.0f64;
    for (bar, slot) in levels.iter_mut().enumerate() {
        let from = bar * pcm.len() / WAVEFORM_BARS;
        let to = ((bar + 1) * pcm.len() / WAVEFORM_BARS).max((from + 1).min(pcm.len()));
        if to > from {
            let sum: f64 = pcm[from..to].iter().map(|&s| f64::from(s).abs()).sum();
            let avg = sum / (to - from) as f64;
            *slot = avg;
            if avg > peak {
                peak = avg;
            }
        }
    }
    normalize_waveform_levels(&levels, peak)
}

/// Peak-normalized 64-bar waveform (`0..=100`) from interleaved stereo f32 PCM.
pub fn waveform_stereo_f32(pcm_stereo: &[f32]) -> [u8; WAVEFORM_BARS] {
    let frames = pcm_stereo.len() / 2;
    if frames == 0 {
        return [0u8; WAVEFORM_BARS];
    }
    let mut levels = [0.0f64; WAVEFORM_BARS];
    let mut peak = 0.0f64;
    for (bar, slot) in levels.iter_mut().enumerate() {
        let from = bar * frames / WAVEFORM_BARS;
        let to = ((bar + 1) * frames / WAVEFORM_BARS).max((from + 1).min(frames));
        if to > from {
            let mut sum = 0.0f64;
            for f in from..to {
                let mono = 0.5 * (f64::from(pcm_stereo[f * 2]) + f64::from(pcm_stereo[f * 2 + 1]));
                sum += mono.abs();
            }
            let avg = sum / (to - from) as f64;
            *slot = avg;
            if avg > peak {
                peak = avg;
            }
        }
    }
    normalize_waveform_levels(&levels, peak)
}

fn normalize_waveform_levels(levels: &[f64; WAVEFORM_BARS], peak: f64) -> [u8; WAVEFORM_BARS] {
    let mut out = [0u8; WAVEFORM_BARS];
    if peak <= 0.0 {
        return out;
    }
    for (dst, &level) in out.iter_mut().zip(levels.iter()) {
        *dst = ((level / peak) * 100.0).round().clamp(0.0, 100.0) as u8;
    }
    out
}

/// Encodes 48 kHz mono `i16` PCM into an RFC 7845 Ogg Opus voice note
/// (VOIP, 24 kbps VBR, complexity 5, Signal::Voice, 20 ms / 960-sample frames).
pub fn encode_voice_note(pcm: &[i16]) -> Result<EncodedVoiceNote, AudioError> {
    if pcm.is_empty() {
        return Err(AudioError::Empty);
    }

    let mut enc = OpusEncoder::new(OPUS_SAMPLE_RATE as i32, 1, Application::Voip)
        .map_err(|e| AudioError::Opus(format!("{e:?}")))?;
    enc.bitrate_bps = VOICE_BITRATE_BPS;
    enc.rate_control = RateControl::Vbr;
    enc.complexity = VOICE_COMPLEXITY;
    enc.signal_type = Some(Signal::Voice);

    let pre_skip = u16::try_from(enc.lookahead()).unwrap_or(312);
    let serial = 0x5741_5a49u32 ^ (pcm.len() as u32);
    let mut muxer = OggOpusMuxer::new(serial, 1, pre_skip, DEFAULT_VENDOR);

    let total_samples = pcm.len() + usize::from(pre_skip);
    let total_frames = total_samples.div_ceil(OPUS_FRAME_SAMPLES);
    let mut frame_buf = [0i16; OPUS_FRAME_SAMPLES];
    let mut pkt_buf = vec![0u8; MAX_PACKET_BYTES];

    for frame_idx in 0..total_frames {
        frame_buf.fill(0);
        let from = frame_idx * OPUS_FRAME_SAMPLES;
        if from < pcm.len() {
            let copy_len = (pcm.len() - from).min(OPUS_FRAME_SAMPLES);
            frame_buf[..copy_len].copy_from_slice(&pcm[from..from + copy_len]);
        }
        let encoded_len = enc
            .encode_s16(&frame_buf, OPUS_FRAME_SAMPLES, &mut pkt_buf)
            .map_err(|e| AudioError::Opus(format!("{e:?}")))?;
        let granule = ((frame_idx + 1) * OPUS_FRAME_SAMPLES).min(total_samples) as u64;
        muxer.push_packet(&pkt_buf[..encoded_len], granule);
    }

    let ogg_bytes = muxer.finish(total_samples as u64);
    let duration_secs = duration_seconds(pcm.len());
    let duration_ms = (pcm.len() as u64).saturating_mul(1000) / u64::from(OPUS_SAMPLE_RATE);
    let waveform = waveform_i16(pcm);

    Ok(EncodedVoiceNote {
        ogg_bytes,
        duration_secs,
        duration_ms,
        waveform,
        pre_skip,
        pcm_samples: pcm.len(),
    })
}

/// Decodes an Ogg Opus byte stream to 48 kHz interleaved stereo `f32`, honoring `pre_skip`
/// and `output_gain`, and extracts its 64-bar waveform and duration.
pub fn decode_ogg_opus(data: &[u8]) -> Result<DecodedVoiceNote, AudioError> {
    let parsed = parse_ogg_opus(data)?;
    let mut dec = OpusDecoder::new(OPUS_SAMPLE_RATE as i32, 2)
        .map_err(|e| AudioError::Opus(format!("{e:?}")))?;
    dec.gain_q8 = i32::from(parsed.head.output_gain);

    let max_possible_frames = parsed.packets.len().saturating_mul(MAX_PACKET_SAMPLES);
    let hint_frames = usize::try_from(parsed.duration_samples)
        .unwrap_or(max_possible_frames)
        .min(max_possible_frames)
        .min(OPUS_SAMPLE_RATE as usize * 60);
    let mut out_f32 = Vec::with_capacity(hint_frames.saturating_mul(2));
    let mut decode_buf = vec![0.0f32; MAX_PACKET_SAMPLES * 2];
    let mut pending_skip = usize::from(parsed.head.pre_skip);

    for pkt in &parsed.packets {
        let frames = dec
            .decode(&pkt.data, MAX_PACKET_SAMPLES, &mut decode_buf)
            .map_err(|e| AudioError::Opus(format!("{e:?}")))?;
        if pending_skip >= frames {
            pending_skip -= frames;
            continue;
        }
        let start_frame = pending_skip;
        pending_skip = 0;
        out_f32.extend_from_slice(&decode_buf[start_frame * 2..frames * 2]);
    }

    // Trim any encoder flush padding past `duration_samples` if final granule was smaller.
    if let Some(expected_samples) = usize::try_from(parsed.duration_samples)
        .ok()
        .and_then(|s| s.checked_mul(2))
        && out_f32.len() > expected_samples
    {
        out_f32.truncate(expected_samples);
    }

    let decoded_frames = out_f32.len() / 2;
    let duration_ms = (decoded_frames as u64).saturating_mul(1000) / u64::from(OPUS_SAMPLE_RATE);
    let waveform = waveform_stereo_f32(&out_f32);

    Ok(DecodedVoiceNote {
        pcm_stereo_f32: out_f32,
        duration_ms,
        waveform,
        channels: parsed.head.channels,
        pre_skip: parsed.head.pre_skip,
    })
}

/// Entry in the 32-slot FIFO used to echo-match own sent voice notes with local staged audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceSentEntry {
    pub chat: ChatKey,
    pub local_path: PathBuf,
    pub sha256: String,
    pub queued_unix: i64,
    pub duration_secs: u32,
    pub waveform: [u8; WAVEFORM_BARS],
    pub matched_id: Option<String>,
    pub seq: u64,
}

/// Cap-32 FIFO echo matcher matching Zig `src/voice_sent.zig` (`row_unix >= queued_unix - 5`,
/// preventing duplicate server message claims).
#[derive(Debug, Default, Clone)]
pub struct VoiceSentFifo {
    entries: VecDeque<VoiceSentEntry>,
    next_seq: u64,
}

impl VoiceSentFifo {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(
        &mut self,
        chat: ChatKey,
        local_path: PathBuf,
        sha256: String,
        queued_unix: i64,
        duration_secs: u32,
        waveform: [u8; WAVEFORM_BARS],
    ) -> u64 {
        self.next_seq += 1;
        let seq = self.next_seq;
        if self.entries.len() >= VOICE_SENT_FIFO_CAP {
            self.entries.pop_front();
        }
        self.entries.push_back(VoiceSentEntry {
            chat,
            local_path,
            sha256,
            queued_unix,
            duration_secs,
            waveform,
            matched_id: None,
            seq,
        });
        seq
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &VecDeque<VoiceSentEntry> {
        &self.entries
    }

    pub fn claims_id(&self, chat: &ChatKey, msg_id: &str) -> bool {
        self.entries
            .iter()
            .any(|e| &e.chat == chat && e.matched_id.as_deref() == Some(msg_id))
    }

    /// Attempts to match an incoming echoed message row against an existing entry in the FIFO.
    /// Returns the matched `(local_path, sha256, waveform)` if matched.
    pub fn match_echo_row(
        &mut self,
        chat: &ChatKey,
        msg_id: &str,
        from_me: bool,
        is_voice_or_audio: bool,
        row_unix: i64,
    ) -> Option<(PathBuf, String, [u8; WAVEFORM_BARS])> {
        if !from_me || !is_voice_or_audio || msg_id.is_empty() {
            return None;
        }
        // First check if already matched to this exact msg_id.
        if let Some(existing) = self
            .entries
            .iter()
            .find(|e| &e.chat == chat && e.matched_id.as_deref() == Some(msg_id))
        {
            return Some((
                existing.local_path.clone(),
                existing.sha256.clone(),
                existing.waveform,
            ));
        }
        // Never allow two entries to claim the same server message ID.
        if self.claims_id(chat, msg_id) {
            return None;
        }
        // Claim oldest unclaimed entry for this chat within 5-second clock skew tolerance.
        for entry in &mut self.entries {
            if &entry.chat == chat
                && entry.matched_id.is_none()
                && row_unix >= entry.queued_unix.saturating_sub(5)
            {
                entry.matched_id = Some(msg_id.to_owned());
                return Some((
                    entry.local_path.clone(),
                    entry.sha256.clone(),
                    entry.waveform,
                ));
            }
        }
        None
    }

    /// Marks an entry matched explicitly (e.g. when the Hub outbox ACK returns `msg_id`).
    pub fn bind_acked_id(&mut self, chat: &ChatKey, sha256: &str, msg_id: &str) -> bool {
        if self.claims_id(chat, msg_id) {
            return false;
        }
        for entry in &mut self.entries {
            if &entry.chat == chat && entry.matched_id.is_none() && entry.sha256 == sha256 {
                entry.matched_id = Some(msg_id.to_owned());
                return true;
            }
        }
        false
    }
}

pub fn is_audio_placeholder(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed == "Sent audio" || trimmed == "[Audio]"
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::Provider;

    fn sine_48k_mono_i16(duration_secs: f32, freq_hz: f32) -> Vec<i16> {
        let samples = (48_000.0 * duration_secs) as usize;
        (0..samples)
            .map(|i| {
                let s = (2.0 * std::f32::consts::PI * freq_hz * (i as f32) / 48_000.0).sin();
                (s * 14_000.0) as i16
            })
            .collect()
    }

    #[test]
    fn opus_encode_decode_round_trip_preserves_signal_and_duration() {
        let pcm = sine_48k_mono_i16(1.0, 440.0);
        let encoded = encode_voice_note(&pcm).unwrap();
        assert_eq!(encoded.duration_secs, 1);
        assert_eq!(encoded.duration_ms, 1000);
        assert_eq!(encoded.waveform.len(), WAVEFORM_BARS);
        assert_eq!(*encoded.waveform.iter().max().unwrap(), 100);
        assert!(encoded.ogg_bytes.starts_with(b"OggS"));

        let decoded = decode_ogg_opus(&encoded.ogg_bytes).unwrap();
        assert_eq!(decoded.channels, 1);
        assert_eq!(decoded.duration_ms, 1000);
        let decoded_frames = decoded.pcm_stereo_f32.len() / 2;
        assert_eq!(decoded_frames, 48_000);

        // Verify non-trivial RMS energy on the round-tripped sine wave.
        let energy: f64 = decoded
            .pcm_stereo_f32
            .iter()
            .step_by(2)
            .map(|&s| f64::from(s) * f64::from(s))
            .sum();
        let rms = (energy / decoded_frames as f64).sqrt();
        assert!(rms > 0.20, "expected strong round-trip RMS, got {rms}");
    }

    #[test]
    fn opus_round_trip_on_speech_like_harmonic_signal() {
        let samples = 48_000;
        let pcm: Vec<i16> = (0..samples)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                let env = (2.0 * std::f32::consts::PI * 3.0 * t).sin().abs();
                let sig = 0.5 * (2.0 * std::f32::consts::PI * 180.0 * t).sin()
                    + 0.3 * (2.0 * std::f32::consts::PI * 540.0 * t).sin()
                    + 0.2 * (2.0 * std::f32::consts::PI * 1080.0 * t).sin();
                (env * sig * 18_000.0) as i16
            })
            .collect();

        let encoded = encode_voice_note(&pcm).unwrap();
        let decoded = decode_ogg_opus(&encoded.ogg_bytes).unwrap();
        assert_eq!(decoded.pcm_stereo_f32.len() / 2, samples);
        assert_eq!(*decoded.waveform.iter().max().unwrap(), 100);
    }

    #[test]
    fn resampler_handles_44100_stereo_f32_and_16000_mono_pcm16() {
        // 44.1 kHz stereo f32 -> 1 second = 44100 frames -> ~48000 samples @ 48 kHz.
        let mut r44 = Resampler::new(CaptureFormat {
            sample_rate: 44_100,
            channels: 2,
            is_float: true,
            bits_per_sample: 32,
        });
        let mut bytes44 = Vec::with_capacity(44_100 * 8);
        for i in 0..44_100 {
            let s = (2.0 * std::f32::consts::PI * 440.0 * (i as f32) / 44_100.0).sin() * 0.5;
            bytes44.extend_from_slice(&s.to_le_bytes());
            bytes44.extend_from_slice(&s.to_le_bytes());
        }
        let mut out44 = Vec::new();
        r44.push_bytes(&bytes44[..bytes44.len() / 2], &mut out44);
        r44.push_bytes(&bytes44[bytes44.len() / 2..], &mut out44);
        assert!((out44.len() as isize - 48_000).abs() <= 4);

        // 16 kHz mono PCM16 -> 1 second = 16000 frames -> 48000 samples @ 48 kHz.
        let mut r16 = Resampler::new(CaptureFormat {
            sample_rate: 16_000,
            channels: 1,
            is_float: false,
            bits_per_sample: 16,
        });
        let mut bytes16 = Vec::with_capacity(16_000 * 2);
        for i in 0..16_000 {
            let s = ((2.0 * std::f32::consts::PI * 300.0 * (i as f32) / 16_000.0).sin() * 12_000.0)
                as i16;
            bytes16.extend_from_slice(&s.to_le_bytes());
        }
        let mut out16 = Vec::new();
        r16.push_bytes(&bytes16, &mut out16);
        assert!((out16.len() as isize - 48_000).abs() <= 4);
    }

    #[test]
    fn duration_seconds_uses_ceiling_min_one() {
        assert_eq!(duration_seconds(0), 1);
        assert_eq!(duration_seconds(10), 1);
        assert_eq!(duration_seconds(48_000), 1);
        assert_eq!(duration_seconds(48_001), 2);
        assert_eq!(duration_seconds(96_000), 2);
    }

    #[test]
    fn voice_sent_fifo_echo_matches_with_5s_skew_and_cap_32() {
        let mut fifo = VoiceSentFifo::new();
        let chat = ChatKey::new(Provider::Whatsapp, "main", "1@s.whatsapp.net");
        let wave = [50u8; WAVEFORM_BARS];

        for i in 0..35 {
            fifo.push(
                chat.clone(),
                PathBuf::from(format!("/tmp/v-{i}.ogg")),
                format!("sha-{i}"),
                1000 + i as i64,
                3,
                wave,
            );
        }
        assert_eq!(fifo.len(), VOICE_SENT_FIFO_CAP);
        // Entry 0..2 evicted; oldest remaining is entry 3 (queued_unix = 1003).
        // Row at 997 (< 1003 - 5 = 998) does NOT match.
        assert!(
            fifo.match_echo_row(&chat, "srv-1", true, true, 997)
                .is_none()
        );
        // Row at 998 (>= 1003 - 5) matches entry 3.
        let m1 = fifo
            .match_echo_row(&chat, "srv-1", true, true, 998)
            .unwrap();
        assert_eq!(m1.0, PathBuf::from("/tmp/v-3.ogg"));
        // Same srv-1 returns the already-matched entry without double-claiming entry 4.
        let m1_again = fifo
            .match_echo_row(&chat, "srv-1", true, true, 998)
            .unwrap();
        assert_eq!(m1_again.0, PathBuf::from("/tmp/v-3.ogg"));
        // Next server ID claims entry 4.
        let m2 = fifo
            .match_echo_row(&chat, "srv-2", true, true, 1004)
            .unwrap();
        assert_eq!(m2.0, PathBuf::from("/tmp/v-4.ogg"));
    }

    #[test]
    fn resampler_clamps_stereo_f32_max_without_nan_overflow() {
        let mut resampler = Resampler::new(CaptureFormat {
            sample_rate: 48_000,
            channels: 2,
            is_float: true,
            bits_per_sample: 32,
        });
        let mut bytes = Vec::new();
        for _ in 0..4 {
            bytes.extend_from_slice(&f32::MAX.to_le_bytes());
            bytes.extend_from_slice(&f32::MAX.to_le_bytes());
        }
        let mut out = Vec::new();
        resampler.push_bytes(&bytes, &mut out);
        assert_eq!(out, vec![0, i16::MAX, i16::MAX, i16::MAX]);
    }

    #[test]
    fn decode_ogg_opus_bounds_allocation_and_duration_on_crafted_granules() {
        // Encode a single valid 20 ms (960-sample) Opus frame.
        let mut enc = OpusEncoder::new(OPUS_SAMPLE_RATE as i32, 1, Application::Voip).unwrap();
        let pcm_frame = sine_48k_mono_i16(0.02, 440.0);
        let mut pkt_buf = vec![0u8; MAX_PACKET_BYTES];
        let pkt_len = enc
            .encode_s16(&pcm_frame, OPUS_FRAME_SAMPLES, &mut pkt_buf)
            .unwrap();
        let pkt = &pkt_buf[..pkt_len];

        let pre_skip = 312u16;
        let expected_frames = OPUS_FRAME_SAMPLES - usize::from(pre_skip);
        let expected_duration_ms =
            (expected_frames as u64).saturating_mul(1000) / u64::from(OPUS_SAMPLE_RATE);
        assert_eq!(expected_duration_ms, 13);

        for crafted_granule in [
            u64::MAX - 1,
            0x1000_0000_0000_0000,
            48_000 * 600 + u64::from(pre_skip),
        ] {
            let mut muxer = OggOpusMuxer::new(0xBEEF_CAFE, 1, pre_skip, DEFAULT_VENDOR);
            muxer.push_packet(pkt, crafted_granule);
            let ogg = muxer.finish(crafted_granule);

            let decoded = decode_ogg_opus(&ogg).unwrap();
            assert_eq!(decoded.pcm_stereo_f32.len(), expected_frames * 2);
            assert!(decoded.pcm_stereo_f32.len() <= OPUS_FRAME_SAMPLES * 2);
            assert!(
                decoded.pcm_stereo_f32.capacity() <= MAX_PACKET_SAMPLES * 2,
                "expected bounded capacity for granule {crafted_granule:#x}, got {}",
                decoded.pcm_stereo_f32.capacity()
            );
            assert!(
                decoded.pcm_stereo_f32.capacity() * std::mem::size_of::<f32>() < 1024 * 1024,
                "capacity exceeded 1 MiB for granule {crafted_granule:#x}"
            );
            assert_eq!(decoded.duration_ms, expected_duration_ms);
        }
    }
}
