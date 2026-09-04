//! `FormatBridge` implementation that turns host-decoded PCM into a channel bed.
//!
//! The bridge buffers the raw bytes delivered through `push_packet`, parses one
//! [`OPCM` header](crate::header) to learn the geometry, then converts the
//! accumulated PCM into [`RDecodedFrame`]s. Each frame carries the labels the
//! host declared and **empty** metadata: that is exactly how the renderer
//! recognises a plain channel bed (`Engine::process_decoded_frame` treats any
//! non-empty metadata as object content and skips the bed path). The renderer
//! then spatialises the bed through its virtual-bed / VBAP stage according to
//! those labels.
//!
//! The difference from `reference_bridge` is only where the labels come from.
//! That bridge reads WAV files, so it infers positions from the channel count;
//! this one is fed by a host that already decoded the stream and knows exactly
//! what each channel is, so it is told.

use abi_stable::std_types::{RSlice, RStr, RString, RVec};
use bridge_api::{
    FormatBridge, RChannelLabel, RCoordinateFormat, RDecodedFrame, RInputTransport, RMetadataFrame,
    RPushResult, RVbapCartesianDefaults, RVbapTableMode,
};

use crate::header::{HeaderParse, PcmFormat, parse_header};
use crate::logging::bridge_diag_log;

/// Maximum number of sample-frames emitted in a single [`RDecodedFrame`].
/// Bounds per-frame allocation and keeps the renderer's per-frame work modest
/// while staying large enough to avoid per-call overhead dominating.
const BLOCK_FRAMES: usize = 2048;

/// Streaming parse state.
enum State {
    /// Waiting for a complete `OPCM` header.
    Header,
    /// Header parsed; PCM is being streamed until the host resets us.
    Data { format: PcmFormat },
}

pub(crate) struct PcmBridge {
    /// Accumulates raw input bytes across `push_packet` calls.
    buf: Vec<u8>,
    state: State,
    /// Cached per-channel labels for the active format (computed once, cloned
    /// per emitted frame — never per sample).
    labels: Vec<RChannelLabel>,
    frames_emitted: u64,
}

impl PcmBridge {
    /// The `strict` flag is accepted and ignored, as the ABI now intends: the
    /// host says so itself in `bridge_loader::LoadedBridge::load_with_params` -
    /// "strict mode removed from the host; bridges ignore the flag. The ABI
    /// parameter is kept for compatibility and always passed as `false`."
    ///
    /// Honouring it would mean production - which passes `false` - never heard
    /// about a malformed header at all.
    pub(crate) fn new(_strict: bool) -> Self {
        Self {
            buf: Vec::new(),
            state: State::Header,
            labels: Vec::new(),
            frames_emitted: 0,
        }
    }

    fn reset_state(&mut self) {
        self.buf.clear();
        self.state = State::Header;
        self.labels.clear();
    }

    /// Emit one error into `result`, resetting the parser.
    ///
    /// Always reported, never merely logged. A malformed header is not a
    /// damaged packet the decoder will resynchronise past - it means the host
    /// and this bridge disagree about the protocol, and every byte after it is
    /// PCM being read as though it were another header. Staying quiet about
    /// that produces a stream that returns success and no audio, for as long as
    /// the file lasts; saying so gives the host something to fall back on.
    fn fail(&mut self, result: &mut RPushResult, message: &str) {
        bridge_diag_log(log::Level::Warn, message);
        self.reset_state();
        result.did_reset = true;
        result.error_message = RString::from(message);
    }

    /// Try to parse the header from the front of `buf`. On success transitions
    /// to [`State::Data`] and drains the consumed bytes. Returns `true` once
    /// streaming can proceed.
    fn try_parse_header(&mut self, result: &mut RPushResult) -> bool {
        match parse_header(&self.buf) {
            HeaderParse::NeedMore => false,
            HeaderParse::Invalid(reason) => {
                self.fail(
                    result,
                    &format!("pcm-bridge: invalid OPCM header: {reason}"),
                );
                false
            }
            HeaderParse::Found { format, consumed } => {
                self.labels = format.labels.clone();
                self.buf.drain(0..consumed);
                bridge_diag_log(
                    log::Level::Info,
                    &format!(
                        "pcm-bridge: header parsed: {} ch, {} Hz, {:?}, labels {:?}",
                        format.channels, format.sample_rate, format.sample_format, format.labels
                    ),
                );
                self.state = State::Data { format };
                true
            }
        }
    }

