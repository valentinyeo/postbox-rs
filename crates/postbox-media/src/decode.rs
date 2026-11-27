use crate::{MAX_DECODE_PIXELS, MediaError, PREVIEW_MAX_H, PREVIEW_MAX_W};
use image::codecs::gif::GifDecoder;
use image::codecs::webp::WebPDecoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Maximum raw image file read size (32 MiB, `main.zig:7056`).
pub const MAX_IMAGE_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// Maximum animated GIF decoded frames kept in RAM per bubble so a single GIF cannot blow budget.
pub const MAX_GIF_FRAMES: usize = 64;

/// Exact port of `webp.fitBox` (`webp.zig:12-24`): fits `(src_w, src_h)` inside `(max_w, max_h)`
/// preserving aspect ratio with NO upscaling, clamping zero dimensions to `1`.
pub fn fit_box(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 || max_w == 0 || max_h == 0 {
        return (1, 1);
    }
    let mut width = src_w.min(max_w);
    let mut height = (u64::from(src_h) * u64::from(width)) / u64::from(src_w);
    if height > u64::from(max_h) {
        height = u64::from(max_h);
        width = ((u64::from(src_w) * u64::from(max_h)) / u64::from(src_h)).max(1) as u32;
    }
    if height == 0 {
        height = 1;
    }
    (width.max(1), height as u32)
}

/// Exact port of `media_layout.fit` (`media_layout.zig:5-13`) for paint/layout-time bubble fitting.
pub fn media_layout_fit(src_w: i32, src_h: i32, max_w: i32, max_h: i32) -> (i32, i32) {
    if src_w <= 0 || src_h <= 0 || max_w <= 0 || max_h <= 0 {
        return (1, 1);
    }
    let w = i64::from(src_w.min(max_w));
    let h = i64::from(src_h.min(max_h));
    let sw = i64::from(src_w);
    let sh = i64::from(src_h);
    if w.saturating_mul(sh) <= h.saturating_mul(sw) {
        let out_h = (w.saturating_mul(sh) / sw).max(1) as i32;
        (w as i32, out_h)
    } else {
        let out_w = (h.saturating_mul(sw) / sh).max(1) as i32;
        (out_w, h as i32)
    }
}

/// Pixel-bomb guard (`webp.zig:7-10`): strictly checked BEFORE allocating pixel buffers!
/// Allows up to `MAX_DECODE_PIXELS` (`24 MiB / 4 = 6_291_456` pixels, e.g. `3072×2048` OK,
/// `3072×2049` rejected, `16383×16383` rejected).
pub const fn decode_allowed(width: u32, height: u32) -> bool {
    if width == 0 || height == 0 {
        return false;
    }
    match (width as u64).checked_mul(height as u64) {
        Some(pixels) => pixels <= MAX_DECODE_PIXELS,
        None => false,
    }
}

/// Detects RIFF WEBP container magic (`webp.zig:3-5`).
pub fn is_webp_bytes(data: &[u8]) -> bool {
    data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP"
}

/// Detects GIF87a / GIF89a magic.
pub fn is_gif_bytes(data: &[u8]) -> bool {
    data.len() >= 6 && (&data[0..6] == b"GIF87a" || &data[0..6] == b"GIF89a")
}

