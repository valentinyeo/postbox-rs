//! Windows Media Foundation + Intel Arc DXVA video backend (Milestone 6).
//!
//! Every `unsafe` block in the workspace lives in this module. The shape mirrors
//! `wazig/src/mf_lazy.zig`:
//!
//! - `mfplat.dll` / `mfreadwrite.dll` are loaded with `LoadLibraryExW` on the **first** video
//!   thumbnail or playback request, so a session that never opens a video touches neither the
//!   Media Foundation DLLs nor `MFStartup`. That keeps the M5.5 idle-RAM budget intact.
//! - Decoding goes through `IMFSourceReader` with an `IMFDXGIDeviceManager` built on D3D11
//!   (Intel Arc media engine / DXVA), advanced video processing for memory-backed RGB32
//!   output, and `MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS`. If the D3D11 device cannot be
//!   created (headless CI VM, RDP session, no Arc driver) the same reader is rebuilt in
//!   software with `MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING`.
//! - `.f4v` / `.f4p` files are handed to Media Foundation through an `.mp4` alias on disk
//!   (`video_alias.zig`).

use crate::PlatformError;
use crate::video::{
    THUMBNAIL_SEEK_MS, VideoAudioChunk, VideoBackend, VideoFrame, VideoMetadata,
    VideoPlaybackSession, downscale_rgba, fit_thumbnail_box,
};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
};
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFDXGIDeviceManager, IMFMediaType, IMFSample, IMFSourceReader,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_PD_DURATION,
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SOURCE_READER_ALL_STREAMS,
    MF_SOURCE_READER_D3D_MANAGER, MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_AUDIO_STREAM,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READER_MEDIASOURCE,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_VERSION, MFAudioFormat_Float, MFAudioFormat_PCM,
    MFMediaType_Audio, MFMediaType_Video, MFSTARTUP_FULL, MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::Win32::System::Variant::{VT_I8, VT_UI4, VT_UI8};
use windows::core::{GUID, HRESULT, HSTRING, Interface, PCSTR, PCWSTR};

/// `MF_SOURCE_READER_FIRST_VIDEO_STREAM` as the `DWORD` the API expects.
const FIRST_VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
/// `MF_SOURCE_READER_FIRST_AUDIO_STREAM` as the `DWORD` the API expects.
const FIRST_AUDIO: u32 = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
/// `MF_SOURCE_READER_ALL_STREAMS` as the `DWORD` the API expects (`-2`).
const ALL_STREAMS: u32 = MF_SOURCE_READER_ALL_STREAMS.0 as u32;
/// `MF_SOURCE_READER_MEDIASOURCE` as the `DWORD` the API expects (`-1`).
const MEDIA_SOURCE: u32 = MF_SOURCE_READER_MEDIASOURCE.0 as u32;
/// `MF_SOURCE_READERF_ENDOFSTREAM` flag value.
const END_OF_STREAM: u32 = MF_SOURCE_READERF_ENDOFSTREAM.0 as u32;
/// Media Foundation timestamps are 100 ns ticks.
const TICKS_PER_MS: u64 = 10_000;

// ---------------------------------------------------------------------------
// Lazy Media Foundation entry points (`mf_lazy.zig`)
// ---------------------------------------------------------------------------

type MfStartupFn = unsafe extern "system" fn(u32, u32) -> HRESULT;
type MfCreateAttributesFn = unsafe extern "system" fn(*mut *mut core::ffi::c_void, u32) -> HRESULT;
type MfCreateMediaTypeFn = unsafe extern "system" fn(*mut *mut core::ffi::c_void) -> HRESULT;
type MfCreateSourceReaderFromUrlFn = unsafe extern "system" fn(
    PCWSTR,
    *mut core::ffi::c_void,
    *mut *mut core::ffi::c_void,
) -> HRESULT;
type MfCreateDxgiDeviceManagerFn =
    unsafe extern "system" fn(*mut u32, *mut *mut core::ffi::c_void) -> HRESULT;

/// Lazily resolved `mfplat.dll` / `mfreadwrite.dll` entry points.
struct MfApi {
    create_attributes: MfCreateAttributesFn,
    create_media_type: MfCreateMediaTypeFn,
    create_source_reader: MfCreateSourceReaderFromUrlFn,
    create_dxgi_device_manager: MfCreateDxgiDeviceManagerFn,
}