    /// Convert all complete sample-frames currently buffered into decoded
    /// frames. A partial trailing frame stays buffered for the next call.
    fn drain_pcm(&mut self, result: &mut RPushResult) {
        let State::Data { format } = &self.state else {
            return;
        };
        let sample_rate = format.sample_rate;
        let channel_count = format.channels as u32;
        let sample_format = format.sample_format;
        let bytes_per_sample = sample_format.bytes_per_sample();
        let channels = format.channels as usize;
        let bytes_per_frame = format.bytes_per_frame();
        if bytes_per_frame == 0 {
            return;
        }

        let total_frames = self.buf.len() / bytes_per_frame;
        if total_frames == 0 {
            return;
        }

        let mut frame_start = 0usize; // running byte cursor into `self.buf`
        let mut frames_left = total_frames;
        while frames_left > 0 {
            let n = frames_left.min(BLOCK_FRAMES);
            let sample_total = n * channels;
            let mut pcm: RVec<i32> = RVec::with_capacity(sample_total);

            // Interleaved conversion. One reserved allocation for the whole
            // block; no per-sample heap activity.
            let mut byte_idx = frame_start;
            for _ in 0..sample_total {
                let s =
                    sample_format.decode_sample(&self.buf[byte_idx..byte_idx + bytes_per_sample]);
                pcm.push(s);
                byte_idx += bytes_per_sample;
            }

            result.frames.push(RDecodedFrame {
                sampling_frequency: sample_rate,
                sample_count: n as u32,
                channel_count,
                pcm,
                channel_labels: RVec::from(self.labels.clone()),
                // Empty metadata ⇒ the renderer treats this as a channel bed.
                metadata: RVec::<RMetadataFrame>::new(),
                drc_gain: 1.0,
                drc_ramp_duration: 0,
                dialogue_level: abi_stable::std_types::ROption::RNone,
                is_new_segment: false,
            });

            frame_start += n * bytes_per_frame;
            frames_left -= n;
        }

        self.frames_emitted += total_frames as u64;
        let consumed = total_frames * bytes_per_frame;
        // Single O(remaining) compaction per call; leftover is < one frame.
        self.buf.drain(0..consumed);
    }
}

impl FormatBridge for PcmBridge {
    fn push_packet(
        &mut self,
        data: RSlice<'_, u8>,
        transport: RInputTransport,
        data_type: u8,
    ) -> RPushResult {
        let mut result = RPushResult {
            frames: RVec::new(),
            error_message: RString::new(),
            did_reset: false,
        };

        // The ABI asks each bridge to say what it accepts rather than assume.
        // An OPCM stream is raw bytes with `data_type` zero; an IEC 61937
        // payload is a bitstream this bridge has no decoder for, and appending
        // it to the buffer would read it as a header and then as samples.
        if !matches!(transport, RInputTransport::Raw) || data_type != 0 {
            self.fail(
                &mut result,
                &format!(
                    "pcm-bridge: expects raw input with data_type 0, got {transport:?}/{data_type}"
                ),
            );
            return result;
        }

        // The bridge is byte-stream oriented; a chunk carries whatever part of
        // the header or the PCM happens to fall in it, so it is simply
        // appended and the state machine decides what it was.
        self.buf.extend_from_slice(data.as_slice());

        if matches!(self.state, State::Header) && !self.try_parse_header(&mut result) {
            return result;
        }
        self.drain_pcm(&mut result);
        result
    }

    fn reset(&mut self) {
        // Back to awaiting a header. The host resends one with the next chunk,
        // which is also how it declares a format change: reset, then describe
        // the new geometry.
        self.reset_state();
    }

    fn is_ready(&self) -> bool {
        self.frames_emitted > 0
    }

    fn has_objects(&self) -> bool {
        // Host-decoded PCM is a fixed channel bed: no dynamic objects.
        false
    }

