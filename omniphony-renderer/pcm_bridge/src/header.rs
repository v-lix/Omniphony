//! The `OPCM` stream header: what the host declares before it sends PCM.
//!
//! A WAV header cannot say which speaker a channel belongs to without going
//! through `WAVE_FORMAT_EXTENSIBLE`'s `dwChannelMask`, and a mask is a lossy
//! round trip: its bit order is the interleave order, so the host has to sort
//! its own layout into mask order and the bridge has to translate every bit
//! back into a label. Positions with no mask bit (and `TOP_BACK_CENTER`, which
//! has no [`RChannelLabel`]) cannot survive it at all.
//!
//! So this header carries the labels themselves, one byte each, in the order
//! the samples are interleaved. There is nothing to translate and nothing to
//! sort: what the host decoded is what the renderer places.
//!
//! ```text
//! offset  size  field
//! 0       4     magic "OPCM"
//! 4       2     version, currently 1                        (u16 LE)
//! 6       2     channel count N, 1..=MAX_CHANNELS           (u16 LE)
//! 8       4     sample rate in Hz                           (u32 LE)
//! 12      1     sample encoding, see `SampleFormat`
//! 13      1     reserved, must be zero
//! 14      N     one RChannelLabel per channel, interleave order
//! ```
//!
//! Total header length is `14 + N`. No length field follows: the stream runs
//! until the host resets the bridge, which returns it here to await a fresh
//! header. That is the whole of the framing — a host that has to describe a new
//! format resets and sends another header.
//!
//! # The sample rate is the host's promise, not this bridge's check
//!
//! The rate field must equal the rate the session was opened at. The engine is
//! built once, at `OrenderConfig::sample_rate`: the spatial renderer and the
//! binaural DSP are sized for it, and `orender_process` timestamps its output
//! by dividing the running sample position by `Engine::sample_rate()`, the
//! session's rate rather than the frame's. A header declaring a different rate
//! would therefore play at the wrong speed through DSP tuned for another one.
//!
//! This bridge cannot enforce that, because nothing tells it what the session
//! rate is - `configure` is the only channel a host has, and the production
//! host calls it only for `presentation`. So the field is validated for being
//! non-zero and otherwise trusted. Kodi's side holds the invariant by
//! converting every stream to 48 kHz before it gets here; a host that cannot
//! do the same must open the session at the rate it intends to send.
//!
//! Changing rate mid-stream is not supported by any amount of resetting:
//! `Engine::reset` rebuilds the segment state, not the renderer.

use bridge_api::RChannelLabel;

/// Full-scale magnitude of the renderer's 24-bit-in-`i32` PCM convention.
/// `orender_engine::render::fill_pcm_f32_drc` divides decoded PCM by `2^23` to
/// obtain `f32`, so this is what `1.0` has to become.
const PCM_FULL_SCALE: f32 = 8_388_607.0; // 2^23 - 1

/// Bytes before the label array.
pub(crate) const FIXED_HEADER_LEN: usize = 14;

/// Magic identifying an `OPCM` stream.
pub(crate) const MAGIC: &[u8; 4] = b"OPCM";

/// The only header version this bridge understands.
pub(crate) const VERSION: u16 = 1;

/// Upper bound on the declared channel count. Well above any layout the
/// renderer places, and low enough that a corrupt header cannot make the
/// header parser wait for a payload that will never arrive.
pub(crate) const MAX_CHANNELS: u16 = 64;

/// How the host encoded its samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SampleFormat {
    /// Signed 24-bit scaled into an `i32`, which is the renderer's own
    /// convention: the bridge passes these straight through.
    I32Scaled24 = 0,
    /// 32-bit float, nominally -1.0..=1.0.
    F32 = 1,
}

impl SampleFormat {
    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(SampleFormat::I32Scaled24),
            1 => Some(SampleFormat::F32),
            _ => None,
        }
    }

    /// Bytes occupied by one sample. Both encodings are four.
    pub(crate) fn bytes_per_sample(self) -> usize {
        4
    }

    /// Convert one little-endian sample to the renderer's 24-bit-scaled `i32`.
    #[inline]
    pub(crate) fn decode_sample(self, bytes: &[u8]) -> i32 {
        let raw = [bytes[0], bytes[1], bytes[2], bytes[3]];
        match self {
            // Already in the renderer's convention.
            SampleFormat::I32Scaled24 => i32::from_le_bytes(raw),
            // Clamping is inherent to the fixed-point convention, not to this
            // bridge: anything past full scale has nowhere to go. A host that
            // wants to decide for itself what happens above 0 dBFS should send
            // I32Scaled24 and do the conversion where it still has the float.
            SampleFormat::F32 => {
                let v = f32::from_le_bytes(raw);
                if !v.is_finite() {
                    0
                } else {
                    (v.clamp(-1.0, 1.0) * PCM_FULL_SCALE) as i32
                }
            }
        }
    }
}

