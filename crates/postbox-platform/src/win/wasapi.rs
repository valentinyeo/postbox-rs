//! Windows WASAPI shared-mode audio render, capture, and microphone enumeration.
//! Strictly quarantined inside `crates/postbox-platform/src/win/`.

use crate::PlatformError;
use crate::audio::{AudioCaptureStream, AudioRenderStream, CaptureFormat, CapturePacket};
use postbox_core::voice::AudioDeviceInfo;
use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE_ACTIVE, IAudioCaptureClient,
    IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    WAVE_FORMAT_PCM, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eCapture, eCommunications, eConsole,
    eRender,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, STGM_READ,
};

const MAX_CAPTURE_DEVICES: usize = 16;
const RENDER_BUFFER_HNS: i64 = 200 * 10_000; // 200 ms in 100-ns units
const CAPTURE_BUFFER_HNS: i64 = 1_000 * 10_000; // 1 s in 100-ns units

struct ComGuard {
    should_uninit: bool,
}

impl ComGuard {
    fn init_mta() -> Result<Self, PlatformError> {
        // SAFETY: Initializes COM on the current thread in MTA mode; handles RPC_E_CHANGED_MODE
        // without double-uninitializing.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == S_OK || hr == S_FALSE {
            Ok(Self {
                should_uninit: true,
            })
        } else if hr == RPC_E_CHANGED_MODE {
            Ok(Self {
                should_uninit: false,
            })
        } else {
            Err(PlatformError::AudioError(format!(
                "CoInitializeEx failed: {hr:?}"
            )))
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            // SAFETY: Matches a successful CoInitializeEx on this same thread.
            unsafe { CoUninitialize() };
        }
    }
}

struct CoTaskMemGuard(*mut c_void);

impl Drop for CoTaskMemGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: Pointer was allocated by COM CoTaskMemAlloc (e.g. GetMixFormat / GetId).
            unsafe { CoTaskMemFree(Some(self.0)) };
        }
    }
}

fn create_enumerator() -> Result<IMMDeviceEnumerator, PlatformError> {
    // SAFETY: COM is initialized by ComGuard on the calling thread.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .map_err(|err| PlatformError::AudioError(format!("MMDeviceEnumerator failed: {err}")))
}

fn device_id(device: &IMMDevice) -> Result<String, PlatformError> {
    // SAFETY: GetId returns a CoTaskMem-allocated UTF-16 string freed by CoTaskMemGuard.
    let pwstr = unsafe { device.GetId() }
        .map_err(|err| PlatformError::AudioError(format!("IMMDevice::GetId failed: {err}")))?;
    let _guard = CoTaskMemGuard(pwstr.0.cast());
    // SAFETY: pwstr is valid and NUL-terminated until `_guard` drops.
    let id = unsafe { pwstr.to_string() }.unwrap_or_default();
    Ok(id)
}

fn device_friendly_name(device: &IMMDevice) -> Result<String, PlatformError> {
    // SAFETY: Opens read-only property store and reads PKEY_Device_FriendlyName.
    // PROPVARIANT implements Drop (PropVariantClear) and Display (PropVariantToBSTR).
    let store = unsafe { device.OpenPropertyStore(STGM_READ) }
        .map_err(|err| PlatformError::AudioError(format!("OpenPropertyStore failed: {err}")))?;
    // SAFETY: PKEY_Device_FriendlyName is a valid static PROPERTYKEY.
    let prop = unsafe { store.GetValue(&PKEY_Device_FriendlyName) }.map_err(|err| {
        PlatformError::AudioError(format!("GetValue friendly name failed: {err}"))
    })?;
    Ok(prop.to_string())
}

