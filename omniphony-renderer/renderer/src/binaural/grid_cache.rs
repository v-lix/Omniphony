//! A file a host names to keep the finished HRIR grid of a SOFA set in.
//!
//! Building the grid of a SOFA set - resampling, minimum-phase reconstruction,
//! triangulation, the 5-degree grid, its equalisation and level - takes
//! hundreds of milliseconds of CPU, and the HRIR worker runs at background
//! priority, so a busy machine can stretch it to seconds of the embedded set
//! at every session start. The finished grid is a few MB that loads in
//! milliseconds.
//!
//! A cache serves sessions of one diffuse-field setting, at one sample rate or
//! at every rate, as the host declares: a session it serves loads the grid
//! from the file for its rate when the file holds the grid of the same SOFA
//! bytes ([`SourceId`]), rate and setting, in this engine's format, and
//! otherwise builds it as without a cache and writes it over the file. A
//! path naming `{khz}` (the rate in kHz, 44 for 44.1 kHz) or `{rate}` (in Hz)
//! keeps one file per rate, each written at the first session of its rate.
//! Every session the cache does not serve builds as before and leaves the
//! files alone.
//!
//! A grid is of one format ([`GRID_FORMAT`]), which this engine writes and
//! reads and no other: a grid of another, or one that does not read whole,
//! is built again, never played.

use std::path::PathBuf;

use super::hrir::HrirSet;
use super::source_id::SourceId;

const MAGIC: &[u8; 8] = b"OMNIGRID";
/// The grid format this engine writes and reads, and no other. Any change to
/// what a grid holds, or to how a SOFA set's grid is built, is a new format,
/// so that every grid an older engine kept is built again rather than played
/// as something it is not. `grids_are_what_this_format_makes` fails on such
/// a change until this is raised.
pub const GRID_FORMAT: u32 = 1;
/// The magic, the format, the rate and setting served (`u32` each), and the
/// SOFA file's identity.
const HEADER_LEN: usize = 8 + 3 * 4 + SourceId::BYTES;

/// What [`super::BinauralRenderer::prepare_grid_cache`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prepared {
    /// Built from the SOFA file and written.
    Built,
    /// The file already held this set's grid, in this format.
    Kept,
}

/// Why [`super::BinauralRenderer::prepare_grid_cache`] kept no grid.
#[derive(Debug)]
pub enum PrepareError {
    /// The SOFA file cannot be read, or is not a set the HRTF stage loads.
    Unusable(String),
    /// The grid was built but cannot be written.
    Write(std::io::Error),
}

/// Where a host keeps the grid, and for which sessions.
#[derive(Clone, Debug, PartialEq)]
pub struct GridCache {
    /// The file, or with `{khz}` or `{rate}` in it one file per rate - see
    /// [`Self::file`].
    pub path: PathBuf,
    /// The one session rate the cache serves, or `None` for every rate.
    pub sample_rate: Option<u32>,
    /// The diffuse-field equalisation the cache serves.
    pub diffuse_field_eq: bool,
}

impl GridCache {
    /// Whether a session at `sample_rate` with `diffuse_field_eq` uses it.
    pub fn serves(&self, sample_rate: u32, diffuse_field_eq: bool) -> bool {
        self.sample_rate.is_none_or(|r| r == sample_rate)
            && self.diffuse_field_eq == diffuse_field_eq
    }

    /// The file a session at `sample_rate` keeps its grid in: the path with
    /// `{khz}` replaced by the rate in whole kHz (`hrtf{khz}.grid` is
    /// `hrtf44.grid` at 44.1 kHz) and `{rate}` by the rate in Hz. A path
    /// naming neither is the one file.
    pub fn file(&self, sample_rate: u32) -> PathBuf {
        let path = self.path.to_string_lossy();
        if !path.contains("{khz}") && !path.contains("{rate}") {
            return self.path.clone();
        }
        PathBuf::from(
            path.replace("{khz}", &(sample_rate / 1000).to_string())
                .replace("{rate}", &sample_rate.to_string()),
        )
    }