/// Stream geometry parsed from an `OPCM` header.
#[derive(Debug, Clone)]
pub(crate) struct PcmFormat {
    pub(crate) channels: u16,
    pub(crate) sample_rate: u32,
    pub(crate) sample_format: SampleFormat,
    pub(crate) labels: Vec<RChannelLabel>,
}

impl PcmFormat {
    /// Bytes occupied by one interleaved sample-frame (all channels).
    pub(crate) fn bytes_per_frame(&self) -> usize {
        self.sample_format.bytes_per_sample() * self.channels as usize
    }
}

/// Outcome of a header-parse attempt over the accumulated buffer.
pub(crate) enum HeaderParse {
    /// Not enough bytes buffered yet.
    NeedMore,
    /// Header parsed; PCM begins at `consumed`.
    Found { format: PcmFormat, consumed: usize },
    /// The buffer does not begin with a header this bridge can use.
    Invalid(&'static str),
}

/// Map one header byte to a channel label.
///
/// Only the fixed speaker positions are accepted. `Object` (24) and `Unknown`
/// (255) are both refused, for the same reason: the renderer has nowhere to put
/// them. `virtual_bed::fallback_virtual_bed_pose` returns `None` for both —
/// "`Object`/`Unknown` have no direct route" — so their audio is not placed
/// badly, it is dropped, and a bed of nothing but unknown channels renders as
/// silence.
///
/// An earlier version of this took `Unknown` to mean "carry it anyway, just
/// without a canonical position". That is not what the renderer does with it,
/// and accepting it turned a host mapping mistake into a channel that vanished
/// without a word. Refusing the header instead sends the stream back to the
/// host's ordinary decoder, where every channel is at least audible.
fn label_from_byte(b: u8) -> Option<RChannelLabel> {
    use RChannelLabel::*;
    Some(match b {
        0 => L,
        1 => R,
        2 => C,
        3 => LFE,
        4 => Ls,
        5 => Rs,
        6 => Tfl,
        7 => Tfr,
        8 => Tsl,
        9 => Tsr,
        10 => Tbl,
        11 => Tbr,
        12 => Lsc,
        13 => Rsc,
        14 => Lb,
        15 => Rb,
        16 => Cb,
        17 => Tc,
        18 => Lsd,
        19 => Rsd,
        20 => Lw,
        21 => Rw,
        22 => Tfc,
        23 => LFE2,
        _ => return None,
    })
}

fn read_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

fn read_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

/// Attempt to parse an `OPCM` header from the front of `buf`.
pub(crate) fn parse_header(buf: &[u8]) -> HeaderParse {
    if buf.len() < FIXED_HEADER_LEN {
        // Only reject early on bytes we already have: a header arriving one
        // byte at a time must not be judged on its first byte alone.
        let known = buf.len().min(MAGIC.len());
        if buf[..known] != MAGIC[..known] {
            return HeaderParse::Invalid("missing OPCM magic");
        }
        return HeaderParse::NeedMore;
    }
    if &buf[0..4] != MAGIC {
        return HeaderParse::Invalid("missing OPCM magic");
    }

    let version = read_u16(buf, 4);
    if version != VERSION {
        return HeaderParse::Invalid("unsupported OPCM header version");
    }

    let channels = read_u16(buf, 6);
    if channels == 0 {
        return HeaderParse::Invalid("zero channels");
    }
    if channels > MAX_CHANNELS {
        return HeaderParse::Invalid("channel count above the supported maximum");
    }

    let sample_rate = read_u32(buf, 8);
    if sample_rate == 0 {
        return HeaderParse::Invalid("zero sample rate");
    }

    let Some(sample_format) = SampleFormat::from_byte(buf[12]) else {
        return HeaderParse::Invalid("unsupported sample encoding");
    };
    if buf[13] != 0 {
        return HeaderParse::Invalid("reserved header byte is not zero");
    }

    let consumed = FIXED_HEADER_LEN + channels as usize;
    if buf.len() < consumed {
        return HeaderParse::NeedMore;
    }

    let mut labels = Vec::with_capacity(channels as usize);
    for &b in &buf[FIXED_HEADER_LEN..consumed] {
        match label_from_byte(b) {
            Some(label) => labels.push(label),
            None => return HeaderParse::Invalid("unrecognised channel label"),
        }
    }

    HeaderParse::Found {
        format: PcmFormat {
            channels,
            sample_rate,
            sample_format,
            labels,
        },
        consumed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a header for `labels`, so tests read as the layout they describe.
    pub(crate) fn header(labels: &[u8], sample_rate: u32, fmt: u8) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&VERSION.to_le_bytes());
        buf.extend_from_slice(&(labels.len() as u16).to_le_bytes());
        buf.extend_from_slice(&sample_rate.to_le_bytes());
        buf.push(fmt);
        buf.push(0);
        buf.extend_from_slice(labels);
        buf
    }

