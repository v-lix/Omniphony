//! Pure DSP helpers shared by the engine and the CLI host (no I/O).

use bridge_api::RChannelLabel;

/// Convert the `render.lfe_gain` trim from dB to the linear multiplier
/// [`fill_pcm_f32_drc`] takes.
///
/// Exactly 1.0 at the 0 dB default — the trim costs not even a transcendental
/// until someone moves it. Shared by the engine and the CLI host so the two
/// cannot disagree about what a decibel means.
#[inline]
pub fn lfe_gain_linear(lfe_gain_db: f32) -> f32 {
    if lfe_gain_db == 0.0 {
        1.0
    } else {
        10.0_f32.powf(lfe_gain_db / 20.0)
    }
}

/// The LFE trim as the PCM conversion sees it: which lanes it applies to, where
/// it is heading, and the multiplier actually in force.
///
/// `current` is the host's, not this function's: it is carried across decoded
/// frames so a live change to `render.lfe_gain` is slewed rather than stepped.
/// A gain that jumps mid-waveform is a sample discontinuity, and sub-bass at
/// +10 dB is exactly where that is audible as a click; the renderer slews every
/// other gain for the same reason (`spatial_renderer::GAIN_SLEW_SECS`).
pub struct LfeTrim<'a> {
    /// Per-channel labels for this frame; only `LFE`/`LFE2` lanes are touched.
    pub labels: &'a [RChannelLabel],
    /// Linear target, from `render.lfe_gain` via [`lfe_gain_linear`].
    pub target: f32,
    /// Multiplier in force, owned by the host and advanced here.
    pub current: &'a mut f32,
    /// Samples a full-scale move is allowed to take — the stream rate times
    /// `GAIN_SLEW_SECS`. `0.0` applies the target immediately.
    pub ramp_samples: f32,
}

