//! Safe video boundary (Milestone 6): Windows Media Foundation + Intel Arc DXVA behind two
//! narrow traits, with a deterministic in-memory mock so every video code path is testable on
//! Linux CI without a GPU, a DLL, or a decoder.
//!
//! Everything here is pure Rust: the `unsafe` FFI lives only in `crate::win::video`, so the
//! workspace-wide `unsafe_code = "deny"` lint stays intact everywhere else.

use crate::PlatformError;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Seek target for the still thumbnail: the frame at or after ~1 s is far more representative
/// than a black fade-in first frame (`mf_lazy.zig` opens straight at the start).
pub const THUMBNAIL_SEEK_MS: u64 = 1_000;

/// Frames per second the mock decoder emits (23.976 rounded so timestamps stay integral).
pub const MOCK_FPS: u64 = 24;
/// Length of one mock PCM audio chunk in milliseconds.
pub const MOCK_AUDIO_CHUNK_MS: u64 = 20;
/// Mock audio sample rate.
pub const MOCK_SAMPLE_RATE: u32 = 48_000;
/// Mock audio channel count.
pub const MOCK_CHANNELS: u16 = 2;

/// Decoded video properties shared by the thumbnail and playback paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoMetadata {
    pub width: u32,
    pub height: u32,
    /// Total duration, `0` when the container does not report one.
    pub duration_ms: u64,
    pub has_audio: bool,
    /// True only when the Intel Arc DXVA / D3D11 device manager produced the frames.
    pub hardware_accelerated: bool,
}

impl VideoMetadata {
    /// Whether the file can actually be played (a zero-sized frame means the decode failed).
    pub const fn is_decodable(&self) -> bool {
        self.width > 0 && self.height > 0
    }
}

/// One decoded RGBA8888 frame at presentation timestamp `pts_ms`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub pts_ms: u64,
    pub rgba: Vec<u8>,
}

impl VideoFrame {
    pub fn new(width: u32, height: u32, pts_ms: u64, rgba: Vec<u8>) -> Option<Self> {
        let expected = usize::try_from(u64::from(width) * u64::from(height) * 4).ok()?;
        if width == 0 || height == 0 || rgba.len() != expected {
            return None;
        }
        Some(Self {
            width,
            height,
            pts_ms,
            rgba,
        })
    }

    pub fn byte_size(&self) -> usize {
        self.rgba.len()
    }

    /// Converts RGBA8888 to the BGRA8888 layout GPUI uploads.
    pub fn to_bgra(&self) -> Vec<u8> {
        let mut out = self.rgba.clone();
        crate::video::rgba_to_bgra_in_place(&mut out);
        out
    }
}

/// Swaps red and blue in place (RGBA8888 <-> BGRA8888).
pub fn rgba_to_bgra_in_place(pixels: &mut [u8]) {
    for px in pixels.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
}

/// One interleaved float32 PCM chunk decoded from the video's audio stream.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoAudioChunk {
    pub pts_ms: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub pcm_f32: Vec<f32>,
}

impl VideoAudioChunk {
    pub fn frames(&self) -> usize {
        self.pcm_f32.len() / usize::from(self.channels.max(1))
    }

    /// How long this chunk plays for, used to advance the audio clock.
    pub fn duration_ms(&self) -> u64 {
        let frames = self.frames() as u64;
        if self.sample_rate == 0 {
            return 0;
        }
        frames.saturating_mul(1_000) / u64::from(self.sample_rate)
    }
}

/// An open, seekable video file streaming frames and PCM chunks.
pub trait VideoPlaybackSession {
    fn metadata(&self) -> VideoMetadata;
    /// Next frame at or after the current position, `None` at end of stream.
    fn next_video_frame(&mut self) -> Result<Option<VideoFrame>, PlatformError>;
    /// Next PCM chunk at or after the current position, `None` when the file has no audio.
    fn next_audio_chunk(&mut self) -> Result<Option<VideoAudioChunk>, PlatformError>;
    /// Seeks to `pts_ms`, clamped into `0..=duration`; returns the position actually reached.
    fn seek_ms(&mut self, pts_ms: u64) -> Result<u64, PlatformError>;
    /// Releases decoder, D3D11 and audio resources. Idempotent.
    fn close(&mut self);
}

