#![forbid(unsafe_code)]

use crate::wire::{crc32_ieee, crc32_ogg};

fn png_chunk(chunk_type: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + data.len());
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(chunk_type);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(chunk_type);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32_ieee(&crc_input).to_be_bytes());
    out
}

/// Pure-Rust RFC 1950 Adler-32 checksum used by zlib streams (e.g., PNG IDAT).
pub fn adler32(data: &[u8]) -> u32 {
    const MOD_ADLER: u32 = 65_521;
    let mut s1: u32 = 1;
    let mut s2: u32 = 0;
    for &b in data {
        s1 = (s1 + u32::from(b)) % MOD_ADLER;
        s2 = (s2 + s1) % MOD_ADLER;
    }
    (s2 << 16) | s1
}

fn build_png_1x1(rgba: [u8; 4]) -> Vec<u8> {
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    // IHDR: width=1, height=1, bit_depth=8, color_type=6 (RGBA), comp=0, filter=0, interlace=0
    let ihdr = [0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0];
    out.extend_from_slice(&png_chunk(b"IHDR", &ihdr));

    // Valid RFC 1950 zlib stored deflate block: filter=0 + 1 RGBA pixel
    let scanline = [0x00, rgba[0], rgba[1], rgba[2], rgba[3]];
    let checksum = adler32(&scanline).to_be_bytes();
    let mut idat = Vec::with_capacity(16);
    idat.extend_from_slice(&[0x78, 0x01]); // zlib header (CMF=0x78, FLG=0x01; 30721 % 31 == 0)
    idat.push(0x01); // BFINAL=1, BTYPE=00 (stored)
    idat.extend_from_slice(&[0x05, 0x00, 0xFA, 0xFF]); // LEN=5, NLEN=!5
    idat.extend_from_slice(&scanline);
    idat.extend_from_slice(&checksum);

    out.extend_from_slice(&png_chunk(b"IDAT", &idat));
    out.extend_from_slice(&png_chunk(b"IEND", &[]));
    out
}

/// Valid minimal 1x1 RGBA PNG image with verified PNG chunk CRCs and zlib Adler-32 (`0x0600027F`).
pub fn minimal_png_1x1() -> Vec<u8> {
    build_png_1x1([0xFF, 0x00, 0x80, 0xFF])
}

/// Distinct valid 1x1 PNG with a different RGBA pixel color (`0x04C2028F`) for isolation tests.
pub fn minimal_png_1x1_alt() -> Vec<u8> {
    build_png_1x1([0x10, 0x80, 0xFF, 0xFF])
}

/// Valid minimal 1x1 GIF89a image with trailer 0x3B.
pub fn minimal_gif89a() -> Vec<u8> {
    vec![
        b'G', b'I', b'F', b'8', b'9', b'a', // Header
        0x01, 0x00, 0x01, 0x00, // 1x1 logical screen
        0x80, 0x00, 0x00, // GCT flag, bg, aspect
        0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, // 2-entry GCT
        0x21, 0xF9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, // GCE
        0x2C, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, // Image descriptor
        0x02, 0x02, 0x44, 0x01, 0x00, // LZW data
        0x3B, // Trailer
    ]
}

/// Valid minimal 1x1 GIF87a image with trailer 0x3B.
pub fn minimal_gif87a() -> Vec<u8> {
    vec![
        b'G', b'I', b'F', b'8', b'7', b'a', 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0xFF,
        0x00, 0x00, 0x00, 0x00, 0x2C, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02,
        0x02, 0x44, 0x01, 0x00, 0x3B,
    ]
}

/// Minimal JPEG with valid SOI, APP0 JFIF header, and EOI markers.
pub fn minimal_jpeg() -> Vec<u8> {
    vec![
        0xFF, 0xD8, // SOI
        0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00,
        0x01, 0x00, 0x00, // APP0 JFIF
        0xFF, 0xD9, // EOI
    ]
}

fn build_ogg_page(
    header_type: u8,
    granule_pos: u64,
    serial: u32,
    sequence: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut page = Vec::with_capacity(28 + payload.len());
    page.extend_from_slice(b"OggS");
    page.push(0); // stream structure version
    page.push(header_type);
    page.extend_from_slice(&granule_pos.to_le_bytes());
    page.extend_from_slice(&serial.to_le_bytes());
    page.extend_from_slice(&sequence.to_le_bytes());
    page.extend_from_slice(&0u32.to_le_bytes()); // CRC placeholder at bytes 22..26
    page.push(1); // 1 segment
    page.push(payload.len() as u8);
    page.extend_from_slice(payload);
    let crc = crc32_ogg(&page);
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    page
}

/// Valid minimal Ogg Opus container (`OpusHead` 48 kHz mono + `OpusTags` + valid Ogg CRCs).
pub fn minimal_ogg_opus_packet() -> Vec<u8> {
    let mut opus_head = Vec::with_capacity(19);
    opus_head.extend_from_slice(b"OpusHead");
    opus_head.push(1); // version
    opus_head.push(1); // mono
    opus_head.extend_from_slice(&312u16.to_le_bytes()); // pre-skip
    opus_head.extend_from_slice(&48_000u32.to_le_bytes()); // input sample rate
    opus_head.extend_from_slice(&0i16.to_le_bytes()); // output gain
    opus_head.push(0); // channel mapping family

    let mut opus_tags = Vec::new();
    opus_tags.extend_from_slice(b"OpusTags");
    let vendor = b"postbox-e2e";
    opus_tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    opus_tags.extend_from_slice(vendor);
    opus_tags.extend_from_slice(&0u32.to_le_bytes()); // 0 user comments

    let mut out = build_ogg_page(0x02, 0, 0x504F_5354, 0, &opus_head);
    out.extend_from_slice(&build_ogg_page(0x04, 960, 0x504F_5354, 1, &opus_tags));
    out
}