pub fn list_capture_devices() -> Result<Vec<AudioDeviceInfo>, PlatformError> {
    let _com = ComGuard::init_mta()?;
    let enumerator = create_enumerator()?;

    // SAFETY: Queries default capture endpoint (communications, fallback console) to mark default.
    let default_id = unsafe { enumerator.GetDefaultAudioEndpoint(eCapture, eCommunications) }
        .or_else(|_| unsafe { enumerator.GetDefaultAudioEndpoint(eCapture, eConsole) })
        .ok()
        .and_then(|dev| device_id(&dev).ok());

    // SAFETY: Enumerates active capture endpoints.
    let collection = unsafe { enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) }
        .map_err(|err| PlatformError::AudioError(format!("EnumAudioEndpoints failed: {err}")))?;
    // SAFETY: Valid IMMDeviceCollection pointer.
    let count = unsafe { collection.GetCount() }
        .map_err(|err| PlatformError::AudioError(format!("GetCount failed: {err}")))?;

    let mut out = Vec::new();
    for idx in 0..count {
        if out.len() >= MAX_CAPTURE_DEVICES {
            break;
        }
        // SAFETY: idx < count.
        let Ok(dev) = (unsafe { collection.Item(idx) }) else {
            continue;
        };
        let Ok(id) = device_id(&dev) else {
            continue;
        };
        let friendly_name = device_friendly_name(&dev).unwrap_or_default();
        let trimmed = friendly_name.trim();
        if trimmed.is_empty() {
            continue;
        }
        let is_default = default_id.as_deref() == Some(id.as_str());
        out.push(AudioDeviceInfo {
            id,
            friendly_name: trimmed.to_owned(),
            is_default,
        });
    }
    Ok(out)
}

enum RenderCmd {
    Padding(Sender<Result<usize, PlatformError>>),
    Write(Vec<f32>, Sender<Result<usize, PlatformError>>),
    Pause(Sender<Result<(), PlatformError>>),
    Resume(Sender<Result<(), PlatformError>>),
    Reset(Sender<Result<(), PlatformError>>),
    Stop(Sender<Result<(), PlatformError>>),
}

pub struct WasapiRenderStream {
    sample_rate: u32,
    channels: u16,
    buffer_frames: usize,
    tx: Option<Sender<RenderCmd>>,
    handle: Option<JoinHandle<()>>,
}

impl WasapiRenderStream {
    fn send_cmd<T>(
        &self,
        build: impl FnOnce(Sender<Result<T, PlatformError>>) -> RenderCmd,
    ) -> Result<T, PlatformError> {
        let Some(tx) = self.tx.as_ref() else {
            return Err(PlatformError::AudioError("render stream closed".into()));
        };
        let (reply_tx, reply_rx) = mpsc::channel();
        tx.send(build(reply_tx))
            .map_err(|_| PlatformError::AudioError("render thread exited".into()))?;
        reply_rx
            .recv()
            .map_err(|_| PlatformError::AudioError("render thread dropped reply".into()))?
    }
}

impl Drop for WasapiRenderStream {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let (reply_tx, reply_rx) = mpsc::channel();
            let _ = tx.send(RenderCmd::Stop(reply_tx));
            let _ = reply_rx.recv();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl AudioRenderStream for WasapiRenderStream {
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
        self.send_cmd(RenderCmd::Padding)
    }

    fn write_frames(&mut self, interleaved_f32: &[f32]) -> Result<usize, PlatformError> {
        if interleaved_f32.is_empty() {
            return Ok(0);
        }
        self.send_cmd(|reply| RenderCmd::Write(interleaved_f32.to_vec(), reply))
    }

    fn pause(&mut self) -> Result<(), PlatformError> {
        self.send_cmd(RenderCmd::Pause)
    }

    fn resume(&mut self) -> Result<(), PlatformError> {
        self.send_cmd(RenderCmd::Resume)
    }

    fn reset(&mut self) -> Result<(), PlatformError> {
        self.send_cmd(RenderCmd::Reset)
    }

    fn stop(&mut self) -> Result<(), PlatformError> {
        if let Some(tx) = self.tx.take() {
            let (reply_tx, reply_rx) = mpsc::channel();
            let _ = tx.send(RenderCmd::Stop(reply_tx));
            let res = reply_rx
                .recv()
                .map_err(|_| PlatformError::AudioError("render thread dropped reply".into()))?;
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
            return res;
        }
        Ok(())
    }
}