/// Safe OS video boundary (Media Foundation + Intel Arc DXVA on Windows, mock elsewhere).
pub trait VideoBackend: Send + Sync {
    /// Decodes one still frame (seeking to `THUMBNAIL_SEEK_MS` or the first keyframe) fitted
    /// inside `max_w` x `max_h`. Returns `Ok(None)` when the file decodes to nothing.
    fn extract_thumbnail(
        &self,
        path: &Path,
        max_w: u32,
        max_h: u32,
    ) -> Result<Option<VideoFrame>, PlatformError>;

    fn open_session(&self, path: &Path) -> Result<Box<dyn VideoPlaybackSession>, PlatformError>;

    /// Whether this backend would use the GPU media engine for `path` (drives the UI badge).
    fn hardware_acceleration_available(&self) -> bool {
        false
    }

    /// Opens the file just long enough to read width, height, duration and audio presence.
    /// Used once per video, right before the thumbnail is extracted and cached.
    fn probe_metadata(&self, path: &Path) -> Result<VideoMetadata, PlatformError> {
        let mut session = self.open_session(path)?;
        let metadata = session.metadata();
        session.close();
        Ok(metadata)
    }
}

/// Fits `(src_w, src_h)` inside `(max_w, max_h)` without upscaling (`webp.fitBox` rules).
pub fn fit_thumbnail_box(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 || max_w == 0 || max_h == 0 {
        return (1, 1);
    }
    let width = src_w.min(max_w);
    let mut height = (u64::from(src_h) * u64::from(width) / u64::from(src_w)).max(1);
    let mut out_w = width;
    if height > u64::from(max_h) {
        height = u64::from(max_h);
        out_w = ((u64::from(src_w) * u64::from(max_h)) / u64::from(src_h)).max(1) as u32;
    }
    (out_w.max(1), height as u32)
}

/// Nearest-neighbour box downscale used by the mock decoder and the software thumbnail path.
pub fn downscale_rgba(
    rgba: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> Option<Vec<u8>> {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return None;
    }
    let src_len = usize::try_from(u64::from(src_w) * u64::from(src_h) * 4).ok()?;
    if rgba.len() != src_len {
        return None;
    }
    if (src_w, src_h) == (dst_w, dst_h) {
        return Some(rgba.to_vec());
    }
    let mut out = vec![0u8; usize::try_from(u64::from(dst_w) * u64::from(dst_h) * 4).ok()?];
    for y in 0..dst_h {
        let sy = (u64::from(y) * u64::from(src_h) / u64::from(dst_h)) as u32;
        for x in 0..dst_w {
            let sx = (u64::from(x) * u64::from(src_w) / u64::from(dst_w)) as u32;
            let src_ix = ((u64::from(sy) * u64::from(src_w) + u64::from(sx)) * 4) as usize;
            let dst_ix = ((u64::from(y) * u64::from(dst_w) + u64::from(x)) * 4) as usize;
            out[dst_ix..dst_ix + 4].copy_from_slice(&rgba[src_ix..src_ix + 4]);
        }
    }
    Some(out)
}

/// Deterministic synthetic frame: a stable gradient that changes with `pts_ms`, so tests can
/// tell frames apart without any real codec.
pub fn synthetic_rgba_frame(width: u32, height: u32, pts_ms: u64) -> Vec<u8> {
    let w = width.max(1);
    let h = height.max(1);
    let phase = (pts_ms / 40) as u8;
    let mut out = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for y in 0..h {
        let g = ((u64::from(y) * 255) / u64::from(h)) as u8;
        for x in 0..w {
            let r = ((u64::from(x) * 255) / u64::from(w)) as u8;
            out.extend_from_slice(&[r, g, phase, 255]);
        }
    }
    out
}

/// Knobs for `MockVideoBackend`: one struct holds every property a test needs to assert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MockVideoConfig {
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
    pub has_audio: bool,
    pub hardware_accelerated: bool,
    pub fail_extract: bool,
    pub fail_open: bool,
}

