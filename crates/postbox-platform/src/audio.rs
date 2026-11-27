use crate::PlatformError;
use postbox_core::voice::AudioDeviceInfo;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Native audio capture stream format returned by the OS capture backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub is_float: bool,
    pub bits_per_sample: u16,
}

impl CaptureFormat {
    pub const fn bytes_per_sample(self) -> usize {
        (self.bits_per_sample / 8) as usize
    }

    pub const fn block_align(self) -> usize {
        self.bytes_per_sample() * self.channels as usize
    }
}

/// A single packet read from an active capture stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturePacket {
    /// Raw PCM bytes in `CaptureFormat` (already zeroed if `silent == true`).
    pub data: Vec<u8>,
    pub frames: usize,
    pub silent: bool,
}

/// Active audio output stream accepting interleaved 32-bit float PCM frames.
pub trait AudioRenderStream: Send {
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
    fn buffer_frames(&self) -> usize;
    fn padding_frames(&self) -> Result<usize, PlatformError>;
    /// Writes up to `interleaved_f32.len() / channels` frames into the device buffer.
    /// Returns the number of frames actually written.
    fn write_frames(&mut self, interleaved_f32: &[f32]) -> Result<usize, PlatformError>;
    fn pause(&mut self) -> Result<(), PlatformError>;
    fn resume(&mut self) -> Result<(), PlatformError>;
    fn reset(&mut self) -> Result<(), PlatformError>;
    fn stop(&mut self) -> Result<(), PlatformError>;
}

/// Active microphone capture stream delivering native mix-format packets.
pub trait AudioCaptureStream: Send {
    fn format(&self) -> CaptureFormat;
    fn read_packet(&mut self) -> Result<Option<CapturePacket>, PlatformError>;
    fn stop(&mut self) -> Result<(), PlatformError>;
}

/// Safe OS audio boundary (Win32 WASAPI on Windows, deterministic mock on Linux/tests).
pub trait AudioBackend: Send + Sync {
    fn list_capture_devices(&self) -> Result<Vec<AudioDeviceInfo>, PlatformError>;
    fn open_render(
        &self,
        sample_rate: u32,
        channels: u16,
    ) -> Result<Box<dyn AudioRenderStream>, PlatformError>;
    fn open_capture(
        &self,
        preferred_mic_name: Option<&str>,
    ) -> Result<Box<dyn AudioCaptureStream>, PlatformError>;
}

#[derive(Debug, Clone)]
pub struct MockRenderSnapshot {
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
    pub paused: bool,
    pub stopped: bool,
    pub reset_count: usize,
}

#[derive(Debug)]
struct MockAudioInner {
    devices: Vec<AudioDeviceInfo>,
    fail_render: bool,
    fail_capture: bool,
    capture_format: CaptureFormat,
    capture_packets: VecDeque<CapturePacket>,
    last_capture_mic: Option<String>,
    render_history: Vec<Arc<Mutex<MockRenderSnapshot>>>,
}

impl Default for MockAudioInner {
    fn default() -> Self {
        Self {
            devices: vec![
                AudioDeviceInfo {
                    id: "mock-mic-default".to_owned(),
                    friendly_name: "Built-in Microphone".to_owned(),
                    is_default: true,
                },
                AudioDeviceInfo {
                    id: "mock-mic-usb".to_owned(),
                    friendly_name: "USB Studio Mic".to_owned(),
                    is_default: false,
                },
            ],
            fail_render: false,
            fail_capture: false,
            capture_format: CaptureFormat {
                sample_rate: 48_000,
                channels: 1,
                is_float: true,
                bits_per_sample: 32,
            },
            capture_packets: VecDeque::new(),
            last_capture_mic: None,
            render_history: Vec::new(),
        }
    }
}

/// Deterministic in-memory audio backend for Linux CI and round-trip unit/integration tests.
#[derive(Debug, Clone, Default)]
pub struct MockAudioBackend {
    inner: Arc<Mutex<MockAudioInner>>,
}

impl MockAudioBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_devices(&self, devices: Vec<AudioDeviceInfo>) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.devices = devices;
        }
    }

    pub fn set_fail_render(&self, fail: bool) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.fail_render = fail;
        }
    }

    pub fn set_fail_capture(&self, fail: bool) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.fail_capture = fail;
        }
    }

    pub fn set_capture_format(&self, format: CaptureFormat) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.capture_format = format;
        }
    }

    pub fn push_capture_packet(&self, packet: CapturePacket) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.capture_packets.push_back(packet);
        }
    }

    /// Helper: pushes float32 mono/stereo samples chunked into packets of `frames_per_packet`.
    pub fn push_capture_f32(&self, samples: &[f32], channels: u16, frames_per_packet: usize) {
        let ch = usize::from(channels.max(1));
        let chunk_samples = frames_per_packet.max(1) * ch;
        for chunk in samples.chunks(chunk_samples) {
            let frames = chunk.len() / ch;
            let mut data = Vec::with_capacity(chunk.len() * 4);
            for &s in chunk {
                data.extend_from_slice(&s.to_le_bytes());
            }
            self.push_capture_packet(CapturePacket {
                data,
                frames,
                silent: false,
            });
        }
    }

    /// Helper: pushes i16 PCM samples chunked into packets of `frames_per_packet`.
    pub fn push_capture_i16(&self, samples: &[i16], channels: u16, frames_per_packet: usize) {
        let ch = usize::from(channels.max(1));
        let chunk_samples = frames_per_packet.max(1) * ch;
        for chunk in samples.chunks(chunk_samples) {
            let frames = chunk.len() / ch;
            let mut data = Vec::with_capacity(chunk.len() * 2);
            for &s in chunk {
                data.extend_from_slice(&s.to_le_bytes());
            }
            self.push_capture_packet(CapturePacket {
                data,
                frames,
                silent: false,
            });
        }
    }

    pub fn last_capture_mic(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.last_capture_mic.clone())
    }

    pub fn render_snapshots(&self) -> Vec<MockRenderSnapshot> {
        let Ok(guard) = self.inner.lock() else {
            return Vec::new();
        };
        guard
            .render_history
            .iter()
            .filter_map(|s| s.lock().ok().map(|v| v.clone()))
            .collect()
    }

    pub fn total_rendered_samples(&self) -> usize {
        self.render_snapshots()
            .iter()
            .map(|s| s.samples.len())
            .sum()
    }
}