static MF_API: OnceLock<Option<MfApi>> = OnceLock::new();
static D3D_DEVICE: OnceLock<Option<D3dDevice>> = OnceLock::new();

/// Whether Media Foundation is running in this process (test/telemetry observability).
pub fn mf_is_started() -> bool {
    MF_API.get().is_some_and(Option::is_some)
}

unsafe fn load_module(name: &str) -> Option<HMODULE> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let module = LoadLibraryExW(
            PCWSTR::from_raw(wide.as_ptr()),
            None,
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
        .ok()?;
        if module.is_invalid() {
            None
        } else {
            Some(module)
        }
    }
}

unsafe fn resolve<T: Copy>(module: HMODULE, name: &str) -> Option<T> {
    let mut symbol: Vec<u8> = name.as_bytes().to_vec();
    symbol.push(0);
    let address = unsafe { GetProcAddress(module, PCSTR::from_raw(symbol.as_ptr())) }?;
    Some(unsafe { std::mem::transmute_copy(&address) })
}

unsafe fn load_mf_api() -> Option<MfApi> {
    unsafe {
        // Media Foundation source readers are free-threaded; an MTA apartment is enough.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let plat = load_module("mfplat.dll")?;
        let readwrite = load_module("mfreadwrite.dll")?;

        let startup: MfStartupFn = resolve(plat, "MFStartup")?;
        let create_attributes: MfCreateAttributesFn = resolve(plat, "MFCreateAttributes")?;
        let create_media_type: MfCreateMediaTypeFn = resolve(plat, "MFCreateMediaType")?;
        let create_dxgi_device_manager: MfCreateDxgiDeviceManagerFn =
            resolve(plat, "MFCreateDXGIDeviceManager")?;
        let create_source_reader: MfCreateSourceReaderFromUrlFn =
            resolve(readwrite, "MFCreateSourceReaderFromURL")?;

        if !startup(MF_VERSION, MFSTARTUP_FULL).is_ok() {
            return None;
        }

        Some(MfApi {
            create_attributes,
            create_media_type,
            create_source_reader,
            create_dxgi_device_manager,
        })
    }
}

/// Starts Media Foundation once, on the first video request. `false` means the caller falls
/// back to no video support (a session that never opens a video never gets here).
fn ensure_started() -> bool {
    let resolved = MF_API.get_or_init(|| unsafe { load_mf_api() });
    resolved.is_some()
}

fn mf_api() -> Result<&'static MfApi, PlatformError> {
    if !ensure_started() {
        return Err(PlatformError::VideoError(
            "media_foundation_unavailable".into(),
        ));
    }
    MF_API
        .get()
        .and_then(Option::as_ref)
        .ok_or_else(|| PlatformError::VideoError("media_foundation_unavailable".into()))
}

fn hresult_error(context: &str, hr: HRESULT) -> PlatformError {
    PlatformError::VideoError(format!("{context}: hresult {:#010x}", hr.0))
}

// ---------------------------------------------------------------------------
// D3D11 + DXGI device manager (Intel Arc media engine)
// ---------------------------------------------------------------------------

/// Live D3D11 device plus the Media Foundation DXGI device manager bound to it.
struct D3dDevice {
    manager: IMFDXGIDeviceManager,
    _device: ID3D11Device,
    _context: ID3D11DeviceContext,
}

// SAFETY: an `ID3D11Device` is free-threaded by contract, and the Media Foundation DXGI device
// manager is a free-threaded COM object (`MFDXGIDeviceManager`), so one process-wide instance
// can be shared. The COM interface wrappers themselves are just reference-counted pointers.
unsafe impl Send for D3dDevice {}
unsafe impl Sync for D3dDevice {}