    fn configure(&mut self, key: RStr<'_>, _value: RStr<'_>) -> bool {
        // A PCM stream exposes a single presentation, so the host's mandatory
        // `presentation` selection is accepted (and ignored) — returning false
        // here makes the CLI abort with "Bridge rejected presentation value".
        // All other keys are unrecognised.
        key.as_str() == "presentation"
    }

    fn coordinate_format(&self) -> RCoordinateFormat {
        RCoordinateFormat::Cartesian
    }

    fn vbap_cartesian_defaults(&self) -> RVbapCartesianDefaults {
        // Balanced default grid, matching the production bridge's hint.
        RVbapCartesianDefaults {
            x_size: 62,
            y_size: 62,
            z_size: 15,
            allow_negative_z: false,
        }
    }

    fn preferred_vbap_table_mode(&self) -> RVbapTableMode {
        RVbapTableMode::Cartesian
    }

    fn supported_drc_modes(&self) -> RVec<RString> {
        // Linear PCM carries no dynamic-range metadata.
        RVec::new()
    }

    fn set_drc_mode(&mut self, _mode: RStr<'_>) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{FIXED_HEADER_LEN, MAGIC, VERSION};

    /// Build an `OPCM` header, mirroring what the host emits.
    fn header(labels: &[u8], sample_rate: u32, fmt: u8) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&(labels.len() as u16).to_le_bytes());
        buf.extend_from_slice(&sample_rate.to_le_bytes());
        buf.push(fmt);
        buf.push(0);
        buf.extend_from_slice(labels);
        assert_eq!(buf.len(), FIXED_HEADER_LEN + labels.len());
        buf
    }

    fn f32_frames(frames: &[Vec<f32>]) -> Vec<u8> {
        let mut out = Vec::new();
        for frame in frames {
            for &s in frame {
                out.extend_from_slice(&s.to_le_bytes());
            }
        }
        out
    }

    fn i32_frames(frames: &[Vec<i32>]) -> Vec<u8> {
        let mut out = Vec::new();
        for frame in frames {
            for &s in frame {
                out.extend_from_slice(&s.to_le_bytes());
            }
        }
        out
    }

    fn push(bridge: &mut PcmBridge, bytes: &[u8]) -> RPushResult {
        bridge.push_packet(RSlice::from_slice(bytes), RInputTransport::Raw, 0)
    }

    #[test]
    fn decodes_header_and_pcm_in_one_push() {
        use RChannelLabel::*;
        let mut stream = header(&[0, 1, 2, 3, 4, 5], 48_000, 0);
        stream.extend(i32_frames(&[
            vec![1, 2, 3, 4, 5, 6],
            vec![7, 8, 9, 10, 11, 12],
        ]));

        let mut bridge = PcmBridge::new(true);
        let r = push(&mut bridge, &stream);

        assert!(r.error_message.is_empty(), "{}", r.error_message);
        assert!(!r.did_reset);
        assert!(bridge.is_ready());
        assert!(!bridge.has_objects());

        let total: u32 = r.frames.iter().map(|f| f.sample_count).sum();
        assert_eq!(total, 2);
        let f = &r.frames[0];
        assert_eq!(f.channel_count, 6);
        assert_eq!(f.sampling_frequency, 48_000);
        assert!(f.metadata.is_empty(), "bed frames must carry no metadata");
        assert_eq!(
            f.channel_labels.as_slice(),
            &[L, R, C, LFE, Ls, Rs],
            "the labels the host declared reach the renderer unchanged"
        );
        assert_eq!(f.pcm.as_slice(), &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
    }

    #[test]
    fn f32_input_is_scaled_to_the_renderer_convention() {
        let mut stream = header(&[0, 1], 44_100, 1);
        stream.extend(f32_frames(&[vec![1.0, -1.0], vec![0.0, 0.5]]));

        let mut bridge = PcmBridge::new(true);
        let r = push(&mut bridge, &stream);
        assert!(r.error_message.is_empty());

        let f = &r.frames[0];
        assert_eq!(f.sampling_frequency, 44_100);
        assert_eq!(f.pcm[0], 8_388_607);
        assert_eq!(f.pcm[1], -8_388_607);
        assert_eq!(f.pcm[2], 0);
        assert_eq!(f.pcm[3], (0.5f32 * 8_388_607.0) as i32);
    }

    /// The header and the PCM both straddle chunk boundaries in the real feed,
    /// because the host chunks by sample-frame count, not by our framing.
    #[test]
    fn decodes_across_arbitrary_chunk_boundaries() {
        let mut stream = header(&[0, 1, 2, 3, 4, 5], 48_000, 0);
        let frames: Vec<Vec<i32>> = (0..50)
            .map(|i| (0..6).map(|c| i * 10 + c).collect())
            .collect();
        stream.extend(i32_frames(&frames));

        for chunk_size in [1usize, 3, 7, 13, 64] {
            let mut bridge = PcmBridge::new(true);
            let mut total = 0u32;
            let mut collected: Vec<i32> = Vec::new();
            for chunk in stream.chunks(chunk_size) {
                let r = push(&mut bridge, chunk);
                assert!(
                    r.error_message.is_empty(),
                    "chunk size {chunk_size}: {}",
                    r.error_message
                );
                for f in r.frames.iter() {
                    total += f.sample_count;
                    collected.extend_from_slice(f.pcm.as_slice());
                }
            }
            assert_eq!(total, 50, "chunk size {chunk_size}");
            assert_eq!(
                collected,
                frames.concat(),
                "chunk size {chunk_size}: samples must survive reassembly in order"
            );
        }
    }

    /// A frame split mid-sample must not be emitted early or mangled.
    #[test]
    fn holds_a_partial_sample_frame() {
        let mut bridge = PcmBridge::new(true);
        let hdr = header(&[0, 1], 48_000, 0);
        assert!(push(&mut bridge, &hdr).frames.is_empty());

        // One full frame (8 bytes) plus 3 bytes of the next.
        let pcm = i32_frames(&[vec![11, 22], vec![33, 44]]);
        let r = push(&mut bridge, &pcm[..11]);
        assert_eq!(r.frames.iter().map(|f| f.sample_count).sum::<u32>(), 1);
        assert_eq!(r.frames[0].pcm.as_slice(), &[11, 22]);

        // The rest of it arrives.
        let r = push(&mut bridge, &pcm[11..]);
        assert_eq!(r.frames.iter().map(|f| f.sample_count).sum::<u32>(), 1);
        assert_eq!(r.frames[0].pcm.as_slice(), &[33, 44]);
    }

    /// A block larger than BLOCK_FRAMES is split, and nothing is lost.
    #[test]
    fn splits_large_pushes_into_blocks() {
        let mut stream = header(&[0, 1], 48_000, 0);
        let n = BLOCK_FRAMES * 2 + 5;
        let frames: Vec<Vec<i32>> = (0..n as i32).map(|i| vec![i, -i]).collect();
        stream.extend(i32_frames(&frames));

        let mut bridge = PcmBridge::new(true);
        let r = push(&mut bridge, &stream);
        assert_eq!(r.frames.len(), 3);
        assert_eq!(r.frames[0].sample_count as usize, BLOCK_FRAMES);
        assert_eq!(r.frames[1].sample_count as usize, BLOCK_FRAMES);
        assert_eq!(r.frames[2].sample_count as usize, 5);
        assert_eq!(
            r.frames.iter().map(|f| f.sample_count).sum::<u32>() as usize,
            n
        );
    }

    /// The seek path: reset, then a header describing a different geometry.
    #[test]
    fn reset_then_a_new_header_of_a_different_shape() {
        use RChannelLabel::*;
        let mut bridge = PcmBridge::new(false);

        let mut first = header(&[0, 1, 2, 3, 4, 5], 48_000, 0);
        first.extend(i32_frames(&[vec![1, 2, 3, 4, 5, 6]]));
        assert_eq!(push(&mut bridge, &first).frames.len(), 1);

        bridge.reset();

        // Channel geometry changes; the rate deliberately does not. The engine
        // is built once at the session rate and timestamps by it, so a header
        // announcing a different one would play at the wrong speed - see the
        // invariant in the `header` module. A test that changed it here would
        // be advertising a capability nothing implements.
        let mut second = header(&[0, 1], 48_000, 0);
        second.extend(i32_frames(&[vec![9, 8]]));
        let r = push(&mut bridge, &second);
        assert!(r.error_message.is_empty(), "{}", r.error_message);
        assert_eq!(r.frames.len(), 1);
        assert_eq!(r.frames[0].channel_count, 2);
        assert_eq!(r.frames[0].sampling_frequency, 48_000);
        assert_eq!(r.frames[0].channel_labels.as_slice(), &[L, R]);
    }

    /// Partial PCM buffered before a reset must not be prepended to the next
    /// stream, where it would be read at the new format's frame size.
    #[test]
    fn reset_discards_buffered_partial_pcm() {
        let mut bridge = PcmBridge::new(true);
        let hdr = header(&[0, 1], 48_000, 0);
        push(&mut bridge, &hdr);
        push(&mut bridge, &[0xAA, 0xBB, 0xCC]); // three orphan bytes

        bridge.reset();

        let mut next = header(&[0, 1], 48_000, 0);
        next.extend(i32_frames(&[vec![7, 7]]));
        let r = push(&mut bridge, &next);
        assert!(r.error_message.is_empty(), "{}", r.error_message);
        assert_eq!(r.frames[0].pcm.as_slice(), &[7, 7]);
    }

    #[test]
    /// The host always constructs with `false`, so that is the mode that has to
    /// report. Both are asserted because the flag is ignored, not inverted.
    fn a_bad_header_is_reported_whatever_the_ignored_strict_flag_says() {
        let junk = b"RIFF____WAVEfmt junk".to_vec();

        for strict in [true, false] {
            let mut bridge = PcmBridge::new(strict);
            let r = push(&mut bridge, &junk);
            assert!(
                !r.error_message.is_empty(),
                "strict={strict}: a malformed header must be reported, not swallowed"
            );
            assert!(r.did_reset);
            assert!(r.frames.is_empty());
        }
    }

    /// The failure the silence came from: a rejected header leaves the parser
    /// waiting for another one, and the PCM that follows is not a header. Every
    /// one of those pushes has to keep saying so, or the host sees a stream
    /// that returns success and no audio for as long as the file lasts.
    #[test]
    fn every_push_after_a_rejected_header_keeps_reporting() {
        let mut bridge = PcmBridge::new(false);
        assert!(
            !push(&mut bridge, b"NOPE________________")
                .error_message
                .is_empty()
        );

        // What Kodi would send next: plain interleaved samples.
        let pcm = i32_frames(&[vec![1, 2], vec![3, 4]]);
        for _ in 0..3 {
            let r = push(&mut bridge, &pcm);
            assert!(
                !r.error_message.is_empty(),
                "silence must not look like success"
            );
            assert!(r.frames.is_empty());
        }
    }

    #[test]
    fn refuses_labels_the_renderer_cannot_place() {
        // 24 is Object and 255 is Unknown; virtual_bed routes neither, so their
        // audio would be dropped rather than misplaced.
        for label in [24u8, 255u8] {
            let mut bridge = PcmBridge::new(false);
            let r = push(&mut bridge, &header(&[0, label], 48_000, 0));
            assert!(!r.error_message.is_empty(), "label {label} must be refused");
            assert!(r.frames.is_empty());
        }
    }

    #[test]
    fn refuses_input_that_is_not_raw_opcm() {
        let mut stream = header(&[0, 1], 48_000, 0);
        stream.extend(i32_frames(&[vec![1, 2]]));

        let mut bridge = PcmBridge::new(false);
        let r = bridge.push_packet(RSlice::from_slice(&stream), RInputTransport::Iec61937, 0);
        assert!(
            !r.error_message.is_empty(),
            "IEC 61937 is not an OPCM stream"
        );
        assert!(r.frames.is_empty());

        let mut bridge = PcmBridge::new(false);
        let r = bridge.push_packet(RSlice::from_slice(&stream), RInputTransport::Raw, 7);
        assert!(
            !r.error_message.is_empty(),
            "raw input must carry data_type 0"
        );
        assert!(r.frames.is_empty());
    }

    #[test]
    fn accepts_the_presentation_key_and_nothing_else() {
        let mut bridge = PcmBridge::new(true);
        assert!(bridge.configure(RStr::from("presentation"), RStr::from("0")));
        assert!(!bridge.configure(RStr::from("anything-else"), RStr::from("1")));
        assert!(bridge.supported_drc_modes().is_empty());
        assert!(!bridge.set_drc_mode(RStr::from("film_standard")));
    }
}