/// Extracts the first `VP8 `/`VP8L` bitstream from an animated WebP `ANMF` chunk (`webp.zig:26-56`).
pub fn first_webp_animation_frame(data: &[u8]) -> Option<&[u8]> {
    if !is_webp_bytes(data) {
        return None;
    }
    let mut offset = 12usize;
    while offset + 8 <= data.len() {
        let fourcc = &data[offset..offset + 4];
        let size = u32::from_le_bytes(data[offset + 4..offset + 8].try_into().ok()?) as usize;
        let payload_start = offset + 8;
        if size > data.len().saturating_sub(payload_start) {
            return None;
        }
        if fourcc == b"ANMF" {
            let chunk_end = payload_start + size;
            let mut inner = payload_start.checked_add(16)?;
            if inner + 8 > chunk_end {
                return None;
            }
            if &data[inner..inner + 4] == b"ALPH" {
                let alph_size =
                    u32::from_le_bytes(data[inner + 4..inner + 8].try_into().ok()?) as usize;
                let step = 8usize.checked_add(alph_size)?.checked_add(alph_size & 1)?;
                inner = inner.checked_add(step)?;
                if inner + 8 > chunk_end {
                    return None;
                }
            }
            if &data[inner..inner + 3] == b"VP8" {
                let bs_size =
                    u32::from_le_bytes(data[inner + 4..inner + 8].try_into().ok()?) as usize;
                let bs_start = inner + 8;
                let bs_end = bs_start.checked_add(bs_size)?;
                if bs_end <= chunk_end {
                    return Some(&data[bs_start..bs_end]);
                }
            }
            return None;
        }
        let step = 8usize.checked_add(size)?.checked_add(size & 1)?;
        offset = offset.checked_add(step)?;
    }
    None
}

/// A single decoded 32bpp BGRA image frame ready for `gpui::RenderImage` upload (R and B already
/// swapped to BGRA so Windows DirectX/wgpu renders accurate colors!).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    pub delay_ms: u32,
}

impl DecodedFrame {
    pub fn byte_size(&self) -> usize {
        self.bgra.len()
    }
}

/// Decoded image or animated GIF with exact byte accounting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedImage {
    pub src_width: u32,
    pub src_height: u32,
    pub width: u32,
    pub height: u32,
    pub frames: Vec<DecodedFrame>,
    pub is_animated_gif: bool,
}

impl DecodedImage {
    pub fn byte_size(&self) -> usize {
        self.frames.iter().map(DecodedFrame::byte_size).sum()
    }

    pub fn first_frame(&self) -> Option<&DecodedFrame> {
        self.frames.first()
    }
}

/// Converts RGBA8888 pixels in-place to BGRA8888 (required by GPUI `RenderImage`).
pub fn rgba_to_bgra_in_place(pixels: &mut [u8]) {
    for px in pixels.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
}

fn strict_image_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_DECODE_PIXELS * 4 * 2);
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits
}

fn downscale_rgba_to_bgra_frame(
    img: DynamicImage,
    max_w: u32,
    max_h: u32,
    allow_upscale_2x: bool,
    delay_ms: u32,
) -> DecodedFrame {
    let (sw, sh) = (img.width().max(1), img.height().max(1));
    let (tw, th) = if allow_upscale_2x {
        crate::lightbox::viewer_target_size(sw, sh, max_w as i32, max_h as i32)
    } else {
        fit_box(sw, sh, max_w, max_h)
    };
    let resized = if tw != sw || th != sh {
        img.resize_exact(tw, th, FilterType::Triangle)
    } else {
        img
    };
    let mut buf = resized.into_rgba8().into_raw();
    rgba_to_bgra_in_place(&mut buf);
    DecodedFrame {
        width: tw,
        height: th,
        bgra: buf,
        delay_ms,
    }
}