impl D3dDevice {
    fn create() -> Option<Self> {
        let api = mf_api().ok()?;
        unsafe {
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            let mut level = D3D_FEATURE_LEVEL_11_0;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )
            .ok()?;

            let mut token = 0u32;
            let mut manager_raw: *mut core::ffi::c_void = std::ptr::null_mut();
            let hr = (api.create_dxgi_device_manager)(&mut token, &mut manager_raw);
            hr.ok().ok()?;
            if manager_raw.is_null() {
                return None;
            }
            let manager = IMFDXGIDeviceManager::from_raw(manager_raw);

            let device = device?;
            let unknown = device.cast::<windows::core::IUnknown>().ok()?;
            if manager.ResetDevice(&unknown, token).is_err() {
                return None;
            }

            Some(Self {
                manager,
                _device: device,
                _context: context?,
            })
        }
    }
}

/// Process-wide D3D11 device, created on the first video request and reused afterwards.
fn d3d_device() -> Option<&'static D3dDevice> {
    D3D_DEVICE.get_or_init(D3dDevice::create).as_ref()
}

// ---------------------------------------------------------------------------
// Source reader construction
// ---------------------------------------------------------------------------

struct ReaderInit {
    reader: IMFSourceReader,
    hardware: bool,
}

fn wide_path(path: &Path) -> HSTRING {
    #[cfg(windows)]
    use std::os::windows::ffi::OsStrExt as _;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    HSTRING::from_wide(&wide[..wide.len().saturating_sub(1)])
}

fn make_attributes(
    api: &MfApi,
    hardware: Option<&D3dDevice>,
) -> Result<IMFAttributes, PlatformError> {
    unsafe {
        let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
        let hr = (api.create_attributes)(&mut raw, 4);
        if hr.is_err() || raw.is_null() {
            return Err(hresult_error("MFCreateAttributes", hr));
        }
        let attributes = IMFAttributes::from_raw(raw);
        match hardware {
            Some(device) => {
                // DXVA: hand the reader the Intel Arc device so the media engine decodes.
                attributes
                    .SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &device.manager)
                    .map_err(|e| PlatformError::VideoError(format!("set_d3d_manager: {e}")))?;
                attributes
                    .SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)
                    .map_err(|e| PlatformError::VideoError(format!("set_advanced_video: {e}")))?;
            }
            None => {
                attributes
                    .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
                    .map_err(|e| PlatformError::VideoError(format!("set_video_processing: {e}")))?;
            }
        }
        attributes
            .SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)
            .map_err(|e| PlatformError::VideoError(format!("set_hw_transforms: {e}")))?;
        Ok(attributes)
    }
}

fn open_reader(path: &Path) -> Result<ReaderInit, PlatformError> {
    let api = mf_api()?;
    let url = wide_path(path);

    // Preferred path: Intel Arc DXVA through the D3D11 device manager, then software.
    let hardware = d3d_device();
    for candidate in [hardware, None] {
        let attributes = make_attributes(api, candidate)?;
        unsafe {
            let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
            let hr = (api.create_source_reader)(
                PCWSTR::from_raw(url.as_ptr()),
                attributes.as_raw(),
                &mut raw,
            );
            if hr.is_err() || raw.is_null() {
                continue;
            }
            let reader = IMFSourceReader::from_raw(raw);
            if reader.SetStreamSelection(ALL_STREAMS, false).is_err() {
                continue;
            }
            return Ok(ReaderInit {
                reader,
                hardware: candidate.is_some(),
            });
        }
    }
    Err(PlatformError::VideoError(format!(
        "open_reader failed for {}",
        path.display()
    )))
}

fn make_media_type(api: &MfApi) -> Result<IMFMediaType, PlatformError> {
    unsafe {
        let mut raw: *mut core::ffi::c_void = std::ptr::null_mut();
        let hr = (api.create_media_type)(&mut raw);
        if hr.is_err() || raw.is_null() {
            return Err(hresult_error("MFCreateMediaType", hr));
        }
        Ok(IMFMediaType::from_raw(raw))
    }
}

fn frame_size(media_type: &IMFMediaType) -> Option<(u32, u32)> {
    let packed = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE).ok()? };
    let (width, height) = unpack_frame_size(packed);
    if width == 0 || height == 0 {
        None
    } else {
        Some((width, height))
    }
}

fn default_stride(media_type: &IMFMediaType) -> Option<i32> {
    let raw = unsafe { media_type.GetUINT32(&MF_MT_DEFAULT_STRIDE).ok()? };
    Some(raw as i32)
}