impl Default for MockVideoConfig {
    fn default() -> Self {
        Self {
            width: 640,
            height: 360,
            duration_ms: 4_000,
            has_audio: true,
            hardware_accelerated: true,
            fail_extract: false,
            fail_open: false,
        }
    }
}

#[derive(Debug, Default)]
struct MockVideoCounters {
    extracts: AtomicUsize,
    opens: AtomicUsize,
    last_extract_size: AtomicU64,
}

/// Deterministic in-memory video backend for Linux CI and unit tests.
#[derive(Debug, Clone)]
pub struct MockVideoBackend {
    config: MockVideoConfig,
    counters: Arc<MockVideoCounters>,
}

impl Default for MockVideoBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MockVideoBackend {
    pub fn new() -> Self {
        Self::with_config(MockVideoConfig::default())
    }

    pub fn with_config(config: MockVideoConfig) -> Self {
        Self {
            config,
            counters: Arc::new(MockVideoCounters::default()),
        }
    }

    pub fn config(&self) -> MockVideoConfig {
        self.config
    }

    /// Number of `extract_thumbnail` calls (thumbnail cache tests assert it stays at 1).
    pub fn extract_count(&self) -> usize {
        self.counters.extracts.load(Ordering::Relaxed)
    }

    /// Number of `open_session` calls.
    pub fn open_count(&self) -> usize {
        self.counters.opens.load(Ordering::Relaxed)
    }

    pub fn last_extract_size(&self) -> Option<(u32, u32)> {
        let packed = self.counters.last_extract_size.load(Ordering::Relaxed);
        if packed == 0 {
            return None;
        }
        Some(((packed >> 32) as u32, packed as u32))
    }

    fn thumbnail(&self, max_w: u32, max_h: u32) -> Option<VideoFrame> {
        self.counters.extracts.fetch_add(1, Ordering::Relaxed);
        let (w, h) = fit_thumbnail_box(self.config.width, self.config.height, max_w, max_h);
        let src = synthetic_rgba_frame(self.config.width, self.config.height, THUMBNAIL_SEEK_MS);
        let rgba = downscale_rgba(&src, self.config.width, self.config.height, w, h)?;
        self.counters
            .last_extract_size
            .store(u64::from(w) << 32 | u64::from(h), Ordering::Relaxed);
        VideoFrame::new(w, h, THUMBNAIL_SEEK_MS.min(self.config.duration_ms), rgba)
    }
}

/// Deterministic mock playback session: frames at `MOCK_FPS`, 20 ms PCM chunks, real seeking.
pub struct MockVideoSession {
    config: MockVideoConfig,
    position_ms: u64,
    next_frame_pts_ms: u64,
    next_audio_pts_ms: u64,
    closed: bool,
}

impl MockVideoSession {
    pub fn new(config: MockVideoConfig) -> Self {
        Self {
            config,
            position_ms: 0,
            next_frame_pts_ms: 0,
            next_audio_pts_ms: 0,
            closed: false,
        }
    }

    fn frame_interval_ms(&self) -> u64 {
        (1_000 / MOCK_FPS).max(1)
    }
}

impl VideoPlaybackSession for MockVideoSession {
    fn metadata(&self) -> VideoMetadata {
        VideoMetadata {
            width: self.config.width,
            height: self.config.height,
            duration_ms: self.config.duration_ms,
            has_audio: self.config.has_audio,
            hardware_accelerated: self.config.hardware_accelerated,
        }
    }

    fn next_video_frame(&mut self) -> Result<Option<VideoFrame>, PlatformError> {
        if self.closed || self.config.duration_ms == 0 {
            return Ok(None);
        }
        let interval = self.frame_interval_ms();
        let pts = self.next_frame_pts_ms;
        if pts >= self.config.duration_ms {
            return Ok(None);
        }
        let rgba = synthetic_rgba_frame(self.config.width, self.config.height, pts);
        self.next_frame_pts_ms = pts.saturating_add(interval);
        self.position_ms = pts;
        Ok(VideoFrame::new(
            self.config.width,
            self.config.height,
            pts,
            rgba,
        ))
    }