    #[test]
    fn parses_a_5_1_side_header() {
        use RChannelLabel::*;
        let buf = header(&[0, 1, 2, 3, 4, 5], 48_000, 1);
        match parse_header(&buf) {
            HeaderParse::Found { format, consumed } => {
                assert_eq!(consumed, buf.len());
                assert_eq!(format.channels, 6);
                assert_eq!(format.sample_rate, 48_000);
                assert_eq!(format.sample_format, SampleFormat::F32);
                assert_eq!(format.labels, vec![L, R, C, LFE, Ls, Rs]);
                assert_eq!(format.bytes_per_frame(), 24);
            }
            _ => panic!("expected Found"),
        }
    }

    /// The distinction a WAV channel count cannot express: 5.1 with side
    /// surrounds and 5.1 with back surrounds are both six channels.
    #[test]
    fn side_and_back_5_1_are_distinguishable() {
        use RChannelLabel::*;
        let side = header(&[0, 1, 2, 3, 4, 5], 48_000, 0);
        let back = header(&[0, 1, 2, 3, 14, 15], 48_000, 0);
        let labels = |b: &[u8]| match parse_header(b) {
            HeaderParse::Found { format, .. } => format.labels,
            _ => panic!("expected Found"),
        };
        assert_eq!(labels(&side), vec![L, R, C, LFE, Ls, Rs]);
        assert_eq!(labels(&back), vec![L, R, C, LFE, Lb, Rb]);
        assert_ne!(labels(&side), labels(&back));
    }

    /// Whatever order the host interleaves in is the order the renderer gets.
    /// A WAVE mask would have re-sorted this into ascending bit order.
    #[test]
    fn label_order_is_preserved_verbatim() {
        use RChannelLabel::*;
        // 7.1.4 as FFmpeg lays it out: back pair before side pair.
        let buf = header(&[0, 1, 2, 3, 14, 15, 4, 5, 6, 7, 10, 11], 48_000, 1);
        match parse_header(&buf) {
            HeaderParse::Found { format, .. } => assert_eq!(
                format.labels,
                vec![L, R, C, LFE, Lb, Rb, Ls, Rs, Tfl, Tfr, Tbl, Tbr]
            ),
            _ => panic!("expected Found"),
        }
    }

    #[test]
    fn needs_more_while_the_header_is_incomplete() {
        let buf = header(&[0, 1], 48_000, 1);
        for n in 0..buf.len() {
            assert!(
                matches!(parse_header(&buf[..n]), HeaderParse::NeedMore),
                "a {n}-byte prefix should ask for more, not fail"
            );
        }
        assert!(matches!(parse_header(&buf), HeaderParse::Found { .. }));
    }

    #[test]
    fn rejects_bad_headers() {
        let bad = |b: Vec<u8>| matches!(parse_header(&b), HeaderParse::Invalid(_));

        assert!(bad(b"RIFF____WAVEfmt ".to_vec()), "wrong magic");

        let mut wrong_version = header(&[0, 1], 48_000, 1);
        wrong_version[4] = 2;
        assert!(bad(wrong_version), "version 2");

        assert!(bad(header(&[], 48_000, 1)), "zero channels");
        assert!(bad(header(&[0, 1], 0, 1)), "zero sample rate");
        assert!(bad(header(&[0, 1], 48_000, 7)), "unknown encoding");
        assert!(bad(header(&[0, 24], 48_000, 1)), "Object label");
        assert!(bad(header(&[0, 99], 48_000, 1)), "undefined label");

        let mut reserved_set = header(&[0, 1], 48_000, 1);
        reserved_set[13] = 1;
        assert!(bad(reserved_set), "reserved byte set");

        let mut too_many = header(&[0, 1], 48_000, 1);
        too_many[6..8].copy_from_slice(&(MAX_CHANNELS + 1).to_le_bytes());
        assert!(bad(too_many), "channel count over the maximum");
    }

    #[test]
    fn i32_samples_pass_through_and_f32_is_scaled() {
        assert_eq!(
            SampleFormat::I32Scaled24.decode_sample(&8_388_607i32.to_le_bytes()),
            8_388_607
        );
        assert_eq!(
            SampleFormat::I32Scaled24.decode_sample(&(-1234i32).to_le_bytes()),
            -1234
        );

        assert_eq!(
            SampleFormat::F32.decode_sample(&1.0f32.to_le_bytes()),
            8_388_607
        );
        assert_eq!(
            SampleFormat::F32.decode_sample(&(-1.0f32).to_le_bytes()),
            -8_388_607
        );
        assert_eq!(SampleFormat::F32.decode_sample(&0.0f32.to_le_bytes()), 0);
        // Past full scale, and not a number at all.
        assert_eq!(
            SampleFormat::F32.decode_sample(&2.0f32.to_le_bytes()),
            8_388_607
        );
        assert_eq!(SampleFormat::F32.decode_sample(&f32::NAN.to_le_bytes()), 0);
    }
}