fn stream_duration_ms(reader: &IMFSourceReader) -> u64 {
    unsafe {
        match reader.GetPresentationAttribute(MEDIA_SOURCE, &MF_PD_DURATION) {
            Ok(value) => propvariant_u64(&value, TICKS_PER_MS),
            Err(_) => 0,
        }
    }
}

unsafe fn propvariant_u64(value: &PROPVARIANT, scale: u64) -> u64 {
    unsafe {
        let inner: &PROPVARIANT_0_0 = &value.Anonymous.Anonymous;
        let ticks = match inner.vt {
            VT_UI8 => inner.Anonymous.uhVal,
            VT_UI4 => u64::from(inner.Anonymous.ulVal),
            VT_I8 => u64::try_from(inner.Anonymous.hVal).unwrap_or(0),
            _ => 0,
        };
        ticks / scale.max(1)
    }
}

fn seek_reader(reader: &IMFSourceReader, pts_ms: u64) {
    let ticks = i64::try_from(pts_ms.saturating_mul(TICKS_PER_MS)).unwrap_or(i64::MAX);
    let position = PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_I8,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 { hVal: ticks },
            }),
        },
    };
    unsafe {
        let _ = reader.SetCurrentPosition(&GUID::zeroed(), &position);
        let _ = reader.Flush(ALL_STREAMS);
    }
}

// ---------------------------------------------------------------------------
// Frame conversion
// ---------------------------------------------------------------------------

/// Copies an RGB32 (BGRX byte order) buffer into packed RGBA8888, honouring a negative
/// (bottom-up) stride.
fn rgb32_to_rgba(buffer: &[u8], stride: i32, width: u32, height: u32) -> Option<Vec<u8>> {
    let rows = usize::try_from(height).ok()?;
    let row_bytes = usize::try_from(width).ok()?.checked_mul(4)?;
    if rows == 0 || row_bytes == 0 {
        return None;
    }
    let abs_stride = stride.unsigned_abs() as usize;
    if abs_stride < row_bytes {
        return None;
    }
    let needed = abs_stride.checked_mul(rows - 1)?.checked_add(row_bytes)?;
    if buffer.len() < needed {
        return None;
    }
    let bottom_up = stride < 0;
    let mut out = vec![0u8; row_bytes * rows];
    for row in 0..rows {
        let src_row = if bottom_up { rows - 1 - row } else { row };
        let src_start = src_row * abs_stride;
        let dst_start = row * row_bytes;
        for x in 0..usize::try_from(width).ok()? {
            let s = src_start + x * 4;
            let d = dst_start + x * 4;
            out[d] = buffer[s + 2];
            out[d + 1] = buffer[s + 1];
            out[d + 2] = buffer[s];
            out[d + 3] = 255;
        }
    }
    Some(out)
}

fn sample_bytes(sample: &IMFSample) -> Option<Vec<u8>> {
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer().ok()?;
        let mut data: *mut u8 = std::ptr::null_mut();
        let mut length = 0u32;
        if buffer.Lock(&mut data, None, Some(&mut length)).is_err() {
            return None;
        }
        if data.is_null() || length == 0 {
            let _ = buffer.Unlock();
            return None;
        }
        let bytes = std::slice::from_raw_parts(data, length as usize).to_vec();
        let _ = buffer.Unlock();
        Some(bytes)
    }
}

fn read_sample(
    reader: &IMFSourceReader,
    stream: u32,
) -> Result<Option<(IMFSample, u64)>, PlatformError> {
    unsafe {
        let mut actual_stream = 0u32;
        let mut flags = 0u32;
        let mut timestamp = 0i64;
        let mut sample: Option<IMFSample> = None;
        reader
            .ReadSample(
                stream,
                0,
                Some(&mut actual_stream),
                Some(&mut flags),
                Some(&mut timestamp),
                Some(&mut sample),
            )
            .map_err(|e| PlatformError::VideoError(format!("ReadSample: {e}")))?;
        if flags & END_OF_STREAM != 0 {
            return Ok(None);
        }
        let Some(sample) = sample else {
            return Ok(None);
        };
        Ok(Some((
            sample,
            ticks_to_ms(u64::try_from(timestamp).unwrap_or(0)),
        )))
    }
}

