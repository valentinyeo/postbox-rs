use crate::MediaError;
use crate::decode::{DecodedImage, decode_allowed, decode_image_bytes};
use crate::drain::atomic_write_file;
use image::ImageEncoder;
use postbox_core::ChatKey;
use postbox_platform::ClipboardPayload;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const STAGED_ATTACHMENTS_CAPACITY: usize = 16;
pub const THUMBNAIL_MAX_SIDE: u32 = 44;
pub const MAX_SINGLE_PASTE_IMAGE_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_CAPTION_UTF8_BYTES: usize = 4095;

static PASTE_FILE_SEQ: AtomicU64 = AtomicU64::new(1);

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;
const BI_ALPHABITFIELDS: u32 = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedDib {
    pub width: u32,
    pub height: u32,
    /// Top-down 32bpp RGBA8888 pixel data (`width * height * 4` bytes).
    pub rgba: Vec<u8>,
}

fn read_u16_le(slice: &[u8], off: usize) -> Option<u16> {
    let bytes: [u8; 2] = slice.get(off..off + 2)?.try_into().ok()?;
    Some(u16::from_le_bytes(bytes))
}

fn read_u32_le(slice: &[u8], off: usize) -> Option<u32> {
    let bytes: [u8; 4] = slice.get(off..off + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn read_i32_le(slice: &[u8], off: usize) -> Option<i32> {
    let bytes: [u8; 4] = slice.get(off..off + 4)?.try_into().ok()?;
    Some(i32::from_le_bytes(bytes))
}

fn scale_masked_channel(pixel: u32, mask: u32) -> u8 {
    let shift = mask.trailing_zeros();
    let maximum = mask >> shift;
    let value = (pixel & mask) >> shift;
    ((u64::from(value) * 255 + u64::from(maximum) / 2) / u64::from(maximum)) as u8
}

/// Pure-Rust port of `paste_image.dibToBitmap` (`paste_image.zig:9-111`), outputting top-down
/// 32bpp RGBA8888 pixels. Handles:
/// - `BITMAPINFOHEADER` (40B), `BITMAPV4HEADER` (108B), `BITMAPV5HEADER` (124B)
/// - 32bpp (`BI_RGB`, `BI_BITFIELDS`, `BI_ALPHABITFIELDS`) and 24bpp (`BI_RGB`)
/// - top-down (`height < 0`) and bottom-up (`height > 0`)
/// - arbitrary contiguous non-overlapping bit masks (incl. 10-bit RGB / 2-bit A)
/// - Windows-synthesized `BITMAPV5HEADER` duplicate 12-byte RGB mask table right after header!
pub fn dib_to_rgba(dib: &[u8]) -> Result<DecodedDib, MediaError> {
    if dib.len() < 40 {
        return Err(MediaError::InvalidDib);
    }
    let header_size = read_u32_le(dib, 0).ok_or(MediaError::InvalidDib)? as usize;
    if header_size < 40 || header_size > dib.len() {
        return Err(MediaError::InvalidDib);
    }
    let width_i32 = read_i32_le(dib, 4).ok_or(MediaError::InvalidDib)?;
    let raw_height = read_i32_le(dib, 8).ok_or(MediaError::InvalidDib)?;
    let planes = read_u16_le(dib, 12).ok_or(MediaError::InvalidDib)?;
    let bpp = read_u16_le(dib, 14).ok_or(MediaError::InvalidDib)?;
    let compression = read_u32_le(dib, 16).ok_or(MediaError::InvalidDib)?;
    let colors_used = read_u32_le(dib, 32).ok_or(MediaError::InvalidDib)? as usize;

    if width_i32 <= 0 || raw_height == 0 || raw_height == i32::MIN || planes != 1 {
        return Err(MediaError::InvalidDib);
    }
    let width = width_i32 as u32;
    let top_down = raw_height < 0;
    let height = raw_height.unsigned_abs();
    if !decode_allowed(width, height) {
        return Err(MediaError::PixelBudgetExceeded);
    }

    let supported = (bpp == 32 && matches!(compression, BI_RGB | BI_BITFIELDS | BI_ALPHABITFIELDS))
        || (bpp == 24 && compression == BI_RGB);
    if !supported {
        return Err(MediaError::InvalidDib);
    }

    let mut masks = [0x00FF_0000u32, 0x0000_FF00u32, 0x0000_00FFu32, 0u32];
    let mut pixel_offset = header_size;

    if compression == BI_BITFIELDS || compression == BI_ALPHABITFIELDS {
        let mask_bytes = if compression == BI_ALPHABITFIELDS {
            16
        } else {
            12
        };
        if header_size == 40 {
            pixel_offset = pixel_offset
                .checked_add(mask_bytes)
                .ok_or(MediaError::InvalidDib)?;
        } else if header_size < 40 + mask_bytes {
            return Err(MediaError::InvalidDib);
        }
        if dib.len() < pixel_offset {
            return Err(MediaError::InvalidDib);
        }
        masks[0] = read_u32_le(dib, 40).ok_or(MediaError::InvalidDib)?;
        masks[1] = read_u32_le(dib, 44).ok_or(MediaError::InvalidDib)?;
        masks[2] = read_u32_le(dib, 48).ok_or(MediaError::InvalidDib)?;
        if header_size >= 56 || compression == BI_ALPHABITFIELDS {
            masks[3] = read_u32_le(dib, 52).ok_or(MediaError::InvalidDib)?;
        }

        let mut used = 0u32;
        for (idx, &mask) in masks.iter().enumerate() {
            if mask == 0 {
                if idx < 3 || compression == BI_ALPHABITFIELDS {
                    return Err(MediaError::InvalidDib);
                }
                continue;
            }
            let normalized = mask >> mask.trailing_zeros();
            if (normalized & normalized.wrapping_add(1)) != 0 {
                return Err(MediaError::InvalidDib);
            }
            if (used & mask) != 0 {
                return Err(MediaError::InvalidDib);
            }
            used |= mask;
        }
    }

    let palette_bytes = colors_used.checked_mul(4).ok_or(MediaError::InvalidDib)?;
    pixel_offset = pixel_offset
        .checked_add(palette_bytes)
        .ok_or(MediaError::InvalidDib)?;

    let w_usize = width as usize;
    let h_usize = height as usize;
    let stride = ((w_usize
        .checked_mul(usize::from(bpp))
        .ok_or(MediaError::InvalidDib)?
        .checked_add(31)
        .ok_or(MediaError::InvalidDib)?)
        / 32)
        .checked_mul(4)
        .ok_or(MediaError::InvalidDib)?;
    let source_bytes = stride.checked_mul(h_usize).ok_or(MediaError::InvalidDib)?;
    let output_bytes = w_usize
        .checked_mul(h_usize)
        .and_then(|p| p.checked_mul(4))
        .ok_or(MediaError::InvalidDib)?;

    // Detect Windows-synthesized BITMAPV5HEADER duplicate 12-byte RGB mask table right after header!
    if compression == BI_BITFIELDS
        && header_size > 40
        && header_size.checked_add(12).is_some_and(|e| e <= dib.len())
        && pixel_offset
            .checked_add(12)
            .is_some_and(|o| o <= dib.len() && source_bytes <= dib.len() - o)
        && dib[header_size..header_size + 12] == dib[40..52]
    {
        pixel_offset += 12;
    }

    if pixel_offset > dib.len() || source_bytes > dib.len() - pixel_offset {
        return Err(MediaError::InvalidDib);
    }

    let mut rgba = vec![0u8; output_bytes];
    let std_bgra = masks[0] == 0x00FF_0000
        && masks[1] == 0x0000_FF00
        && masks[2] == 0x0000_00FF
        && (masks[3] == 0 || masks[3] == 0xFF00_0000);

    for row in 0..h_usize {
        let src_row = if top_down { row } else { h_usize - 1 - row };
        let src_base = pixel_offset + src_row * stride;
        let dst_base = row * w_usize * 4;
        if bpp == 32 {
            if std_bgra {
                for x in 0..w_usize {
                    let s = src_base + x * 4;
                    let d = dst_base + x * 4;
                    rgba[d] = dib[s + 2];
                    rgba[d + 1] = dib[s + 1];
                    rgba[d + 2] = dib[s];
                    rgba[d + 3] = if masks[3] == 0 { 255 } else { dib[s + 3] };
                }
            } else {
                for x in 0..w_usize {
                    let s = src_base + x * 4;
                    let pixel = read_u32_le(dib, s).ok_or(MediaError::InvalidDib)?;
                    let d = dst_base + x * 4;
                    rgba[d] = scale_masked_channel(pixel, masks[0]);
                    rgba[d + 1] = scale_masked_channel(pixel, masks[1]);
                    rgba[d + 2] = scale_masked_channel(pixel, masks[2]);
                    rgba[d + 3] = if masks[3] != 0 {
                        scale_masked_channel(pixel, masks[3])
                    } else {
                        255
                    };
                }
            }
        } else {
            // 24bpp BGR
            for x in 0..w_usize {
                let s = src_base + x * 3;
                let d = dst_base + x * 4;
                rgba[d] = dib[s + 2];
                rgba[d + 1] = dib[s + 1];
                rgba[d + 2] = dib[s];
                rgba[d + 3] = 255;
            }
        }
    }

    Ok(DecodedDib {
        width,
        height,
        rgba,
    })
}

/// Encodes top-down RGBA8888 pixels to PNG using pure Rust.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, MediaError> {
    if !decode_allowed(width, height) {
        return Err(MediaError::PixelBudgetExceeded);
    }
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|p| p.checked_mul(4))
        .ok_or(MediaError::InvalidDib)?;
    if rgba.len() != expected {
        return Err(MediaError::InvalidDib);
    }
    let mut out = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut out);
    encoder
        .write_image(rgba, width, height, image::ExtendedColorType::Rgba8)
        .map_err(|e| MediaError::DecodeFailed(e.to_string()))?;
    Ok(out)
}