    /// The grid `sofa` builds at `sample_rate`, when its file holds it: in
    /// this format, from these bytes, for this rate and setting, and whole.
    /// `None` for anything else, a missing file included.
    pub fn load(&self, sample_rate: u32, sofa: &[u8]) -> Option<HrirSet> {
        let file = self.file(sample_rate);
        let bytes = std::fs::read(&file).ok()?;
        let grid = bytes.strip_prefix(self.header(sample_rate, sofa).as_slice())?;
        HrirSet::from_bytes(grid)
            .map_err(|e| log::warn!("HRIR grid cache {}: {e:#}", file.display()))
            .ok()
    }

    /// Keep `set`, built from `sofa` at `sample_rate`, for the next session
    /// at that rate: through `<file>.part`, renamed into place.
    pub fn store(&self, sample_rate: u32, sofa: &[u8], set: &HrirSet) -> std::io::Result<()> {
        let mut bytes = self.header(sample_rate, sofa);
        bytes.extend_from_slice(&set.to_bytes());
        let file = self.file(sample_rate);
        let mut part = file.clone().into_os_string();
        part.push(".part");
        let part = PathBuf::from(part);
        std::fs::write(&part, &bytes)
            .and_then(|()| std::fs::rename(&part, &file))
            .inspect_err(|_| {
                let _ = std::fs::remove_file(&part);
            })
    }

    /// What a grid for `sofa` starts with: the magic, the format, the rate
    /// and setting served, and the SOFA bytes' identity.
    fn header(&self, sample_rate: u32, sofa: &[u8]) -> Vec<u8> {
        let mut h = Vec::with_capacity(HEADER_LEN);
        h.extend_from_slice(MAGIC);
        h.extend_from_slice(&GRID_FORMAT.to_le_bytes());
        h.extend_from_slice(&sample_rate.to_le_bytes());
        h.extend_from_slice(&u32::from(self.diffuse_field_eq).to_le_bytes());
        h.extend_from_slice(&SourceId::of(sofa).to_bytes());
        h
    }
}

#[cfg(all(test, feature = "sofa"))]
mod tests {
    use super::*;

    fn sofa_file() -> (String, Vec<u8>) {
        let path = format!("{}/tests/sofa/tester.sofa", env!("CARGO_MANIFEST_DIR"));
        let bytes = std::fs::read(&path).unwrap();
        (path, bytes)
    }

