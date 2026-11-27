//! Pure-Rust RFC 3533 / RFC 7845 Ogg Opus container muxer and demuxer.
//! Uses direct MSB-first CRC32 (polynomial `0x04c11db7`) verified on both mux and demux.

use thiserror::Error;

pub const MAX_OGG_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_PACKETS_PER_PAGE: usize = 50;
pub const MAX_SEGMENTS_PER_PAGE: usize = 255;
pub const OPUS_SAMPLE_RATE: u32 = 48_000;
pub const OPUS_FRAME_SAMPLES: usize = 960;
pub const OGG_FLAG_CONTINUED: u8 = 0x01;
pub const OGG_FLAG_BOS: u8 = 0x02;
pub const OGG_FLAG_EOS: u8 = 0x04;
pub const DEFAULT_VENDOR: &str = "postbox";
pub const MAX_VENDOR_BYTES: usize = 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum OggError {
    #[error("ogg file exceeds 32 MiB limit")]
    FileTooLarge,
    #[error("unsupported or invalid Ogg Opus container")]
    UnsupportedFile,
    #[error("ogg page CRC32 mismatch")]
    InvalidCrc,
    #[error("truncated Ogg page")]
    Truncated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusHead {
    pub version: u8,
    pub channels: u8,
    pub pre_skip: u16,
    pub input_sample_rate: u32,
    pub output_gain: i16,
    pub mapping_family: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OggPacket {
    pub data: Vec<u8>,
    pub granule_end: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedOggOpus {
    pub serial: u32,
    pub head: OpusHead,
    pub packets: Vec<OggPacket>,
    pub final_granule: u64,
    pub duration_samples: u64,
    pub duration_ms: u64,
}

impl ParsedOggOpus {
    /// Returns the first packet index whose `granule_end > target_granule`, or the last packet.
    pub fn find_packet_for_granule(&self, target_granule: u64) -> usize {
        if self.packets.is_empty() {
            return 0;
        }
        for (idx, pkt) in self.packets.iter().enumerate() {
            if pkt.granule_end > target_granule {
                return idx;
            }
        }
        self.packets.len() - 1
    }
}

/// Direct MSB-first Ogg CRC32 (polynomial `0x04c11db7`, init `0`, final XOR `0`).
pub fn ogg_crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    for &byte in data {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            if (crc & 0x8000_0000) != 0 {
                crc = (crc << 1) ^ 0x04c1_1db7;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

pub fn build_opus_head(channels: u8, pre_skip: u16, input_sample_rate: u32) -> [u8; 19] {
    let mut head = [0u8; 19];
    head[0..8].copy_from_slice(b"OpusHead");
    head[8] = 1; // version
    head[9] = channels;
    head[10..12].copy_from_slice(&pre_skip.to_le_bytes());
    head[12..16].copy_from_slice(&input_sample_rate.to_le_bytes());
    head[16..18].copy_from_slice(&0i16.to_le_bytes()); // output_gain
    head[18] = 0; // mapping_family
    head
}

pub fn build_opus_tags(vendor: &str) -> Vec<u8> {
    let vendor_bytes = vendor.as_bytes();
    let mut out = Vec::with_capacity(8 + 4 + vendor_bytes.len() + 4);
    out.extend_from_slice(b"OpusTags");
    out.extend_from_slice(&(vendor_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(vendor_bytes);
    out.extend_from_slice(&0u32.to_le_bytes()); // 0 user comments
    out
}

/// Stateful Ogg Opus page muxer enforcing max 50 packets and max 255 lacing segments per page.
pub struct OggOpusMuxer {
    serial: u32,
    sequence: u32,
    out: Vec<u8>,
    page_lacing: Vec<u8>,
    page_body: Vec<u8>,
    packets_in_page: usize,
    page_granule: u64,
}

impl OggOpusMuxer {
    pub fn new(serial: u32, channels: u8, pre_skip: u16, vendor: &str) -> Self {
        let mut muxer = Self {
            serial,
            sequence: 0,
            out: Vec::new(),
            page_lacing: Vec::with_capacity(MAX_SEGMENTS_PER_PAGE),
            page_body: Vec::new(),
            packets_in_page: 0,
            page_granule: 0,
        };
        let head = build_opus_head(channels, pre_skip, OPUS_SAMPLE_RATE);
        muxer.write_single_packet_page(&head, OGG_FLAG_BOS, 0);
        let bounded_vendor = &vendor[..vendor.floor_char_boundary(MAX_VENDOR_BYTES)];
        let tags = build_opus_tags(bounded_vendor);
        muxer.write_single_packet_page(&tags, 0x00, 0);
        muxer
    }

    fn write_single_packet_page(&mut self, packet: &[u8], flags: u8, granule: u64) {
        if packet.len() / 255 < MAX_SEGMENTS_PER_PAGE {
            let mut lacing = Vec::new();
            encode_lacing(packet.len(), &mut lacing);
            self.write_raw_page(flags, granule, &lacing, packet);
        } else {
            self.write_spanned_packet(packet, flags, granule);
        }
    }

    fn write_spanned_packet(&mut self, packet: &[u8], initial_flags: u8, granule: u64) {
        let mut offset = 0usize;
        let mut continued = false;
        let max_bytes = MAX_SEGMENTS_PER_PAGE * 255;
        while offset < packet.len() {
            let remaining = packet.len() - offset;
            let flags = if continued {
                OGG_FLAG_CONTINUED
            } else {
                initial_flags
            };
            if remaining >= max_bytes {
                let lacing = [255u8; MAX_SEGMENTS_PER_PAGE];
                self.write_raw_page(
                    flags,
                    u64::MAX,
                    &lacing,
                    &packet[offset..offset + max_bytes],
                );
                offset += max_bytes;
                continued = true;
                if offset == packet.len() {
                    // RFC 3533 §4: packets whose length is an exact multiple of 255 must end
                    // with a 0-byte lacing segment on a continued page carrying the granule.
                    self.write_raw_page(OGG_FLAG_CONTINUED, granule, &[0], &[]);
                }
            } else {
                let mut lacing = Vec::new();
                encode_lacing(remaining, &mut lacing);
                self.write_raw_page(flags, granule, &lacing, &packet[offset..]);
                offset = packet.len();
            }
        }
    }

    fn write_raw_page(&mut self, flags: u8, granule: u64, lacing: &[u8], body: &[u8]) {
        debug_assert!(lacing.len() <= MAX_SEGMENTS_PER_PAGE);
        let page_start = self.out.len();
        self.out.extend_from_slice(b"OggS");
        self.out.push(0); // stream structure version
        self.out.push(flags);
        self.out.extend_from_slice(&granule.to_le_bytes());
        self.out.extend_from_slice(&self.serial.to_le_bytes());
        self.out.extend_from_slice(&self.sequence.to_le_bytes());
        self.out.extend_from_slice(&0u32.to_le_bytes()); // CRC placeholder
        self.out.push(lacing.len() as u8);
        self.out.extend_from_slice(lacing);
        self.out.extend_from_slice(body);

        let crc = ogg_crc32(&self.out[page_start..]);
        self.out[page_start + 22..page_start + 26].copy_from_slice(&crc.to_le_bytes());
        self.sequence = self.sequence.wrapping_add(1);
    }

    fn flush_audio_page(&mut self, flags: u8) {
        if self.packets_in_page == 0 {
            return;
        }
        let lacing = std::mem::take(&mut self.page_lacing);
        let body = std::mem::take(&mut self.page_body);
        let granule = self.page_granule;
        self.packets_in_page = 0;
        self.write_raw_page(flags, granule, &lacing, &body);
    }

    /// Pushes one Opus audio packet with its cumulative granule position.
    pub fn push_packet(&mut self, packet: &[u8], granule: u64) {
        let segments_needed = packet.len() / 255 + 1;
        if self.packets_in_page > 0
            && (self.packets_in_page >= MAX_PACKETS_PER_PAGE
                || self.page_lacing.len() + segments_needed > MAX_SEGMENTS_PER_PAGE)
        {
            self.flush_audio_page(0x00);
        }

        if segments_needed <= MAX_SEGMENTS_PER_PAGE {
            encode_lacing(packet.len(), &mut self.page_lacing);
            self.page_body.extend_from_slice(packet);
            self.packets_in_page += 1;
            self.page_granule = granule;
        } else {
            // Very large packet (> 65024 bytes): span across continued pages.
            self.write_spanned_packet(packet, 0x00, granule);
        }
    }

    /// Flushes the final EOS (`0x04`) page with `final_granule` and returns the encoded Ogg bytes.
    pub fn finish(mut self, final_granule: u64) -> Vec<u8> {
        self.page_granule = final_granule;
        if self.packets_in_page > 0 {
            self.flush_audio_page(OGG_FLAG_EOS);
        } else {
            // Ensure an EOS page exists even if the caller flushed before finish.
            self.write_raw_page(OGG_FLAG_EOS, final_granule, &[], &[]);
        }
        self.out
    }
}

fn encode_lacing(len: usize, out: &mut Vec<u8>) {
    let mut remaining = len;
    while remaining >= 255 {
        out.push(255);
        remaining -= 255;
    }
    out.push(remaining as u8);
}

/// Parses and CRC32-verifies an Ogg Opus byte stream, enforcing the `32 MiB` size cap.
pub fn parse_ogg_opus(data: &[u8]) -> Result<ParsedOggOpus, OggError> {
    if data.len() > MAX_OGG_BYTES {
        return Err(OggError::FileTooLarge);
    }
    if data.len() < 27 || &data[0..4] != b"OggS" {
        return Err(OggError::UnsupportedFile);
    }

    let mut offset = 0usize;
    let mut stream_serial: Option<u32> = None;
    let mut head: Option<OpusHead> = None;
    let mut packets: Vec<OggPacket> = Vec::new();
    let mut pending_packet: Vec<u8> = Vec::new();
    let mut saw_eos = false;

    while offset + 27 <= data.len() {
        if &data[offset..offset + 4] != b"OggS" || data[offset + 4] != 0 {
            return Err(OggError::UnsupportedFile);
        }
        let header_type = data[offset + 5];
        let granule = u64::from_le_bytes(data[offset + 6..offset + 14].try_into().unwrap());
        let serial = u32::from_le_bytes(data[offset + 14..offset + 18].try_into().unwrap());
        let expected_crc = u32::from_le_bytes(data[offset + 22..offset + 26].try_into().unwrap());
        let seg_count = usize::from(data[offset + 26]);

        let lacing_start = offset + 27;
        let payload_start = lacing_start + seg_count;
        if payload_start > data.len() {
            return Err(OggError::Truncated);
        }
        let lacing = &data[lacing_start..payload_start];
        let payload_len: usize = lacing.iter().map(|&b| usize::from(b)).sum();
        let page_end = payload_start + payload_len;
        if page_end > data.len() {
            return Err(OggError::Truncated);
        }

        // Verify Ogg page CRC32 with checksum bytes [22..26] zeroed.
        let computed_crc = page_crc32(&data[offset..page_end]);
        if computed_crc != expected_crc {
            return Err(OggError::InvalidCrc);
        }

        let target_serial = *stream_serial.get_or_insert(serial);
        if serial == target_serial {
            if (header_type & OGG_FLAG_CONTINUED) == 0 && !pending_packet.is_empty() {
                pending_packet.clear();
            }
            let mut cursor = payload_start;
            for &seg in lacing {
                let seg_len = usize::from(seg);
                pending_packet.extend_from_slice(&data[cursor..cursor + seg_len]);
                cursor += seg_len;
                if seg < 255 {
                    let pkt_bytes = std::mem::take(&mut pending_packet);
                    if pkt_bytes.starts_with(b"OpusHead") {
                        head = Some(parse_opus_head(&pkt_bytes)?);
                    } else if pkt_bytes.starts_with(b"OpusTags") {
                        // Skip metadata header.
                    } else if head.is_some() {
                        packets.push(OggPacket {
                            data: pkt_bytes,
                            granule_end: granule,
                        });
                    }
                }
            }
            if (header_type & OGG_FLAG_EOS) != 0 {
                saw_eos = true;
                offset = page_end;
                break;
            }
        }

        offset = page_end;
    }

    if !saw_eos && offset < data.len() {
        return Err(OggError::Truncated);
    }

    let Some(head) = head else {
        return Err(OggError::UnsupportedFile);
    };
    if packets.is_empty() {
        return Err(OggError::UnsupportedFile);
    }

    let Some(final_granule) = packets
        .iter()
        .rev()
        .find(|p| p.granule_end != u64::MAX)
        .map(|p| p.granule_end)
    else {
        return Err(OggError::UnsupportedFile);
    };
    if final_granule < u64::from(head.pre_skip) {
        return Err(OggError::UnsupportedFile);
    }
    let duration_samples = final_granule - u64::from(head.pre_skip);
    let duration_ms = duration_samples.saturating_mul(1000) / u64::from(OPUS_SAMPLE_RATE);

    Ok(ParsedOggOpus {
        serial: stream_serial.unwrap_or(0),
        head,
        packets,
        final_granule,
        duration_samples,
        duration_ms,
    })
}

fn page_crc32(page: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    for (idx, &raw_byte) in page.iter().enumerate() {
        let byte = if (22..26).contains(&idx) { 0 } else { raw_byte };
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            if (crc & 0x8000_0000) != 0 {
                crc = (crc << 1) ^ 0x04c1_1db7;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

fn parse_opus_head(bytes: &[u8]) -> Result<OpusHead, OggError> {
    if bytes.len() < 19 || &bytes[0..8] != b"OpusHead" {
        return Err(OggError::UnsupportedFile);
    }
    let version = bytes[8];
    if version == 0 || (version & 0xF0) != 0 {
        return Err(OggError::UnsupportedFile);
    }
    let channels = bytes[9];
    if channels == 0 {
        return Err(OggError::UnsupportedFile);
    }
    let pre_skip = u16::from_le_bytes(bytes[10..12].try_into().unwrap());
    let input_sample_rate = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    let output_gain = i16::from_le_bytes(bytes[16..18].try_into().unwrap());
    let mapping_family = bytes[18];
    Ok(OpusHead {
        version,
        channels,
        pre_skip,
        input_sample_rate,
        output_gain,
        mapping_family,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ogg_crc32_matches_known_rfc_test_vector() {
        assert_eq!(ogg_crc32(b"123456789"), 0x89a1_897f);
    }

    #[test]
    fn mux_and_demux_rollover_at_50_packets_and_preserve_granule() {
        let pre_skip = 312u16;
        let pcm_len = 75 * OPUS_FRAME_SAMPLES;
        let mut muxer = OggOpusMuxer::new(0x1234_5678, 1, pre_skip, DEFAULT_VENDOR);
        for i in 0..75usize {
            let granule = ((i + 1) * OPUS_FRAME_SAMPLES).min(usize::from(pre_skip) + pcm_len);
            let pkt = vec![0x48u8, (i & 0xFF) as u8, 0x11, 0x22];
            muxer.push_packet(&pkt, granule as u64);
        }
        let ogg = muxer.finish((usize::from(pre_skip) + pcm_len) as u64);

        // Count OggS pages: Page 0 (OpusHead) + Page 1 (OpusTags) + Page 2 (50 pkts) + Page 3 (25 pkts EOS) = 4 pages.
        let page_count = ogg.windows(4).filter(|w| w == b"OggS").count();
        assert_eq!(page_count, 4);

        let parsed = parse_ogg_opus(&ogg).unwrap();
        assert_eq!(parsed.serial, 0x1234_5678);
        assert_eq!(parsed.head.channels, 1);
        assert_eq!(parsed.head.pre_skip, pre_skip);
        assert_eq!(parsed.packets.len(), 75);
        assert_eq!(
            parsed.final_granule,
            (usize::from(pre_skip) + pcm_len) as u64
        );
        assert_eq!(parsed.duration_samples, pcm_len as u64);
        assert_eq!(parsed.duration_ms, 1500);
        assert_eq!(parsed.find_packet_for_granule(0), 0);
        assert!(parsed.find_packet_for_granule(50_000) >= 50);
    }

    #[test]
    fn demux_rejects_bad_crc_and_32_mib_guard() {
        let mut muxer = OggOpusMuxer::new(42, 1, 312, DEFAULT_VENDOR);
        muxer.push_packet(&[0x48, 0x01, 0x02], 960);
        let mut ogg = muxer.finish(960);
        assert!(parse_ogg_opus(&ogg).is_ok());

        // Flip a payload byte to corrupt CRC.
        let last = ogg.len() - 1;
        ogg[last] ^= 0xFF;
        assert_eq!(parse_ogg_opus(&ogg), Err(OggError::InvalidCrc));

        // 32 MiB + 1 byte guard.
        let oversized = vec![0u8; MAX_OGG_BYTES + 1];
        assert_eq!(parse_ogg_opus(&oversized), Err(OggError::FileTooLarge));
    }

    #[test]
    fn muxer_preserves_exact_65025_byte_packet_alone_and_with_follower() {
        let big = vec![0xAAu8; MAX_SEGMENTS_PER_PAGE * 255];
        assert_eq!(big.len(), 65_025);

        // Single 65,025-byte packet.
        let mut mux_single = OggOpusMuxer::new(0x2222, 1, 312, DEFAULT_VENDOR);
        mux_single.push_packet(&big, 1272);
        let ogg_single = mux_single.finish(1272);
        let parsed_single = parse_ogg_opus(&ogg_single).unwrap();
        assert_eq!(parsed_single.packets.len(), 1);
        assert_eq!(parsed_single.packets[0].data, big);
        assert_eq!(parsed_single.packets[0].granule_end, 1272);
        assert_eq!(parsed_single.duration_samples, 960);

        // 65,025-byte packet followed by a 42-byte packet.
        let small = vec![0xBBu8; 42];
        let mut mux_pair = OggOpusMuxer::new(0x5555, 1, 312, DEFAULT_VENDOR);
        mux_pair.push_packet(&big, 1272);
        mux_pair.push_packet(&small, 2232);
        let ogg_pair = mux_pair.finish(2232);
        let parsed_pair = parse_ogg_opus(&ogg_pair).unwrap();
        assert_eq!(parsed_pair.packets.len(), 2);
        assert_eq!(parsed_pair.packets[0].data, big);
        assert_eq!(parsed_pair.packets[0].granule_end, 1272);
        assert_eq!(parsed_pair.packets[1].data, small);
        assert_eq!(parsed_pair.packets[1].granule_end, 2232);
        assert_eq!(parsed_pair.duration_samples, 1920);
    }

    #[test]
    fn demux_rejects_pre_skip_exceeding_granule_invalid_version_and_trailing_truncated_header() {
        // pre_skip (5000) > final_granule (960) -> UnsupportedFile.
        let mut mux_bad_granule = OggOpusMuxer::new(0x1111, 1, 5000, DEFAULT_VENDOR);
        mux_bad_granule.push_packet(&[0x48, 0x01, 0x02], 960);
        let ogg_bad_granule = mux_bad_granule.finish(960);
        assert_eq!(
            parse_ogg_opus(&ogg_bad_granule),
            Err(OggError::UnsupportedFile)
        );

        // RFC 7845 §5.1.1 invalid OpusHead version (0 and 0x10).
        for bad_ver in [0u8, 0x10u8] {
            let mut head = build_opus_head(1, 312, OPUS_SAMPLE_RATE);
            head[8] = bad_ver;
            assert_eq!(parse_opus_head(&head), Err(OggError::UnsupportedFile));
        }

        // Trailing truncated header bytes after a non-EOS page -> Truncated.
        let mut mux_no_eos = OggOpusMuxer::new(0x3333, 1, 312, DEFAULT_VENDOR);
        mux_no_eos.push_packet(&[0x48, 0x01, 0x02], 1272);
        mux_no_eos.flush_audio_page(0x00);
        let mut bytes = mux_no_eos.out;
        bytes.extend_from_slice(b"OggS\x00\x04");
        assert_eq!(parse_ogg_opus(&bytes), Err(OggError::Truncated));
    }

    #[test]
    fn demux_rejects_all_u64_max_granule_audio_pages() {
        for pre_skip in [0u16, 312u16] {
            let mut muxer = OggOpusMuxer::new(0x4444, 1, pre_skip, DEFAULT_VENDOR);
            muxer.push_packet(&[0x48, 0x01, 0x02], u64::MAX);
            let ogg = muxer.finish(u64::MAX);
            assert_eq!(parse_ogg_opus(&ogg), Err(OggError::UnsupportedFile));
        }
    }

    #[test]
    fn muxer_handles_oversized_vendor_string_cleanly() {
        for vendor in ["v".repeat(70_000), "🦀".repeat(20_000)] {
            let mut muxer = OggOpusMuxer::new(0x6666, 1, 312, &vendor);
            muxer.push_packet(&[0x48, 0x01, 0x02], 1272);
            let ogg = muxer.finish(1272);
            let parsed = parse_ogg_opus(&ogg).unwrap();
            assert_eq!(parsed.packets.len(), 1);
            assert_eq!(parsed.packets[0].data, vec![0x48, 0x01, 0x02]);
            assert_eq!(parsed.duration_samples, 960);
        }
    }
}