// ---------------------------------------------------------------------------
// Playback session
// ---------------------------------------------------------------------------

/// Media Foundation playback session streaming RGB32 frames and float32 PCM chunks.
pub struct MfVideoSession {
    reader: Option<IMFSourceReader>,
    metadata: VideoMetadata,
    width: u32,
    height: u32,
    stride: i32,
    audio_sample_rate: u32,
    audio_channels: u16,
    audio_float: bool,
    position_ms: u64,
    /// Frames decoded before `target_pts_ms` are dropped after a seek so the UI never shows a
    /// stale pre-seek frame.
    target_pts_ms: u64,
    closed: bool,
    _device: Option<&'static D3dDevice>,
}

impl MfVideoSession {
    fn open(path: &Path) -> Result<Self, PlatformError> {
        let init = open_reader(path)?;
        let api = mf_api()?;
        let reader = init.reader;

        // Video: ask for memory-backed RGB32 (the video processor copies DXVA surfaces down).
        let video_type = make_media_type(api)?;
        unsafe {
            video_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .and_then(|()| video_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32))
                .map_err(|e| PlatformError::VideoError(format!("video_media_type: {e}")))?;
            reader
                .SetStreamSelection(FIRST_VIDEO, true)
                .map_err(|e| PlatformError::VideoError(format!("select_video: {e}")))?;
            reader
                .SetCurrentMediaType(FIRST_VIDEO, None, &video_type)
                .map_err(|e| PlatformError::VideoError(format!("set_video_type: {e}")))?;
        }

        let current_video = unsafe { reader.GetCurrentMediaType(FIRST_VIDEO).ok() };
        let (width, height) = current_video
            .as_ref()
            .and_then(frame_size)
            .or_else(|| {
                unsafe { reader.GetNativeMediaType(FIRST_VIDEO, 0).ok() }
                    .as_ref()
                    .and_then(frame_size)
            })
            .ok_or_else(|| PlatformError::VideoError("frame_size_missing".into()))?;
        let stride = current_video
            .as_ref()
            .and_then(default_stride)
            .unwrap_or_else(|| i32::try_from(width.saturating_mul(4)).unwrap_or(i32::MAX));

        // Audio: float32 PCM when the decoder offers it, 16-bit PCM otherwise.
        let mut audio_sample_rate = 0u32;
        let mut audio_channels = 0u16;
        let mut audio_float = false;
        let has_audio_stream = unsafe { reader.GetNativeMediaType(FIRST_AUDIO, 0).is_ok() };
        if has_audio_stream {
            let audio_type = make_media_type(api)?;
            unsafe {
                let _ = audio_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio);
                let _ = audio_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float);
            }
            let float_ok = unsafe {
                reader.SetStreamSelection(FIRST_AUDIO, true).is_ok()
                    && reader
                        .SetCurrentMediaType(FIRST_AUDIO, None, &audio_type)
                        .is_ok()
            };
            let pcm_ok = if float_ok {
                true
            } else {
                unsafe {
                    let _ = audio_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM);
                    reader
                        .SetCurrentMediaType(FIRST_AUDIO, None, &audio_type)
                        .is_ok()
                }
            };
            if pcm_ok {
                audio_float = float_ok;
                if let Ok(current_audio) = unsafe { reader.GetCurrentMediaType(FIRST_AUDIO) } {
                    audio_sample_rate = unsafe {
                        current_audio
                            .GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)
                            .unwrap_or(0)
                    };
                    audio_channels = unsafe {
                        current_audio
                            .GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)
                            .unwrap_or(0)
                            .min(u32::from(u16::MAX)) as u16
                    };
                }
            } else {
                let _ = unsafe { reader.SetStreamSelection(FIRST_AUDIO, false) };
            }
        }

        let metadata = VideoMetadata {
            width,
            height,
            duration_ms: stream_duration_ms(&reader),
            has_audio: audio_sample_rate > 0 && audio_channels > 0,
            hardware_accelerated: init.hardware,
        };

        Ok(Self {
            reader: Some(reader),
            metadata,
            width,
            height,
            stride,
            audio_sample_rate,
            audio_channels,
            audio_float,
            position_ms: 0,
            target_pts_ms: 0,
            closed: false,
            _device: d3d_device(),
        })
    }

    fn decode_frame(&mut self, sample: &IMFSample, pts_ms: u64) -> Option<VideoFrame> {
        let bytes = sample_bytes(sample)?;
        let rgba = rgb32_to_rgba(&bytes, self.stride, self.width, self.height)?;
        self.position_ms = pts_ms;
        VideoFrame::new(self.width, self.height, pts_ms, rgba)
    }
}

