//! Binaural room impulse responses (BRIR): a measured room as raw kernel
//! pairs for the partitioned convolver.
//!
//! A BRIR set is the opposite of the free-field HRIR set the direct binaural
//! path uses. The HRIR loader ([`super::measured`]) keeps a few milliseconds
//! after the onset of each ear, makes it minimum-phase, supplies the
//! interaural delay analytically and synthesises the room separately. A room
//! response *is* the room: the propagation delay, the interaural delay, the
//! early reflections and the tail are the measurement, so this loader keeps
//! the pair intact and only:
//!
//! * removes the silence common to the whole set ahead of the earliest onset
//!   (the relative delays between emitters and ears survive);
//! * cuts each tail where its backward-integrated energy falls
//!   [`BrirLoadOptions::tail_floor_db`] below the total (Schroeder) and
//!   under [`BrirLoadOptions::max_length_s`], with a short raised-cosine fade;
//! * resamples to the engine rate with the crate's own windowed-sinc kernel
//!   (the SOFA reader's resampler works on a misread shape for
//!   `MultiSpeakerBRIR`, see below);
//! * normalises the set so the mean direct-sound energy is one, the same
//!   scale as an HRIR set, so switching between the two keeps the level.
//!
//! # Geometry
//!
//! The set is organised as **emitters × head orientations**. An emitter is a
//! virtual loudspeaker: the position a response was measured *from*,
//! relative to the listener, in the renderer's frame (`x` right, `y` front,
//! `z` up, metres). A head orientation is where the listener looked during
//! the measurement, as `(yaw, pitch)` in degrees with the renderer's sign
//! convention (yaw positive to the right, pitch positive up). Every SOFA
//! room convention maps onto that grid:
//!
//! * `MultiSpeakerBRIR` — `E` emitters per measurement (`Data.IR` is
//!   `[M][R][E][N]`), `M` head orientations in `ListenerView`;
//! * one-emitter conventions (`SingleRoomSRIR`, `SingleRoomDRIR`, or a
//!   `SimpleFreeFieldHRIR` that carries room-length responses, the ASH
//!   Toolset export): each measurement is one `(SourcePosition,
//!   ListenerView)`; distinct source positions are the emitters, distinct
//!   views the orientations.
//!
//! Every emitter must have been measured at every kept orientation.
//!
//! # SOFA shape
//!
//! `sofar` assumes `Data.IR` is `[M][R][N]` and takes the third axis for
//! `N`, so a `[M][R][E][N]` file reports `N = E` (issue #219). The file is
//! therefore opened without its responses (`LazySofa`, which neither
//! resamples nor normalises), the shape is read from the HDF dataspace, and
//! only the measurements the kept orientations come from are read: one head
//! orientation of a 274 MB set when the listener is not tracked.

use rayon::prelude::*;

use super::hrir::HRIR_SPAN_S;
use super::measured::ResampleKernel;

/// Onset threshold relative to the response's peak (−40 dB).
const ONSET_FRAC: f32 = 0.01;
/// Samples kept ahead of the earliest onset of the set.
const LEAD_GUARD: usize = 32;
/// Raised-cosine fade at the tail cut, seconds.
const TAIL_FADE_S: f32 = 0.005;
/// Positions closer than this are the same emitter, metres.
const SAME_POINT_M: f32 = 0.01;
/// Orientations closer than this are the same, degrees.
const SAME_ANGLE_DEG: f32 = 0.05;
/// Below this peak a set is silence (the HRIR loader's bound).
const SILENT_PEAK: f32 = 1e-9;

/// Which measured head orientations to keep.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OrientationSelection {
    /// Every orientation in the file (head tracking over the full set).
    All,
    /// Only the orientation nearest to straight ahead — no head tracking,
    /// and the memory of a single orientation.
    FrontOnly,
    /// One orientation per `step_deg` of yaw within `±max_yaw_deg` (the
    /// nearest measured one each time), plus straight ahead.
    Decimated { step_deg: f32, max_yaw_deg: f32 },
}

/// Load-time choices for [`BrirSet::from_raw`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrirLoadOptions {
    pub orientations: OrientationSelection,
    /// Upper bound on the kept response length in seconds, counted after
    /// the common lead is removed and including the fade. `0` = no bound.
    pub max_length_s: f32,
    /// Tail cut: decibels below the response's total energy at which the
    /// remaining tail is dropped (Schroeder backward integration).
    pub tail_floor_db: f32,
}

impl Default for BrirLoadOptions {
    fn default() -> Self {
        Self {
            orientations: OrientationSelection::All,
            max_length_s: 2.0,
            tail_floor_db: 60.0,
        }
    }
}

/// One measured pair, engine rate, equal lengths.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BrirPair {
    pub left: Vec<f32>,
    pub right: Vec<f32>,
}

impl BrirPair {
    /// Kernel length in samples (both ears).
    pub fn taps(&self) -> usize {
        self.left.len()
    }
}

/// The raw arrays of a room-response SOFA file, decoupled from the reader so
/// the loader can be exercised in memory. Coordinates are cartesian in the
/// SOFA frame (`x` front, `y` left, `z` up, metres), as `sofar` delivers
/// them after opening.
#[derive(Clone, Copy, Debug)]
pub struct RawRoomIr<'a> {
    /// The file's `SOFAConventions` attribute (informational).
    pub conventions: &'a str,
    pub sample_rate: f32,
    /// Measurements, receivers, emitters, taps: the true `Data.IR` shape.
    pub m: usize,
    pub r: usize,
    pub e: usize,
    pub n: usize,
    /// `[M][C]` or `[I][C]`.
    pub source_position: &'a [f32],
    /// `[E][C][I]`, relative to the source.
    pub emitter_position: &'a [f32],
    /// `[M][C]` or `[I][C]`.
    pub listener_position: &'a [f32],
    /// `[M][C]` or `[I][C]`; a view vector.
    pub listener_view: &'a [f32],
    /// `[M][R][N]` (`E = 1`) or `[M][R][E][N]`: every measurement, or the
    /// run from `ir_first` that [`ExtractedRoom::measurements`] names for a
    /// selection.
    pub data_ir: &'a [f32],
    /// The measurement `data_ir` starts at: 0 for the whole set.
    pub ir_first: usize,
    /// `[I][R]`, `[M][R]`, `[I][R][E]` or `[M][R][E]`; empty = none.
    pub data_delay: &'a [f32],
}

/// A loaded BRIR set: emitters × orientations kernel pairs at the engine
/// rate. Immutable once built; `Send + Sync` for the rebuild worker.
#[derive(Clone, Debug)]
pub struct BrirSet {
    sample_rate: u32,
    /// Virtual loudspeakers relative to the listener, renderer frame,
    /// metres.
    emitters: Vec<[f32; 3]>,
    /// `(yaw, pitch)` degrees, renderer convention, sorted by yaw then pitch.
    orientations: Vec<(f32, f32)>,
    /// `pairs[emitter * orientations.len() + orientation]`.
    pairs: Vec<BrirPair>,
    max_taps: usize,
    conventions: String,
    /// The file's `RoomType` attribute (`shoebox`, `reverberant`, …), when
    /// it states one.
    room_type: Option<String>,
    /// The room's two opposite corners (`RoomCornerA`, `RoomCornerB`), when
    /// the file states them: relative to the listener, renderer frame,
    /// metres — the box the loudspeakers stand in.
    room_corners: Option<[[f32; 3]; 2]>,
}

/// The two corners of a SOFA shoebox room, SOFA frame, taken relative to
/// the listener and into the renderer's frame: the box a client draws the
/// set's loudspeakers in.
pub fn room_corners_relative(corners: [[f32; 3]; 2], listener: [f32; 3]) -> [[f32; 3]; 2] {
    let relative = |c: [f32; 3]| {
        omniphony_geometry::f32::sofa_to_adm([
            c[0] - listener[0],
            c[1] - listener[1],
            c[2] - listener[2],
        ])
    };
    [relative(corners[0]), relative(corners[1])]
}

/// The room a BRIR set's loudspeakers stand in, relative to the listener in
/// the renderer's frame (`x` right, `y` front, `z` up, metres): the box the
/// stage pans in while the set renders, in place of the user's room
/// ([`crate::live_params::RoomRatios`]). The file's two corners when it
/// states them (`RoomCornerA`/`RoomCornerB`), grown to contain every
/// loudspeaker so that each keeps its place in the panning space; else an
/// estimate from the loudspeakers' bounding box — a margin on every side, a
/// floor under the listener's ears and some headroom — which the state says
/// is one (`estimated`).
#[derive(Clone, Debug, PartialEq)]
pub struct MeasuredRoom {
    /// `[min, max]` corners.
    pub box_m: [[f32; 3]; 2],
    /// The box is the loudspeakers' bounding box with margins, not the
    /// file's room.
    pub estimated: bool,
}

impl MeasuredRoom {
    /// Metres kept beyond the farthest loudspeaker on each side when the
    /// file states no room.
    pub const BOX_MARGIN_M: f32 = 0.3;
    /// The floor at least this far below the listener's ears, and this much
    /// headroom above, when the file states no room.
    pub const FLOOR_M: f32 = 1.2;
    pub const HEADROOM_M: f32 = 1.0;

    /// The room of a set whose loudspeakers stand at `emitters` (relative
    /// to the listener, renderer frame, metres), inside `corners` when the
    /// file states them.
    pub fn of(emitters: &[[f32; 3]], corners: Option<[[f32; 3]; 2]>) -> Self {
        let mut lo = [f32::INFINITY; 3];
        let mut hi = [f32::NEG_INFINITY; 3];
        let mut grow = |p: [f32; 3]| {
            for axis in 0..3 {
                lo[axis] = lo[axis].min(p[axis]);
                hi[axis] = hi[axis].max(p[axis]);
            }
        };
        for &e in emitters {
            grow(e);
        }
        if let Some([a, b]) = corners {
            grow(a);
            grow(b);
            // The listener stands in the room too.
            grow([0.0; 3]);
            return Self {
                box_m: [lo, hi],
                estimated: false,
            };
        }
        if emitters.is_empty() {
            lo = [0.0; 3];
            hi = [0.0; 3];
        }
        for axis in 0..3 {
            lo[axis] -= Self::BOX_MARGIN_M;
            hi[axis] += Self::BOX_MARGIN_M;
        }
        lo[2] = lo[2].min(-Self::FLOOR_M);
        hi[2] = hi[2].max(Self::HEADROOM_M);
        Self {
            box_m: [lo, hi],
            estimated: true,
        }
    }

    /// Metres to one unit of the room's ratios: the half-width, as the
    /// user's room counts it (`config_fields::room`), taken to the farther
    /// side wall so that every loudspeaker reads as a fraction within the
    /// cube — the ratios describe a room the listener is centred in
    /// across, which a measured room need not be.
    pub fn radius_m(&self) -> f32 {
        let [lo, hi] = self.box_m;
        lo[0].abs().max(hi[0].abs()).max(0.01)
    }

    /// The room as the stage pans in it: width 1 (the reference, like the
    /// user's room), the other extents as multiples of [`Self::radius_m`].
    /// `center_blend` is the user's front/rear blend — how the cube's depth
    /// is mapped into a room the listener is not centred in along, a
    /// panning policy rather than a measurement.
    pub fn ratios(&self, center_blend: f32) -> crate::live_params::RoomRatios {
        let radius = self.radius_m();
        let [lo, hi] = self.box_m;
        let extent = |m: f32| (m / radius).max(omniphony_geometry::f32::MIN_ROOM_RATIO);
        crate::live_params::RoomRatios {
            ratio: [1.0, extent(hi[1]), extent(hi[2])],
            rear: extent(-lo[1]),
            lower: extent(-lo[2]),
            center_blend: center_blend.clamp(0.0, 1.0),
        }
    }
}

/// Row `i` of a `[M][C]` or `[I][C]` array (the single row when the array
/// holds one), or the origin.
fn row3(arr: &[f32], i: usize) -> [f32; 3] {
    let at = |k: usize| [arr[k], arr[k + 1], arr[k + 2]];
    if arr.len() >= 3 * (i + 1) {
        at(3 * i)
    } else if arr.len() >= 3 {
        at(0)
    } else {
        [0.0; 3]
    }
}

/// `(yaw, pitch)` in degrees, renderer convention, of a SOFA view vector.
/// A degenerate vector is straight ahead.
fn view_to_yaw_pitch(v: [f32; 3]) -> (f32, f32) {
    let horiz = (v[0] * v[0] + v[1] * v[1]).sqrt();
    if horiz + v[2].abs() < 1e-6 {
        return (0.0, 0.0);
    }
    // SOFA azimuth is counter-clockwise (left positive); ours is right positive.
    let yaw = (-v[1]).atan2(v[0]).to_degrees();
    let pitch = v[2].atan2(horiz).to_degrees();
    (wrap_deg(yaw), pitch)
}

/// Wrap into `(-180, 180]`.
///
/// Not `omniphony_geometry::wrap_deg`: that one adds 180 before reducing,
/// which rounds away the low bits of small angles (0.1 → 0.100006), and the
/// nearest-orientation scans below compare these distances for ties.
fn wrap_deg(a: f32) -> f32 {
    let mut a = a.rem_euclid(360.0);
    if a > 180.0 {
        a -= 360.0;
    }
    a
}

/// Squared angular distance between two orientations, degrees².
fn orientation_dist2(a: (f32, f32), b: (f32, f32)) -> f32 {
    let dy = wrap_deg(a.0 - b.0);
    let dp = a.1 - b.1;
    dy * dy + dp * dp
}

/// First sample at or above `ONSET_FRAC` of the peak, or 0 for silence.
fn onset(ir: &[f32]) -> usize {
    let peak = ir.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
    if peak <= SILENT_PEAK {
        return 0;
    }
    let thresh = ONSET_FRAC * peak;
    ir.iter().position(|&x| x.abs() >= thresh).unwrap_or(0)
}

/// Length to keep so that the dropped tail holds less than `floor_db` below
/// the total energy (Schroeder backward integration). At least 1 for a
/// non-silent response.
fn tail_cut(ir: &[f32], floor_db: f32) -> usize {
    let total: f64 = ir.iter().map(|&x| (x as f64) * (x as f64)).sum();
    if total <= 0.0 {
        return 0;
    }
    let floor = total * 10f64.powf(-floor_db as f64 / 10.0);
    let mut acc = 0.0f64;
    for (k, &x) in ir.iter().enumerate().rev() {
        acc += (x as f64) * (x as f64);
        if acc > floor {
            return k + 1;
        }
    }
    1
}

/// Apply a raised-cosine fade over the last `fade` samples of `ir`.
fn fade_out(ir: &mut [f32], fade: usize) {
    let n = ir.len();
    let fade = fade.min(n);
    if fade < 2 {
        return;
    }
    for i in 0..fade {
        let t = (i + 1) as f32 / fade as f32;
        let w = 0.5 * (1.0 + (std::f32::consts::PI * t).cos());
        ir[n - fade + i] *= w;
    }
}

/// Longest `Data.Delay` a set may declare, in seconds.
const MAX_DATA_DELAY_S: f32 = 1.0;

/// A room's measured pairs at the kept head orientations, before anything is
/// done to them: at the file's rate, `Data.Delay` applied, full length. What
/// [`BrirSet::from_raw`] extracts from a file before [`BrirSet::finish`]
/// trims, resamples and normalises it, and what a prepared room holds (see
/// [`ExtractedRoom::to_prepared`]): a file reduced to the orientation a host
/// renders loads exactly as the whole file would.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtractedRoom {
    conventions: String,
    file_rate: u32,
    /// Virtual loudspeakers relative to the listener, renderer frame, metres.
    emitters: Vec<[f32; 3]>,
    /// `(yaw, pitch)` degrees, renderer convention, sorted.
    orientations: Vec<(f32, f32)>,
    /// `pairs[emitter * orientations.len() + orientation]`.
    pairs: Vec<BrirPair>,
    /// What the host says the room was made from, kept verbatim in a prepared
    /// room so that the host can tell, from the room alone, whether it is the
    /// one a file would prepare. The engine never reads meaning into it
    /// ([`Self::with_source`]).
    source: String,
    /// The file's `RoomType`, when it states one (see [`BrirSet`]).
    room_type: Option<String>,
    /// The room's corners around the listener, renderer frame, metres, when
    /// the file states them (see [`BrirSet`]).
    room_corners: Option<[[f32; 3]; 2]>,
}