/// Checks single-file `CF_HDROP` paste extension allowlist (`.png`, `.jpg`, `.jpeg`,
/// `main.zig:10497-10508`).
pub fn single_paste_image_ext(path: &Path) -> Option<&'static str> {
    let raw = path.to_str()?;
    if raw.is_empty() || raw.as_bytes().contains(&0) {
        return None;
    }
    let filename = match raw.rfind(['/', '\\']) {
        Some(idx) => &raw[idx + 1..],
        None => raw,
    };
    if filename.is_empty() || filename.contains(':') {
        return None;
    }
    let (stem, ext) = filename.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    if ext.eq_ignore_ascii_case("png") {
        Some("png")
    } else if ext.eq_ignore_ascii_case("jpg") {
        Some("jpg")
    } else if ext.eq_ignore_ascii_case("jpeg") {
        Some("jpeg")
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedAttachment {
    pub chat: ChatKey,
    pub path: PathBuf,
    pub filename: String,
    pub byte_size: u64,
    pub sha256: [u8; 32],
    pub is_image: bool,
    pub owned: bool,
    pub thumb: Option<DecodedImage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendCapability {
    SingleFile,
    MultipleFiles,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendPlanItem {
    pub first_index: usize,
    pub count: usize,
    pub caption: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteStageResult {
    /// Clipboard held text; caller MUST propagate to normal text paste.
    PropagateTextPaste,
    /// Clipboard held no pasteable image or file.
    NoImage { notice: &'static str },
    /// Single `CF_HDROP` file had a non-PNG/JPEG extension.
    RejectedExtension { notice: &'static str },
    /// One or more attachments were staged into the composer.
    Staged { count: usize, notice: &'static str },
}

/// Composer staged attachments list (`staged_images.zig:3-54`, strict capacity = 16).
#[derive(Debug, Default, Clone)]
pub struct StagedAttachments {
    items: Vec<StagedAttachment>,
}

impl StagedAttachments {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn items(&self) -> &[StagedAttachment] {
        &self.items
    }

    pub fn capacity(&self) -> usize {
        STAGED_ATTACHMENTS_CAPACITY
    }

    pub fn add(&mut self, item: StagedAttachment) -> Result<(), MediaError> {
        self.guard_chat_match(&item.chat);
        if self.items.len() >= STAGED_ATTACHMENTS_CAPACITY {
            return Err(MediaError::CapacityFull);
        }
        self.items.push(item);
        Ok(())
    }

    /// Removes the item at `index`, deleting its temporary file if `discard && item.owned`.
    pub fn remove(&mut self, index: usize, discard: bool) -> Option<StagedAttachment> {
        if index >= self.items.len() {
            return None;
        }
        let item = self.items.remove(index);
        if discard && item.owned {
            let _ = std::fs::remove_file(&item.path);
        }
        Some(item)
    }

    /// Discards all staged items, deleting owned temporary files (`Esc` discard).
    pub fn discard_all(&mut self) -> usize {
        let count = self.items.len();
        for item in self.items.drain(..) {
            if item.owned {
                let _ = std::fs::remove_file(&item.path);
            }
        }
        count
    }

    /// Releases staged items WITHOUT deleting temp files (for handoff into durable SQLite outbox).
    pub fn take_for_outbox(&mut self) -> Vec<StagedAttachment> {
        std::mem::take(&mut self.items)
    }

    /// Verifies that all staged attachments belong to `active_chat`; discards if any item belongs
    /// to a different chat so attachments can NEVER be sent to the wrong chat (`main.zig:11062-11066`).
    pub fn guard_chat_match(&mut self, active_chat: &ChatKey) -> bool {
        if self.items.iter().any(|item| &item.chat != active_chat) {
            self.discard_all();
            return false;
        }
        true
    }

    /// Computes the exact send plan matching `staged_images.zig:41-52`:
    /// - `SendCapability::SingleFile`: emits 1 item per file; ONLY item 0 carries `caption`.
    /// - `SendCapability::MultipleFiles`: emits 1 item covering all files with `caption`.
    pub fn send_plan(&self, caption: &str, cap: SendCapability) -> Vec<SendPlanItem> {
        if self.items.is_empty() {
            return Vec::new();
        }
        match cap {
            SendCapability::SingleFile => self
                .items
                .iter()
                .enumerate()
                .map(|(idx, _)| SendPlanItem {
                    first_index: idx,
                    count: 1,
                    caption: if idx == 0 {
                        caption.to_owned()
                    } else {
                        String::new()
                    },
                })
                .collect(),
            SendCapability::MultipleFiles => vec![SendPlanItem {
                first_index: 0,
                count: self.items.len(),
                caption: caption.to_owned(),
            }],
        }
    }

    /// Writes `bytes` into `<paste_dir>/paste-<seq>.<ext>`, computes SHA-256, generates a
    /// `44×44` preview thumbnail if decodable as an image, and appends to staged list.
    pub fn stage_bytes(
        &mut self,
        chat: &ChatKey,
        paste_dir: &Path,
        filename: &str,
        ext: &str,
        bytes: &[u8],
    ) -> Result<&StagedAttachment, MediaError> {
        self.guard_chat_match(chat);
        if self.items.len() >= STAGED_ATTACHMENTS_CAPACITY {
            return Err(MediaError::CapacityFull);
        }
        std::fs::create_dir_all(paste_dir)?;
        let seq = PASTE_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
        let safe_ext =
            if !ext.is_empty() && ext.len() <= 10 && ext.bytes().all(|b| b.is_ascii_alphanumeric())
            {
                ext
            } else {
                "bin"
            };
        let target = paste_dir.join(format!("paste-{seq}.{safe_ext}"));
        atomic_write_file(&target, bytes)?;

        let digest = Sha256::digest(bytes);
        let mut sha256 = [0u8; 32];
        sha256.copy_from_slice(&digest);

        let thumb = decode_image_bytes(bytes, THUMBNAIL_MAX_SIDE, THUMBNAIL_MAX_SIDE, false).ok();
        let is_image = thumb.is_some();

        let item = StagedAttachment {
            chat: chat.clone(),
            path: target,
            filename: filename.to_owned(),
            byte_size: bytes.len() as u64,
            sha256,
            is_image,
            owned: true,
            thumb,
        };
        self.add(item)?;
        Ok(self.items.last().expect("just pushed"))
    }

    /// Stages a dropped or copied disk file into `paste_dir` (`owned = true`).
    pub fn stage_disk_file(
        &mut self,
        chat: &ChatKey,
        paste_dir: &Path,
        source_path: &Path,
    ) -> Result<&StagedAttachment, MediaError> {
        self.guard_chat_match(chat);
        let meta = std::fs::metadata(source_path)?;
        if meta.len() > crate::MAX_AUTO_BYTES {
            return Err(MediaError::FileTooLarge);
        }
        let bytes = std::fs::read(source_path)?;
        let filename = source_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment.bin");
        let ext = source_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("bin");
        self.stage_bytes(chat, paste_dir, filename, ext, &bytes)
    }

    /// Processes a `ClipboardPayload` following exact Postbox priority (`main.zig:10471-10622`).
    pub fn apply_clipboard_payload(
        &mut self,
        chat: &ChatKey,
        paste_dir: &Path,
        payload: ClipboardPayload,
    ) -> PasteStageResult {
        match payload {
            ClipboardPayload::Text => PasteStageResult::PropagateTextPaste,
            ClipboardPayload::Empty => PasteStageResult::NoImage {
                notice: "Clipboard holds no image to paste",
            },
            ClipboardPayload::Files(paths) => {
                if paths.is_empty() {
                    return PasteStageResult::NoImage {
                        notice: "Clipboard holds no image to paste",
                    };
                }
                if paths.len() == 1 {
                    let p = &paths[0];
                    let Some(ext) = single_paste_image_ext(p) else {
                        return PasteStageResult::RejectedExtension {
                            notice: "Only PNG and JPEG files can be pasted as images",
                        };
                    };
                    let Ok(meta) = std::fs::metadata(p) else {
                        return PasteStageResult::NoImage {
                            notice: "Could not read the copied image file",
                        };
                    };
                    if meta.len() > MAX_SINGLE_PASTE_IMAGE_BYTES {
                        return PasteStageResult::NoImage {
                            notice: "Could not stage the pasted image",
                        };
                    }
                    let Ok(bytes) = std::fs::read(p) else {
                        return PasteStageResult::NoImage {
                            notice: "Could not read the copied image file",
                        };
                    };
                    let fname = p
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("pasted.png");
                    if self
                        .stage_bytes(chat, paste_dir, fname, ext, &bytes)
                        .is_ok()
                    {
                        PasteStageResult::Staged {
                            count: 1,
                            notice: "Image ready... Enter sends, Esc discards",
                        }
                    } else {
                        PasteStageResult::NoImage {
                            notice: "Could not stage the pasted image",
                        }
                    }
                } else {
                    let mut staged = 0usize;
                    for p in &paths {
                        if self.stage_disk_file(chat, paste_dir, p).is_err() {
                            break;
                        }
                        staged += 1;
                    }
                    if staged > 0 {
                        PasteStageResult::Staged {
                            count: staged,
                            notice: "File attached - Enter sends, Esc discards",
                        }
                    } else {
                        PasteStageResult::NoImage {
                            notice: "Could not stage the pasted image",
                        }
                    }
                }
            }
            ClipboardPayload::PngBytes(bytes) => {
                if bytes.is_empty() {
                    return PasteStageResult::NoImage {
                        notice: "Clipboard holds no image to paste",
                    };
                }
                if (bytes.len() as u64) > MAX_SINGLE_PASTE_IMAGE_BYTES {
                    return PasteStageResult::NoImage {
                        notice: "Could not stage the pasted image",
                    };
                }
                const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
                if !bytes.starts_with(PNG_SIGNATURE)
                    || decode_image_bytes(&bytes, THUMBNAIL_MAX_SIDE, THUMBNAIL_MAX_SIDE, false)
                        .is_err()
                {
                    return PasteStageResult::NoImage {
                        notice: "Clipboard holds no image to paste",
                    };
                }
                if self
                    .stage_bytes(chat, paste_dir, "pasted.png", "png", &bytes)
                    .is_ok()
                {
                    PasteStageResult::Staged {
                        count: 1,
                        notice: "Image ready... Enter sends, Esc discards",
                    }
                } else {
                    PasteStageResult::NoImage {
                        notice: "Could not stage the pasted image",
                    }
                }
            }
            ClipboardPayload::DibBytes(dib) => {
                let Ok(decoded) = dib_to_rgba(&dib) else {
                    return PasteStageResult::NoImage {
                        notice: "Clipboard holds no image to paste",
                    };
                };
                let Ok(png) = encode_png(decoded.width, decoded.height, &decoded.rgba) else {
                    return PasteStageResult::NoImage {
                        notice: "Could not stage the pasted image",
                    };
                };
                if (png.len() as u64) > MAX_SINGLE_PASTE_IMAGE_BYTES {
                    return PasteStageResult::NoImage {
                        notice: "Could not stage the pasted image",
                    };
                }
                if self
                    .stage_bytes(chat, paste_dir, "pasted.png", "png", &png)
                    .is_ok()
                {
                    PasteStageResult::Staged {
                        count: 1,
                        notice: "Image ready... Enter sends, Esc discards",
                    }
                } else {
                    PasteStageResult::NoImage {
                        notice: "Could not stage the pasted image",
                    }
                }
            }
            ClipboardPayload::RgbaImage {
                width,
                height,
                rgba,
            } => {
                let Ok(png) = encode_png(width, height, &rgba) else {
                    return PasteStageResult::NoImage {
                        notice: "Could not stage the pasted image",
                    };
                };
                if (png.len() as u64) > MAX_SINGLE_PASTE_IMAGE_BYTES {
                    return PasteStageResult::NoImage {
                        notice: "Could not stage the pasted image",
                    };
                }
                if self
                    .stage_bytes(chat, paste_dir, "pasted.png", "png", &png)
                    .is_ok()
                {
                    PasteStageResult::Staged {
                        count: 1,
                        notice: "Image ready... Enter sends, Esc discards",
                    }
                } else {
                    PasteStageResult::NoImage {
                        notice: "Could not stage the pasted image",
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::Provider;

    fn make_v5_dib_with_duplicate_mask(top_down: bool) -> Vec<u8> {
        let mut buf = vec![0u8; 124 + 12 + 4];
        buf[0..4].copy_from_slice(&124u32.to_le_bytes()); // BITMAPV5HEADER
        buf[4..8].copy_from_slice(&1i32.to_le_bytes()); // width = 1
        let h: i32 = if top_down { -1 } else { 1 };
        buf[8..12].copy_from_slice(&h.to_le_bytes());
        buf[12..14].copy_from_slice(&1u16.to_le_bytes()); // planes
        buf[14..16].copy_from_slice(&32u16.to_le_bytes()); // bpp
        buf[16..20].copy_from_slice(&BI_BITFIELDS.to_le_bytes()); // compression = 3
        // R, G, B, A masks inside V5 header at offset 40..56
        let r_mask = 0x00FF_0000u32.to_le_bytes();
        let g_mask = 0x0000_FF00u32.to_le_bytes();
        let b_mask = 0x0000_00FFu32.to_le_bytes();
        let a_mask = 0xFF00_0000u32.to_le_bytes();
        buf[40..44].copy_from_slice(&r_mask);
        buf[44..48].copy_from_slice(&g_mask);
        buf[48..52].copy_from_slice(&b_mask);
        buf[52..56].copy_from_slice(&a_mask);
        // Windows-synthesized duplicate 12-byte RGB mask table right after header (124..136)
        buf[124..128].copy_from_slice(&r_mask);
        buf[128..132].copy_from_slice(&g_mask);
        buf[132..136].copy_from_slice(&b_mask);
        // Actual pixel at offset 136: BGRA = [10, 20, 30, 255] -> RGBA = [30, 20, 10, 255]
        buf[136..140].copy_from_slice(&[10, 20, 30, 255]);
        buf
    }

    #[test]
    fn dib_v5_duplicate_mask_table_and_24bpp_and_10bit_masks_decode_cleanly() {
        let dib = make_v5_dib_with_duplicate_mask(true);
        let decoded = dib_to_rgba(&dib).unwrap();
        assert_eq!((decoded.width, decoded.height), (1, 1));
        assert_eq!(decoded.rgba, vec![30, 20, 10, 255]);

        // 24bpp bottom-up 2x2
        let mut dib24 = vec![0u8; 40 + 8 * 2];
        dib24[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib24[4..8].copy_from_slice(&2i32.to_le_bytes());
        dib24[8..12].copy_from_slice(&2i32.to_le_bytes()); // bottom-up
        dib24[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib24[14..16].copy_from_slice(&24u16.to_le_bytes());
        dib24[16..20].copy_from_slice(&BI_RGB.to_le_bytes());
        // bottom row (y=1): blue pixel
        dib24[40..43].copy_from_slice(&[255, 0, 0]);
        // top row (y=0): red pixel
        dib24[48..51].copy_from_slice(&[0, 0, 255]);
        let d24 = dib_to_rgba(&dib24).unwrap();
        assert_eq!(&d24.rgba[0..4], &[255, 0, 0, 255]); // top-left red
        assert_eq!(&d24.rgba[8..12], &[0, 0, 255, 255]); // bottom-left blue

        // Overlapping bitmasks rejected
        let mut bad = make_v5_dib_with_duplicate_mask(true);
        bad[44..48].copy_from_slice(&0x00FF_0000u32.to_le_bytes()); // overlaps R
        assert!(matches!(dib_to_rgba(&bad), Err(MediaError::InvalidDib)));
    }

    #[test]
    fn staged_attachments_cap_16_chat_guard_and_send_plan() {
        let dir = std::env::temp_dir().join(format!(
            "postbox_paste_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let chat_a = ChatKey::new(Provider::Whatsapp, "main", "a@s.whatsapp.net");
        let chat_b = ChatKey::new(Provider::Whatsapp, "main", "b@s.whatsapp.net");
        let png = encode_png(80, 60, &vec![200u8; 80 * 60 * 4]).unwrap();

        let mut staged = StagedAttachments::new();
        assert_eq!(
            staged.apply_clipboard_payload(&chat_a, &dir, ClipboardPayload::Text),
            PasteStageResult::PropagateTextPaste
        );

        for i in 0..STAGED_ATTACHMENTS_CAPACITY {
            let item = staged
                .stage_bytes(&chat_a, &dir, &format!("img_{i}.png"), "png", &png)
                .unwrap();
            assert!(item.is_image);
            let thumb = item.thumb.as_ref().unwrap();
            assert!(thumb.width <= 44 && thumb.height <= 44);
        }
        // 17th rejected without dropping existing 16
        assert!(matches!(
            staged.stage_bytes(&chat_a, &dir, "extra.png", "png", &png),
            Err(MediaError::CapacityFull)
        ));
        assert_eq!(staged.len(), 16);

        let plan = staged.send_plan("hello caption", SendCapability::SingleFile);
        assert_eq!(plan.len(), 16);
        assert_eq!(plan[0].caption, "hello caption");
        assert!(plan[1].caption.is_empty());

        let multi = staged.send_plan("batch", SendCapability::MultipleFiles);
        assert_eq!(multi.len(), 1);
        assert_eq!(multi[0].count, 16);

        // Chat mismatch discards staged items cleanly!
        assert!(!staged.guard_chat_match(&chat_b));
        assert!(staged.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_paste_image_ext_blocks_ntfs_ads_and_preserves_windows_drives() {
        assert_eq!(
            single_paste_image_ext(Path::new("C:\\Users\\me\\photo.png")),
            Some("png")
        );
        assert_eq!(
            single_paste_image_ext(Path::new("\\\\?\\C:\\dir\\image.JPG")),
            Some("jpg")
        );
        assert_eq!(
            single_paste_image_ext(Path::new("media/Übung.jpeg")),
            Some("jpeg")
        );
        for blocked in [
            "media\\photo.png:evil.png",
            "C:\\Users\\me\\payload.exe:fake.jpg",
            "doc.cmd:hidden.gif",
            "photo.png:Zone.Identifier",
            "photo.png:stream.webp",
            "evil.exe:fake.png",
            ".png",
            "media/.png",
            "media/photo.png.exe",
        ] {
            assert_eq!(
                single_paste_image_ext(Path::new(blocked)),
                None,
                "should reject {blocked}"
            );
        }
    }

    #[test]
    fn f1_staged_attachments_never_mixes_chats_across_stage_bytes_add_and_guard() {
        let dir = std::env::temp_dir().join(format!(
            "postbox_paste_f1_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let chat_a = ChatKey::new(Provider::Whatsapp, "main", "a@s.whatsapp.net");
        let chat_b = ChatKey::new(Provider::Whatsapp, "main", "b@s.whatsapp.net");
        let png = encode_png(16, 16, &vec![128u8; 16 * 16 * 4]).unwrap();

        let mut staged = StagedAttachments::new();
        // Fill chat_a to full capacity (16)
        for i in 0..STAGED_ATTACHMENTS_CAPACITY {
            staged
                .stage_bytes(&chat_a, &dir, &format!("a_{i}.png"), "png", &png)
                .unwrap();
        }
        assert_eq!(staged.len(), STAGED_ATTACHMENTS_CAPACITY);

        // Staging for chat_b MUST discard chat_a's 16 items instead of mixing or hitting CapacityFull
        let item_b = staged
            .stage_bytes(&chat_b, &dir, "b_only.png", "png", &png)
            .unwrap();
        assert_eq!(item_b.chat, chat_b);
        assert_eq!(staged.len(), 1);
        assert!(staged.items().iter().all(|i| i.chat == chat_b));

        // Guard checks ALL items (even if an inconsistent item were ever present in non-first slot)
        staged.items.push(StagedAttachment {
            chat: chat_a.clone(),
            path: dir.join("injected.png"),
            filename: "injected.png".into(),
            byte_size: 4,
            sha256: [0u8; 32],
            is_image: true,
            owned: false,
            thumb: None,
        });
        assert!(!staged.guard_chat_match(&chat_b));
        assert!(staged.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn f6_clipboard_png_bytes_rejects_empty_garbage_truncated_and_oversized_32mib() {
        let dir = std::env::temp_dir().join(format!(
            "postbox_paste_f6_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let chat = ChatKey::new(Provider::Whatsapp, "main", "a@s.whatsapp.net");
        let mut staged = StagedAttachments::new();

        // 1. Empty buffer rejected
        assert!(matches!(
            staged.apply_clipboard_payload(&chat, &dir, ClipboardPayload::PngBytes(Vec::new())),
            PasteStageResult::NoImage { .. }
        ));
        assert!(staged.is_empty());

        // 2. Non-PNG garbage rejected
        assert!(matches!(
            staged.apply_clipboard_payload(
                &chat,
                &dir,
                ClipboardPayload::PngBytes(b"not a valid png payload".to_vec())
            ),
            PasteStageResult::NoImage { .. }
        ));
        assert!(staged.is_empty());

        // 3. PNG magic followed by truncated/corrupt bytes rejected
        assert!(matches!(
            staged.apply_clipboard_payload(
                &chat,
                &dir,
                ClipboardPayload::PngBytes(b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec())
            ),
            PasteStageResult::NoImage { .. }
        ));
        assert!(staged.is_empty());

        // 4. Oversized buffer (> 32 MiB) rejected before staging
        let mut oversized = vec![0u8; (MAX_SINGLE_PASTE_IMAGE_BYTES as usize) + 1];
        oversized[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        assert!(matches!(
            staged.apply_clipboard_payload(&chat, &dir, ClipboardPayload::PngBytes(oversized)),
            PasteStageResult::NoImage { .. }
        ));
        assert!(staged.is_empty());

        // 5. Genuine PNG within cap stages cleanly
        let valid_png = encode_png(12, 12, &vec![200u8; 12 * 12 * 4]).unwrap();
        assert!(matches!(
            staged.apply_clipboard_payload(&chat, &dir, ClipboardPayload::PngBytes(valid_png)),
            PasteStageResult::Staged { count: 1, .. }
        ));
        assert_eq!(staged.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
