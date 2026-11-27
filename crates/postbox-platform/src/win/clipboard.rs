//! Windows clipboard reader strictly quarantined inside `crates/postbox-platform/src/win/`.
//! Implements exact Postbox format precedence (`main.zig:10471-10622`):
//! 1. `CF_UNICODETEXT` / `CF_TEXT` -> `ClipboardPayload::Text` (never hijack text paste)
//! 2. `CF_HDROP` -> `ClipboardPayload::Files`
//! 3. Registered `"PNG"` format -> `ClipboardPayload::PngBytes`
//! 4. `CF_DIBV5` -> `ClipboardPayload::DibBytes`
//! 5. `CF_DIB` -> `ClipboardPayload::DibBytes`
//! 6. `CF_BITMAP` -> `ClipboardPayload::RgbaImage` via `GetDIBits`

use crate::PlatformError;
use crate::clipboard::{ClipboardPayload, MAX_CLIPBOARD_BYTES, MAX_CLIPBOARD_PIXELS};
use std::ffi::OsString;
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    GetDIBits, GetObjectW, HBITMAP, HDC, HGDIOBJ,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::core::w;

const CF_TEXT: u32 = 1;
const CF_BITMAP: u32 = 2;
const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;
const CF_DIBV5: u32 = 17;

struct ClipboardGuard;

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: `ClipboardGuard` is only constructed after a successful `OpenClipboard(None)`.
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

struct GlobalLockGuard(HGLOBAL);

impl Drop for GlobalLockGuard {
    fn drop(&mut self) {
        // SAFETY: `GlobalLockGuard` is only constructed after a non-null `GlobalLock(hglobal)`.
        unsafe {
            let _ = GlobalUnlock(self.0);
        }
    }
}

struct DcGuard(HDC);

impl Drop for DcGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: `self.0` was created by `CreateCompatibleDC(None)` and has not been deleted.
            unsafe {
                let _ = DeleteDC(self.0);
            }
        }
    }
}

/// SAFETY helper: locks `HGLOBAL` from clipboard handle and copies up to `MAX_CLIPBOARD_BYTES`.
unsafe fn copy_hglobal_bytes(handle: HANDLE) -> Option<Vec<u8>> {
    let hglobal = HGLOBAL(handle.0);
    // SAFETY: Caller verified `handle` came from `GetClipboardData` for an HGLOBAL-backed format.
    let size = unsafe { GlobalSize(hglobal) };
    if size == 0 || size > MAX_CLIPBOARD_BYTES {
        return None;
    }
    // SAFETY: `hglobal` is owned by the open clipboard for the duration of `ClipboardGuard`.
    let ptr = unsafe { GlobalLock(hglobal) };
    if ptr.is_null() {
        return None;
    }
    let _unlock = GlobalLockGuard(hglobal);
    // SAFETY: `GlobalLock` returned a valid readable pointer of at least `size` bytes.
    let slice = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), size) };
    Some(slice.to_vec())
}

/// SAFETY helper: queries all file paths from an `HDROP` clipboard handle.
unsafe fn read_hdrop_files(handle: HANDLE) -> Vec<PathBuf> {
    let hdrop = HDROP(handle.0);
    // SAFETY: `hdrop` is a valid `CF_HDROP` clipboard handle while clipboard is open.
    let count = unsafe { DragQueryFileW(hdrop, u32::MAX, None) };
    let mut files = Vec::with_capacity((count as usize).min(64));
    for idx in 0..count {
        // SAFETY: Querying required UTF-16 code unit count (excluding NUL).
        let len = unsafe { DragQueryFileW(hdrop, idx, None) };
        if len == 0 || len > 32_768 {
            continue;
        }
        let mut buf = vec![0u16; len as usize + 1];
        // SAFETY: `buf` has space for `len + 1` UTF-16 code units.
        let written = unsafe { DragQueryFileW(hdrop, idx, Some(&mut buf)) };
        if written > 0 {
            let os = OsString::from_wide(&buf[..written as usize]);
            files.push(PathBuf::from(os));
        }
    }
    files
}