impl BrirSet {
    /// Build a set from raw SOFA arrays. Errors name the shape or geometry
    /// problem; a set that comes out silent is refused rather than rendered.
    pub fn from_raw(
        raw: &RawRoomIr<'_>,
        engine_rate: u32,
        opts: &BrirLoadOptions,
    ) -> anyhow::Result<Self> {
        if engine_rate == 0 {
            anyhow::bail!("engine rate is zero");
        }
        Self::finish(
            ExtractedRoom::extract(raw, opts.orientations)?,
            engine_rate,
            opts,
        )
    }
}

impl ExtractedRoom {
    /// The pairs of `raw` at the orientations `selection` keeps, untouched.
    /// Errors name the shape or geometry problem, as [`BrirSet::from_raw`]'s.
    pub fn extract(raw: &RawRoomIr<'_>, selection: OrientationSelection) -> anyhow::Result<Self> {
        RoomPlan::new(raw, selection)?.fill(raw)
    }

    /// The run of measurements the orientations `selection` keeps are read
    /// from: what a reader needs of `Data.IR` (passed as `data_ir` from
    /// `ir_first`) to extract them. Only `raw`'s geometry is consulted, so
    /// its `data_ir` may be empty; the errors are [`Self::extract`]'s.
    pub fn measurements(
        raw: &RawRoomIr<'_>,
        selection: OrientationSelection,
    ) -> anyhow::Result<std::ops::Range<usize>> {
        Ok(RoomPlan::new(raw, selection)?.measurements(raw.e))
    }
}

/// What [`ExtractedRoom::extract`] decides from a file's geometry before it
/// touches a response: the loudspeakers, the kept orientations, and the
/// measurement each kept pair is read from.
struct RoomPlan {
    file_rate: u32,
    emitters: Vec<[f32; 3]>,
    /// The kept orientations, sorted.
    orientations: Vec<(f32, f32)>,
    /// Head orientations the file measured, kept or not.
    #[cfg_attr(not(feature = "sofa"), allow(dead_code))]
    measured_orientations: usize,
    /// Per pair (`emitter * orientations.len() + orientation`): the slot
    /// `measurement * E + emitter slot` it is read from.
    slots: Vec<usize>,
}