struct MockRenderStream {
    snapshot: Arc<Mutex<MockRenderSnapshot>>,
    sample_rate: u32,
    channels: u16,
    buffer_frames: usize,
}

impl AudioRenderStream for MockRenderStream {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn buffer_frames(&self) -> usize {
        self.buffer_frames
    }

    fn padding_frames(&self) -> Result<usize, PlatformError> {
        Ok(0)
    }

    fn write_frames(&mut self, interleaved_f32: &[f32]) -> Result<usize, PlatformError> {
        let ch = usize::from(self.channels.max(1));
        let frames = interleaved_f32.len() / ch;
        if frames == 0 {
            return Ok(0);
        }
        let mut guard = self
            .snapshot
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        if guard.stopped {
            return Err(PlatformError::AudioError("render stream stopped".into()));
        }
        let to_write = frames.min(self.buffer_frames);
        guard
            .samples
            .extend_from_slice(&interleaved_f32[..to_write * ch]);
        Ok(to_write)
    }

    fn pause(&mut self) -> Result<(), PlatformError> {
        let mut guard = self
            .snapshot
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        guard.paused = true;
        Ok(())
    }

    fn resume(&mut self) -> Result<(), PlatformError> {
        let mut guard = self
            .snapshot
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        guard.paused = false;
        Ok(())
    }

    fn reset(&mut self) -> Result<(), PlatformError> {
        let mut guard = self
            .snapshot
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        guard.reset_count += 1;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), PlatformError> {
        let mut guard = self
            .snapshot
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        guard.stopped = true;
        Ok(())
    }
}

struct MockCaptureStream {
    inner: Arc<Mutex<MockAudioInner>>,
    format: CaptureFormat,
    stopped: bool,
}

impl AudioCaptureStream for MockCaptureStream {
    fn format(&self) -> CaptureFormat {
        self.format
    }

    fn read_packet(&mut self) -> Result<Option<CapturePacket>, PlatformError> {
        if self.stopped {
            return Ok(None);
        }
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        Ok(guard.capture_packets.pop_front())
    }

    fn stop(&mut self) -> Result<(), PlatformError> {
        self.stopped = true;
        Ok(())
    }
}

impl AudioBackend for MockAudioBackend {
    fn list_capture_devices(&self) -> Result<Vec<AudioDeviceInfo>, PlatformError> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        Ok(guard.devices.clone())
    }

    fn open_render(
        &self,
        sample_rate: u32,
        channels: u16,
    ) -> Result<Box<dyn AudioRenderStream>, PlatformError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        if guard.fail_render {
            return Err(PlatformError::AudioError("mock render open failed".into()));
        }
        let snapshot = Arc::new(Mutex::new(MockRenderSnapshot {
            sample_rate,
            channels,
            samples: Vec::new(),
            paused: false,
            stopped: false,
            reset_count: 0,
        }));
        guard.render_history.push(Arc::clone(&snapshot));
        let buffer_frames = (sample_rate as usize / 5).max(960);
        Ok(Box::new(MockRenderStream {
            snapshot,
            sample_rate,
            channels,
            buffer_frames,
        }))
    }

    fn open_capture(
        &self,
        preferred_mic_name: Option<&str>,
    ) -> Result<Box<dyn AudioCaptureStream>, PlatformError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        if guard.fail_capture {
            return Err(PlatformError::AudioError("mock capture open failed".into()));
        }
        guard.last_capture_mic = preferred_mic_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let format = guard.capture_format;
        Ok(Box::new(MockCaptureStream {
            inner: Arc::clone(&self.inner),
            format,
            stopped: false,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_audio_backend_records_render_and_delivers_capture_packets() {
        let mock = MockAudioBackend::new();
        assert_eq!(mock.list_capture_devices().unwrap().len(), 2);

        let mut render = mock.open_render(48_000, 2).unwrap();
        let written = render.write_frames(&[0.25, -0.25, 0.5, -0.5]).unwrap();
        assert_eq!(written, 2);
        render.pause().unwrap();
        render.resume().unwrap();
        render.reset().unwrap();
        render.stop().unwrap();

        let snaps = mock.render_snapshots();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].samples, vec![0.25, -0.25, 0.5, -0.5]);
        assert_eq!(snaps[0].reset_count, 1);
        assert!(snaps[0].stopped);

        mock.push_capture_f32(&[0.1, 0.2, 0.3, 0.4], 1, 2);
        let mut cap = mock.open_capture(Some("USB Studio Mic")).unwrap();
        assert_eq!(mock.last_capture_mic().as_deref(), Some("USB Studio Mic"));
        let p1 = cap.read_packet().unwrap().unwrap();
        assert_eq!(p1.frames, 2);
        let p2 = cap.read_packet().unwrap().unwrap();
        assert_eq!(p2.frames, 2);
        assert!(cap.read_packet().unwrap().is_none());
    }
}