/// Decodes raw PNG, JPEG, WebP (static + animated first frame), GIF (static + animated), BMP, or
/// ICO bytes using 100% pure-Rust codecs, enforcing `decode_allowed` BEFORE full allocation.
pub fn decode_image_bytes(
    bytes: &[u8],
    max_w: u32,
    max_h: u32,
    allow_upscale_2x: bool,
) -> Result<DecodedImage, MediaError> {
    if bytes.is_empty() {
        return Err(MediaError::UnsupportedFormat);
    }
    if bytes.len() as u64 > MAX_IMAGE_FILE_BYTES {
        return Err(MediaError::FileTooLarge);
    }

    // 1. Animated / static GIF fast path (preserves animation frames capped by byte budget).
    if is_gif_bytes(bytes) {
        return decode_gif_bytes(bytes, max_w, max_h, allow_upscale_2x);
    }

    // 2. Static & animated WebP (pure-Rust `image-webp`).
    if is_webp_bytes(bytes) {
        return decode_webp_bytes(bytes, max_w, max_h, allow_upscale_2x);
    }

    // 3. PNG / JPEG / BMP / ICO via pure-Rust `image` reader with header dimension check first.
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| MediaError::DecodeFailed(e.to_string()))?;
    reader.limits(strict_image_limits());

    let format = reader.format().ok_or(MediaError::UnsupportedFormat)?;
    if !matches!(
        format,
        ImageFormat::Png
            | ImageFormat::Jpeg
            | ImageFormat::Gif
            | ImageFormat::WebP
            | ImageFormat::Bmp
            | ImageFormat::Ico
    ) {
        return Err(MediaError::UnsupportedFormat);
    }

    let (src_w, src_h) = reader
        .into_dimensions()
        .map_err(|e| MediaError::DecodeFailed(e.to_string()))?;
    if !decode_allowed(src_w, src_h) {
        return Err(MediaError::PixelBudgetExceeded);
    }

    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(strict_image_limits());
    let img = reader
        .decode()
        .map_err(|e| MediaError::DecodeFailed(e.to_string()))?;
    let frame = downscale_rgba_to_bgra_frame(img, max_w, max_h, allow_upscale_2x, 0);
    let (w, h) = (frame.width, frame.height);
    Ok(DecodedImage {
        src_width: src_w,
        src_height: src_h,
        width: w,
        height: h,
        frames: vec![frame],
        is_animated_gif: false,
    })
}

fn decode_gif_bytes(
    bytes: &[u8],
    max_w: u32,
    max_h: u32,
    allow_upscale_2x: bool,
) -> Result<DecodedImage, MediaError> {
    let decoder =
        GifDecoder::new(Cursor::new(bytes)).map_err(|e| MediaError::DecodeFailed(e.to_string()))?;
    let (src_w, src_h) = decoder.dimensions();
    if !decode_allowed(src_w, src_h) {
        return Err(MediaError::PixelBudgetExceeded);
    }
    let mut frames = Vec::new();
    let mut total_bytes = 0usize;
    let max_anim_bytes = 8 * 1024 * 1024;
    for frame_res in decoder.into_frames() {
        if frames.len() >= MAX_GIF_FRAMES {
            break;
        }
        let frame = frame_res.map_err(|e| MediaError::DecodeFailed(e.to_string()))?;
        let (num, den) = frame.delay().numer_denom_ms();
        let delay_ms = num.checked_div(den).map_or(120, |ms| ms.max(20));
        let dyn_img = DynamicImage::ImageRgba8(frame.into_buffer());
        let decoded =
            downscale_rgba_to_bgra_frame(dyn_img, max_w, max_h, allow_upscale_2x, delay_ms);
        total_bytes = total_bytes.saturating_add(decoded.byte_size());
        frames.push(decoded);
        if total_bytes >= max_anim_bytes {
            break;
        }
    }
    if frames.is_empty() {
        return Err(MediaError::DecodeFailed("empty gif".into()));
    }
    let (w, h) = (frames[0].width, frames[0].height);
    let is_animated = frames.len() > 1;
    Ok(DecodedImage {
        src_width: src_w,
        src_height: src_h,
        width: w,
        height: h,
        frames,
        is_animated_gif: is_animated,
    })
}