impl VideoPlaybackSession for MfVideoSession {
    fn metadata(&self) -> VideoMetadata {
        self.metadata
    }

    fn next_video_frame(&mut self) -> Result<Option<VideoFrame>, PlatformError> {
        if self.closed {
            return Ok(None);
        }
        let Some(reader) = self.reader.clone() else {
            return Ok(None);
        };
        loop {
            let Some((sample, pts_ms)) = read_sample(&reader, FIRST_VIDEO)? else {
                return Ok(None);
            };
            if pts_ms.saturating_add(1) < self.target_pts_ms {
                continue;
            }
            if let Some(frame) = self.decode_frame(&sample, pts_ms) {
                return Ok(Some(frame));
            }
        }
    }

    fn next_audio_chunk(&mut self) -> Result<Option<VideoAudioChunk>, PlatformError> {
        if self.closed || !self.metadata.has_audio {
            return Ok(None);
        }
        let Some(reader) = self.reader.clone() else {
            return Ok(None);
        };
        let channels = self.audio_channels.max(1);
        loop {
            let Some((sample, pts_ms)) = read_sample(&reader, FIRST_AUDIO)? else {
                return Ok(None);
            };
            if pts_ms.saturating_add(1) < self.target_pts_ms {
                continue;
            }
            let Some(bytes) = sample_bytes(&sample) else {
                continue;
            };
            let pcm_f32: Vec<f32> = if self.audio_float {
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| f32::from_le_bytes(*c))
                    .collect()
            } else {
                bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| f32::from(i16::from_le_bytes(*c)) / 32_768.0)
                    .collect()
            };
            if pcm_f32.is_empty() {
                continue;
            }
            return Ok(Some(VideoAudioChunk {
                pts_ms,
                sample_rate: self.audio_sample_rate,
                channels,
                pcm_f32,
            }));
        }
    }

    fn seek_ms(&mut self, pts_ms: u64) -> Result<u64, PlatformError> {
        let target = if self.metadata.duration_ms == 0 {
            pts_ms
        } else {
            pts_ms.min(self.metadata.duration_ms)
        };
        if let Some(reader) = &self.reader {
            seek_reader(reader, target);
        }
        self.position_ms = target;
        self.target_pts_ms = target;
        Ok(target)
    }

    fn close(&mut self) {
        self.closed = true;
        // Dropping the source reader releases the decoder, its DXVA surfaces and the hardware
        // session, so no frame buffer survives into the next chat.
        self.reader = None;
    }
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

/// Extracts one still frame for the video bubble thumbnail (`PREVIEW_MAX_W` x `PREVIEW_MAX_H`).
pub fn extract_thumbnail(
    path: &Path,
    max_w: u32,
    max_h: u32,
) -> Result<Option<VideoFrame>, PlatformError> {
    let media_path = alias_path_for_mf(path);
    let mut session = MfVideoSession::open(&media_path)?;

    if session.metadata.duration_ms > 0 {
        let seek_to = THUMBNAIL_SEEK_MS.min(session.metadata.duration_ms.saturating_sub(1));
        let _ = session.seek_ms(seek_to);
    }

    // The seek lands on the keyframe at or after the ~1 s target; its first decoded frame is
    // the still the thumbnail cache stores.
    let mut frame = session.next_video_frame()?;
    // Some containers refuse to seek; fall back to the first decodable frame.
    if frame.is_none() {
        let _ = session.seek_ms(0);
        frame = session.next_video_frame()?;
    }
    session.close();

    let Some(frame) = frame else {
        return Ok(None);
    };
    let (fit_w, fit_h) = fit_thumbnail_box(frame.width, frame.height, max_w, max_h);
    let rgba = downscale_rgba(&frame.rgba, frame.width, frame.height, fit_w, fit_h)
        .ok_or_else(|| PlatformError::VideoError("thumbnail_downscale_failed".into()))?;
    Ok(VideoFrame::new(fit_w, fit_h, frame.pts_ms, rgba))
}