/// Valid minimal ISO BMFF MP4 header (`ftypisom` + empty `moov` box).
pub fn minimal_mp4_ftyp() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&20u32.to_be_bytes());
    out.extend_from_slice(b"ftyp");
    out.extend_from_slice(b"isom");
    out.extend_from_slice(&512u32.to_be_bytes());
    out.extend_from_slice(b"isom");
    out.extend_from_slice(&8u32.to_be_bytes());
    out.extend_from_slice(b"moov");
    out
}

/// Generates raw 16-bit signed little-endian mono PCM frames.
pub fn pcm16_raw(sample_rate: u32, duration_ms: u32, amplitude: i16) -> Vec<u8> {
    let num_samples = (u64::from(sample_rate) * u64::from(duration_ms) / 1000) as usize;
    let mut out = Vec::with_capacity(num_samples * 2);
    for i in 0..num_samples {
        let sample = if amplitude == 0 {
            0i16
        } else if (i / 16).is_multiple_of(2) {
            amplitude
        } else {
            amplitude.saturating_neg()
        };
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// Generates a valid RIFF/WAVE 16-bit mono PCM WAV buffer.
pub fn pcm16_wav(sample_rate: u32, duration_ms: u32, amplitude: i16) -> Vec<u8> {
    let pcm = pcm16_raw(sample_rate, duration_ms, amplitude);
    let data_len = pcm.len() as u32;
    let byte_rate = sample_rate * 2;
    let riff_size = 36 + data_len;

    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM format
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(&pcm);
    out
}

/// Computes the WCAG 2.1 relative-luminance contrast ratio between two `0xRRGGBB` colors.
pub fn wcag_contrast_ratio(rgb_a: u32, rgb_b: u32) -> f64 {
    fn channel(c: u8) -> f64 {
        let s = f64::from(c) / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }
    fn luminance(rgb: u32) -> f64 {
        let r = channel(((rgb >> 16) & 0xFF) as u8);
        let g = channel(((rgb >> 8) & 0xFF) as u8);
        let b = channel((rgb & 0xFF) as u8);
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }
    let l1 = luminance(rgb_a);
    let l2 = luminance(rgb_b);
    let (hi, lo) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
    (hi + 0.05) / (lo + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adler32_rfc1950_vectors() {
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
        assert_eq!(adler32(&[0x00, 0xFF, 0x00, 0x80, 0xFF]), 0x0600_027F);
        assert_eq!(adler32(&[0x00, 0x10, 0x80, 0xFF, 0xFF]), 0x04C2_028F);
    }

    #[test]
    fn minimal_png_fixtures_have_valid_chunk_crc32_and_zlib_adler32() {
        let png1 = minimal_png_1x1();
        let png2 = minimal_png_1x1_alt();
        assert_ne!(png1, png2);

        for (png, expected_adler) in [(&png1, 0x0600_027Fu32), (&png2, 0x04C2_028Fu32)] {
            assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
            let mut pos = 8usize;
            let mut seen_chunks = Vec::new();
            let mut idat_payload = Vec::new();
            while pos < png.len() {
                let len = u32::from_be_bytes([png[pos], png[pos + 1], png[pos + 2], png[pos + 3]])
                    as usize;
                let chunk_type = &png[pos + 4..pos + 8];
                let data = &png[pos + 8..pos + 8 + len];
                let stored_crc = u32::from_be_bytes([
                    png[pos + 8 + len],
                    png[pos + 9 + len],
                    png[pos + 10 + len],
                    png[pos + 11 + len],
                ]);
                let mut crc_buf = Vec::with_capacity(4 + len);
                crc_buf.extend_from_slice(chunk_type);
                crc_buf.extend_from_slice(data);
                assert_eq!(crc32_ieee(&crc_buf), stored_crc, "chunk CRC mismatch");
                seen_chunks.push(chunk_type.to_vec());
                if chunk_type == b"IDAT" {
                    idat_payload.extend_from_slice(data);
                }
                pos += 12 + len;
            }
            assert_eq!(pos, png.len());
            assert_eq!(
                seen_chunks,
                vec![b"IHDR".to_vec(), b"IDAT".to_vec(), b"IEND".to_vec()]
            );

            // Verify RFC 1950 zlib header + stored deflate block + trailing Adler-32
            let cmf = u16::from(idat_payload[0]);
            let flg = u16::from(idat_payload[1]);
            assert_eq!((cmf * 256 + flg) % 31, 0);
            assert_eq!(idat_payload[2], 0x01); // BFINAL=1, BTYPE=00
            let block_len = u16::from_le_bytes([idat_payload[3], idat_payload[4]]);
            let block_nlen = u16::from_le_bytes([idat_payload[5], idat_payload[6]]);
            assert_eq!(block_len, !block_nlen);
            let scanline = &idat_payload[7..7 + usize::from(block_len)];
            let stored_adler = u32::from_be_bytes([
                idat_payload[7 + usize::from(block_len)],
                idat_payload[8 + usize::from(block_len)],
                idat_payload[9 + usize::from(block_len)],
                idat_payload[10 + usize::from(block_len)],
            ]);
            assert_eq!(stored_adler, adler32(scanline));
            assert_eq!(stored_adler, expected_adler);
        }
    }
}
