//! Pure-Rust (zero `unsafe`) audio, Ogg Opus voice playback/recording, WSOLA time-stretching,
//! Deepgram Nova-3 REST transcription & live WebSocket dictation, and optional ADHD transcript formatting.

pub mod dictation;
pub mod format;
pub mod ogg;
pub mod player;
pub mod recorder;
pub mod stretch;
pub mod transcribe;
pub mod voice_note;

pub use dictation::{
    CLOSE_STREAM_DRAIN_TIMEOUT, DEFAULT_DEEPGRAM_WS_BASE_URL, DICTATION_CAPTURE_POLL_INTERVAL,
    DictationError, DictationMeter, LIVE_BYTES_PER_SECOND, LIVE_SAMPLE_RATE,
    LIVE_TRANSCRIPT_CAPACITY, LiveDictationConfig, LiveDictationPhase, LiveDictationSession,
    LiveDictationSnapshot, LiveTranscript, MAX_LIVE_AUDIO_BYTES, MAX_LIVE_WS_FRAME_BYTES,
    MAX_LIVE_WS_MESSAGE_BYTES, SILENCE_RMS_THRESHOLD, SILENCE_WINDOW_MS, SilenceGate,
    TIMESTAMP_TOLERANCE, WS_CONNECT_TIMEOUT, build_deepgram_ws_url, pcm16_has_speech,
    wav_has_speech,
};
pub use format::{
    DEFAULT_ANTHROPIC_BASE_URL, DEFAULT_OPENROUTER_BASE_URL, DominantLanguage,
    FORMAT_CACHE_HEADER_V2, FORMAT_CACHE_MAX_BYTES, FORMAT_REQUEST_RING_CAP, FormatCache,
    FormatCacheKey, FormatError, FormatRequests, TranscriptFormatter, build_format_system_prompt,
    dominant_language, is_refusal, looks_formatted, speaker_instruction,
};
pub use ogg::{
    DEFAULT_VENDOR, MAX_OGG_BYTES, MAX_PACKETS_PER_PAGE, MAX_SEGMENTS_PER_PAGE, OPUS_FRAME_SAMPLES,
    OPUS_SAMPLE_RATE, OggError, OggOpusMuxer, OggPacket, OpusHead, ParsedOggOpus, build_opus_head,
    build_opus_tags, ogg_crc32, parse_ogg_opus,
};
pub use player::{PLAYBACK_POLL_INTERVAL, PlayerSnapshot, PlayerState, VoicePlayer};
pub use recorder::{
    MAX_RECORD_SAMPLES, MIN_RECORD_SAMPLES, RECORDING_POLL_INTERVAL, RECORDING_TICK_INTERVAL,
    RecordError, VoiceRecorder,
};
pub use stretch::{
    HOP_FRAMES, LOOKAHEAD_FRAMES, MAX_CHANNELS, MAX_SPEED_PERCENT, MIN_SPEED_PERCENT,
    OVERLAP_FRAMES, SEARCH_FRAMES, TimeStretcher, WINDOW_FRAMES, clamp_speed_percent,
    stretch_buffer,
};
pub use transcribe::{
    DEEPGRAM_HTTP_TIMEOUT, DEFAULT_DEEPGRAM_BASE_URL, DeepgramRestClient, MAX_SIDECAR_TEXT_BYTES,
    MAX_TRANSCRIBE_AUDIO_BYTES, MarkedTranscriptExport, MarkedVoiceExportInput,
    TranscriptionResult, export_marked_raw_transcripts, load_cached_transcript,
    load_verified_raw_transcript, map_http_status, transcript_raw_path, transcript_txt_path,
    write_formatted_txt_sidecar, write_transcript_sidecars,
};
pub use voice_note::{
    AudioError, DecodedVoiceNote, EncodedVoiceNote, Resampler, VOICE_BITRATE_BPS, VOICE_COMPLEXITY,
    VoiceSentEntry, VoiceSentFifo, WAVEFORM_BARS, decode_ogg_opus, duration_seconds,
    encode_voice_note, is_audio_placeholder, waveform_i16, waveform_stereo_f32,
};