fn decode_webp_bytes(
    bytes: &[u8],
    max_w: u32,
    max_h: u32,
    allow_upscale_2x: bool,
) -> Result<DecodedImage, MediaError> {
    let decoder = WebPDecoder::new(Cursor::new(bytes))
        .map_err(|e| MediaError::DecodeFailed(e.to_string()))?;
    let (src_w, src_h) = decoder.dimensions();
    if !decode_allowed(src_w, src_h) {
        return Err(MediaError::PixelBudgetExceeded);
    }

    let img = if decoder.has_animation() {
        // Extract first frame of animated WebP safely (`webp.zig:26-56`).
        let mut frames = decoder.into_frames();
        let first = frames
            .next()
            .ok_or_else(|| MediaError::DecodeFailed("empty animated webp".into()))?
            .map_err(|e| MediaError::DecodeFailed(e.to_string()))?;
        DynamicImage::ImageRgba8(first.into_buffer())
    } else {
        DynamicImage::from_decoder(decoder).map_err(|e| MediaError::DecodeFailed(e.to_string()))?
    };

    let frame = downscale_rgba_to_bgra_frame(img, max_w, max_h, allow_upscale_2x, 0);
    let (w, h) = (frame.width, frame.height);
    Ok(DecodedImage {
        src_width: src_w,
        src_height: src_h,
        width: w,
        height: h,
        frames: vec![frame],
        is_animated_gif: false,
    })
}

/// Convenience helper decoding an image file from disk fitted to `420×250` inline preview box.
pub fn decode_preview_file(path: &Path) -> Result<DecodedImage, MediaError> {
    let meta = std::fs::metadata(path)?;
    if meta.len() > MAX_IMAGE_FILE_BYTES {
        return Err(MediaError::FileTooLarge);
    }
    let bytes = std::fs::read(path)?;
    decode_image_bytes(&bytes, PREVIEW_MAX_W, PREVIEW_MAX_H, false)
}

/// Recommended off-UI-thread decode concurrency (`min(4, max(1, cpus/2))`).
pub fn recommended_decode_workers() -> usize {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    (cpus / 2).clamp(1, 4)
}

/// Reads (capped at `MAX_IMAGE_FILE_BYTES`) and decodes an image file to BGRA8888 frames.
pub fn decode_file_sync(
    path: &Path,
    max_w: u32,
    max_h: u32,
    allow_upscale_2x: bool,
) -> Result<DecodedImage, MediaError> {
    let meta = std::fs::metadata(path)?;
    if meta.len() > MAX_IMAGE_FILE_BYTES {
        return Err(MediaError::FileTooLarge);
    }
    let bytes = std::fs::read(path)?;
    decode_image_bytes(&bytes, max_w, max_h, allow_upscale_2x)
}

/// Decodes a chat avatar image file and applies bilinear resample + AA circle mask
/// (`scale_circle_avatar`) at `target_size × target_size` BGRA8888.
pub fn decode_circle_avatar_file(
    path: &Path,
    target_size: usize,
) -> Result<DecodedImage, MediaError> {
    let decoded = decode_file_sync(path, 256, 256, false)?;
    let first = decoded
        .frames
        .first()
        .ok_or_else(|| MediaError::DecodeFailed("empty avatar".into()))?;
    let bgra = crate::avatar::scale_circle_avatar(
        &first.bgra,
        first.width as usize,
        first.height as usize,
        target_size,
    );
    if bgra.is_empty() {
        return Err(MediaError::DecodeFailed("avatar mask failed".into()));
    }
    let side = target_size as u32;
    Ok(DecodedImage {
        src_width: decoded.src_width,
        src_height: decoded.src_height,
        width: side,
        height: side,
        frames: vec![DecodedFrame {
            width: side,
            height: side,
            bgra,
            delay_ms: 0,
        }],
        is_animated_gif: false,
    })
}