/// SAFETY helper: extracts a top-down RGBA8888 buffer from a `CF_BITMAP` `HBITMAP`.
unsafe fn read_hbitmap_rgba(handle: HANDLE) -> Option<(u32, u32, Vec<u8>)> {
    let hbitmap = HBITMAP(handle.0);
    let mut bm = BITMAP::default();
    // SAFETY: `bm` is a valid mutable `BITMAP` struct; `hbitmap` is a clipboard GDI bitmap handle.
    let got = unsafe {
        GetObjectW(
            HGDIOBJ(hbitmap.0),
            size_of::<BITMAP>() as i32,
            Some((&mut bm as *mut BITMAP).cast()),
        )
    };
    if got == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
        return None;
    }
    let width = bm.bmWidth as u32;
    let height = bm.bmHeight as u32;
    let pixels = u64::from(width).checked_mul(u64::from(height))?;
    if pixels == 0 || pixels > MAX_CLIPBOARD_PIXELS {
        return None;
    }
    let byte_len = usize::try_from(pixels.checked_mul(4)?).ok()?;
    let mut bgra = vec![0u8; byte_len];

    // SAFETY: Creating a memory DC compatible with the screen; cleaned up by `DcGuard`.
    let hdc = unsafe { CreateCompatibleDC(None) };
    if hdc.is_invalid() {
        return None;
    }
    let _dc_guard = DcGuard(hdc);

    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: bm.bmWidth,
            // Negative height requests top-down scanline order.
            biHeight: -bm.bmHeight,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: Default::default(),
    };

    // SAFETY: `bgra` is sized to `width * height * 4` bytes matching 32bpp top-down `bmi`.
    let lines = unsafe {
        GetDIBits(
            hdc,
            hbitmap,
            0,
            height,
            Some(bgra.as_mut_ptr().cast()),
            &mut bmi,
            DIB_RGB_COLORS,
        )
    };
    if lines == 0 {
        return None;
    }

    // Convert BGRA -> RGBA and force opaque alpha (matching `main.zig:10607`).
    for px in bgra.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
        px[3] = 255;
    }
    Some((width, height, bgra))
}

pub fn read_clipboard_payload() -> Result<ClipboardPayload, PlatformError> {
    // SAFETY: Opening the thread clipboard with no owner HWND; closed unconditionally by `_guard`.
    unsafe {
        if OpenClipboard(None).is_err() {
            return Ok(ClipboardPayload::Empty);
        }
    }
    let _guard = ClipboardGuard;

    // 1. Text precedence: never hijack plain/rich text paste (`main.zig:10472-10473`).
    // SAFETY: Clipboard is open via `_guard`.
    let has_text = unsafe {
        IsClipboardFormatAvailable(CF_UNICODETEXT).is_ok()
            || IsClipboardFormatAvailable(CF_TEXT).is_ok()
    };
    if has_text {
        return Ok(ClipboardPayload::Text);
    }

    // 2. CF_HDROP (copied files from Explorer).
    // SAFETY: Clipboard is open via `_guard`.
    if unsafe { IsClipboardFormatAvailable(CF_HDROP).is_ok() }
        && let Ok(handle) = unsafe { GetClipboardData(CF_HDROP) }
    {
        let files = unsafe { read_hdrop_files(handle) };
        if !files.is_empty() {
            return Ok(ClipboardPayload::Files(files));
        }
    }

    // 3. Registered "PNG" clipboard format.
    // SAFETY: `w!("PNG")` is a static NUL-terminated UTF-16 literal.
    let png_fmt = unsafe { RegisterClipboardFormatW(w!("PNG")) };
    if png_fmt != 0
        && unsafe { IsClipboardFormatAvailable(png_fmt).is_ok() }
        && let Ok(handle) = unsafe { GetClipboardData(png_fmt) }
        && let Some(bytes) = unsafe { copy_hglobal_bytes(handle) }
    {
        return Ok(ClipboardPayload::PngBytes(bytes));
    }

    // 4. CF_DIBV5.
    if unsafe { IsClipboardFormatAvailable(CF_DIBV5).is_ok() }
        && let Ok(handle) = unsafe { GetClipboardData(CF_DIBV5) }
        && let Some(bytes) = unsafe { copy_hglobal_bytes(handle) }
    {
        return Ok(ClipboardPayload::DibBytes(bytes));
    }

    // 5. CF_DIB.
    if unsafe { IsClipboardFormatAvailable(CF_DIB).is_ok() }
        && let Ok(handle) = unsafe { GetClipboardData(CF_DIB) }
        && let Some(bytes) = unsafe { copy_hglobal_bytes(handle) }
    {
        return Ok(ClipboardPayload::DibBytes(bytes));
    }

    // 6. CF_BITMAP via GetDIBits.
    if unsafe { IsClipboardFormatAvailable(CF_BITMAP).is_ok() }
        && let Ok(handle) = unsafe { GetClipboardData(CF_BITMAP) }
        && let Some((width, height, rgba)) = unsafe { read_hbitmap_rgba(handle) }
    {
        return Ok(ClipboardPayload::RgbaImage {
            width,
            height,
            rgba,
        });
    }

    Ok(ClipboardPayload::Empty)
}