    fn cache(dir: &std::path::Path) -> GridCache {
        GridCache {
            path: dir.join("hrtf.grid"),
            sample_rate: Some(48_000),
            diffuse_field_eq: true,
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("grid-cache-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A stored grid loads back bit for bit, for the bytes it was built from.
    #[test]
    fn a_stored_grid_loads_back_as_it_was_built() {
        let dir = scratch("round");
        let (path, sofa) = sofa_file();
        let c = cache(&dir);
        assert!(c.load(48_000, &sofa).is_none(), "nothing stored yet");
        let built = super::super::measured::hrir_set_from_sofa(&path, 48_000, true).unwrap();
        c.store(48_000, &sofa, &built).unwrap();
        let loaded = c.load(48_000, &sofa).expect("loads");
        assert_eq!(loaded.to_bytes(), built.to_bytes());
        assert!(!dir.join("hrtf.grid.part").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Another file or another configuration does not get the grid; nor
    /// does a grid of another format, or a file cut short.
    #[test]
    fn a_grid_serves_only_what_built_it() {
        let dir = scratch("stale");
        let (path, sofa) = sofa_file();
        let c = cache(&dir);
        let built = super::super::measured::hrir_set_from_sofa(&path, 48_000, true).unwrap();
        c.store(48_000, &sofa, &built).unwrap();

        let mut other = sofa.clone();
        *other.last_mut().unwrap() ^= 1;
        assert!(c.load(48_000, &other).is_none(), "other SOFA bytes");
        let whole = std::fs::read(&c.path).unwrap();
        assert_eq!(&whole[8..12], &GRID_FORMAT.to_le_bytes());
        assert_eq!(&whole[20..HEADER_LEN], &SourceId::of(&sofa).to_bytes());
        let mut other_format = whole.clone();
        other_format[8..12].copy_from_slice(&(GRID_FORMAT + 1).to_le_bytes());
        std::fs::write(&c.path, &other_format).unwrap();
        assert!(c.load(48_000, &sofa).is_none(), "another format");
        std::fs::write(&c.path, &whole).unwrap();
        assert!(c.load(48_000, &sofa).is_some(), "this format");
        assert!(c.load(96_000, &sofa).is_none(), "another rate");
        let flat = GridCache {
            diffuse_field_eq: false,
            ..c.clone()
        };
        assert!(flat.load(48_000, &sofa).is_none(), "another setting");

        std::fs::write(&c.path, &whole[..whole.len() - 4]).unwrap();
        assert!(c.load(48_000, &sofa).is_none(), "a file cut short");
        assert!(c.serves(48_000, true) && !c.serves(96_000, true) && !c.serves(48_000, false));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A cache for every rate keeps one file per rate, named by it, and a
    /// grid built at one rate is never served at another.
    #[test]
    fn a_cache_for_every_rate_keeps_a_file_per_rate() {
        let dir = scratch("rates");
        let (path, sofa) = sofa_file();
        let c = GridCache {
            path: dir.join("hrtf{khz}.grid"),
            sample_rate: None,
            ..cache(&dir)
        };
        assert!(c.serves(44_100, true) && c.serves(96_000, true) && !c.serves(48_000, false));
        assert_eq!(c.file(44_100), dir.join("hrtf44.grid"));
        assert_eq!(c.file(48_000), dir.join("hrtf48.grid"));
        assert_eq!(c.file(192_000), dir.join("hrtf192.grid"));
        let hz = GridCache {
            path: dir.join("hrtf-{rate}.grid"),
            ..c.clone()
        };
        assert_eq!(hz.file(88_200), dir.join("hrtf-88200.grid"));
        assert_eq!(cache(&dir).file(96_000), dir.join("hrtf.grid"), "one file");

        for rate in [48_000, 44_100] {
            let built = super::super::measured::hrir_set_from_sofa(&path, rate, true).unwrap();
            c.store(rate, &sofa, &built).unwrap();
            assert_eq!(c.load(rate, &sofa).unwrap().to_bytes(), built.to_bytes());
        }
        assert!(dir.join("hrtf48.grid").exists() && dir.join("hrtf44.grid").exists());
        // A 44.1 kHz grid copied over the 48 kHz one is refused by its header.
        std::fs::copy(dir.join("hrtf44.grid"), dir.join("hrtf48.grid")).unwrap();
        assert!(c.load(48_000, &sofa).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// What this format makes of a SOFA set is pinned: a change to what a
    /// grid holds, or to how a set's grid is built, fails here until
    /// [`GRID_FORMAT`] is raised and the figures below updated, so that every
    /// grid an older engine kept is built again rather than played as
    /// something it is not. The grid's shape is compared exactly, its taps
    /// through two sums to a part in a million: tighter than any change to
    /// how a grid is built, looser than the rounding of one processor's
    /// arithmetic against another's.
    #[test]
    fn grids_are_what_this_format_makes() {
        let (path, _) = sofa_file();
        let grid = super::super::measured::hrir_set_from_sofa(&path, 48_000, true)
            .unwrap()
            .to_bytes();
        let words: Vec<u32> = grid[..16]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|w| u32::from_le_bytes(*w))
            .collect();
        let (energy, moment) = grid[16..].as_chunks::<4>().0.iter().enumerate().fold(
            (0f64, 0f64),
            |(e, m), (i, b)| {
                let v = f64::from(f32::from_le_bytes(*b));
                (e + v * v, m + v * (i % 97) as f64)
            },
        );
        let close = |got: f64, want: f64| (got - want).abs() <= 1e-6 * want.abs().max(1e-3);
        assert_eq!(GRID_FORMAT, 1);
        assert!(
            words == GOLDEN_WORDS && close(energy, GOLDEN_ENERGY) && close(moment, GOLDEN_MOMENT),
            "the grid changed: raise GRID_FORMAT and update this - {words:?} {energy:e} {moment:e}"
        );
    }

    /// `tester.sofa` at 48 kHz, equalised: the rate, the taps in use and the
    /// grid's azimuths and elevations; then the taps' energy and moment.
    const GOLDEN_WORDS: [u32; 4] = [48_000, 128, 72, 27];
    const GOLDEN_ENERGY: f64 = 3.842_619_721_832_446e3;
    const GOLDEN_MOMENT: f64 = 2.219_693_799_717_918e5;
}