/// Convert interleaved 24-bit-scaled `i32` PCM to `f32`, applying a per-sample
/// DRC gain ramp from `*current_gain` toward `target_gain` over the remaining
/// `*ramp_remaining` samples, then the LFE trim.
///
/// `out` is cleared and refilled with the same length as `pcm`. `current_gain`
/// and `ramp_remaining` are advanced in place so the ramp continues seamlessly
/// across calls (one decoded frame per call).
///
/// The trim is applied only to the lanes [`LfeTrim::labels`] marks
/// [`RChannelLabel::LFE`] or [`RChannelLabel::LFE2`]. Unity (the 0 dB default,
/// with nothing in flight) is a no-op, and every other lane is untouched at any
/// value, so the trim follows the LFE channel wherever the renderer routes it:
/// direct to a sub on a speaker layout, to both ears in binaural, or VBAP-panned
/// if the virtual bed spatializes it. Applying it here rather than in the
/// renderer is what makes one implementation cover all of those.
///
/// A non-finite target is ignored rather than propagated: the config layer
/// clamps the value long before this, and a NaN reaching the render buffer
/// would poison the whole frame. Dropping bad input matches the OSC contract.
#[inline]
pub fn fill_pcm_f32_drc(
    out: &mut Vec<f32>,
    pcm: &[i32],
    channel_count: usize,
    current_gain: &mut f32,
    target_gain: f32,
    ramp_remaining: &mut u32,
    lfe: LfeTrim<'_>,
) {
    const SCALE: f32 = 8_388_608.0; // 2^23 — decoded samples are 24-bit in i32

    out.clear();
    out.reserve(pcm.len().saturating_sub(out.capacity()));

    if channel_count == 0 {
        return;
    }
    let sample_count = pcm.len() / channel_count;

    for s in 0..sample_count {
        let gain = if *ramp_remaining > 0 {
            let step = (target_gain - *current_gain) / *ramp_remaining as f32;
            *current_gain += step;
            *ramp_remaining -= 1;
            *current_gain
        } else {
            *current_gain = target_gain;
            target_gain
        };

        let scaled_gain = gain / SCALE;

        for c in 0..channel_count {
            let val = pcm[s * channel_count + c];
            out.push(val as f32 * scaled_gain);
        }
    }

    // The LFE trim runs as a second strided pass over the LFE lanes rather than
    // as a branch inside the conversion loop above. At the 0 dB default with
    // nothing in flight nothing here executes at all, and when it does it
    // touches one or two lanes, so every other channel stays bit-identical by
    // construction rather than by care — the property the tests below pin.
    let start = *lfe.current;
    if !lfe.target.is_finite() {
        return;
    }
    if start == 1.0 && lfe.target == 1.0 {
        return;
    }

    // Constant-rate slew, the same shape the renderer's per-channel gains use
    // (`ChannelState::slew_gain`): a full-scale move takes `ramp_samples`, so a
    // live change is spread over the frames it needs instead of landing on one
    // sample. Settled, or slewing switched off, it is one constant multiplier
    // and the interpolation costs nothing.
    let (base, step) = if lfe.ramp_samples > 0.0 && start != lfe.target {
        let max_step = sample_count as f32 / lfe.ramp_samples;
        let delta = (lfe.target - start).clamp(-max_step, max_step);
        *lfe.current = start + delta;
        let step = if sample_count > 0 {
            delta / sample_count as f32
        } else {
            0.0
        };
        (start, step)
    } else {
        *lfe.current = lfe.target;
        (lfe.target, 0.0)
    };

    for (c, label) in lfe.labels.iter().enumerate() {
        if c >= channel_count {
            break;
        }
        if !matches!(label, RChannelLabel::LFE | RChannelLabel::LFE2) {
            continue;
        }
        for s in 0..sample_count {
            out[s * channel_count + c] *= base + step * s as f32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bridge_api::RChannelLabel::*;

    /// Convert with the DRC ramp idle and the trim already settled at `lfe`, so
    /// the only variable under test is the trim's steady-state effect.
    fn convert(pcm: &[i32], channel_count: usize, labels: &[RChannelLabel], lfe: f32) -> Vec<f32> {
        let mut current = lfe;
        convert_slewed(pcm, channel_count, labels, lfe, &mut current, 0.0)
    }

    /// Convert with the trim mid-move: `current` is carried in and advanced, as
    /// a host carries it between decoded frames.
    fn convert_slewed(
        pcm: &[i32],
        channel_count: usize,
        labels: &[RChannelLabel],
        target: f32,
        current: &mut f32,
        ramp_samples: f32,
    ) -> Vec<f32> {
        let mut out = Vec::new();
        let mut gain = 1.0;
        let mut ramp = 0;
        fill_pcm_f32_drc(
            &mut out,
            pcm,
            channel_count,
            &mut gain,
            1.0,
            &mut ramp,
            LfeTrim {
                labels,
                target,
                current,
                ramp_samples,
            },
        );
        out
    }

    /// 5.1 with the LFE in its usual slot 3, two sample frames.
    fn pcm_5_1() -> (Vec<i32>, Vec<RChannelLabel>) {
        let pcm = vec![
            1 << 20,
            2 << 20,
            3 << 20,
            4 << 20,
            5 << 20,
            6 << 20,
            7 << 20,
            8 << 20,
            9 << 20,
            10 << 20,
            11 << 20,
            12 << 20,
        ];
        (pcm, vec![L, R, C, LFE, Ls, Rs])
    }

    /// +10 dB, the ceiling the Kodi host exposes.
    fn plus_10_db() -> f32 {
        10.0_f32.powf(10.0 / 20.0)
    }

    #[test]
    fn unity_lfe_gain_is_the_plain_conversion() {
        let (pcm, labels) = pcm_5_1();
        let expected: Vec<f32> = pcm.iter().map(|v| *v as f32 / 8_388_608.0).collect();
        assert_eq!(convert(&pcm, 6, &labels, 1.0), expected);
    }

    #[test]
    fn lfe_gain_scales_the_lfe_lane_and_nothing_else() {
        let (pcm, labels) = pcm_5_1();
        let g = plus_10_db();
        let base = convert(&pcm, 6, &labels, 1.0);
        let boosted = convert(&pcm, 6, &labels, g);

        assert_eq!(base.len(), boosted.len());
        for (i, (b, x)) in base.iter().zip(boosted.iter()).enumerate() {
            if i % 6 == 3 {
                assert_eq!(*x, *b * g, "LFE lane at {i} not scaled");
            } else {
                assert_eq!(*x, *b, "non-LFE lane at {i} was modified");
            }
        }
    }

    #[test]
    fn lfe_gain_scales_lfe2_as_well() {
        let pcm = vec![1 << 20, 2 << 20, 3 << 20, 4 << 20, 5 << 20, 6 << 20];
        let labels = vec![L, LFE, LFE2];
        let g = plus_10_db();
        let base = convert(&pcm, 3, &labels, 1.0);
        let boosted = convert(&pcm, 3, &labels, g);

        for (i, (b, x)) in base.iter().zip(boosted.iter()).enumerate() {
            if i % 3 == 0 {
                assert_eq!(*x, *b, "L lane at {i} was modified");
            } else {
                assert_eq!(*x, *b * g, "LFE/LFE2 lane at {i} not scaled");
            }
        }
    }

    #[test]
    fn a_stream_without_an_lfe_label_is_unchanged() {
        let pcm = vec![1 << 20, 2 << 20, 3 << 20, 4 << 20];
        let labels = vec![L, R];
        assert_eq!(
            convert(&pcm, 2, &labels, plus_10_db()),
            convert(&pcm, 2, &labels, 1.0)
        );
    }

    #[test]
    fn object_lanes_past_the_bed_are_untouched() {
        // A bed of L/R/LFE followed by two dynamic objects: only the labelled
        // LFE moves, objects carry their own gain from the stream metadata.
        let pcm = vec![1 << 20, 2 << 20, 3 << 20, 4 << 20, 5 << 20];
        let labels = vec![L, R, LFE, Object, Object];
        let g = plus_10_db();
        let base = convert(&pcm, 5, &labels, 1.0);
        let boosted = convert(&pcm, 5, &labels, g);

        assert_eq!(boosted[0], base[0]);
        assert_eq!(boosted[1], base[1]);
        assert_eq!(boosted[2], base[2] * g);
        assert_eq!(boosted[3], base[3]);
        assert_eq!(boosted[4], base[4]);
    }

    #[test]
    fn more_labels_than_channels_does_not_index_out_of_bounds() {
        let pcm = vec![1 << 20, 2 << 20];
        // Six labels, two real channels: the LFE label sits past the end.
        let labels = vec![L, R, C, LFE, Ls, Rs];
        assert_eq!(
            convert(&pcm, 2, &labels, plus_10_db()),
            convert(&pcm, 2, &labels, 1.0)
        );
    }

    #[test]
    fn fewer_labels_than_channels_scales_what_it_can_name() {
        let pcm = vec![1 << 20, 2 << 20, 3 << 20, 4 << 20];
        // Labels cover only the first two of four channels.
        let labels = vec![LFE, R];
        let g = plus_10_db();
        let base = convert(&pcm, 4, &labels, 1.0);
        let boosted = convert(&pcm, 4, &labels, g);

        assert_eq!(boosted[0], base[0] * g);
        assert_eq!(boosted[1], base[1]);
        assert_eq!(boosted[2], base[2]);
        assert_eq!(boosted[3], base[3]);
    }

    #[test]
    fn empty_labels_leave_every_lane_alone() {
        let (pcm, _) = pcm_5_1();
        assert_eq!(
            convert(&pcm, 6, &[], plus_10_db()),
            convert(&pcm, 6, &[], 1.0)
        );
    }

    #[test]
    fn a_non_finite_lfe_gain_is_ignored() {
        let (pcm, labels) = pcm_5_1();
        let base = convert(&pcm, 6, &labels, 1.0);
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(convert(&pcm, 6, &labels, bad), base, "{bad} was applied");
        }
    }

    #[test]
    fn a_zero_channel_count_yields_nothing() {
        let (pcm, labels) = pcm_5_1();
        assert!(convert(&pcm, 0, &labels, plus_10_db()).is_empty());
    }

    #[test]
    fn muting_the_lfe_zeroes_only_that_lane() {
        let (pcm, labels) = pcm_5_1();
        let base = convert(&pcm, 6, &labels, 1.0);
        let muted = convert(&pcm, 6, &labels, 0.0);
        for (i, (b, m)) in base.iter().zip(muted.iter()).enumerate() {
            if i % 6 == 3 {
                assert_eq!(*m, 0.0);
            } else {
                assert_eq!(*m, *b);
            }
        }
    }

    #[test]
    fn the_drc_ramp_still_advances_across_calls_with_the_trim_active() {
        // The trim must not disturb the ramp state the caller carries between
        // frames: same ramp arithmetic with the trim on as with it off.
        let (pcm, labels) = pcm_5_1();
        let mut run = |lfe: f32| {
            let mut out = Vec::new();
            let mut gain = 0.0;
            let mut ramp = 6;
            let mut current = lfe;
            fill_pcm_f32_drc(
                &mut out,
                &pcm,
                6,
                &mut gain,
                1.0,
                &mut ramp,
                LfeTrim {
                    labels: &labels,
                    target: lfe,
                    current: &mut current,
                    ramp_samples: 0.0,
                },
            );
            (gain, ramp)
        };
        assert_eq!(run(plus_10_db()), run(1.0));
    }

    /// The finding this ramp exists for: changing the trim between two decoded
    /// frames of a constant signal must not step the waveform. Without the
    /// slew the boundary sample jumps by the full gain ratio at once.
    #[test]
    fn a_trim_change_between_frames_does_not_step_the_waveform() {
        // Constant full-ish LFE, so any gain step shows up directly as a jump.
        const N: usize = 64;
        let pcm: Vec<i32> = std::iter::repeat_n(1 << 22, N * 2).collect();
        let labels = vec![LFE, R];
        let ramp_samples = 960.0; // 20 ms at 48 kHz, as the hosts pass

        let mut current = 1.0;
        let a = convert_slewed(&pcm, 2, &labels, 1.0, &mut current, ramp_samples);
        // Now the user moves the control to +10 dB.
        let b = convert_slewed(&pcm, 2, &labels, plus_10_db(), &mut current, ramp_samples);

        // Step across the frame boundary, and the largest step within frame b.
        let lfe_of = |v: &[f32], s: usize| v[s * 2];
        let boundary = (lfe_of(&b, 0) - lfe_of(&a, N - 1)).abs();
        let mut worst_inside: f32 = 0.0;
        for s in 1..N {
            worst_inside = worst_inside.max((lfe_of(&b, s) - lfe_of(&b, s - 1)).abs());
        }
        // One sample of a 20 ms full-scale slew moves the multiplier by
        // 1/960 of the way, so on this signal every step is a small fraction
        // of the sample value. An unslewed change would jump by ~2.16x it.
        let sample = lfe_of(&a, 0);
        assert!(
            boundary < sample * 0.02,
            "boundary step {boundary} too large for sample {sample}"
        );
        assert!(
            worst_inside < sample * 0.02,
            "in-frame step {worst_inside} too large for sample {sample}"
        );
        // And it is actually moving toward the target, not stuck.
        assert!(current > 1.0 && current < plus_10_db(), "current={current}");
    }

    /// The slew must converge: held at the target across enough frames, the
    /// multiplier arrives exactly and stops.
    #[test]
    fn the_slew_reaches_the_target_and_settles() {
        let pcm: Vec<i32> = std::iter::repeat_n(1 << 20, 2 * 480).collect();
        let labels = vec![LFE, R];
        let target = plus_10_db();
        let mut current = 1.0;
        for _ in 0..16 {
            let _ = convert_slewed(&pcm, 2, &labels, target, &mut current, 960.0);
        }
        assert_eq!(current, target, "slew never settled at the target");
    }

    /// A zero ramp length applies the target at once — the path the settled
    /// steady-state tests above rely on.
    #[test]
    fn a_zero_ramp_applies_the_target_immediately() {
        let (pcm, labels) = pcm_5_1();
        let mut current = 1.0;
        let out = convert_slewed(&pcm, 6, &labels, plus_10_db(), &mut current, 0.0);
        assert_eq!(current, plus_10_db());
        // First LFE sample already carries the full target.
        let base = convert(&pcm, 6, &labels, 1.0);
        assert_eq!(out[3], base[3] * plus_10_db());
    }
}