pub fn open_render(
    sample_rate: u32,
    channels: u16,
) -> Result<Box<dyn AudioRenderStream>, PlatformError> {
    let (init_tx, init_rx) = mpsc::channel::<Result<usize, PlatformError>>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<RenderCmd>();

    let handle = std::thread::Builder::new()
        .name("postbox-wasapi-render".into())
        .spawn(move || run_render_thread(sample_rate, channels, init_tx, cmd_rx))
        .map_err(|err| PlatformError::AudioError(format!("spawn render thread failed: {err}")))?;

    match init_rx.recv() {
        Ok(Ok(buffer_frames)) => Ok(Box::new(WasapiRenderStream {
            sample_rate,
            channels,
            buffer_frames,
            tx: Some(cmd_tx),
            handle: Some(handle),
        })),
        Ok(Err(err)) => {
            let _ = handle.join();
            Err(err)
        }
        Err(_) => {
            let _ = handle.join();
            Err(PlatformError::AudioError(
                "render thread exited during init".into(),
            ))
        }
    }
}

fn run_render_thread(
    sample_rate: u32,
    channels: u16,
    init_tx: Sender<Result<usize, PlatformError>>,
    cmd_rx: Receiver<RenderCmd>,
) {
    let setup =
        (|| -> Result<(ComGuard, IAudioClient, IAudioRenderClient, usize), PlatformError> {
            let com = ComGuard::init_mta()?;
            let enumerator = create_enumerator()?;
            // SAFETY: Gets default console render endpoint.
            let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }.map_err(
                |err| PlatformError::AudioError(format!("GetDefaultAudioEndpoint failed: {err}")),
            )?;
            // SAFETY: Activates IAudioClient on the render endpoint.
            let client: IAudioClient =
                unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|err| {
                    PlatformError::AudioError(format!("Activate IAudioClient failed: {err}"))
                })?;

            let block_align = channels.saturating_mul(4);
            let avg_bytes = sample_rate.saturating_mul(u32::from(block_align));
            let wfx = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
                nChannels: channels,
                nSamplesPerSec: sample_rate,
                nAvgBytesPerSec: avg_bytes,
                nBlockAlign: block_align,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            let flags =
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            // SAFETY: Initializes shared-mode render stream with valid WAVEFORMATEX pointer.
            unsafe {
                client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    flags,
                    RENDER_BUFFER_HNS,
                    0,
                    &wfx,
                    None,
                )
            }
            .map_err(|err| {
                PlatformError::AudioError(format!("IAudioClient::Initialize failed: {err}"))
            })?;

            // SAFETY: Valid initialized IAudioClient.
            let buffer_frames = unsafe { client.GetBufferSize() }
                .map_err(|err| PlatformError::AudioError(format!("GetBufferSize failed: {err}")))?
                as usize;
            // SAFETY: Obtains IAudioRenderClient service from initialized IAudioClient.
            let render: IAudioRenderClient = unsafe { client.GetService() }.map_err(|err| {
                PlatformError::AudioError(format!("GetService render failed: {err}"))
            })?;
            // SAFETY: Starts the render stream.
            unsafe { client.Start() }.map_err(|err| {
                PlatformError::AudioError(format!("IAudioClient::Start failed: {err}"))
            })?;
            Ok((com, client, render, buffer_frames))
        })();

    let (_com, client, render, buffer_frames) = match setup {
        Ok(v) => {
            let _ = init_tx.send(Ok(v.3));
            v
        }
        Err(err) => {
            let _ = init_tx.send(Err(err));
            return;
        }
    };

    let ch = usize::from(channels.max(1));
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            RenderCmd::Padding(reply) => {
                // SAFETY: Valid initialized IAudioClient on owning MTA thread.
                let res = unsafe { client.GetCurrentPadding() }
                    .map(|p| p as usize)
                    .map_err(|err| PlatformError::AudioError(format!("GetCurrentPadding: {err}")));
                let _ = reply.send(res);
            }
            RenderCmd::Write(samples, reply) => {
                let requested = samples.len() / ch;
                // SAFETY: Queries available buffer frames, acquires buffer, copies f32 bytes, and releases.
                let res = (|| -> Result<usize, PlatformError> {
                    let padding = unsafe { client.GetCurrentPadding() }
                        .map_err(|e| PlatformError::AudioError(format!("GetCurrentPadding: {e}")))?
                        as usize;
                    let avail = buffer_frames.saturating_sub(padding);
                    let to_write = requested.min(avail);
                    if to_write == 0 {
                        return Ok(0);
                    }
                    let dst = unsafe { render.GetBuffer(to_write as u32) }
                        .map_err(|e| PlatformError::AudioError(format!("GetBuffer: {e}")))?;
                    let byte_len = to_write * ch * std::mem::size_of::<f32>();
                    unsafe {
                        std::ptr::copy_nonoverlapping(samples.as_ptr().cast::<u8>(), dst, byte_len);
                        render.ReleaseBuffer(to_write as u32, 0).map_err(|e| {
                            PlatformError::AudioError(format!("ReleaseBuffer: {e}"))
                        })?;
                    }
                    Ok(to_write)
                })();
                let _ = reply.send(res);
            }
            RenderCmd::Pause(reply) => {
                // SAFETY: Stops playback clock without resetting buffer.
                let res = unsafe { client.Stop() }
                    .map_err(|e| PlatformError::AudioError(format!("Stop: {e}")));
                let _ = reply.send(res);
            }
            RenderCmd::Resume(reply) => {
                // SAFETY: Resumes playback clock.
                let res = unsafe { client.Start() }
                    .map_err(|e| PlatformError::AudioError(format!("Start: {e}")));
                let _ = reply.send(res);
            }
            RenderCmd::Reset(reply) => {
                // SAFETY: Stops, resets buffer, and restarts playback stream for seek.
                let res = (|| -> Result<(), PlatformError> {
                    let _ = unsafe { client.Stop() };
                    unsafe { client.Reset() }
                        .map_err(|e| PlatformError::AudioError(format!("Reset: {e}")))?;
                    unsafe { client.Start() }.map_err(|e| {
                        PlatformError::AudioError(format!("Start after reset: {e}"))
                    })?;
                    Ok(())
                })();
                let _ = reply.send(res);
            }
            RenderCmd::Stop(reply) => {
                // SAFETY: Stops render stream before exiting thread and releasing COM objects.
                let res = unsafe { client.Stop() }
                    .map_err(|e| PlatformError::AudioError(format!("Stop: {e}")));
                let _ = reply.send(res);
                break;
            }
        }
    }
}