impl RoomPlan {
    fn new(raw: &RawRoomIr<'_>, selection: OrientationSelection) -> anyhow::Result<Self> {
        let (m, r, e, n) = (raw.m, raw.r, raw.e, raw.n);
        if r < 2 {
            anyhow::bail!("{r} receiver(s); a binaural set needs the two ears");
        }
        if m == 0 || e == 0 || n == 0 {
            anyhow::bail!("empty set (M = {m}, E = {e}, N = {n})");
        }
        // The dimensions come from the file: their product must not wrap.
        let total = m
            .checked_mul(r)
            .and_then(|v| v.checked_mul(e))
            .and_then(|v| v.checked_mul(n));
        if total.is_none() {
            anyhow::bail!(
                "Data.IR holds {} values for M×R×E×N = {}×{}×{}×{}",
                raw.data_ir.len(),
                m,
                r,
                e,
                n
            );
        }
        if !raw.sample_rate.is_finite() || raw.sample_rate < 1.0 {
            anyhow::bail!("invalid sampling rate {}", raw.sample_rate);
        }
        let file_rate = raw.sample_rate.round() as u32;
        // Data.Delay pads each response with that many samples; a direct-path
        // delay is milliseconds. One past a second is a broken file, and
        // honouring it would allocate the padding for every response.
        let max_delay = file_rate as f32 * MAX_DATA_DELAY_S;
        if let Some(d) = raw
            .data_delay
            .iter()
            .find(|d| d.is_finite() && **d > max_delay)
        {
            anyhow::bail!("Data.Delay of {d} samples is implausible (over {MAX_DATA_DELAY_S} s)");
        }

        // --- geometry: (measurement, emitter slot) → (emitter, orientation)
        let mut emitters: Vec<[f32; 3]> = Vec::new();
        let mut orientations: Vec<(f32, f32)> = Vec::new();
        let mut find_or_push_emitter = |p: [f32; 3]| -> usize {
            if let Some(i) = emitters.iter().position(|q| {
                let d = [q[0] - p[0], q[1] - p[1], q[2] - p[2]];
                (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt() <= SAME_POINT_M
            }) {
                i
            } else {
                emitters.push(p);
                emitters.len() - 1
            }
        };
        let mut find_or_push_orientation = |o: (f32, f32)| -> usize {
            if let Some(i) = orientations
                .iter()
                .position(|q| orientation_dist2(*q, o).sqrt() <= SAME_ANGLE_DEG)
            {
                i
            } else {
                orientations.push(o);
                orientations.len() - 1
            }
        };
        // Per (measurement, emitter slot): emitter index, orientation index.
        let mut meas: Vec<(usize, usize)> = Vec::with_capacity(m * e);
        for mi in 0..m {
            let listener = row3(raw.listener_position, mi);
            let source = row3(raw.source_position, mi);
            let view = row3(raw.listener_view, mi);
            let o = find_or_push_orientation(view_to_yaw_pitch(view));
            for k in 0..e {
                // The emitter is relative to the source; a one-emitter
                // convention leaves it at the source (zero), so the same sum
                // serves both.
                let em = row3(raw.emitter_position, k);
                let rel = [
                    source[0] + em[0] - listener[0],
                    source[1] + em[1] - listener[1],
                    source[2] + em[2] - listener[2],
                ];
                let ei = find_or_push_emitter(omniphony_geometry::f32::sofa_to_adm(rel));
                meas.push((ei, o));
            }
        }
        // Emitters at the origin cannot be a direction.
        if let Some(p) = emitters
            .iter()
            .find(|p| (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt() < SAME_POINT_M)
        {
            anyhow::bail!(
                "an emitter sits on the listener ({:.3}, {:.3}, {:.3}); no direction to render",
                p[0],
                p[1],
                p[2]
            );
        }

        // --- orientations: sort, then select
        let mut order: Vec<usize> = (0..orientations.len()).collect();
        order.sort_by(|&a, &b| {
            orientations[a]
                .partial_cmp(&orientations[b])
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let sorted: Vec<(f32, f32)> = order.iter().map(|&i| orientations[i]).collect();
        let mut remap = vec![0usize; orientations.len()];
        for (new, &old) in order.iter().enumerate() {
            remap[old] = new;
        }
        for (_, o) in meas.iter_mut() {
            *o = remap[*o];
        }
        let orientations = sorted;
        let kept = select_orientations(&orientations, selection);
        if kept.is_empty() {
            anyhow::bail!("no head orientation selected");
        }

        // --- completeness: one measurement per (emitter, kept orientation)
        let ne = emitters.len();
        let no = kept.len();
        let mut slot: Vec<Option<usize>> = vec![None; ne * no];
        for (idx, &(ei, oi)) in meas.iter().enumerate() {
            let Some(ki) = kept.iter().position(|&k| k == oi) else {
                continue;
            };
            let s = &mut slot[ei * no + ki];
            if s.is_none() {
                *s = Some(idx);
            } else {
                log::warn!(
                    "BRIR: emitter {ei} measured twice at orientation {:?}; keeping the first",
                    orientations[oi]
                );
            }
        }
        if let Some(missing) = slot.iter().position(Option::is_none) {
            let (ei, ki) = (missing / no, missing % no);
            anyhow::bail!(
                "emitter {ei} at ({:.2}, {:.2}, {:.2}) m has no measurement at orientation \
                 yaw {:.1}°, pitch {:.1}°",
                emitters[ei][0],
                emitters[ei][1],
                emitters[ei][2],
                orientations[kept[ki]].0,
                orientations[kept[ki]].1
            );
        }
        Ok(Self {
            file_rate,
            emitters,
            measured_orientations: orientations.len(),
            orientations: kept.iter().map(|&k| orientations[k]).collect(),
            slots: slot
                .into_iter()
                .map(|s| s.expect("checked complete above"))
                .collect(),
        })
    }

    /// The run of measurements the kept pairs are read from.
    fn measurements(&self, e: usize) -> std::ops::Range<usize> {
        let first = self.slots.iter().map(|s| s / e).min().unwrap_or(0);
        let last = self.slots.iter().map(|s| s / e).max().unwrap_or(0);
        first..last + 1
    }

    /// Read the kept pairs out of `raw.data_ir`, applying `Data.Delay`: the
    /// whole set, or exactly the run [`Self::measurements`] names.
    fn fill(self, raw: &RawRoomIr<'_>) -> anyhow::Result<ExtractedRoom> {
        let (m, r, e, n) = (raw.m, raw.r, raw.e, raw.n);
        // The product was checked in `new`.
        let row = r * e * n;
        let run = self.measurements(e);
        let whole = raw.ir_first == 0 && raw.data_ir.len() == m * row;
        let just_run = raw.ir_first == run.start && raw.data_ir.len() == run.len() * row;
        if !whole && !just_run {
            anyhow::bail!(
                "Data.IR holds {} values{} for M×R×E×N = {}×{}×{}×{}",
                raw.data_ir.len(),
                if raw.ir_first > 0 {
                    format!(" from measurement {}", raw.ir_first)
                } else {
                    String::new()
                },
                m,
                r,
                e,
                n
            );
        }
        // A NaN would pass the silence guard (max skips it) and reach the
        // convolver, which would then output nothing but NaN.
        if let Some(i) = raw.data_ir.iter().position(|v| !v.is_finite()) {
            anyhow::bail!(
                "Data.IR value {} is not finite ({})",
                raw.ir_first * row + i,
                raw.data_ir[i]
            );
        }

        // --- extract the pairs (file rate), applying Data.Delay
        let delay_of = |mi: usize, ri: usize, k: usize| -> usize {
            let d = raw.data_delay;
            let v = if d.len() == m * r * e {
                d[(mi * r + ri) * e + k]
            } else if d.len() == r * e {
                d[ri * e + k]
            } else if d.len() == m * r {
                d[mi * r + ri]
            } else if d.len() == r {
                d[ri]
            } else {
                0.0
            };
            if v.is_finite() && v > 0.0 {
                v.round() as usize
            } else {
                0
            }
        };
        let extract = |mi: usize, ri: usize, k: usize| -> Vec<f32> {
            let base = (((mi - raw.ir_first) * r + ri) * e + k) * n;
            let d = delay_of(mi, ri, k);
            let mut ir = vec![0.0f32; d + n];
            ir[d..].copy_from_slice(&raw.data_ir[base..base + n]);
            ir
        };
        let pairs: Vec<BrirPair> = self
            .slots
            .iter()
            .map(|&idx| {
                let (mi, k) = (idx / e, idx % e);
                BrirPair {
                    left: extract(mi, 0, k),
                    right: extract(mi, 1, k),
                }
            })
            .collect();
        Ok(ExtractedRoom {
            conventions: raw.conventions.to_string(),
            file_rate: self.file_rate,
            emitters: self.emitters,
            orientations: self.orientations,
            pairs,
            source: String::new(),
            room_type: None,
            room_corners: None,
        })
    }
}

/// Samples of silence every pair of a set starts with, less
/// [`LEAD_GUARD`]: what [`BrirSet::finish`] drops so the relative delays
/// between emitters and ears survive.
fn common_lead(pairs: &[BrirPair]) -> usize {
    pairs
        .iter()
        .map(|p| onset(&p.left).min(onset(&p.right)))
        .min()
        .unwrap_or(0)
        .saturating_sub(LEAD_GUARD)
}

impl BrirSet {
    /// Make an extracted room renderable at `engine_rate`: drop the silence
    /// common to the set, cut each tail under `opts` with a short fade,
    /// resample, and normalise to unit mean direct-sound energy (see the
    /// module doc). `opts.orientations` is not consulted: the room holds the
    /// orientations it was extracted with. A set that comes out silent is
    /// refused rather than rendered.
    pub fn finish(
        room: ExtractedRoom,
        engine_rate: u32,
        opts: &BrirLoadOptions,
    ) -> anyhow::Result<Self> {
        if engine_rate == 0 {
            anyhow::bail!("engine rate is zero");
        }
        let ExtractedRoom {
            conventions,
            file_rate,
            emitters,
            orientations,
            pairs,
            source: _,
            room_type,
            room_corners,
        } = room;

        // --- silence guard
        let peak = pairs
            .iter()
            .flat_map(|p| p.left.iter().chain(p.right.iter()))
            .fold(0.0f32, |m, &x| m.max(x.abs()));
        if peak <= SILENT_PEAK {
            anyhow::bail!("every response is silent (peak {peak:e})");
        }

        // --- common lead: keep the relative delays, drop the shared silence
        let lead = common_lead(&pairs);

        // --- tail cut + fade, then resample
        let fade = (TAIL_FADE_S * file_rate as f32).round() as usize;
        let max_len = if opts.max_length_s > 0.0 {
            (opts.max_length_s * file_rate as f32).round() as usize
        } else {
            usize::MAX
        };
        let kernel =
            (file_rate != engine_rate).then(|| ResampleKernel::new(file_rate, engine_rate));
        let pairs: Vec<BrirPair> = crate::background_pool::install(|| {
            pairs
                .into_par_iter()
                .map_init(Vec::new, |buf, mut p| {
                    p.left.drain(..lead.min(p.left.len()));
                    p.right.drain(..lead.min(p.right.len()));
                    // The fade lies beyond the cut point, so what is faded is
                    // already below the floor; a length bound is a hard limit
                    // and may fade audible content instead.
                    let cut = tail_cut(&p.left, opts.tail_floor_db)
                        .max(tail_cut(&p.right, opts.tail_floor_db));
                    let keep = (cut + fade).min(max_len).max(1);
                    p.left.resize(keep, 0.0);
                    p.right.resize(keep, 0.0);
                    fade_out(&mut p.left, fade);
                    fade_out(&mut p.right, fade);
                    if let Some(k) = &kernel {
                        k.resample_into(&p.left, buf);
                        p.left.clear();
                        p.left.extend_from_slice(buf);
                        k.resample_into(&p.right, buf);
                        p.right.clear();
                        p.right.extend_from_slice(buf);
                    }
                    p
                })
                .collect()
        });

        // --- normalise: unit mean direct-sound energy (the HRIR scale)
        let window = (HRIR_SPAN_S * engine_rate as f32).ceil() as usize;
        let mut acc = 0.0f64;
        let mut count = 0usize;
        for p in &pairs {
            for ir in [&p.left, &p.right] {
                let start = onset(ir);
                let end = (start + window).min(ir.len());
                acc += ir[start..end]
                    .iter()
                    .map(|&x| (x as f64) * (x as f64))
                    .sum::<f64>();
                count += 1;
            }
        }
        let mean = if count > 0 { acc / count as f64 } else { 0.0 };
        let mut pairs = pairs;
        if mean > 0.0 {
            let gain = (1.0 / mean.sqrt()) as f32;
            for p in &mut pairs {
                for x in p.left.iter_mut().chain(p.right.iter_mut()) {
                    *x *= gain;
                }
            }
        }

        let max_taps = pairs.iter().map(BrirPair::taps).max().unwrap_or(0);
        let set = Self {
            sample_rate: engine_rate,
            emitters,
            orientations,
            pairs,
            max_taps,
            conventions,
            room_type,
            room_corners,
        };
        log::info!(
            "BRIR: {} ({}): {} emitters × {} orientations, up to {} taps ({:.3} s) at {} Hz, {:.1} MiB",
            if set.conventions.is_empty() {
                "unnamed convention"
            } else {
                set.conventions.as_str()
            },
            if file_rate == engine_rate {
                "native rate".to_string()
            } else {
                format!("resampled from {file_rate} Hz")
            },
            set.emitters.len(),
            set.orientations.len(),
            set.max_taps,
            set.max_taps as f32 / engine_rate as f32,
            engine_rate,
            set.bytes() as f32 / (1024.0 * 1024.0)
        );
        Ok(set)
    }

    /// A set holding `pairs` exactly as given
    /// (`pairs[emitter * orientations.len() + orientation]`, orientations
    /// sorted): no lead removal, tail cut or normalisation. For tests that
    /// need kernels of exact lengths.
    #[cfg(test)]
    pub(crate) fn from_pairs(
        emitters: Vec<[f32; 3]>,
        orientations: Vec<(f32, f32)>,
        pairs: Vec<BrirPair>,
    ) -> Self {
        assert_eq!(pairs.len(), emitters.len() * orientations.len());
        let max_taps = pairs.iter().map(BrirPair::taps).max().unwrap_or(0);
        Self {
            sample_rate: 48_000,
            emitters,
            orientations,
            pairs,
            max_taps,
            conventions: "test".to_string(),
            room_type: None,
            room_corners: None,
        }
    }

    /// The file's `RoomType`, when it states one.
    pub fn room_type(&self) -> Option<&str> {
        self.room_type.as_deref()
    }

    /// The room's two opposite corners, relative to the listener in the
    /// renderer's frame, when the file states them.
    pub fn room_corners(&self) -> Option<[[f32; 3]; 2]> {
        self.room_corners
    }

    /// Engine rate the pairs are at.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Virtual loudspeakers relative to the listener (renderer frame, metres).
    pub fn emitters(&self) -> &[[f32; 3]] {
        &self.emitters
    }

    /// Kept head orientations, `(yaw, pitch)` degrees, sorted.
    pub fn orientations(&self) -> &[(f32, f32)] {
        &self.orientations
    }

    /// The pair measured from emitter `e` at orientation `o`.
    pub fn pair(&self, e: usize, o: usize) -> &BrirPair {
        &self.pairs[e * self.orientations.len() + o]
    }

    /// Longest kernel of the set, in samples.
    pub fn max_taps(&self) -> usize {
        self.max_taps
    }

    /// The file's `SOFAConventions`, for status displays.
    pub fn conventions(&self) -> &str {
        &self.conventions
    }

    /// Index of the kept orientation nearest to `(yaw, pitch)` degrees
    /// (yaw wraps). Linear scan: at most a few hundred entries, evaluated
    /// once per head-pose update.
    pub fn nearest_orientation(&self, yaw_deg: f32, pitch_deg: f32) -> usize {
        let q = (yaw_deg, pitch_deg);
        let mut best = 0;
        let mut best_d = f32::INFINITY;
        for (i, &o) in self.orientations.iter().enumerate() {
            let d = orientation_dist2(o, q);
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        best
    }

    /// Resident size of the kernels, bytes.
    pub fn bytes(&self) -> usize {
        self.pairs
            .iter()
            .map(|p| (p.left.len() + p.right.len()) * std::mem::size_of::<f32>())
            .sum()
    }
}

/// First bytes of a prepared room ([`ExtractedRoom::to_prepared`]).
pub const PREPARED_ROOM_MAGIC: [u8; 8] = *b"OMNIROOM";
/// Layout version of the prepared rooms this build writes. 2 ends with the
/// room's geometry (its `RoomType` and corners, when the file states them);
/// 1, without it, is still read.
const PREPARED_ROOM_VERSION: u32 = 2;
/// Magic, version, rate, emitter, orientation and conventions-length words.
const PREPARED_HEADER_LEN: usize = 8 + 5 * 4;
/// Most loudspeakers a prepared room holds. A measured listening room has
/// tens (the largest public sets have 32 and 24); a free-field HRTF set read
/// as a room has hundreds of directions, and is refused with that hint.
pub const PREPARED_MAX_EMITTERS: usize = 64;

// The binaural path builds its virtual array on every loudspeaker of a room
// it was given, with an LFE: the panner has to hold that many.
const _: () = assert!(PREPARED_MAX_EMITTERS < crate::spatial_vbap::MAX_SPEAKERS);

/// Most head orientations a prepared room holds.
const PREPARED_MAX_ORIENTATIONS: usize = 4096;
/// Longest `SOFAConventions` text kept, bytes.
const PREPARED_MAX_CONVENTIONS: usize = 256;
/// Longest source text a prepared room holds, bytes.
pub const PREPARED_MAX_SOURCE: usize = 4096;
/// Response kept after the set's common lead, seconds: the longest
/// `brir_max_length_s` takes, so any value of it but 0 (whole responses)
/// renders a prepared room exactly as it renders the file.
pub const PREPARED_MAX_LENGTH_S: f32 = 10.0;
/// Sampling rates a prepared room may declare, Hz.
const PREPARED_RATES: std::ops::RangeInclusive<u32> = 1_000..=768_000;

/// Cursor over a prepared room's bytes; every read is bounds-checked.
struct PreparedReader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> PreparedReader<'a> {
    fn take(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(n)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| anyhow::anyhow!("truncated at byte {}", self.at))?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn u32(&mut self) -> anyhow::Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn f32(&mut self) -> anyhow::Result<f32> {
        let v = f32::from_bits(self.u32()?);
        if !v.is_finite() {
            anyhow::bail!("non-finite value before byte {}", self.at);
        }
        Ok(v)
    }

    /// `n` finite floats, the length checked against what is left first.
    fn f32s(&mut self, n: usize) -> anyhow::Result<Vec<f32>> {
        let bytes = self.take(
            n.checked_mul(4)
                .ok_or_else(|| anyhow::anyhow!("length overflows"))?,
        )?;
        let out: Vec<f32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        if out.iter().any(|v| !v.is_finite()) {
            anyhow::bail!("non-finite sample before byte {}", self.at);
        }
        Ok(out)
    }

    /// The fixed header: rate, emitter count, orientation count,
    /// conventions length. Checks the magic and the version.
    fn header(&mut self) -> anyhow::Result<(u32, usize, usize, usize)> {
        if self.take(8)? != PREPARED_ROOM_MAGIC {
            anyhow::bail!("not a prepared room");
        }
        let version = self.u32()?;
        if !(1..=PREPARED_ROOM_VERSION).contains(&version) {
            anyhow::bail!(
                "prepared room layout {version}; this build reads {PREPARED_ROOM_VERSION}, \
                 so prepare the room again"
            );
        }
        let rate = self.u32()?;
        if !PREPARED_RATES.contains(&rate) {
            anyhow::bail!("implausible sampling rate {rate}");
        }
        let e = self.u32()? as usize;
        if !(1..=PREPARED_MAX_EMITTERS).contains(&e) {
            anyhow::bail!("{e} loudspeakers (1 to {PREPARED_MAX_EMITTERS} supported)");
        }
        let o = self.u32()? as usize;
        if !(1..=PREPARED_MAX_ORIENTATIONS).contains(&o) {
            anyhow::bail!("{o} head orientations (1 to {PREPARED_MAX_ORIENTATIONS} supported)");
        }
        let c = self.u32()? as usize;
        if c > PREPARED_MAX_CONVENTIONS {
            anyhow::bail!("conventions text of {c} bytes");
        }
        Ok((rate, e, o, c))
    }

    /// The room's geometry (layout 2), which follows the source text: its
    /// type, then whether its corners follow, and they.
    fn geometry(&mut self) -> anyhow::Result<RoomGeometry> {
        let n = self.u32()? as usize;
        if n > PREPARED_MAX_CONVENTIONS {
            anyhow::bail!("room type of {n} bytes");
        }
        let text = std::str::from_utf8(self.take(n)?)
            .map_err(|_| anyhow::anyhow!("room type is not UTF-8"))?;
        let room_type = (!text.is_empty()).then(|| text.to_string());
        let corners = match self.u32()? {
            0 => None,
            1 => {
                let values = self.f32s(6)?;
                if !values.iter().all(|v| v.is_finite()) {
                    anyhow::bail!("a room corner is not finite");
                }
                Some([
                    [values[0], values[1], values[2]],
                    [values[3], values[4], values[5]],
                ])
            }
            other => anyhow::bail!("room corners flag {other}"),
        };
        Ok((room_type, corners))
    }

    /// The source text, which follows the conventions.
    fn source(&mut self) -> anyhow::Result<String> {
        let n = self.u32()? as usize;
        if n > PREPARED_MAX_SOURCE {
            anyhow::bail!("source text of {n} bytes");
        }
        Ok(std::str::from_utf8(self.take(n)?)
            .map_err(|_| anyhow::anyhow!("source text is not UTF-8"))?
            .to_string())
    }

    /// `n` emitter positions, each a direction (not on the listener).
    fn emitters(&mut self, n: usize) -> anyhow::Result<Vec<[f32; 3]>> {
        (0..n)
            .map(|_| {
                let p = [self.f32()?, self.f32()?, self.f32()?];
                if (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt() < SAME_POINT_M {
                    anyhow::bail!("an emitter sits on the listener");
                }
                Ok(p)
            })
            .collect()
    }
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// `text` cut to at most `max` bytes, on a character boundary.
fn cut_to(text: &str, max: usize) -> &str {
    let mut cut = text.len().min(max);
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

fn put_f32s(out: &mut Vec<u8>, values: &[f32]) {
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

impl ExtractedRoom {
    /// The file's `SOFAConventions`.
    pub fn conventions(&self) -> &str {
        &self.conventions
    }

    /// Sampling rate of the pairs, Hz.
    pub fn file_rate(&self) -> u32 {
        self.file_rate
    }

    /// What the host said the room was made from.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The room, carrying `source` into its prepared image, cut to
    /// [`PREPARED_MAX_SOURCE`] bytes at a character boundary.
    pub fn with_source(mut self, source: &str) -> Self {
        let mut cut = source.len().min(PREPARED_MAX_SOURCE);
        while !source.is_char_boundary(cut) {
            cut -= 1;
        }
        self.source = source[..cut].to_string();
        self
    }

    /// Virtual loudspeakers relative to the listener, renderer frame (`x`
    /// right, `y` front, `z` up), metres.
    pub fn emitters(&self) -> &[[f32; 3]] {
        &self.emitters
    }

    /// Kept head orientations, `(yaw, pitch)` degrees, sorted.
    pub fn orientations(&self) -> &[(f32, f32)] {
        &self.orientations
    }

    /// Keep only the orientations `selection` picks of those held. Picking
    /// again from what a selection kept picks the same, so a prepared room
    /// loads under any selection as the file did under the one it was
    /// prepared with.
    pub fn select(self, selection: OrientationSelection) -> anyhow::Result<Self> {
        let kept = select_orientations(&self.orientations, selection);
        if kept.is_empty() {
            anyhow::bail!("no head orientation selected");
        }
        if kept.len() == self.orientations.len() {
            return Ok(self);
        }
        let no = self.orientations.len();
        let pairs = (0..self.emitters.len())
            .flat_map(|e| kept.iter().map(move |&k| e * no + k))
            .map(|i| self.pairs[i].clone())
            .collect();
        Ok(Self {
            orientations: kept.iter().map(|&k| self.orientations[k]).collect(),
            pairs,
            ..self
        })
    }

    /// Bound every response to [`PREPARED_MAX_LENGTH_S`] after the set's
    /// common lead.
    #[cfg_attr(not(feature = "sofa"), allow(dead_code))]
    fn cap_length(&mut self) {
        let cap = common_lead(&self.pairs)
            + (PREPARED_MAX_LENGTH_S * self.file_rate as f32).round() as usize;
        for p in &mut self.pairs {
            p.left.truncate(cap);
            p.right.truncate(cap);
        }
    }

    /// The prepared-room bytes of this room: a versioned little-endian
    /// image [`Self::from_prepared`] reads back exactly. Layout: the magic,
    /// then `u32` version, rate, emitter count `E`, orientation count `O`
    /// and conventions length; the conventions (UTF-8); the source text, a
    /// `u32` length then UTF-8; `E` positions of three `f32`;
    /// `O` orientations of two `f32`; then the `E·O` pairs, emitter-major,
    /// each its two `u32` lengths then the left and right samples.
    pub fn to_prepared(&self) -> Vec<u8> {
        let conventions = cut_to(&self.conventions, PREPARED_MAX_CONVENTIONS);
        let samples: usize = self
            .pairs
            .iter()
            .map(|p| p.left.len() + p.right.len())
            .sum();
        let mut out = Vec::with_capacity(
            PREPARED_HEADER_LEN
                + conventions.len()
                + 4
                + self.source.len()
                + 12 * self.emitters.len()
                + 8 * self.orientations.len()
                + 8 * self.pairs.len()
                + 4 * samples,
        );
        out.extend_from_slice(&PREPARED_ROOM_MAGIC);
        put_u32(&mut out, PREPARED_ROOM_VERSION);
        put_u32(&mut out, self.file_rate);
        put_u32(&mut out, self.emitters.len() as u32);
        put_u32(&mut out, self.orientations.len() as u32);
        put_u32(&mut out, conventions.len() as u32);
        out.extend_from_slice(conventions.as_bytes());
        put_u32(&mut out, self.source.len() as u32);
        out.extend_from_slice(self.source.as_bytes());
        // The room's geometry (layout 2), after the source text, which is as
        // far as a host reads, and ahead of the loudspeakers, so that a reader
        // of the header (`prepared_room_loudspeakers`) has both.
        let room_type = cut_to(
            self.room_type.as_deref().unwrap_or(""),
            PREPARED_MAX_CONVENTIONS,
        );
        put_u32(&mut out, room_type.len() as u32);
        out.extend_from_slice(room_type.as_bytes());
        put_u32(&mut out, u32::from(self.room_corners.is_some()));
        if let Some([a, b]) = self.room_corners {
            put_f32s(&mut out, &a);
            put_f32s(&mut out, &b);
        }
        for p in &self.emitters {
            put_f32s(&mut out, p);
        }
        for &(yaw, pitch) in &self.orientations {
            put_f32s(&mut out, &[yaw, pitch]);
        }
        for p in &self.pairs {
            put_u32(&mut out, p.left.len() as u32);
            put_u32(&mut out, p.right.len() as u32);
            put_f32s(&mut out, &p.left);
            put_f32s(&mut out, &p.right);
        }
        out
    }

    /// Read a prepared room back. Every count, length and value is checked
    /// before it is used; whatever the bytes hold is read or refused with a
    /// reason, never trusted.
    pub fn from_prepared(bytes: &[u8]) -> anyhow::Result<Self> {
        let mut r = PreparedReader { bytes, at: 0 };
        let (file_rate, ne, no, nc) = r.header()?;
        let conventions = std::str::from_utf8(r.take(nc)?)
            .map_err(|_| anyhow::anyhow!("conventions are not UTF-8"))?
            .to_string();
        let source = r.source()?;
        let (room_type, room_corners) = if bytes[8..12] == 1u32.to_le_bytes() {
            (None, None)
        } else {
            r.geometry()?
        };
        let emitters = r.emitters(ne)?;
        let orientations: Vec<(f32, f32)> = (0..no)
            .map(|_| Ok((r.f32()?, r.f32()?)))
            .collect::<anyhow::Result<_>>()?;
        if orientations
            .windows(2)
            .any(|w| w[0].partial_cmp(&w[1]) != Some(std::cmp::Ordering::Less))
        {
            anyhow::bail!("head orientations are not sorted and distinct");
        }
        // Data.Delay padding (at most a second) ahead of the kept length.
        let max_len =
            ((PREPARED_MAX_LENGTH_S + 2.0 * MAX_DATA_DELAY_S) * file_rate as f32).ceil() as usize;
        let mut pairs = Vec::with_capacity(ne * no);
        for _ in 0..ne * no {
            let (nl, nr) = (r.u32()? as usize, r.u32()? as usize);
            if !(1..=max_len).contains(&nl) || !(1..=max_len).contains(&nr) {
                anyhow::bail!("a response of {nl}/{nr} samples (1 to {max_len} supported)");
            }
            pairs.push(BrirPair {
                left: r.f32s(nl)?,
                right: r.f32s(nr)?,
            });
        }
        if r.at != bytes.len() {
            anyhow::bail!("{} bytes past the last response", bytes.len() - r.at);
        }
        Ok(Self {
            conventions,
            file_rate,
            emitters,
            orientations,
            pairs,
            source,
            room_type,
            room_corners,
        })
    }
}

/// A room's virtual loudspeakers, relative to the listener in the
/// renderer's frame, metres, and the corners of the room they stand in when
/// the file states them: what a host builds its virtual array on before the
/// room loads ([`measured_room_layout`]).
#[derive(Clone, Debug, PartialEq)]
pub struct RoomLoudspeakers {
    pub emitters: Vec<[f32; 3]>,
    pub corners: Option<[[f32; 3]; 2]>,
}

/// The loudspeaker positions of the prepared room at `path`, read from its
/// header alone: what a host sizes its virtual array by before the room
/// loads. `Ok(None)` when the file is not a prepared room (a SOFA file, or
/// too short to tell); an error when it is one whose header is unusable or
/// it cannot be read.
pub fn prepared_room_emitters(path: &std::path::Path) -> anyhow::Result<Option<Vec<[f32; 3]>>> {
    Ok(prepared_room_loudspeakers(path)?.map(|room| room.emitters))
}

/// [`prepared_room_emitters`], with the room's corners where the room keeps
/// them (layout 2).
pub fn prepared_room_loudspeakers(
    path: &std::path::Path,
) -> anyhow::Result<Option<RoomLoudspeakers>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut head = [0u8; PREPARED_HEADER_LEN];
    let mut got = 0;
    while got < head.len() {
        match file.read(&mut head[got..])? {
            0 => return Ok(None),
            n => got += n,
        }
    }
    if head[..8] != PREPARED_ROOM_MAGIC {
        return Ok(None);
    }
    let (_, ne, _, nc) = PreparedReader {
        bytes: &head,
        at: 0,
    }
    .header()?;
    let short = |e: std::io::Error| anyhow::anyhow!("prepared room header: {e}");
    let read_u32 = |file: &mut std::fs::File| -> anyhow::Result<usize> {
        let mut word = [0u8; 4];
        file.read_exact(&mut word).map_err(short)?;
        Ok(u32::from_le_bytes(word) as usize)
    };
    // The conventions, then the source text and its length.
    let mut skip = vec![0u8; nc];
    file.read_exact(&mut skip).map_err(short)?;
    let source = read_u32(&mut file)?;
    if source > PREPARED_MAX_SOURCE {
        anyhow::bail!("prepared room header: source text of {source} bytes");
    }
    let mut skip = vec![0u8; source];
    file.read_exact(&mut skip).map_err(short)?;
    // Layout 2: the room's geometry.
    let mut corners = None;
    if head[8..12] != 1u32.to_le_bytes() {
        let n = read_u32(&mut file)?;
        if n > PREPARED_MAX_CONVENTIONS {
            anyhow::bail!("prepared room header: room type of {n} bytes");
        }
        let mut geometry = vec![0u8; n + 4];
        file.read_exact(&mut geometry).map_err(short)?;
        if geometry[n..] != 0u32.to_le_bytes() {
            let mut values = vec![0u8; 24];
            file.read_exact(&mut values).map_err(short)?;
            let mut r = PreparedReader {
                bytes: &values,
                at: 0,
            };
            let v = r.f32s(6)?;
            if !v.iter().all(|x| x.is_finite()) {
                anyhow::bail!("prepared room header: a room corner is not finite");
            }
            corners = Some([[v[0], v[1], v[2]], [v[3], v[4], v[5]]]);
        }
    }
    let mut rest = vec![0u8; 12 * ne];
    file.read_exact(&mut rest).map_err(short)?;
    let mut r = PreparedReader {
        bytes: &rest,
        at: 0,
    };
    Ok(Some(RoomLoudspeakers {
        emitters: r.emitters(ne)?,
        corners,
    }))
}

/// The loudspeaker positions of the room at `path`, whichever form it
/// takes: a prepared room's from its header ([`prepared_room_emitters`]), a
/// room-response SOFA file's from its geometry, front orientation, as the
/// room loads (the file is read, its responses are not). What a host builds
/// its virtual array on before the room loads. `Ok(None)` when the file is
/// neither; an error when it is a room whose loudspeakers cannot be had.
pub fn room_emitters(path: &std::path::Path) -> anyhow::Result<Option<Vec<[f32; 3]>>> {
    Ok(room_loudspeakers(path)?.map(|room| room.emitters))
}

/// [`room_emitters`], with the room's corners where the file states them.
pub fn room_loudspeakers(path: &std::path::Path) -> anyhow::Result<Option<RoomLoudspeakers>> {
    if let Some(room) = prepared_room_loudspeakers(path)? {
        return Ok(Some(room));
    }
    sofa_room_loudspeakers(path)
}

#[cfg(feature = "sofa")]
fn sofa_room_loudspeakers(path: &std::path::Path) -> anyhow::Result<Option<RoomLoudspeakers>> {
    let bytes = std::fs::read(path)?;
    let Ok(sofa) = sofar::reader::LazySofa::open(&bytes) else {
        return Ok(None);
    };
    let emitters = with_sofa_geometry(&sofa, |raw| {
        let plan = RoomPlan::new(&raw, OrientationSelection::FrontOnly)?;
        check_room_size(plan.emitters.len())?;
        Ok(plan.emitters)
    })?;
    Ok(Some(RoomLoudspeakers {
        emitters,
        corners: sofa_room_geometry(&bytes).1,
    }))
}

#[cfg(not(feature = "sofa"))]
fn sofa_room_loudspeakers(_path: &std::path::Path) -> anyhow::Result<Option<RoomLoudspeakers>> {
    Ok(None)
}

/// The layout a session renders a room on, as
/// [`crate::live_params::RendererControl::brir_layout`] builds it once the
/// room is resident: its loudspeakers placed in the room they were measured
/// in ([`MeasuredRoom`]), with the user's front/rear `center_blend`. A host
/// that builds its session on it before the room loads renders the same
/// array throughout.
pub fn measured_room_layout(
    emitters: &[[f32; 3]],
    corners: Option<[[f32; 3]; 2]>,
    center_blend: f32,
) -> anyhow::Result<crate::speaker_layout::SpeakerLayout> {
    let measured = MeasuredRoom::of(emitters, corners);
    crate::speaker_layout::SpeakerLayout::from_brir_emitters(
        emitters,
        &measured.ratios(center_blend),
        measured.radius_m(),
    )
}

impl BrirSet {
    /// Load the room at `path`: a prepared room ([`prepare_room`]) or a
    /// room-response SOFA file, told apart by the prepared room's magic.
    /// Either way the set comes out as [`Self::from_raw`] makes it from the
    /// file the room was prepared from.
    pub fn load(path: &str, engine_rate: u32, opts: &BrirLoadOptions) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path).map_err(|e| anyhow::anyhow!("read '{path}': {e}"))?;
        if bytes.starts_with(&PREPARED_ROOM_MAGIC) {
            return ExtractedRoom::from_prepared(&bytes)
                .and_then(|room| room.select(opts.orientations))
                .and_then(|room| Self::finish(room, engine_rate, opts))
                .map_err(|e| anyhow::anyhow!("prepared room '{path}': {e}"));
        }
        Self::from_sofa_bytes(path, &bytes, engine_rate, opts)
    }

    #[cfg(not(feature = "sofa"))]
    fn from_sofa_bytes(
        path: &str,
        _bytes: &[u8],
        _engine_rate: u32,
        _opts: &BrirLoadOptions,
    ) -> anyhow::Result<Self> {
        anyhow::bail!(
            "'{path}' is not a prepared room, and SOFA support is not built into this \
             renderer (enable the 'sofa' feature)"
        )
    }
}

/// A room's `RoomType` and corners (see [`BrirSet`]), each where the file
/// states it.
type RoomGeometry = (Option<String>, Option<[[f32; 3]; 2]>);

/// A room prepared for a host: the extracted room ([`ExtractedRoom`], front
/// orientation only) and what a host shows about it.
#[derive(Clone, Debug)]
pub struct PreparedRoom {
    pub room: ExtractedRoom,
    /// The virtual loudspeakers' names, in the room's order, as the layout
    /// built on them names them ([`crate::speaker_layout::SpeakerLayout::from_brir_emitters`]):
    /// a standard name where an emitter stands near one, `E<n>` elsewhere.
    pub speaker_names: Vec<String>,
    /// Longest response kept under the default cut, seconds.
    pub seconds: f32,
}

/// Prepare a room-response SOFA file, held in memory, for a host without
/// head tracking: keep the head orientation nearest straight ahead, bound
/// the responses to [`PREPARED_MAX_LENGTH_S`], and check that the result
/// loads and builds a speaker layout. The returned room's
/// [`ExtractedRoom::to_prepared`] bytes are a file [`BrirSet::load`] reads
/// in a fraction of the time and memory the SOFA file takes, rendering it
/// exactly as the file renders without head tracking.
#[cfg(feature = "sofa")]
pub fn prepare_room(sofa: &[u8]) -> anyhow::Result<PreparedRoom> {
    let mut room = with_sofa_room(sofa, OrientationSelection::FrontOnly, |raw| {
        ExtractedRoom::extract(raw, OrientationSelection::FrontOnly)
    })?;
    (room.room_type, room.room_corners) = sofa_room_geometry(sofa);
    check_room_size(room.emitters.len())?;
    room.cap_length();
    let set = BrirSet::finish(room.clone(), room.file_rate, &BrirLoadOptions::default())?;
    let speaker_names = room_speaker_names(&room.emitters, room.room_corners)?;
    Ok(PreparedRoom {
        seconds: set.max_taps() as f32 / room.file_rate as f32,
        room,
        speaker_names,
    })
}

/// Refuse more directions than a listening room is prepared with: what has
/// hundreds is a free-field HRTF set, which belongs to the HRTF stage.
#[cfg(feature = "sofa")]
fn check_room_size(emitters: usize) -> anyhow::Result<()> {
    if emitters > PREPARED_MAX_EMITTERS {
        anyhow::bail!(
            "{emitters} measured directions, more than the {PREPARED_MAX_EMITTERS} loudspeakers \
             a listening room is prepared with: a free-field HRTF set is chosen as an HRTF"
        );
    }
    Ok(())
}

/// The loudspeakers' names, in the room's order, as the layout built on them
/// names them.
fn room_speaker_names(
    emitters: &[[f32; 3]],
    corners: Option<[[f32; 3]; 2]>,
) -> anyhow::Result<Vec<String>> {
    let layout = measured_room_layout(
        emitters,
        corners,
        crate::config_fields::room::DEFAULT_CENTER_BLEND,
    )?;
    // The layout appends its LFE after the room's loudspeakers.
    Ok(layout
        .speaker_names()
        .into_iter()
        .take(emitters.len())
        .map(str::to_string)
        .collect())
}

/// What a SOFA file or a prepared room holds, read from its shape and
/// geometry alone (no response is read), and which of this engine's two
/// binaural stages takes it, for a host to tell its user before anything is
/// copied or prepared.
///
/// The HRTF stage (`hrtf_sofa_path`) wants one direction per measurement,
/// `Data.IR` as `[M][R][N]`, and convolves the first few milliseconds of
/// each response, time-aligned. The room stage (`brir_sofa_path`) wants up
/// to [`PREPARED_MAX_EMITTERS`] loudspeakers, each measured with its room at
/// the head orientation nearest straight ahead. A multi-speaker room file
/// suits only the room stage: its responses hold several loudspeakers per
/// measurement, and cut to a few milliseconds the room in them is gone. A
/// free-field set of hundreds of directions suits only the HRTF stage. A
/// per-direction set of a few room-length responses suits both.
#[derive(Clone, Debug, PartialEq)]
pub struct SofaContents {
    /// `SOFAConventions`, as the file names it.
    pub conventions: String,
    /// A prepared room rather than a SOFA file.
    pub prepared: bool,
    /// The responses' rate, Hz.
    pub rate: u32,
    /// `Data.IR`'s measurements, receivers, emitters per measurement (1 for
    /// a three-axis array) and samples per response.
    pub measurements: usize,
    pub receivers: usize,
    pub emitters: usize,
    pub samples: usize,
    /// Why the HRTF stage would not take it, or `None` when it would.
    pub hrtf_refusal: Option<String>,
    /// The room it prepares as, or why it would not.
    pub room: Result<RoomContents, String>,
}

/// A room as [`SofaContents`] reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct RoomContents {
    /// The loudspeakers, as [`PreparedRoom::speaker_names`] names them.
    pub speakers: Vec<String>,
    /// Head orientations measured; a prepared room keeps one.
    pub orientations: usize,
}

/// Describe a SOFA file or a prepared room held in memory (see
/// [`SofaContents`]). Errors when the bytes are neither.
pub fn describe_room_file(bytes: &[u8]) -> anyhow::Result<SofaContents> {
    if bytes.starts_with(&PREPARED_ROOM_MAGIC) {
        let room = ExtractedRoom::from_prepared(bytes)?;
        let samples = room.pairs.iter().map(|p| p.left.len()).max().unwrap_or(0);
        return Ok(SofaContents {
            conventions: room.conventions.clone(),
            prepared: true,
            rate: room.file_rate,
            measurements: room.orientations.len(),
            receivers: 2,
            emitters: room.emitters.len(),
            samples,
            hrtf_refusal: Some("a prepared room renders only as a room".to_string()),
            room: room_speaker_names(&room.emitters, room.room_corners)
                .map(|speakers| RoomContents {
                    speakers,
                    orientations: room.orientations.len(),
                })
                .map_err(|e| format!("{e:#}")),
        });
    }
    describe_sofa(bytes)
}

#[cfg(feature = "sofa")]
fn describe_sofa(bytes: &[u8]) -> anyhow::Result<SofaContents> {
    let sofa = sofar::reader::LazySofa::open(bytes)
        .map_err(|e| anyhow::anyhow!("not a SOFA file this engine reads: {e}"))?;
    let shape = sofa.ir_shape();
    let (m, r, e, n) = match shape.as_slice() {
        &[m, r, n] => (m, r, 1, n),
        &[m, r, e, n] => (m, r, e, n),
        other => anyhow::bail!("Data.IR has {} axes, expected 3 or 4", other.len()),
    };
    let h = sofa.hrtf();
    let conventions = h
        .attributes
        .get("SOFAConventions")
        .cloned()
        .unwrap_or_default();
    let sample_rate = h.data_sampling_rate.values.first().copied().unwrap_or(0.0);
    let hrtf_refusal = if shape.len() == 4 && e > 1 {
        Some(format!(
            "{e} loudspeakers in every measurement: the HRTF stage takes one direction per \
             measurement"
        ))
    } else if shape.len() == 4 {
        Some("Data.IR has four axes: the HRTF stage reads [M][R][N]".to_string())
    } else if r < 2 {
        Some(format!(
            "{r} receiver(s); a binaural set needs the two ears"
        ))
    } else if m == 0 || n == 0 {
        Some(format!("no measurements (M = {m}, N = {n})"))
    } else if h.source_position.values.len() < 3 {
        Some("no SourcePosition: no direction to place a response at".to_string())
    } else {
        None
    };
    let raw = RawRoomIr {
        conventions: &conventions,
        sample_rate,
        m,
        r,
        e,
        n,
        source_position: &h.source_position.values,
        emitter_position: &h.emitter_position.values,
        listener_position: &h.listener_position.values,
        listener_view: &h.listener_view.values,
        data_ir: &[],
        ir_first: 0,
        data_delay: &h.data_delay.values,
    };
    let room = RoomPlan::new(&raw, OrientationSelection::FrontOnly)
        .and_then(|plan| {
            check_room_size(plan.emitters.len())?;
            Ok(RoomContents {
                speakers: room_speaker_names(&plan.emitters, None)?,
                orientations: plan.measured_orientations,
            })
        })
        .map_err(|e| format!("{e:#}"));
    Ok(SofaContents {
        conventions: conventions.clone(),
        prepared: false,
        rate: sample_rate.round().max(0.0) as u32,
        measurements: m,
        receivers: r,
        emitters: e,
        samples: n,
        hrtf_refusal,
        room,
    })
}

#[cfg(not(feature = "sofa"))]
fn describe_sofa(_bytes: &[u8]) -> anyhow::Result<SofaContents> {
    anyhow::bail!("not a prepared room, and SOFA support is not built into this renderer")
}

/// Indices (into the sorted orientation list) to keep under `sel`.
fn select_orientations(orientations: &[(f32, f32)], sel: OrientationSelection) -> Vec<usize> {
    let nearest = |yaw: f32, pitch: f32| -> Option<usize> {
        orientations
            .iter()
            .enumerate()
            .map(|(i, &o)| (i, orientation_dist2(o, (yaw, pitch))))
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
    };
    match sel {
        OrientationSelection::All => (0..orientations.len()).collect(),
        OrientationSelection::FrontOnly => nearest(0.0, 0.0).into_iter().collect(),
        OrientationSelection::Decimated {
            step_deg,
            max_yaw_deg,
        } => {
            let step = step_deg.max(SAME_ANGLE_DEG);
            let max = max_yaw_deg.clamp(0.0, 180.0);
            let mut kept: Vec<usize> = Vec::new();
            let steps = (max / step).floor() as i32;
            for k in -steps..=steps {
                if let Some(i) = nearest(k as f32 * step, 0.0)
                    && !kept.contains(&i)
                {
                    kept.push(i);
                }
            }
            kept.sort_unstable();
            kept
        }
    }
}

#[cfg(feature = "sofa")]
impl BrirSet {
    /// Load a room-response SOFA file: its geometry, then the responses of
    /// the orientations `opts` keeps (see the module doc).
    pub fn from_sofa(path: &str, engine_rate: u32, opts: &BrirLoadOptions) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path).map_err(|e| anyhow::anyhow!("read '{path}': {e}"))?;
        Self::from_sofa_bytes(path, &bytes, engine_rate, opts)
    }

    /// [`Self::from_sofa`] on the file's bytes; `path` names it in errors.
    fn from_sofa_bytes(
        path: &str,
        bytes: &[u8],
        engine_rate: u32,
        opts: &BrirLoadOptions,
    ) -> anyhow::Result<Self> {
        with_sofa_room(bytes, opts.orientations, |raw| {
            Self::from_raw(raw, engine_rate, opts)
        })
        .map(|mut set| {
            (set.room_type, set.room_corners) = sofa_room_geometry(bytes);
            set
        })
        .map_err(|e| anyhow::anyhow!("SOFA '{path}': {e}"))
    }
}

/// The room a SOFA file says its loudspeakers stand in: its `RoomType`, and
/// its corners around the listener in the renderer's frame
/// ([`room_corners_relative`]). Each `None` where the file does not state it,
/// or the file cannot be read; the loudspeakers' own box then stands for the
/// room.
#[cfg(feature = "sofa")]
fn sofa_room_geometry(bytes: &[u8]) -> RoomGeometry {
    let Ok(sofa) = sofar::reader::LazySofa::open(bytes) else {
        return (None, None);
    };
    let h = sofa.hrtf();
    let room_type = h
        .attributes
        .get("RoomType")
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    let corners = sofa_room_corners(bytes, &h.attributes)
        .map(|c| room_corners_relative(c, row3(&h.listener_position.values, 0)));
    (room_type, corners)
}

/// `RoomCornerA` and `RoomCornerB` of a SOFA file in SOFA cartesian metres,
/// when both are present and their encoding is understood
/// ([`room_corner_metadata`] says where the encoding is read from).
#[cfg(feature = "sofa")]
fn sofa_room_corners(
    bytes: &[u8],
    globals: &std::collections::HashMap<String, String>,
) -> Option<[[f32; 3]; 2]> {
    use std::collections::HashMap;
    let parsed = sofar::hdf::parse_with_children(bytes).ok()?;
    let attributes_of = |obj: &sofar::hdf::DataObject| -> HashMap<String, String> {
        obj.parsed_attributes
            .iter()
            .filter_map(|a| a.value.as_ref().map(|v| (a.name.clone(), v.clone())))
            .collect()
    };
    // The convention's `RoomCorners` variable exists for its attributes
    // alone: the two corners' `Type` and `Units`.
    let shared = parsed
        .get_child("RoomCorners")
        .and_then(|r| r.ok())
        .map(|obj| attributes_of(&obj))
        .unwrap_or_default();
    let corner = |name: &str| -> Option<[f32; 3]> {
        let obj = parsed.get_child(name)?.ok()?;
        let values = hdf_floats(&obj, 3)?;
        let (kind, units) = room_corner_metadata(&shared, &attributes_of(&obj), globals);
        room_corner_metres(
            [values[0], values[1], values[2]],
            kind.as_deref(),
            units.as_deref(),
        )
    };
    Some([corner("RoomCornerA")?, corner("RoomCornerB")?])
}

/// The coordinate metadata (`Type`, `Units`) of a room corner, from where
/// a SOFA file keeps it, in the order it is looked for: the `RoomCorners`
/// variable the convention includes for that alone (`shared`: its
/// `RoomCorners:Type` / `RoomCorners:Units` are that variable's
/// attributes), then the corner variable's own attributes (`own`, which
/// some writers duplicate), then the same names among the global
/// attributes (`globals`, where a writer that knows no `RoomCorners`
/// variable leaves them). `None` where none states it: the convention's
/// default, cartesian metres, applies.
pub fn room_corner_metadata(
    shared: &std::collections::HashMap<String, String>,
    own: &std::collections::HashMap<String, String>,
    globals: &std::collections::HashMap<String, String>,
) -> (Option<String>, Option<String>) {
    let find = |key: &str| {
        shared
            .get(key)
            .or_else(|| own.get(key))
            .or_else(|| globals.get(&format!("RoomCorners:{key}")))
            .cloned()
    };
    (find("Type"), find("Units"))
}

/// A room corner in SOFA cartesian metres, from its stored triplet and its
/// coordinate metadata: cartesian in metres as stored, spherical (azimuth
/// and elevation in degrees, radius in metres) converted, absent metadata
/// read as the convention's default, cartesian metres. Another type or
/// unit is refused — the loudspeakers' own box then stands for the room —
/// rather than published as metres it is not.
pub fn room_corner_metres(
    values: [f32; 3],
    coord_type: Option<&str>,
    units: Option<&str>,
) -> Option<[f32; 3]> {
    if !values.iter().all(|v| v.is_finite()) {
        return None;
    }
    let lower = |s: Option<&str>| s.map(|s| s.trim().to_ascii_lowercase());
    let metres = |unit: &str| matches!(unit.trim(), "metre" | "metres" | "meter" | "meters" | "m");
    let kind = lower(coord_type).unwrap_or_else(|| "cartesian".to_owned());
    match kind.as_str() {
        "cartesian" => {
            let units = lower(units).unwrap_or_else(|| "metre".to_owned());
            metres(&units).then_some(values)
        }
        "spherical" => {
            // "degree, degree, metre" is the convention's spelling; the
            // radius is the last unit named.
            let units = lower(units).unwrap_or_else(|| "degree, degree, metre".to_owned());
            let radius_unit = units.rsplit(',').next().unwrap_or("");
            if !metres(radius_unit) {
                return None;
            }
            let [azimuth, elevation, radius] = values;
            let (az, el) = (azimuth.to_radians(), elevation.to_radians());
            let horizontal = el.cos() * radius;
            Some([
                az.cos() * horizontal,
                az.sin() * horizontal,
                el.sin() * radius,
            ])
        }
        _ => None,
    }
}

/// Hand `f` the raw arrays of the room-response SOFA file in `bytes`, with
/// `Data.IR` holding only the measurements `selection` is extracted from
/// ([`ExtractedRoom::measurements`]): the geometry is read first, then that
/// run of responses, and nothing else of them is read or inflated.
#[cfg(feature = "sofa")]
fn with_sofa_room<T>(
    bytes: &[u8],
    selection: OrientationSelection,
    f: impl FnOnce(&RawRoomIr<'_>) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let sofa = sofar::reader::LazySofa::open(bytes).map_err(|e| anyhow::anyhow!("open: {e}"))?;
    with_sofa_geometry(&sofa, |raw| {
        let run = ExtractedRoom::measurements(&raw, selection)?;
        let data_ir = sofa
            .read_ir(run.start, run.len())
            .map_err(|e| anyhow::anyhow!("Data.IR: {e}"))?;
        f(&RawRoomIr {
            data_ir: &data_ir,
            ir_first: run.start,
            ..raw
        })
    })
}

/// `f` of an open SOFA file's room as its geometry describes it, with no
/// responses read (`data_ir` empty).
#[cfg(feature = "sofa")]
fn with_sofa_geometry<T>(
    sofa: &sofar::reader::LazySofa<'_>,
    f: impl FnOnce(RawRoomIr<'_>) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let [m, r, e, n] = match sofa.ir_shape().as_slice() {
        &[m, r, n] => [m, r, 1, n],
        &[m, r, e, n] => [m, r, e, n],
        other => anyhow::bail!("Data.IR has {} axes, expected 3 or 4", other.len()),
    };
    let h = sofa.hrtf();
    let sample_rate = *h
        .data_sampling_rate
        .values
        .first()
        .ok_or_else(|| anyhow::anyhow!("no Data.SamplingRate"))?;
    let conventions = h
        .attributes
        .get("SOFAConventions")
        .map(String::as_str)
        .unwrap_or("");
    let raw = RawRoomIr {
        conventions,
        sample_rate,
        m,
        r,
        e,
        n,
        source_position: &h.source_position.values,
        emitter_position: &h.emitter_position.values,
        listener_position: &h.listener_position.values,
        listener_view: &h.listener_view.values,
        data_ir: &[],
        ir_first: 0,
        data_delay: &h.data_delay.values,
    };
    f(raw)
}

/// The first `count` values of a floating-point HDF dataset (little-endian,
/// as the SOFA reader assumes); `None` for another class, or too short.
#[cfg(feature = "sofa")]
fn hdf_floats(obj: &sofar::hdf::DataObject, count: usize) -> Option<Vec<f32>> {
    if obj.dt.class_and_version & 0x0F != 1 {
        return None;
    }
    let precision = match obj.dt.data_fmt.as_ref() {
        Some(sofar::hdf::DataFormat::Float { bit_precision, .. }) => *bit_precision,
        _ => 64,
    };
    let width = match precision {
        64 => 8,
        32 => 4,
        _ => return None,
    };
    let bytes = obj.data.get(..width * count)?;
    Some(
        bytes
            .chunks_exact(width)
            .map(|b| match width {
                8 => f64::from_le_bytes(b.try_into().expect("8 bytes")) as f32,
                _ => f32::from_le_bytes(b.try_into().expect("4 bytes")),
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A room's corners are published around the listener in the renderer's
    /// frame: a listener at (3, 2, 1.2) in a 6 × 4 × 2.5 m SOFA room has
    /// the corners 3 m behind and ahead, 2 m to either side, 1.2 m down and
    /// 1.3 m up, with SOFA's left-positive y becoming the renderer's
    /// right-positive x.
    #[test]
    fn room_corners_are_taken_around_the_listener_in_the_renderers_frame() {
        let [a, b] = room_corners_relative([[0.0, 0.0, 0.0], [6.0, 4.0, 2.5]], [3.0, 2.0, 1.2]);
        let near =
            |got: [f32; 3], want: [f32; 3]| got.iter().zip(want).all(|(g, w)| (g - w).abs() < 1e-6);
        assert!(near(a, [2.0, -3.0, -1.2]), "{a:?}");
        assert!(near(b, [-2.0, 3.0, 1.3]), "{b:?}");
    }

    /// The corners' encoding is read where the convention keeps it, the
    /// `RoomCorners` variable's attributes, before a corner's own duplicate
    /// or a global copy; a file stating it nowhere gets the default.
    #[test]
    fn room_corner_metadata_comes_from_the_room_corners_variable_first() {
        use std::collections::HashMap;
        let map = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let shared = map(&[("Type", "spherical"), ("Units", "degree, degree, metre")]);
        let own = map(&[("Type", "cartesian"), ("Units", "metre")]);
        let globals = map(&[
            ("RoomCorners:Type", "cartesian"),
            ("RoomCorners:Units", "metre"),
        ]);
        let none = HashMap::new();
        assert_eq!(
            room_corner_metadata(&shared, &own, &globals),
            (
                Some("spherical".into()),
                Some("degree, degree, metre".into())
            )
        );
        assert_eq!(
            room_corner_metadata(&none, &own, &globals),
            (Some("cartesian".into()), Some("metre".into()))
        );
        let globals = map(&[("RoomCorners:Type", "spherical")]);
        assert_eq!(
            room_corner_metadata(&none, &none, &globals),
            (Some("spherical".into()), None)
        );
        assert_eq!(room_corner_metadata(&none, &none, &none), (None, None));
        // Shared metadata applies to both corners: a spherical pair under
        // it lands on its cartesian twins.
        let r = (36.0f32 + 16.0 + 6.25).sqrt();
        let spherical = [
            4.0f32.atan2(6.0).to_degrees(),
            (2.5 / r).asin().to_degrees(),
            r,
        ];
        let (kind, units) = room_corner_metadata(&shared, &none, &none);
        let got = room_corner_metres(spherical, kind.as_deref(), units.as_deref()).expect("read");
        assert!(
            got.iter()
                .zip([6.0, 4.0, 2.5])
                .all(|(g, w)| (g - w).abs() < 1e-3),
            "{got:?}"
        );
    }

    /// A corner stored as spherical degrees and metres lands where its
    /// cartesian twin does; absent metadata is cartesian metres; another
    /// type or unit is refused rather than read as metres.
    #[test]
    fn room_corners_are_read_in_the_encoding_the_file_states() {
        let cartesian = [6.0, 4.0, 2.5];
        let near =
            |got: [f32; 3], want: [f32; 3]| got.iter().zip(want).all(|(g, w)| (g - w).abs() < 1e-3);
        assert_eq!(room_corner_metres(cartesian, None, None), Some(cartesian));
        assert_eq!(
            room_corner_metres(cartesian, Some("cartesian"), Some("metre")),
            Some(cartesian)
        );
        let r = (36.0f32 + 16.0 + 6.25).sqrt();
        let spherical = [
            4.0f32.atan2(6.0).to_degrees(),
            (2.5 / r).asin().to_degrees(),
            r,
        ];
        let got = room_corner_metres(spherical, Some("spherical"), Some("degree, degree, metre"))
            .expect("spherical corners are read");
        assert!(near(got, cartesian), "{got:?}");
        let got = room_corner_metres(spherical, Some("Spherical"), None).expect("default units");
        assert!(near(got, cartesian), "{got:?}");
        assert_eq!(
            room_corner_metres(cartesian, Some("cartesian"), Some("feet")),
            None
        );
        assert_eq!(
            room_corner_metres(spherical, Some("spherical"), Some("degree, degree, foot")),
            None
        );
        assert_eq!(room_corner_metres(cartesian, Some("geodesic"), None), None);
        assert_eq!(room_corner_metres([f32::NAN, 0.0, 0.0], None, None), None);
    }

    /// SOFA spherical (az ccw-positive, el, r) → SOFA cartesian.
    fn sph(az_deg: f32, el_deg: f32, r: f32) -> [f32; 3] {
        let (az, el) = (az_deg.to_radians(), el_deg.to_radians());
        [
            r * el.cos() * az.cos(),
            r * el.cos() * az.sin(),
            r * el.sin(),
        ]
    }

    /// Marker amplitude identifying (emitter, orientation, ear).
    fn marker(e: usize, o: usize, ear: usize) -> f32 {
        let a = 0.5 + 0.01 * e as f32 + 0.001 * o as f32;
        if ear == 0 { a } else { -a }
    }

    /// A synthetic room response: impulse of `marker` amplitude at `delay`,
    /// then a decaying tail with `tail` peak amplitude.
    fn response(n: usize, delay: usize, amp: f32, tail: f32, seed: u32) -> Vec<f32> {
        let mut s = seed;
        let mut ir = vec![0.0f32; n];
        ir[delay] = amp;
        for (k, x) in ir.iter_mut().enumerate().skip(delay + 20) {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (s >> 8) as f32 / (1 << 24) as f32 * 2.0 - 1.0;
            let t = (k - delay) as f32 / n as f32;
            *x = tail * noise * (-8.0 * t).exp();
        }
        ir
    }

    struct Synth {
        m: usize,
        r: usize,
        e: usize,
        n: usize,
        source: Vec<f32>,
        emitter: Vec<f32>,
        listener: Vec<f32>,
        view: Vec<f32>,
        ir: Vec<f32>,
        delay: Vec<f32>,
        rate: f32,
        conventions: &'static str,
    }

    impl Synth {
        fn raw(&self) -> RawRoomIr<'_> {
            RawRoomIr {
                conventions: self.conventions,
                sample_rate: self.rate,
                m: self.m,
                r: self.r,
                e: self.e,
                n: self.n,
                source_position: &self.source,
                emitter_position: &self.emitter,
                listener_position: &self.listener,
                listener_view: &self.view,
                data_ir: &self.ir,
                ir_first: 0,
                data_delay: &self.delay,
            }
        }
    }

    /// `MultiSpeakerBRIR`-shaped: emitters at SOFA azimuths `spk_az` (2 m),
    /// listener views at SOFA azimuths `yaws`; direct sound at
    /// `base_delay + 10·e` samples, marker-coded.
    fn multi_speaker(spk_az: &[f32], yaws: &[f32], n: usize, rate: f32, tail: f32) -> Synth {
        let (m, r, e) = (yaws.len(), 2, spk_az.len());
        let base_delay = 100;
        let mut ir = vec![0.0f32; m * r * e * n];
        for (mi, _) in yaws.iter().enumerate() {
            for ri in 0..r {
                for (k, _) in spk_az.iter().enumerate() {
                    let base = ((mi * r + ri) * e + k) * n;
                    let resp = response(
                        n,
                        base_delay + 10 * k,
                        marker(k, mi, ri),
                        tail,
                        (mi * 7 + ri * 3 + k) as u32,
                    );
                    ir[base..base + n].copy_from_slice(&resp);
                }
            }
        }
        Synth {
            m,
            r,
            e,
            n,
            source: vec![0.0, 0.0, 0.0],
            emitter: spk_az.iter().flat_map(|&a| sph(a, 0.0, 2.0)).collect(),
            listener: vec![0.0, 0.0, 0.0],
            view: yaws.iter().flat_map(|&y| sph(y, 0.0, 1.0)).collect(),
            ir,
            delay: Vec::new(),
            rate,
            conventions: "MultiSpeakerBRIR",
        }
    }

    fn assert_close(a: f32, b: f32, tol: f32, what: &str) {
        assert!((a - b).abs() <= tol, "{what}: {a} vs {b}");
    }

    /// Marker position and amplitude of a pair's left ear (the largest
    /// sample), after normalisation is undone.
    fn peak_of(ir: &[f32]) -> (usize, f32) {
        ir.iter()
            .enumerate()
            .fold((0, 0.0f32), |(bi, bv), (i, &v)| {
                if v.abs() > bv.abs() { (i, v) } else { (bi, bv) }
            })
    }

    #[test]
    fn multi_speaker_brir_maps_emitters_and_orientations() {
        // L (+30 SOFA = left), R, C, and views −20, 0, +20 (SOFA, left +).
        let s = multi_speaker(&[30.0, -30.0, 0.0], &[-20.0, 0.0, 20.0], 400, 48000.0, 0.0);
        let set = BrirSet::from_raw(&s.raw(), 48000, &BrirLoadOptions::default()).unwrap();
        assert_eq!(set.sample_rate(), 48000);
        assert_eq!(set.emitters().len(), 3);
        assert_eq!(set.orientations().len(), 3);
        // Renderer frame: SOFA +30° (left) → x negative, y positive.
        let l = set.emitters()[0];
        assert!(
            l[0] < -0.9 && l[1] > 1.7,
            "left speaker in renderer frame: {l:?}"
        );
        let r = set.emitters()[1];
        assert!(
            r[0] > 0.9 && r[1] > 1.7,
            "right speaker in renderer frame: {r:?}"
        );
        let c = set.emitters()[2];
        assert_close(c[0], 0.0, 1e-4, "centre x");
        assert_close(c[1], 2.0, 1e-4, "centre y");
        // Views sorted by renderer yaw: SOFA +20 (left) → −20 here.
        let yaws: Vec<f32> = set.orientations().iter().map(|o| o.0).collect();
        assert_close(yaws[0], -20.0, 1e-3, "first yaw");
        assert_close(yaws[1], 0.0, 1e-3, "second yaw");
        assert_close(yaws[2], 20.0, 1e-3, "third yaw");
        // Orientation index 0 (renderer −20) is SOFA measurement index 2.
        // Pair (emitter 1, orientation 0) must carry marker(1, 2, ear).
        let p = set.pair(1, 0);
        let (il, vl) = peak_of(&p.left);
        let (ir_, vr) = peak_of(&p.right);
        assert_eq!(il, ir_, "ears keep their relative timing");
        assert_close(vl / -vr, 1.0, 1e-5, "right ear is the negated marker");
        let ratio = vl / marker(1, 2, 0);
        // Same normalisation gain for the whole set: check another pair.
        let (_, v2) = peak_of(&set.pair(2, 1).left);
        assert_close(v2 / marker(2, 1, 0), ratio, 1e-4, "uniform gain");
        // Relative delays survive: emitter 2 is 10 samples later than 1,
        // and the common lead is stripped down to the guard (emitter 0 is
        // the earliest).
        let (i2, _) = peak_of(&set.pair(2, 1).left);
        assert_eq!(i2, il + 10);
        let (i0, _) = peak_of(&set.pair(0, 2).left);
        assert_eq!(i0, LEAD_GUARD, "earliest onset lands at the guard");
        assert_eq!(il, LEAD_GUARD + 10);
    }

    #[test]
    fn nearest_orientation_wraps_around_yaw() {
        let s = multi_speaker(&[0.0], &[-170.0, 170.0, 0.0], 200, 48000.0, 0.0);
        let set = BrirSet::from_raw(&s.raw(), 48000, &BrirLoadOptions::default()).unwrap();
        // Sorted renderer yaws: −170 (SOFA +170), 0, 170 (SOFA −170).
        assert_eq!(set.nearest_orientation(179.0, 0.0), 2);
        assert_eq!(set.nearest_orientation(-179.0, 0.0), 0);
        assert_eq!(set.nearest_orientation(-100.0, 0.0), 0);
        assert_eq!(set.nearest_orientation(30.0, 0.0), 1);
    }

    #[test]
    fn front_only_keeps_the_orientation_nearest_straight_ahead() {
        let s = multi_speaker(&[30.0, -30.0], &[-20.0, -5.0, 20.0], 300, 48000.0, 0.0);
        let opts = BrirLoadOptions {
            orientations: OrientationSelection::FrontOnly,
            ..Default::default()
        };
        let set = BrirSet::from_raw(&s.raw(), 48000, &opts).unwrap();
        assert_eq!(set.orientations().len(), 1);
        assert_close(set.orientations()[0].0, 5.0, 1e-3, "SOFA −5 → renderer +5");
        // Only that orientation's measurement (SOFA index 1) is resident.
        let (_, v) = peak_of(&set.pair(0, 0).left);
        let (_, w) = peak_of(&set.pair(1, 0).left);
        assert_close(v / marker(0, 1, 0), w / marker(1, 1, 0), 1e-4, "same gain");
        let resident: usize = (0..2).map(|e| set.pair(e, 0).taps() * 2 * 4).sum();
        assert_eq!(set.bytes(), resident);
    }

    #[test]
    fn decimation_picks_nearest_measured_steps() {
        let yaws: Vec<f32> = (-9..=9).map(|k| k as f32 * 10.0).collect(); // −90..90 by 10
        let s = multi_speaker(&[0.0], &yaws, 200, 48000.0, 0.0);
        let opts = BrirLoadOptions {
            orientations: OrientationSelection::Decimated {
                step_deg: 30.0,
                max_yaw_deg: 60.0,
            },
            ..Default::default()
        };
        let set = BrirSet::from_raw(&s.raw(), 48000, &opts).unwrap();
        let kept: Vec<f32> = set.orientations().iter().map(|o| o.0.round()).collect();
        assert_eq!(kept, vec![-60.0, -30.0, 0.0, 30.0, 60.0]);
    }

    #[test]
    fn single_emitter_conventions_group_sources_and_views() {
        // DRIR-shaped: 4 sources, one view.
        let n = 300;
        let az = [30.0f32, -30.0, 0.0, 110.0];
        let mut ir = Vec::new();
        for (mi, _) in az.iter().enumerate() {
            for ri in 0..2 {
                ir.extend(response(n, 100 + 5 * mi, marker(mi, 0, ri), 0.0, mi as u32));
            }
        }
        let s = Synth {
            m: 4,
            r: 2,
            e: 1,
            n,
            source: az.iter().flat_map(|&a| sph(a, 0.0, 2.0)).collect(),
            emitter: vec![0.0, 0.0, 0.0],
            listener: vec![0.0, 0.0, 0.0],
            view: vec![1.0, 0.0, 0.0],
            ir,
            delay: Vec::new(),
            rate: 48000.0,
            conventions: "SingleRoomDRIR",
        };
        let set = BrirSet::from_raw(&s.raw(), 48000, &BrirLoadOptions::default()).unwrap();
        assert_eq!(set.emitters().len(), 4);
        assert_eq!(set.orientations(), &[(0.0, 0.0)]);
        let (i3, _) = peak_of(&set.pair(3, 0).left);
        let (i0, _) = peak_of(&set.pair(0, 0).left);
        assert_eq!(i3, i0 + 15);

        // SRIR-shaped: one source, 3 views.
        let views = [20.0f32, 0.0, -20.0];
        let mut ir = Vec::new();
        for (mi, _) in views.iter().enumerate() {
            for ri in 0..2 {
                ir.extend(response(n, 100, marker(0, mi, ri), 0.0, mi as u32));
            }
        }
        let s = Synth {
            m: 3,
            r: 2,
            e: 1,
            n,
            source: sph(30.0, 0.0, 2.0).to_vec(),
            emitter: vec![0.0, 0.0, 0.0],
            listener: vec![0.0, 0.0, 0.0],
            view: views.iter().flat_map(|&y| sph(y, 0.0, 1.0)).collect(),
            ir,
            delay: Vec::new(),
            rate: 48000.0,
            conventions: "SingleRoomSRIR",
        };
        let set = BrirSet::from_raw(&s.raw(), 48000, &BrirLoadOptions::default()).unwrap();
        assert_eq!(set.emitters().len(), 1);
        assert_eq!(set.orientations().len(), 3);
        // Renderer yaw +20 = SOFA −20 = measurement 2.
        let (_, v) = peak_of(&set.pair(0, 2).left);
        let (_, w) = peak_of(&set.pair(0, 0).left);
        assert_close(
            v / marker(0, 2, 0),
            w / marker(0, 0, 0),
            1e-4,
            "orientation mapping",
        );
    }

    #[test]
    fn tail_is_cut_at_the_floor_and_bounded() {
        // A 1 s response whose tail decays 8 nepers over its length: the −60 dB
        // point of the energy sits well inside.
        let s = multi_speaker(&[0.0], &[0.0], 48000, 48000.0, 0.3);
        let opts = BrirLoadOptions {
            max_length_s: 0.0,
            tail_floor_db: 60.0,
            orientations: OrientationSelection::All,
        };
        let set = BrirSet::from_raw(&s.raw(), 48000, &opts).unwrap();
        let taps = set.max_taps();
        assert!(taps < 48000 - 100, "tail cut shortens the response: {taps}");
        assert!(taps > 10000, "but keeps the audible decay: {taps}");
        let tail = &set.pair(0, 0).left;
        assert_eq!(
            *tail.last().unwrap(),
            0.0,
            "raised-cosine fade ends at zero"
        );
        // A tighter floor keeps less; a length bound wins when lower.
        let opts_40 = BrirLoadOptions {
            tail_floor_db: 40.0,
            ..opts
        };
        let shorter = BrirSet::from_raw(&s.raw(), 48000, &opts_40).unwrap();
        assert!(shorter.max_taps() < taps);
        let bounded = BrirLoadOptions {
            max_length_s: 0.1,
            ..opts
        };
        let set = BrirSet::from_raw(&s.raw(), 48000, &bounded).unwrap();
        assert_eq!(set.max_taps(), 4800);
    }

    #[test]
    fn resamples_to_the_engine_rate() {
        let s = multi_speaker(&[0.0, 90.0], &[0.0], 4410, 44100.0, 0.0);
        let set = BrirSet::from_raw(&s.raw(), 48000, &BrirLoadOptions::default()).unwrap();
        assert_eq!(set.sample_rate(), 48000);
        // The direct sound of emitter 1 (110 samples at 44.1 k) lands at the
        // guard + 10 samples scaled by 48/44.1.
        let (i0, _) = peak_of(&set.pair(0, 0).left);
        let (i1, _) = peak_of(&set.pair(1, 0).left);
        let want = ((LEAD_GUARD + 10) as f32 * 48000.0 / 44100.0).round() as usize;
        assert!((i1 as isize - want as isize).abs() <= 1, "{i1} vs {want}");
        assert!(
            (i0 as isize - (LEAD_GUARD as f32 * 48000.0 / 44100.0).round() as isize).abs() <= 1
        );
        // Pairs keep their own lengths; the later emitter is the longest.
        assert_eq!(set.max_taps(), set.pair(1, 0).taps());
        assert!(set.pair(0, 0).taps() < set.max_taps());
    }

    #[test]
    fn data_delay_shifts_each_receiver() {
        let mut s = multi_speaker(&[0.0], &[0.0], 300, 48000.0, 0.0);
        // [I][R][E]: right ear delayed by 7 samples.
        s.delay = vec![0.0, 7.0];
        let set = BrirSet::from_raw(&s.raw(), 48000, &BrirLoadOptions::default()).unwrap();
        let p = set.pair(0, 0);
        let (il, _) = peak_of(&p.left);
        let (ir_, _) = peak_of(&p.right);
        assert_eq!(ir_, il + 7);
    }

    #[test]
    fn silent_and_incomplete_sets_are_refused() {
        let mut s = multi_speaker(&[0.0, 30.0], &[0.0, 10.0], 200, 48000.0, 0.0);
        s.ir.iter_mut().for_each(|x| *x = 0.0);
        let err = BrirSet::from_raw(&s.raw(), 48000, &BrirLoadOptions::default()).unwrap_err();
        assert!(err.to_string().contains("silent"), "{err}");

        // One-emitter set where measurement views and sources vary jointly:
        // source A at view 0, source B at view 10 → B lacks view 0.
        let n = 200;
        let mut ir = Vec::new();
        for mi in 0..2 {
            for ri in 0..2 {
                ir.extend(response(n, 50, marker(mi, mi, ri), 0.0, mi as u32));
            }
        }
        let s = Synth {
            m: 2,
            r: 2,
            e: 1,
            n,
            source: [sph(30.0, 0.0, 2.0), sph(-30.0, 0.0, 2.0)].concat(),
            emitter: vec![0.0; 3],
            listener: vec![0.0; 3],
            view: [sph(0.0, 0.0, 1.0), sph(10.0, 0.0, 1.0)].concat(),
            ir,
            delay: Vec::new(),
            rate: 48000.0,
            conventions: "",
        };
        let err = BrirSet::from_raw(&s.raw(), 48000, &BrirLoadOptions::default()).unwrap_err();
        assert!(err.to_string().contains("no measurement"), "{err}");

        // Wrong element count.
        let mut s = multi_speaker(&[0.0], &[0.0], 300, 48000.0, 0.0);
        s.ir.pop();
        let err = BrirSet::from_raw(&s.raw(), 48000, &BrirLoadOptions::default()).unwrap_err();
        assert!(err.to_string().contains("Data.IR holds"), "{err}");
    }

    fn refusal(s: &Synth, engine_rate: u32) -> String {
        match BrirSet::from_raw(&s.raw(), engine_rate, &BrirLoadOptions::default()) {
            Ok(_) => panic!("the set was accepted"),
            Err(e) => e.to_string(),
        }
    }

    /// Each field a file can get wrong is refused with what is wrong, before
    /// anything is allocated from it.
    #[test]
    fn a_malformed_set_is_refused_with_its_reason() {
        let good = || multi_speaker(&[0.0, 30.0], &[0.0], 200, 48000.0, 0.0);
        BrirSet::from_raw(&good().raw(), 48000, &BrirLoadOptions::default())
            .expect("the control set is valid");

        let mut s = good();
        s.r = 1;
        assert!(refusal(&s, 48000).contains("needs the two ears"));
        for field in 0..3 {
            let mut s = good();
            match field {
                0 => s.m = 0,
                1 => s.e = 0,
                _ => s.n = 0,
            }
            assert!(refusal(&s, 48000).contains("empty set"), "field {field}");
        }
        // Dimensions whose product wraps around: refused, not trusted.
        let mut s = good();
        s.m = usize::MAX / 2 + 1;
        assert!(refusal(&s, 48000).contains("Data.IR holds"));

        for rate in [0.0, 0.4, -48000.0, f32::NAN, f32::INFINITY] {
            let mut s = good();
            s.rate = rate;
            assert!(
                refusal(&s, 48000).contains("invalid sampling rate"),
                "rate {rate}"
            );
        }
        assert!(refusal(&good(), 0).contains("engine rate is zero"));

        for bad in [f32::NAN, f32::INFINITY] {
            let mut s = good();
            s.ir[5] = bad;
            assert!(refusal(&s, 48000).contains("not finite"), "{bad}");
        }

        // An emitter on the listener has no direction.
        let mut s = good();
        s.emitter[..3].fill(0.0);
        assert!(refusal(&s, 48000).contains("sits on the listener"));
    }

    #[test]
    fn an_implausible_data_delay_is_refused_before_it_is_allocated() {
        let mut s = multi_speaker(&[0.0], &[0.0], 200, 48000.0, 0.0);
        // [R]: a right ear "delayed" by ten billion samples (40 GB of padding).
        s.delay = vec![0.0, 1e10];
        assert!(refusal(&s, 48000).contains("Data.Delay"));
        // Up to a second is honoured; negative and NaN delays read as none.
        s.delay = vec![f32::NAN, 48000.0];
        BrirSet::from_raw(&s.raw(), 48000, &BrirLoadOptions::default()).expect("one second");
        s.delay = vec![-5.0, 0.0];
        BrirSet::from_raw(&s.raw(), 48000, &BrirLoadOptions::default()).expect("negative");
    }

    /// Every field of two sets, sample for sample.
    fn assert_same_set(a: &BrirSet, b: &BrirSet, what: &str) {
        assert_eq!(a.sample_rate(), b.sample_rate(), "{what}: rate");
        assert_eq!(a.conventions(), b.conventions(), "{what}: conventions");
        assert_eq!(a.emitters(), b.emitters(), "{what}: emitters");
        assert_eq!(a.orientations(), b.orientations(), "{what}: orientations");
        assert_eq!(a.max_taps(), b.max_taps(), "{what}: taps");
        for e in 0..a.emitters().len() {
            for o in 0..a.orientations().len() {
                assert_eq!(a.pair(e, o), b.pair(e, o), "{what}: pair ({e}, {o})");
            }
        }
    }

    /// The front-only room through its prepared bytes, loaded as a host
    /// loads it.
    fn through_prepared(s: &Synth, engine_rate: u32, opts: &BrirLoadOptions) -> BrirSet {
        let room = ExtractedRoom::extract(&s.raw(), OrientationSelection::FrontOnly).unwrap();
        let bytes = room.to_prepared();
        let back = ExtractedRoom::from_prepared(&bytes).unwrap();
        assert_eq!(back, room, "the prepared bytes read back exactly");
        BrirSet::finish(back.select(opts.orientations).unwrap(), engine_rate, opts).unwrap()
    }

    /// A prepared room renders exactly as its file does without head
    /// tracking: same geometry, same pairs, bit for bit, whatever the
    /// selection, cut or resampling asked of the load.
    #[test]
    fn a_prepared_room_loads_exactly_as_its_file_does() {
        let front = BrirLoadOptions {
            orientations: OrientationSelection::FrontOnly,
            ..BrirLoadOptions::default()
        };
        let mut cases = vec![
            (
                "multi-speaker, five views",
                multi_speaker(
                    &[30.0, -30.0, 0.0, 110.0, -110.0],
                    &[-40.0, -20.0, 0.0, 20.0, 40.0],
                    2400,
                    48000.0,
                    0.05,
                ),
                48000,
            ),
            (
                "44.1 kHz resampled to 48 kHz",
                multi_speaker(&[30.0, -30.0, 0.0], &[0.0, 90.0], 2205, 44100.0, 0.05),
                48000,
            ),
            (
                "48 kHz resampled to 96 kHz",
                multi_speaker(&[45.0, -45.0], &[0.0], 2400, 48000.0, 0.05),
                96000,
            ),
        ];
        let mut delayed = multi_speaker(&[0.0, 90.0], &[0.0], 1200, 48000.0, 0.05);
        delayed.delay = vec![3.0, 11.0];
        cases.push(("Data.Delay per receiver", delayed, 48000));

        for (what, s, rate) in &cases {
            for opts in [
                front,
                BrirLoadOptions {
                    max_length_s: 0.02,
                    tail_floor_db: 40.0,
                    ..front
                },
                // A host asking for every orientation of a prepared room gets
                // the one it holds.
                BrirLoadOptions::default(),
            ] {
                let want = BrirSet::from_raw(&s.raw(), *rate, &front_or(opts)).unwrap();
                let got = through_prepared(s, *rate, &opts);
                assert_same_set(&got, &want, what);
            }
        }
    }

    /// The options a file load would use to match a prepared room's: the
    /// prepared room holds the front orientation only.
    fn front_or(opts: BrirLoadOptions) -> BrirLoadOptions {
        BrirLoadOptions {
            orientations: OrientationSelection::FrontOnly,
            ..opts
        }
    }

    /// Selecting from what a selection kept keeps it: a room extracted with
    /// every orientation and reduced afterwards is the room extracted with
    /// the reduced selection.
    #[test]
    fn selecting_again_keeps_what_a_selection_kept() {
        let s = multi_speaker(
            &[30.0, -30.0],
            &[-60.0, -30.0, -2.0, 30.0, 60.0],
            600,
            48000.0,
            0.0,
        );
        let all = ExtractedRoom::extract(&s.raw(), OrientationSelection::All).unwrap();
        for sel in [
            OrientationSelection::FrontOnly,
            OrientationSelection::Decimated {
                step_deg: 30.0,
                max_yaw_deg: 60.0,
            },
            OrientationSelection::All,
        ] {
            let want = ExtractedRoom::extract(&s.raw(), sel).unwrap();
            let once = all.clone().select(sel).unwrap();
            assert_eq!(once, want, "{sel:?}");
            assert_eq!(once.clone().select(sel).unwrap(), want, "{sel:?} twice");
        }
    }

    /// Whatever the bytes hold, a prepared room is read or refused with a
    /// reason: every truncation, any flipped byte, trailing data, a NaN.
    #[test]
    fn a_damaged_prepared_room_is_refused_never_trusted() {
        let s = multi_speaker(&[30.0, -30.0, 0.0], &[0.0], 300, 48000.0, 0.05);
        let bytes = ExtractedRoom::extract(&s.raw(), OrientationSelection::FrontOnly)
            .unwrap()
            .to_prepared();
        for cut in (0..bytes.len()).step_by(7) {
            assert!(
                ExtractedRoom::from_prepared(&bytes[..cut]).is_err(),
                "cut at {cut} of {}",
                bytes.len()
            );
        }
        let mut longer = bytes.clone();
        longer.push(0);
        let err = ExtractedRoom::from_prepared(&longer).unwrap_err();
        assert!(err.to_string().contains("past the last response"), "{err}");

        // Each header word set to all ones: a version, rate, count or length
        // nothing could hold.
        for word in 2..7 {
            let mut bad = bytes.clone();
            bad[4 * word..4 * word + 4].copy_from_slice(&[0xff; 4]);
            assert!(ExtractedRoom::from_prepared(&bad).is_err(), "word {word}");
        }
        // A NaN in the last sample.
        let mut nan = bytes.clone();
        let at = nan.len() - 4;
        nan[at..].copy_from_slice(&f32::NAN.to_le_bytes());
        let err = ExtractedRoom::from_prepared(&nan).unwrap_err();
        assert!(err.to_string().contains("non-finite"), "{err}");
        // Any single flipped byte is read or refused, without a panic.
        for at in 0..bytes.len() {
            let mut flipped = bytes.clone();
            flipped[at] ^= 0x5a;
            let _ = ExtractedRoom::from_prepared(&flipped);
        }
    }

    /// A host's source text rides in the prepared room, read back exactly and
    /// without changing how the room renders.
    #[test]
    fn a_prepared_room_carries_its_source() {
        let s = multi_speaker(&[30.0, -30.0, 0.0], &[0.0], 300, 48000.0, 0.05);
        let room = ExtractedRoom::extract(&s.raw(), OrientationSelection::FrontOnly).unwrap();
        let source = "/storage/sofa/bbcrdlr_systemG.sofa\n274319890\n1760000000\n";
        let tagged = room.clone().with_source(source).to_prepared();
        let back = ExtractedRoom::from_prepared(&tagged).unwrap();
        assert_eq!(back.source(), source);
        assert_eq!(back.emitters(), room.emitters());
        let opts = BrirLoadOptions::default();
        assert_same_set(
            &BrirSet::finish(back, 48000, &opts).unwrap(),
            &BrirSet::finish(room.clone(), 48000, &opts).unwrap(),
            "tagged",
        );

        // A source longer than the room holds is cut, at a character.
        let long = "é".repeat(PREPARED_MAX_SOURCE);
        let cut = room.with_source(&long);
        assert!(cut.source().len() <= PREPARED_MAX_SOURCE);
        assert!(long.starts_with(cut.source()));
        // A length word nothing could hold is refused.
        let nc = u32::from_le_bytes(tagged[24..28].try_into().unwrap()) as usize;
        let mut bad = tagged.clone();
        bad[28 + nc..32 + nc].copy_from_slice(&[0xff; 4]);
        assert!(ExtractedRoom::from_prepared(&bad).is_err());
    }

    /// A room-response SOFA file's loudspeakers are the ones it prepares
    /// with, read from its geometry; anything else is no room.
    #[cfg(feature = "sofa")]
    #[test]
    fn a_sofa_rooms_loudspeakers_are_read_from_its_geometry() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/sofa");
        for name in [
            "chunked_multispeaker_brir.sofa",
            "rows_multispeaker_brir.sofa",
        ] {
            let path = dir.join(name);
            let prepared = prepare_room(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(
                room_emitters(&path).unwrap().as_deref(),
                Some(prepared.room.emitters()),
                "{name}"
            );
        }
        // A free-field HRTF set is refused as a room, not taken for one.
        assert!(room_emitters(&dir.join("Pulse.sofa")).is_err());
        assert_eq!(room_emitters(&dir.join("README.md")).unwrap(), None);
    }

    /// A host sizes its virtual array from a prepared room's header alone,
    /// and a file that is not a prepared room says so rather than failing.
    #[test]
    fn a_prepared_rooms_loudspeakers_are_read_from_its_header() {
        let dir = std::env::temp_dir().join(format!("brir-header-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let s = multi_speaker(&[30.0, -30.0, 0.0, 110.0], &[0.0], 300, 48000.0, 0.05);
        let room = ExtractedRoom::extract(&s.raw(), OrientationSelection::FrontOnly)
            .unwrap()
            .with_source("/storage/sofa/room.sofa\n1\n2\n");
        let prepared = dir.join("room.prepared");
        std::fs::write(&prepared, room.to_prepared()).unwrap();
        assert_eq!(
            prepared_room_emitters(&prepared).unwrap().as_deref(),
            Some(room.emitters())
        );
        let other = dir.join("other.sofa");
        std::fs::write(&other, b"\x89HDF\r\n\x1a\nnot a prepared room").unwrap();
        assert_eq!(prepared_room_emitters(&other).unwrap(), None);
        std::fs::write(&other, b"OMNI").unwrap();
        assert_eq!(
            prepared_room_emitters(&other).unwrap(),
            None,
            "too short to tell"
        );

        // The same file loads through the one entry point a host uses.
        let opts = BrirLoadOptions::default();
        let loaded = BrirSet::load(prepared.to_str().unwrap(), 48000, &opts).unwrap();
        let want = BrirSet::from_raw(&s.raw(), 48000, &front_or(opts)).unwrap();
        assert_same_set(&loaded, &want, "BrirSet::load");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A room prepared from a file that states the room it was measured in
    /// keeps it: the room loads from the prepared file in the same box as
    /// from the file, and a host reads the box from the header with the
    /// loudspeakers. A room prepared before the box was kept (layout 1) still
    /// loads, without one.
    #[cfg(feature = "sofa")]
    #[test]
    fn a_prepared_room_keeps_the_room_it_was_measured_in() {
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/sofa");
        let dir = std::env::temp_dir().join(format!("brir-geometry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let opts = BrirLoadOptions {
            orientations: OrientationSelection::FrontOnly,
            ..BrirLoadOptions::default()
        };
        for name in [
            "room_corners_cartesian.sofa",
            "room_corners_offset_listener.sofa",
            "room_corners_unsupported_unit.sofa",
            "chunked_multispeaker_brir.sofa",
        ] {
            let path = fixtures.join(name);
            let from_file = BrirSet::from_sofa(path.to_str().unwrap(), 48000, &opts).unwrap();
            let prepared = dir.join(name).with_extension("room");
            let room = prepare_room(&std::fs::read(&path).unwrap()).unwrap().room;
            std::fs::write(&prepared, room.to_prepared()).unwrap();
            let from_room = BrirSet::load(prepared.to_str().unwrap(), 48000, &opts).unwrap();
            assert_eq!(from_room.room_corners(), from_file.room_corners(), "{name}");
            assert_eq!(from_room.room_type(), from_file.room_type(), "{name}");
            let header = prepared_room_loudspeakers(&prepared).unwrap().unwrap();
            assert_eq!(header.corners, from_file.room_corners(), "{name}");
            assert_eq!(header.emitters, room.emitters(), "{name}");
            let sofa = room_loudspeakers(&path).unwrap().unwrap();
            assert_eq!(sofa, header, "{name}: the same from the file");
        }
        let with_box = BrirSet::from_sofa(
            fixtures
                .join("room_corners_cartesian.sofa")
                .to_str()
                .unwrap(),
            48000,
            &opts,
        )
        .unwrap();
        assert!(with_box.room_corners().is_some() && with_box.room_type().is_some());

        // Layout 1: no geometry between the source and the loudspeakers.
        let s = multi_speaker(&[30.0, -30.0, 0.0], &[0.0], 300, 48000.0, 0.05);
        let room = ExtractedRoom::extract(&s.raw(), OrientationSelection::FrontOnly)
            .unwrap()
            .with_source("old");
        let v2 = room.to_prepared();
        let nc = u32::from_le_bytes(v2[24..28].try_into().unwrap()) as usize;
        let geometry = 28 + nc + 4 + 3;
        let mut v1 = v2.clone();
        v1[8..12].copy_from_slice(&1u32.to_le_bytes());
        v1.drain(geometry..geometry + 8);
        let back = ExtractedRoom::from_prepared(&v1).unwrap();
        assert_eq!(back, room);
        let old = dir.join("old.room");
        std::fs::write(&old, &v1).unwrap();
        let header = prepared_room_loudspeakers(&old).unwrap().unwrap();
        assert_eq!(
            (header.emitters.as_slice(), header.corners),
            (room.emitters(), None)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A prepared room keeps [`PREPARED_MAX_LENGTH_S`] after the set's lead.
    #[test]
    fn a_prepared_room_is_bounded_after_its_lead() {
        // 1 kHz keeps the 12 s response small.
        let mut s = multi_speaker(&[30.0], &[0.0], 12_000, 1000.0, 0.05);
        s.delay = vec![0.0, 0.0];
        let mut room = ExtractedRoom::extract(&s.raw(), OrientationSelection::FrontOnly).unwrap();
        let lead = common_lead(&room.pairs);
        room.cap_length();
        assert_eq!(room.pairs[0].left.len(), lead + 10_000);
        assert_eq!(room.pairs[0].right.len(), lead + 10_000);
        // Under that bound nothing is cut.
        let short = multi_speaker(&[30.0], &[0.0], 900, 1000.0, 0.05);
        let mut room =
            ExtractedRoom::extract(&short.raw(), OrientationSelection::FrontOnly).unwrap();
        let before = room.clone();
        room.cap_length();
        assert_eq!(room, before);
    }

    /// `raw` with `Data.IR` cut down to `run`, as a reader that read only
    /// those measurements hands it over.
    fn cut<'a>(raw: &RawRoomIr<'a>, run: std::ops::Range<usize>) -> RawRoomIr<'a> {
        let row = raw.r * raw.e * raw.n;
        RawRoomIr {
            data_ir: &raw.data_ir[run.start * row..run.end * row],
            ir_first: run.start,
            ..*raw
        }
    }

    /// One emitter per measurement, sources at three azimuths each measured
    /// at two head orientations, interleaved: the front orientation's
    /// measurements (0, 2, 4) are not a contiguous run.
    fn interleaved() -> Synth {
        let (m, r, n) = (6usize, 2usize, 200usize);
        let mut ir = vec![0.0f32; m * r * n];
        for mi in 0..m {
            for ri in 0..r {
                let resp = response(n, 100, marker(0, mi, ri), 0.0, (mi * 7 + ri) as u32);
                ir[(mi * r + ri) * n..][..n].copy_from_slice(&resp);
            }
        }
        Synth {
            m,
            r,
            e: 1,
            n,
            source: (0..m)
                .flat_map(|mi| sph([30.0, -30.0, 0.0][mi / 2], 0.0, 2.0))
                .collect(),
            emitter: vec![0.0; 3],
            listener: vec![0.0; 3],
            view: (0..m)
                .flat_map(|mi| sph([0.0, 10.0][mi % 2], 0.0, 1.0))
                .collect(),
            ir,
            delay: (0..m * r).map(|i| (i % 5) as f32).collect(),
            rate: 48000.0,
            conventions: "SingleRoomSRIR",
        }
    }

    /// A reader that reads only the measurements a selection is extracted
    /// from gets the room the whole set gives, `Data.Delay` included, for
    /// every selection: one orientation of a multi-speaker set is one
    /// measurement, and a run may hold measurements nothing keeps.
    #[test]
    fn the_measurements_a_selection_needs_extract_as_the_whole_set_does() {
        let decimated = OrientationSelection::Decimated {
            step_deg: 20.0,
            max_yaw_deg: 20.0,
        };
        let mut multi = multi_speaker(
            &[30.0, -30.0, 0.0],
            &[-40.0, -20.0, 0.0, 20.0, 40.0],
            200,
            48000.0,
            0.3,
        );
        // [M][R][E], so a run that starts past 0 must still find its own.
        multi.delay = (0..5 * 2 * 3).map(|i| (i % 7) as f32).collect();
        let cases = [
            (&multi, OrientationSelection::FrontOnly, 2..3),
            (&multi, decimated, 1..4),
            (&multi, OrientationSelection::All, 0..5),
        ];
        let inter = interleaved();
        for (s, selection, expected) in cases.into_iter().chain([
            (&inter, OrientationSelection::FrontOnly, 0..5),
            (&inter, OrientationSelection::All, 0..6),
        ]) {
            let raw = s.raw();
            let run = ExtractedRoom::measurements(&raw, selection).expect("plans");
            assert_eq!(run, expected, "{selection:?}");
            let whole = ExtractedRoom::extract(&raw, selection).expect("extracts");
            let part = ExtractedRoom::extract(&cut(&raw, run), selection).expect("extracts");
            assert_eq!(part, whole, "{selection:?}");
        }
        // Only the geometry is consulted to plan.
        let raw = RawRoomIr {
            data_ir: &[],
            ..multi.raw()
        };
        assert_eq!(
            ExtractedRoom::measurements(&raw, OrientationSelection::FrontOnly).unwrap(),
            2..3
        );
    }

    /// Responses that are neither the whole set nor exactly the run the
    /// selection needs are refused, not read out of place.
    #[test]
    fn measurements_other_than_the_run_needed_are_refused() {
        let s = multi_speaker(&[30.0, -30.0], &[-20.0, 0.0, 20.0], 200, 48000.0, 0.0);
        let raw = s.raw();
        for run in [0..1, 2..3, 0..2, 1..3] {
            let err =
                ExtractedRoom::extract(&cut(&raw, run.clone()), OrientationSelection::FrontOnly)
                    .expect_err("refused");
            assert!(err.to_string().contains("Data.IR holds"), "{run:?}: {err}");
        }
        ExtractedRoom::extract(&cut(&raw, 1..2), OrientationSelection::FrontOnly).expect("the run");
        ExtractedRoom::extract(&cut(&raw, 0..3), OrientationSelection::FrontOnly).expect("the set");
    }

    /// Through the SOFA reader: the front orientation of a file is read as
    /// its one measurement, and comes out as the whole file's front.
    /// `rows_multispeaker_brir.sofa` holds seven orientations, three in
    /// each of its chunks, and an `[M][R][E]` `Data.Delay`.
    #[cfg(feature = "sofa")]
    #[test]
    fn a_file_is_read_for_the_measurements_it_is_extracted_from() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/sofa/rows_multispeaker_brir.sofa"
        );
        let bytes = std::fs::read(path).expect("fixture");
        let front = with_sofa_room(&bytes, OrientationSelection::FrontOnly, |raw| {
            assert_eq!((raw.m, raw.r, raw.e, raw.n), (7, 2, 3, 50));
            assert_eq!((raw.ir_first, raw.data_ir.len()), (0, 2 * 3 * 50));
            ExtractedRoom::extract(raw, OrientationSelection::FrontOnly)
        })
        .expect("reads");
        let whole = with_sofa_room(&bytes, OrientationSelection::All, |raw| {
            assert_eq!(raw.data_ir.len(), 7 * 2 * 3 * 50);
            ExtractedRoom::extract(raw, OrientationSelection::All)
        })
        .expect("reads");
        assert_eq!(whole.orientations().len(), 7);
        assert_eq!(
            front,
            whole
                .clone()
                .select(OrientationSelection::FrontOnly)
                .unwrap()
        );
        // The fixture's Data.Delay is m·100 + r·10 + e samples; the front
        // is measurement 0.
        let lengths: Vec<(usize, usize)> = front
            .pairs
            .iter()
            .map(|p| (p.left.len(), p.right.len()))
            .collect();
        assert_eq!(lengths, [(50, 60), (51, 61), (52, 62)]);

        // Views turn 10° a measurement: ±20° keeps measurements 0 and 2.
        let decimated = OrientationSelection::Decimated {
            step_deg: 20.0,
            max_yaw_deg: 20.0,
        };
        let some = with_sofa_room(&bytes, decimated, |raw| {
            assert_eq!((raw.ir_first, raw.data_ir.len()), (0, 3 * 2 * 3 * 50));
            ExtractedRoom::extract(raw, decimated)
        })
        .expect("reads");
        assert_eq!(some.orientations().len(), 2);
        assert_eq!(some, whole.select(decimated).unwrap());
    }

    /// End-to-end through the SOFA reader on files generated outside the
    /// repo (see the loader's PR): `BRIR_SOFA_DIR` must point at a directory
    /// holding `msbrir.sofa` (48 k), `msbrir44.sofa` (44.1 k), `srir.sofa` and
    /// `longhrir.sofa`, all 5 emitters or views at 30/−30/0/110/−110° or
    /// −20..20°. Run it with `--ignored`.
    #[cfg(feature = "sofa")]
    #[test]
    #[ignore = "needs BRIR_SOFA_DIR: SOFA files generated outside the repository"]
    fn loads_generated_sofa_files() {
        let dir = std::env::var_os("BRIR_SOFA_DIR")
            .expect("set BRIR_SOFA_DIR to the directory of generated SOFA files");
        let dir = std::path::PathBuf::from(dir);
        let load = |name: &str| {
            BrirSet::from_sofa(
                dir.join(name).to_str().unwrap(),
                48000,
                &BrirLoadOptions::default(),
            )
            .unwrap_or_else(|e| panic!("{name}: {e}"))
        };
        let ms = load("msbrir.sofa");
        assert_eq!(ms.conventions(), "MultiSpeakerBRIR");
        assert_eq!(ms.emitters().len(), 5);
        assert_eq!(ms.orientations().len(), 5);
        assert!(
            ms.max_taps() > 4800 && ms.max_taps() < 24000,
            "{}",
            ms.max_taps()
        );
        // Emitter 0 is the left speaker (SOFA +30°): renderer x < 0.
        assert!(ms.emitters()[0][0] < -0.9, "{:?}", ms.emitters()[0]);
        // Left-ear marker of (emitter 1, SOFA view index 0 = renderer +20 =
        // orientation 4) is 0.51 × ILD; the right ear is negative.
        let p = ms.pair(1, 4);
        let (il, vl) = peak_of(&p.left);
        let (_, vr) = peak_of(&p.right);
        assert!(vl > 0.0 && vr < 0.0, "ears: {vl} / {vr}");
        assert_eq!(il, LEAD_GUARD);

        let ms44 = load("msbrir44.sofa");
        assert_eq!(ms44.sample_rate(), 48000);
        assert!((ms44.max_taps() as f32 / ms.max_taps() as f32 - 1.0).abs() < 0.02);

        let sr = load("srir.sofa");
        assert_eq!(sr.emitters().len(), 1);
        assert_eq!(sr.orientations().len(), 5);

        let lh = load("longhrir.sofa");
        assert_eq!(lh.emitters().len(), 5);
        assert_eq!(lh.orientations().len(), 1);
    }

    /// The room a set's loudspeakers stand in: the file's corners when it
    /// states them, grown to hold every loudspeaker (and the listener);
    /// else the loudspeakers' bounding box with a margin, a floor and
    /// headroom, flagged as an estimate.
    #[test]
    fn a_measured_room_is_the_files_box_or_an_estimate_around_the_loudspeakers() {
        let emitters = [
            [-1.5, 3.0, 0.0],
            [1.5, 3.0, 0.0],
            [-2.0, 0.0, 0.0],
            [2.0, -1.0, 1.2],
        ];
        let stated = MeasuredRoom::of(&emitters, Some([[2.5, -2.0, -1.3], [-2.5, 3.5, 1.4]]));
        assert!(!stated.estimated);
        assert_eq!(
            stated.box_m,
            [[-2.5, -2.0, -1.3], [2.5, 3.5, 1.4]],
            "corners in any order"
        );
        // A loudspeaker outside the stated box keeps its place in the room.
        let grown = MeasuredRoom::of(&emitters, Some([[-2.5, -2.0, -1.3], [2.5, 2.0, 1.4]]));
        assert_eq!(
            grown.box_m[1][1], 3.0,
            "the front wall moved out to the fronts"
        );
        assert!(!grown.estimated);

        let estimate = MeasuredRoom::of(&emitters, None);
        assert!(estimate.estimated);
        let m = MeasuredRoom::BOX_MARGIN_M;
        assert_eq!(
            estimate.box_m[0],
            [-2.0 - m, -1.0 - m, -MeasuredRoom::FLOOR_M]
        );
        assert_eq!(estimate.box_m[1], [2.0 + m, 3.0 + m, 1.2 + m]);
        // Headroom applies when the loudspeakers are lower than it.
        let flat = MeasuredRoom::of(&[[-1.0, 1.0, 0.0], [1.0, 1.0, 0.0]], None);
        assert_eq!(flat.box_m[1][2], MeasuredRoom::HEADROOM_M);
    }

    /// The stage's ratios of a measured room: the half-width is the unit,
    /// taken to the farther side wall, the other walls as multiples of it,
    /// the user's blend kept. A cube the listener is centred in is the unit
    /// cube: the direction reading of the direct path (#783), which a
    /// measured room generalises (#803).
    #[test]
    fn a_measured_rooms_ratios_put_the_listener_where_it_was_measured() {
        let room = MeasuredRoom {
            box_m: [[-2.0, -1.0, -1.2], [3.0, 4.5, 1.8]],
            estimated: false,
        };
        assert_eq!(room.radius_m(), 3.0, "the farther side wall");
        let ratios = room.ratios(0.25);
        assert_eq!(ratios.ratio[0], 1.0);
        assert!((ratios.ratio[1] - 1.5).abs() < 1e-6 && (ratios.ratio[2] - 0.6).abs() < 1e-6);
        assert!((ratios.rear - 1.0 / 3.0).abs() < 1e-6);
        assert!((ratios.lower - 0.4).abs() < 1e-6);
        assert_eq!(ratios.center_blend, 0.25);

        let cube = MeasuredRoom {
            box_m: [[-2.5; 3], [2.5; 3]],
            estimated: true,
        };
        assert_eq!(
            cube.ratios(0.0),
            crate::live_params::RoomRatios::UNIT,
            "a centred cube pans as the direction reading does"
        );
        // A wall at the listener never collapses an axis.
        let flat = MeasuredRoom {
            box_m: [[-1.0, 0.0, 0.0], [1.0, 2.0, 0.0]],
            estimated: true,
        };
        let ratios = flat.ratios(0.5);
        assert!(ratios.rear > 0.0 && ratios.lower > 0.0 && ratios.ratio[2] > 0.0);
    }
}