    fn next_audio_chunk(&mut self) -> Result<Option<VideoAudioChunk>, PlatformError> {
        if self.closed || !self.config.has_audio {
            return Ok(None);
        }
        let pts = self.next_audio_pts_ms;
        if pts >= self.config.duration_ms {
            return Ok(None);
        }
        let frames = (u64::from(MOCK_SAMPLE_RATE) * MOCK_AUDIO_CHUNK_MS / 1_000) as usize;
        let mut pcm_f32 = Vec::with_capacity(frames * usize::from(MOCK_CHANNELS));
        for frame in 0..frames {
            let t = (pts as f32 / 1_000.0) + (frame as f32 / MOCK_SAMPLE_RATE as f32);
            let sample = (t * 440.0 * std::f32::consts::TAU).sin() * 0.2;
            for _ in 0..MOCK_CHANNELS {
                pcm_f32.push(sample);
            }
        }
        self.next_audio_pts_ms = pts.saturating_add(MOCK_AUDIO_CHUNK_MS);
        Ok(Some(VideoAudioChunk {
            pts_ms: pts,
            sample_rate: MOCK_SAMPLE_RATE,
            channels: MOCK_CHANNELS,
            pcm_f32,
        }))
    }

    fn seek_ms(&mut self, pts_ms: u64) -> Result<u64, PlatformError> {
        let target = pts_ms.min(self.config.duration_ms);
        self.position_ms = target;
        // Frame and audio clocks resume from the seek target (keyframe-accurate enough for the mock).
        self.next_frame_pts_ms = target - (target % self.frame_interval_ms().max(1));
        self.next_audio_pts_ms = target - (target % MOCK_AUDIO_CHUNK_MS.max(1));
        Ok(target)
    }

    fn close(&mut self) {
        self.closed = true;
    }
}

impl VideoBackend for MockVideoBackend {
    fn extract_thumbnail(
        &self,
        _path: &Path,
        max_w: u32,
        max_h: u32,
    ) -> Result<Option<VideoFrame>, PlatformError> {
        if self.config.fail_extract {
            self.counters.extracts.fetch_add(1, Ordering::Relaxed);
            return Err(PlatformError::VideoError("mock extract failed".into()));
        }
        Ok(self.thumbnail(max_w, max_h))
    }

    fn open_session(&self, _path: &Path) -> Result<Box<dyn VideoPlaybackSession>, PlatformError> {
        self.counters.opens.fetch_add(1, Ordering::Relaxed);
        if self.config.fail_open {
            return Err(PlatformError::VideoError("mock open failed".into()));
        }
        Ok(Box::new(MockVideoSession::new(self.config)))
    }