/// Decodes a Slack workspace icon, center-crops to square if needed, area-resamples via
/// `scale_square_area`, and applies SDF rounded-rect AA masking (`apply_rounded_rect_mask`).
pub fn decode_workspace_icon_file(
    path: &Path,
    target_size: usize,
    radius: f32,
) -> Result<DecodedImage, MediaError> {
    let decoded = decode_file_sync(path, 256, 256, false)?;
    let first = decoded
        .frames
        .first()
        .ok_or_else(|| MediaError::DecodeFailed("empty workspace icon".into()))?;
    let sw = first.width as usize;
    let sh = first.height as usize;
    let side = sw.min(sh);
    if side == 0 {
        return Err(MediaError::DecodeFailed("zero workspace icon".into()));
    }
    let square_buf: std::borrow::Cow<'_, [u8]> = if sw == sh {
        std::borrow::Cow::Borrowed(&first.bgra)
    } else {
        let x0 = (sw - side) / 2;
        let y0 = (sh - side) / 2;
        let mut cropped = vec![0u8; side * side * 4];
        for y in 0..side {
            let src_off = ((y0 + y) * sw + x0) * 4;
            let dst_off = y * side * 4;
            cropped[dst_off..dst_off + side * 4]
                .copy_from_slice(&first.bgra[src_off..src_off + side * 4]);
        }
        std::borrow::Cow::Owned(cropped)
    };
    let mut bgra = crate::avatar::scale_square_area(&square_buf, side, target_size);
    if bgra.is_empty() {
        return Err(MediaError::DecodeFailed(
            "workspace icon scale failed".into(),
        ));
    }
    crate::avatar::apply_rounded_rect_mask(&mut bgra, target_size, radius);
    let out_side = target_size as u32;
    Ok(DecodedImage {
        src_width: decoded.src_width,
        src_height: decoded.src_height,
        width: out_side,
        height: out_side,
        frames: vec![DecodedFrame {
            width: out_side,
            height: out_side,
            bgra,
            delay_ms: 0,
        }],
        is_animated_gif: false,
    })
}

/// Bounded off-UI-thread decode pool (`min(4, max(1, cpus/2))` concurrent workers).
#[derive(Clone)]
pub struct DecodePool {
    semaphore: Arc<Semaphore>,
}

impl Default for DecodePool {
    fn default() -> Self {
        Self::new()
    }
}

impl DecodePool {
    pub fn new() -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(recommended_decode_workers())),
        }
    }

    pub async fn decode_file(
        &self,
        path: std::path::PathBuf,
        max_w: u32,
        max_h: u32,
        allow_upscale_2x: bool,
    ) -> Result<DecodedImage, MediaError> {
        let _permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| MediaError::Cancelled)?;
        tokio::task::spawn_blocking(move || decode_file_sync(&path, max_w, max_h, allow_upscale_2x))
            .await
            .map_err(|e| MediaError::DecodeFailed(e.to_string()))?
    }
}

struct LruEntry<V> {
    value: V,
    bytes: usize,
    last_used: u64,
}

/// Strict byte-budgeted LRU cache returning evicted `(K, V)` pairs so callers (e.g., GPUI
/// `PostboxShell`) can deterministically call `cx.drop_image(evicted, Some(window))` and keep
/// decoded RAM `<= 48 MB` and GPU VRAM `<= 64 MB`.
pub struct BitmapLru<K, V> {
    budget_bytes: usize,
    total_bytes: usize,
    clock: u64,
    entries: HashMap<K, LruEntry<V>>,
    order: VecDeque<(K, u64)>,
}