enum CaptureCmd {
    ReadPacket(Sender<Result<Option<CapturePacket>, PlatformError>>),
    Stop(Sender<Result<(), PlatformError>>),
}

pub struct WasapiCaptureStream {
    format: CaptureFormat,
    tx: Option<Sender<CaptureCmd>>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for WasapiCaptureStream {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let (reply_tx, reply_rx) = mpsc::channel();
            let _ = tx.send(CaptureCmd::Stop(reply_tx));
            let _ = reply_rx.recv();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl AudioCaptureStream for WasapiCaptureStream {
    fn format(&self) -> CaptureFormat {
        self.format
    }

    fn read_packet(&mut self) -> Result<Option<CapturePacket>, PlatformError> {
        let Some(tx) = self.tx.as_ref() else {
            return Ok(None);
        };
        let (reply_tx, reply_rx) = mpsc::channel();
        tx.send(CaptureCmd::ReadPacket(reply_tx))
            .map_err(|_| PlatformError::AudioError("capture thread exited".into()))?;
        reply_rx
            .recv()
            .map_err(|_| PlatformError::AudioError("capture thread dropped reply".into()))?
    }

    fn stop(&mut self) -> Result<(), PlatformError> {
        if let Some(tx) = self.tx.take() {
            let (reply_tx, reply_rx) = mpsc::channel();
            let _ = tx.send(CaptureCmd::Stop(reply_tx));
            let res = reply_rx
                .recv()
                .map_err(|_| PlatformError::AudioError("capture thread dropped reply".into()))?;
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
            return res;
        }
        Ok(())
    }
}

fn resolve_capture_device(
    enumerator: &IMMDeviceEnumerator,
    preferred_name: Option<&str>,
) -> Result<IMMDevice, PlatformError> {
    // SAFETY: Enumerates active capture endpoints to match preferred friendly name.
    if let Some(target) = preferred_name.map(str::trim).filter(|s| !s.is_empty())
        && let Ok(collection) =
            unsafe { enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) }
        && let Ok(count) = unsafe { collection.GetCount() }
    {
        for idx in 0..count {
            // SAFETY: idx < count.
            if let Ok(dev) = unsafe { collection.Item(idx) }
                && let Ok(name) = device_friendly_name(&dev)
                && name.trim() == target
            {
                return Ok(dev);
            }
        }
    }
    // SAFETY: Falls back to default communications capture mic, then default console capture mic.
    unsafe { enumerator.GetDefaultAudioEndpoint(eCapture, eCommunications) }
        .or_else(|_| unsafe { enumerator.GetDefaultAudioEndpoint(eCapture, eConsole) })
        .map_err(|err| PlatformError::AudioError(format!("No capture microphone available: {err}")))
}

fn parse_mix_format(mix_ptr: *const WAVEFORMATEX) -> Result<CaptureFormat, PlatformError> {
    // SAFETY: mix_ptr is returned by GetMixFormat and read unaligned because WAVEFORMATEX is packed(1).
    let wf = unsafe { std::ptr::read_unaligned(mix_ptr) };
    let tag = wf.wFormatTag;
    let channels = wf.nChannels;
    let sample_rate = wf.nSamplesPerSec;
    let bits = wf.wBitsPerSample;
    let cb_size = wf.cbSize;

    if channels == 0 || sample_rate == 0 {
        return Err(PlatformError::AudioError(
            "Invalid capture mix format".into(),
        ));
    }

    let is_float = if tag == WAVE_FORMAT_IEEE_FLOAT as u16 && bits == 32 {
        true
    } else if tag == WAVE_FORMAT_PCM as u16 && bits == 16 {
        false
    } else if tag == WAVE_FORMAT_EXTENSIBLE as u16 && cb_size >= 22 {
        // SAFETY: cbSize >= 22 guarantees WAVEFORMATEXTENSIBLE layout; read unaligned because packed(1).
        let ext = unsafe { std::ptr::read_unaligned(mix_ptr.cast::<WAVEFORMATEXTENSIBLE>()) };
        let sub = ext.SubFormat;
        if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT && bits == 32 {
            true
        } else if sub == KSDATAFORMAT_SUBTYPE_PCM && bits == 16 {
            false
        } else {
            return Err(PlatformError::AudioError(
                "Unsupported extensible capture format".into(),
            ));
        }
    } else {
        return Err(PlatformError::AudioError(
            "Unsupported capture mix format".into(),
        ));
    };

    Ok(CaptureFormat {
        sample_rate,
        channels,
        is_float,
        bits_per_sample: bits,
    })
}

pub fn open_capture(
    preferred_mic_name: Option<&str>,
) -> Result<Box<dyn AudioCaptureStream>, PlatformError> {
    let preferred = preferred_mic_name.map(str::to_owned);
    let (init_tx, init_rx) = mpsc::channel::<Result<CaptureFormat, PlatformError>>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<CaptureCmd>();

    let handle = std::thread::Builder::new()
        .name("postbox-wasapi-capture".into())
        .spawn(move || run_capture_thread(preferred.as_deref(), init_tx, cmd_rx))
        .map_err(|err| PlatformError::AudioError(format!("spawn capture thread failed: {err}")))?;

    match init_rx.recv() {
        Ok(Ok(format)) => Ok(Box::new(WasapiCaptureStream {
            format,
            tx: Some(cmd_tx),
            handle: Some(handle),
        })),
        Ok(Err(err)) => {
            let _ = handle.join();
            Err(err)
        }
        Err(_) => {
            let _ = handle.join();
            Err(PlatformError::AudioError(
                "capture thread exited during init".into(),
            ))
        }
    }
}

fn run_capture_thread(
    preferred_name: Option<&str>,
    init_tx: Sender<Result<CaptureFormat, PlatformError>>,
    cmd_rx: Receiver<CaptureCmd>,
) {
    let setup =
        (|| -> Result<(ComGuard, IAudioClient, IAudioCaptureClient, CaptureFormat), PlatformError> {
            let com = ComGuard::init_mta()?;
            let enumerator = create_enumerator()?;
            let device = resolve_capture_device(&enumerator, preferred_name)?;
            // SAFETY: Activates IAudioClient on capture device.
            let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(
                |err| PlatformError::AudioError(format!("Activate capture client failed: {err}")),
            )?;
            // SAFETY: Gets native shared-mode mix format; freed by CoTaskMemGuard.
            let mix_ptr = unsafe { client.GetMixFormat() }
                .map_err(|err| PlatformError::AudioError(format!("GetMixFormat failed: {err}")))?;
            let _mix_guard = CoTaskMemGuard(mix_ptr.cast());
            let format = parse_mix_format(mix_ptr)?;

            // SAFETY: Initializes shared-mode capture with native mix format and 1-second buffer.
            unsafe {
                client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    0,
                    CAPTURE_BUFFER_HNS,
                    0,
                    mix_ptr,
                    None,
                )
            }
            .map_err(|err| {
                PlatformError::AudioError(format!("Capture Initialize failed: {err}"))
            })?;
            // SAFETY: Acquires IAudioCaptureClient service.
            let capture: IAudioCaptureClient = unsafe { client.GetService() }.map_err(|err| {
                PlatformError::AudioError(format!("GetService capture failed: {err}"))
            })?;
            // SAFETY: Starts capture stream.
            unsafe { client.Start() }
                .map_err(|err| PlatformError::AudioError(format!("Capture Start failed: {err}")))?;
            Ok((com, client, capture, format))
        })();