    fn hardware_acceleration_available(&self) -> bool {
        self.config.hardware_accelerated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumbnail_fit_never_upscales_and_fits_every_extreme() {
        assert_eq!(fit_thumbnail_box(1920, 1080, 420, 250), (420, 236));
        assert_eq!(fit_thumbnail_box(1080, 1920, 420, 250), (140, 250));
        assert_eq!(fit_thumbnail_box(200, 100, 420, 250), (200, 100));
        assert_eq!(fit_thumbnail_box(0, 100, 420, 250), (1, 1));
        assert_eq!(fit_thumbnail_box(100, 100, 0, 0), (1, 1));
    }

    #[test]
    fn downscale_preserves_corners_and_rejects_wrong_lengths() {
        let src = synthetic_rgba_frame(4, 2, 0);
        let out = downscale_rgba(&src, 4, 2, 2, 1).unwrap();
        assert_eq!(out.len(), 2 * 4);
        assert_eq!(&out[0..4], &src[0..4]);
        assert!(downscale_rgba(&src, 4, 2, 0, 1).is_none());
        assert!(downscale_rgba(&src[..3], 4, 2, 2, 1).is_none());
    }

    #[test]
    fn frame_rejects_short_buffers_and_swaps_to_bgra() {
        assert!(VideoFrame::new(2, 2, 0, vec![0; 15]).is_none());
        let frame = VideoFrame::new(1, 1, 0, vec![10, 20, 30, 255]).unwrap();
        assert_eq!(frame.byte_size(), 4);
        assert_eq!(frame.to_bgra(), vec![30, 20, 10, 255]);
    }

    #[test]
    fn mock_backend_extracts_fitted_thumbnail_and_counts_calls() {
        let mock = MockVideoBackend::with_config(MockVideoConfig {
            width: 1920,
            height: 1080,
            duration_ms: 9_000,
            ..MockVideoConfig::default()
        });
        assert_eq!(mock.extract_count(), 0);
        let frame = mock
            .extract_thumbnail(Path::new("clip.mp4"), 420, 250)
            .unwrap()
            .unwrap();
        assert_eq!((frame.width, frame.height), (420, 236));
        assert_eq!(frame.pts_ms, THUMBNAIL_SEEK_MS);
        assert_eq!(frame.rgba.len(), 420 * 236 * 4);
        assert_eq!(mock.extract_count(), 1);
        assert_eq!(mock.last_extract_size(), Some((420, 236)));
    }

    #[test]
    fn mock_backend_surfaces_failures_as_platform_errors() {
        let mock = MockVideoBackend::with_config(MockVideoConfig {
            fail_extract: true,
            fail_open: true,
            ..MockVideoConfig::default()
        });
        assert!(
            mock.extract_thumbnail(Path::new("clip.mp4"), 100, 100)
                .is_err()
        );
        assert_eq!(mock.extract_count(), 1);
        assert!(mock.open_session(Path::new("clip.mp4")).is_err());
        assert_eq!(mock.open_count(), 1);
    }

    #[test]
    fn mock_session_streams_timed_frames_audio_and_seeks() {
        let mock = MockVideoBackend::new();
        let mut session = mock.open_session(Path::new("clip.mp4")).unwrap();
        let meta = session.metadata();
        assert_eq!(meta.duration_ms, 4_000);
        assert!(meta.has_audio);
        assert!(meta.hardware_accelerated);
        assert!(meta.is_decodable());
        assert_eq!(mock.open_count(), 1);

        let f0 = session.next_video_frame().unwrap().unwrap();
        let f1 = session.next_video_frame().unwrap().unwrap();
        assert_eq!(f0.pts_ms, 0);
        assert_eq!(f1.pts_ms, 41);
        assert_ne!(f0.rgba, f1.rgba);

        let a0 = session.next_audio_chunk().unwrap().unwrap();
        assert_eq!(a0.pts_ms, 0);
        assert_eq!(a0.sample_rate, MOCK_SAMPLE_RATE);
        assert_eq!(a0.channels, MOCK_CHANNELS);
        assert_eq!(a0.duration_ms(), MOCK_AUDIO_CHUNK_MS);
        let a1 = session.next_audio_chunk().unwrap().unwrap();
        assert_eq!(a1.pts_ms, MOCK_AUDIO_CHUNK_MS);

        assert_eq!(session.seek_ms(2_000).unwrap(), 2_000);
        let after = session.next_video_frame().unwrap().unwrap();
        // Resumes at the frame boundary at or before the seek target.
        assert!(after.pts_ms <= 2_000 && 2_000 - after.pts_ms < 1_000 / MOCK_FPS);
        assert_eq!(session.seek_ms(99_999).unwrap(), 4_000);
        // Seeking to the very end still exposes the last frame boundary, then EOF.
        let last = session.next_video_frame().unwrap().unwrap();
        assert!(last.pts_ms <= 4_000);
        assert!(session.next_video_frame().unwrap().is_none());

        session.close();
        session.close();
        assert!(session.next_video_frame().unwrap().is_none());
        assert!(session.next_audio_chunk().unwrap().is_none());
    }

    #[test]
    fn mock_session_without_audio_yields_no_chunks() {
        let mock = MockVideoBackend::with_config(MockVideoConfig {
            has_audio: false,
            hardware_accelerated: false,
            ..MockVideoConfig::default()
        });
        assert!(!mock.hardware_acceleration_available());
        let mut session = mock.open_session(Path::new("silent.mp4")).unwrap();
        assert!(!session.metadata().has_audio);
        assert!(session.next_audio_chunk().unwrap().is_none());
        assert!(session.next_video_frame().unwrap().is_some());
    }
}