/// `.f4v` / `.f4p` files get an `.mp4` sibling so Media Foundation recognises the container.
pub(crate) fn alias_path_for_mf(path: &Path) -> PathBuf {
    if !needs_mp4_alias(path) {
        return path.to_path_buf();
    }
    let alias = mp4_alias_path(path);
    if alias.is_file() {
        return alias;
    }
    match std::fs::copy(path, &alias) {
        Ok(_) => alias,
        Err(_) => path.to_path_buf(),
    }
}

impl VideoBackend for super::WindowsPlatform {
    fn extract_thumbnail(
        &self,
        path: &Path,
        max_w: u32,
        max_h: u32,
    ) -> Result<Option<VideoFrame>, PlatformError> {
        extract_thumbnail(path, max_w, max_h)
    }

    fn open_session(&self, path: &Path) -> Result<Box<dyn VideoPlaybackSession>, PlatformError> {
        let media_path = alias_path_for_mf(path);
        Ok(Box::new(MfVideoSession::open(&media_path)?))
    }

    fn hardware_acceleration_available(&self) -> bool {
        d3d_device().is_some()
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (tested on every platform)
// ---------------------------------------------------------------------------

/// Decodes a Media Foundation `MF_MT_FRAME_SIZE` packed `UINT64` (high 32 bits = width).
pub const fn unpack_frame_size(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, (packed & 0xffff_ffff) as u32)
}

/// Converts Media Foundation 100 ns ticks to milliseconds.
pub const fn ticks_to_ms(ticks: u64) -> u64 {
    ticks / TICKS_PER_MS
}

/// `.f4v` / `.f4p` are MP4-shaped but Media Foundation's byte-stream handler rejects the
/// extension, so those two need an `.mp4` alias (`video_alias.zig:9-20`).
pub fn needs_mp4_alias(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some("f4v") | Some("f4p")
    )
}

/// `clip.f4v` -> `clip.f4v.mp4` (appended, so the original bytes stay untouched).
pub fn mp4_alias_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".mp4");
    PathBuf::from(os)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb32_top_down_and_bottom_up_convert_to_rgba() {
        // Two rows of one pixel: top row (10,20,30), bottom row (40,50,60).
        let top_down = [10u8, 20, 30, 0, 40, 50, 60, 0];
        let rgba = rgb32_to_rgba(&top_down, 4, 1, 2).unwrap();
        assert_eq!(rgba, vec![30, 20, 10, 255, 60, 50, 40, 255]);

        let bottom_up = [40u8, 50, 60, 0, 10, 20, 30, 0];
        let rgba = rgb32_to_rgba(&bottom_up, -4, 1, 2).unwrap();
        assert_eq!(rgba, vec![30, 20, 10, 255, 60, 50, 40, 255]);
    }

    #[test]
    fn rgb32_rejects_short_buffers_and_narrow_strides() {
        assert!(rgb32_to_rgba(&[1; 4], 2, 1, 1).is_none());
        assert!(rgb32_to_rgba(&[1; 8], 4, 2, 1).is_none());
        assert!(rgb32_to_rgba(&[0; 8], 8, 2, 1).is_some());
    }

    #[test]
    fn frame_size_unpacks_and_ticks_convert_to_ms() {
        let packed = (1920u64 << 32) | 1080u64;
        assert_eq!(unpack_frame_size(packed), (1920, 1080));
        assert_eq!(ticks_to_ms(10_000), 1);
        assert_eq!(ticks_to_ms(45_000), 4);
    }

    #[test]
    fn f4v_and_f4p_get_an_appended_mp4_alias_only() {
        assert!(needs_mp4_alias(Path::new("C:/m/clip.f4v")));
        assert!(needs_mp4_alias(Path::new("clip.F4P")));
        assert!(!needs_mp4_alias(Path::new("clip.mp4")));
        assert!(!needs_mp4_alias(Path::new("clip")));
        assert_eq!(
            mp4_alias_path(Path::new("C:/m/clip.f4v")),
            PathBuf::from("C:/m/clip.f4v.mp4")
        );
    }
}