    let (_com, client, capture, format) = match setup {
        Ok(v) => {
            let _ = init_tx.send(Ok(v.3));
            v
        }
        Err(err) => {
            let _ = init_tx.send(Err(err));
            return;
        }
    };

    let block_align = format.block_align();
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            CaptureCmd::ReadPacket(reply) => {
                // SAFETY: Queries next packet size, reads buffer if available, copies bytes, and releases.
                let res = (|| -> Result<Option<CapturePacket>, PlatformError> {
                    let next_frames = unsafe { capture.GetNextPacketSize() }.map_err(|e| {
                        PlatformError::AudioError(format!("GetNextPacketSize: {e}"))
                    })?;
                    if next_frames == 0 {
                        return Ok(None);
                    }
                    let mut data_ptr: *mut u8 = std::ptr::null_mut();
                    let mut num_frames: u32 = 0;
                    let mut flags: u32 = 0;
                    unsafe {
                        capture
                            .GetBuffer(&mut data_ptr, &mut num_frames, &mut flags, None, None)
                            .map_err(|e| PlatformError::AudioError(format!("GetBuffer: {e}")))?;
                    }
                    let frames = num_frames as usize;
                    let byte_len = frames.saturating_mul(block_align);
                    let silent = (flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)) != 0;
                    let data = if silent || data_ptr.is_null() {
                        vec![0u8; byte_len]
                    } else {
                        // SAFETY: GetBuffer guarantees `byte_len` readable bytes until ReleaseBuffer.
                        unsafe { std::slice::from_raw_parts(data_ptr, byte_len) }.to_vec()
                    };
                    // SAFETY: Releases the buffer obtained from GetBuffer.
                    unsafe {
                        capture.ReleaseBuffer(num_frames).map_err(|e| {
                            PlatformError::AudioError(format!("ReleaseBuffer: {e}"))
                        })?;
                    }
                    Ok(Some(CapturePacket {
                        data,
                        frames,
                        silent,
                    }))
                })();
                let _ = reply.send(res);
            }
            CaptureCmd::Stop(reply) => {
                // SAFETY: Stops capture stream before releasing COM resources.
                let res = unsafe { client.Stop() }
                    .map_err(|e| PlatformError::AudioError(format!("Capture Stop: {e}")));
                let _ = reply.send(res);
                break;
            }
        }
    }
}