impl<K, V> BitmapLru<K, V>
where
    K: Eq + Hash + Clone,
{
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            budget_bytes,
            total_bytes: 0,
            clock: 0,
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub fn budget_bytes(&self) -> usize {
        self.budget_bytes
    }

    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn contains(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    pub fn get(&mut self, key: &K) -> Option<&V> {
        self.clock += 1;
        let tick = self.clock;
        if let Some(entry) = self.entries.get_mut(key) {
            entry.last_used = tick;
            self.order.push_back((key.clone(), tick));
            self.compact_order_if_needed();
            self.entries.get(key).map(|e| &e.value)
        } else {
            None
        }
    }

    pub fn peek<Q>(&self, key: &Q) -> Option<&V>
    where
        K: std::borrow::Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.entries.get(key).map(|e| &e.value)
    }

    /// Inserts or updates `key` with `byte_size` and evicts strictly oldest-used entries until
    /// `total_bytes <= budget_bytes`, returning all evicted `(K, V)` items (including any prior
    /// value displaced by overwriting an existing key).
    pub fn insert(&mut self, key: K, value: V, byte_size: usize) -> Vec<(K, V)> {
        self.clock += 1;
        let tick = self.clock;
        let mut evicted = Vec::new();
        if let Some((old_key, old)) = self.entries.remove_entry(&key) {
            self.total_bytes = self.total_bytes.saturating_sub(old.bytes);
            evicted.push((old_key, old.value));
        }
        self.total_bytes = self.total_bytes.saturating_add(byte_size);
        self.entries.insert(
            key.clone(),
            LruEntry {
                value,
                bytes: byte_size,
                last_used: tick,
            },
        );
        self.order.push_back((key, tick));
        self.compact_order_if_needed();
        evicted.extend(self.evict_to_budget());
        evicted
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        let removed = self.entries.remove(key)?;
        self.total_bytes = self.total_bytes.saturating_sub(removed.bytes);
        Some(removed.value)
    }

    pub fn clear(&mut self) -> Vec<(K, V)> {
        self.total_bytes = 0;
        self.order.clear();
        self.entries.drain().map(|(k, e)| (k, e.value)).collect()
    }

    fn evict_to_budget(&mut self) -> Vec<(K, V)> {
        let mut evicted = Vec::new();
        while self.total_bytes > self.budget_bytes && !self.entries.is_empty() {
            let Some((candidate_key, candidate_tick)) = self.order.pop_front() else {
                break;
            };
            let is_live = self
                .entries
                .get(&candidate_key)
                .is_some_and(|e| e.last_used == candidate_tick);
            if !is_live {
                continue;
            }
            if let Some(removed) = self.entries.remove(&candidate_key) {
                self.total_bytes = self.total_bytes.saturating_sub(removed.bytes);
                evicted.push((candidate_key, removed.value));
            }
        }
        evicted
    }

    fn compact_order_if_needed(&mut self) {
        if self.order.len() > self.entries.len().saturating_mul(4).max(128) {
            let entries = &self.entries;
            self.order
                .retain(|(k, tick)| entries.get(k).is_some_and(|e| e.last_used == *tick));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MAX_RAM_BITMAP_CAP_BYTES, RAM_BITMAP_BUDGET_BYTES, VRAM_TEXTURE_BUDGET_BYTES};
    use image::{ImageEncoder, RgbaImage};

    fn make_png(w: u32, h: u32, r: u8, g: u8, b: u8) -> Vec<u8> {
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for px in rgba.as_chunks_mut::<4>().0 {
            px[0] = r;
            px[1] = g;
            px[2] = b;
            px[3] = 255;
        }
        let mut out = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut out);
        encoder
            .write_image(&rgba, w, h, image::ExtendedColorType::Rgba8)
            .unwrap();
        out
    }

    #[test]
    fn fit_box_and_media_layout_fit_match_exact_vectors() {
        assert_eq!(fit_box(2100, 1250, 420, 250), (420, 250));
        assert_eq!(fit_box(840, 1250, 420, 250), (168, 250));
        assert_eq!(fit_box(2100, 1000, 420, 250), (420, 200));
        assert_eq!(fit_box(40, 25, 420, 250), (40, 25)); // no upscale
        assert_eq!(fit_box(0, 0, 420, 250), (1, 1));
        assert_eq!(fit_box(1, 10_000, 420, 250), (1, 250));

        assert_eq!(media_layout_fit(756, 550, 596, 250), (343, 250));
        assert_eq!(media_layout_fit(720, 1280, 596, 250), (140, 250));
        assert_eq!(media_layout_fit(100, 80, 596, 250), (100, 80));
        assert_eq!(media_layout_fit(756, 550, 176, 250), (176, 128));
        assert_eq!(media_layout_fit(1, i32::MAX, 596, 250), (1, 250));
        assert_eq!(media_layout_fit(i32::MAX, 1, 596, 250), (596, 1));
    }

    #[test]
    fn pixel_bomb_guard_rejects_oversized_dimensions_before_alloc() {
        assert!(decode_allowed(3072, 2048));
        assert!(!decode_allowed(3072, 2049));
        assert!(!decode_allowed(16383, 16383));
        assert!(!decode_allowed(0, 100));

        // Craft a valid PNG IHDR chunk claiming 4000x4000 (= 16M pixels > 6,291,456)
        // so `into_dimensions()` parses (4000, 4000) and our guard rejects BEFORE pixel alloc!
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(b"IHDR");
        ihdr.extend_from_slice(&4000u32.to_be_bytes());
        ihdr.extend_from_slice(&4000u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in &ihdr {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        crc = !crc;

        let mut fake_png = Vec::new();
        fake_png.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]); // PNG sig
        fake_png.extend_from_slice(&13u32.to_be_bytes()); // IHDR len
        fake_png.extend_from_slice(&ihdr);
        fake_png.extend_from_slice(&crc.to_be_bytes());
        // Minimal 0-byte IDAT chunk + IEND so PNG metadata reader stops after IHDR and returns (4000, 4000).
        fake_png.extend_from_slice(&0u32.to_be_bytes());
        fake_png.extend_from_slice(b"IDAT");
        fake_png.extend_from_slice(&0x35AF_061Eu32.to_be_bytes());
        fake_png.extend_from_slice(&0u32.to_be_bytes());
        fake_png.extend_from_slice(b"IEND");
        fake_png.extend_from_slice(&0xAE42_6082u32.to_be_bytes());
        let err = decode_image_bytes(&fake_png, 420, 250, false).unwrap_err();
        assert!(
            matches!(err, MediaError::PixelBudgetExceeded),
            "expected PixelBudgetExceeded, got {err:?}"
        );
    }

    #[test]
    fn decodes_png_jpeg_webp_and_animated_gif_to_bgra() {
        // PNG (red pixel -> BGRA [0, 0, 255, 255])
        let png = make_png(600, 300, 255, 10, 20);
        let decoded = decode_image_bytes(&png, 420, 250, false).unwrap();
        assert_eq!((decoded.width, decoded.height), (420, 210));
        assert_eq!(&decoded.frames[0].bgra[0..4], &[20, 10, 255, 255]);

        // JPEG
        let mut jpg = Vec::new();
        let dyn_img = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            120,
            80,
            image::Rgba([0, 200, 0, 255]),
        ));
        dyn_img
            .to_rgb8()
            .write_to(&mut Cursor::new(&mut jpg), ImageFormat::Jpeg)
            .unwrap();
        let decoded_jpg = decode_image_bytes(&jpg, 420, 250, false).unwrap();
        assert_eq!((decoded_jpg.width, decoded_jpg.height), (120, 80));

        // WebP (lossless pure Rust via image-webp)
        let mut webp = Vec::new();
        dyn_img
            .write_to(&mut Cursor::new(&mut webp), ImageFormat::WebP)
            .unwrap();
        assert!(is_webp_bytes(&webp));
        let decoded_webp = decode_image_bytes(&webp, 420, 250, false).unwrap();
        assert_eq!((decoded_webp.width, decoded_webp.height), (120, 80));

        // 2-frame Animated GIF
        let mut gif_bytes = Vec::new();
        {
            let mut enc = image::codecs::gif::GifEncoder::new(&mut gif_bytes);
            let f1 =
                image::Frame::new(RgbaImage::from_pixel(40, 30, image::Rgba([255, 0, 0, 255])));
            let f2 =
                image::Frame::new(RgbaImage::from_pixel(40, 30, image::Rgba([0, 0, 255, 255])));
            enc.encode_frame(f1).unwrap();
            enc.encode_frame(f2).unwrap();
        }
        let decoded_gif = decode_image_bytes(&gif_bytes, 420, 250, false).unwrap();
        assert!(decoded_gif.is_animated_gif);
        assert_eq!(decoded_gif.frames.len(), 2);
        assert_eq!(&decoded_gif.frames[0].bgra[0..4], &[0, 0, 255, 255]); // red in BGRA
        assert_eq!(&decoded_gif.frames[1].bgra[0..4], &[255, 0, 0, 255]); // blue in BGRA
    }

    #[test]
    fn real_size_638_image_lru_enforces_ram_and_vram_budgets_deterministically() {
        // Real-size 420x250 preview PNG decoded 638 times (~420 KiB BGRA each = ~268 MiB total
        // if uncapped!). Both CPU bitmap LRU (24 MiB <= 48 MiB cap) and GPU VRAM LRU (64 MiB cap)
        // MUST stay strictly within budget with deterministic FIFO/LRU eviction.
        let sample_png = make_png(840, 500, 64, 128, 192);
        let decoded = decode_image_bytes(&sample_png, PREVIEW_MAX_W, PREVIEW_MAX_H, false).unwrap();
        assert_eq!((decoded.width, decoded.height), (420, 250));
        let frame_bytes = decoded.byte_size();
        assert_eq!(frame_bytes, 420 * 250 * 4);

        let mut ram_lru: BitmapLru<usize, DecodedImage> = BitmapLru::new(RAM_BITMAP_BUDGET_BYTES);
        let mut vram_lru: BitmapLru<usize, usize> = BitmapLru::new(VRAM_TEXTURE_BUDGET_BYTES);

        let mut ram_evicted = 0usize;
        let mut vram_evicted = 0usize;

        for idx in 0..638 {
            let ev_r = ram_lru.insert(idx, decoded.clone(), frame_bytes);
            ram_evicted += ev_r.len();
            assert!(
                ram_lru.total_bytes() <= RAM_BITMAP_BUDGET_BYTES,
                "RAM exceeded 24 MiB budget at image {idx}: {}",
                ram_lru.total_bytes()
            );
            assert!(ram_lru.total_bytes() <= MAX_RAM_BITMAP_CAP_BYTES);

            let ev_v = vram_lru.insert(idx, idx, frame_bytes);
            vram_evicted += ev_v.len();
            assert!(
                vram_lru.total_bytes() <= VRAM_TEXTURE_BUDGET_BYTES,
                "VRAM exceeded 64 MiB budget at image {idx}: {}",
                vram_lru.total_bytes()
            );
        }

        // 638 * 420,000 B = 267,960,000 B -> deterministic eviction MUST have occurred.
        assert!(ram_evicted > 550);
        assert!(vram_evicted > 450);
        assert!(!ram_lru.contains(&0));
        assert!(ram_lru.contains(&637));
        assert!(!vram_lru.contains(&0));
        assert!(vram_lru.contains(&637));
    }

    #[test]
    fn o1_bitmap_lru_insert_overwrite_returns_displaced_entry() {
        let mut lru: BitmapLru<String, u32> = BitmapLru::new(1024);
        assert!(lru.insert("tex1".into(), 10, 400).is_empty());
        assert_eq!(lru.total_bytes(), 400);

        let displaced = lru.insert("tex1".into(), 20, 600);
        assert_eq!(displaced, vec![("tex1".to_owned(), 10)]);
        assert_eq!(lru.total_bytes(), 600);
        assert_eq!(lru.peek("tex1"), Some(&20));

        // Overwrite causing budget eviction returns BOTH displaced entry and LRU-evicted entry
        assert!(lru.insert("tex2".into(), 30, 300).is_empty());
        assert_eq!(lru.total_bytes(), 900);
        let evicted = lru.insert("tex2".into(), 40, 800);
        assert_eq!(
            evicted,
            vec![("tex2".to_owned(), 30), ("tex1".to_owned(), 20)]
        );
        assert_eq!(lru.total_bytes(), 800);
        assert!(!lru.contains(&"tex1".to_owned()));
        assert_eq!(lru.peek("tex2"), Some(&40));
    }
}
