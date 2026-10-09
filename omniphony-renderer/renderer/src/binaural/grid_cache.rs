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
//! from the file for its rate when it was built from the same SOFA bytes by
//! the same engine build, and otherwise builds it as without a cache and
//! writes it. A path naming `{khz}` (the rate in kHz, 44 for 44.1 kHz) or
//! `{rate}` (in Hz) keeps one file per rate, each written at the first
//! session of its rate. Every session the cache does not serve builds as
//! before and leaves the files alone.

use std::path::PathBuf;

use super::hrir::HrirSet;

const MAGIC: &[u8; 8] = b"OMNIGRID";
const VERSION: u32 = 1;

/// What [`super::BinauralRenderer::prepare_grid_cache`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prepared {
    /// Built from the SOFA file and written.
    Built,
    /// The file already held this set's grid, from this build.
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
    /// The engine build that writes and reads it: a grid from another build
    /// is not used, so a change to how grids are made never plays a stale one.
    pub stamp: String,
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

    /// The grid `sofa` builds at `sample_rate`, when its file holds it:
    /// written by this build from these bytes for this rate and setting, and
    /// whole. `None` for anything else, a missing file included.
    pub fn load(&self, sample_rate: u32, sofa: &[u8]) -> Option<HrirSet> {
        let file = self.file(sample_rate);
        let bytes = std::fs::read(&file).ok()?;
        let grid = self
            .header(sample_rate, sofa)
            .and_then(|h| bytes.strip_prefix(h.as_slice()))?;
        HrirSet::from_bytes(grid)
            .map_err(|e| log::warn!("HRIR grid cache {}: {e:#}", file.display()))
            .ok()
    }

    /// Keep `set`, built from `sofa` at `sample_rate`, for the next session
    /// at that rate: through `<file>.part`, renamed into place.
    pub fn store(&self, sample_rate: u32, sofa: &[u8], set: &HrirSet) -> std::io::Result<()> {
        let mut bytes = self
            .header(sample_rate, sofa)
            .ok_or_else(|| std::io::Error::other("cache stamp too long"))?;
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

    /// What a grid for `sofa` starts with: the magic, the format version,
    /// the engine build, the rate and setting served, and the SOFA bytes'
    /// length and FNV-1a hash.
    fn header(&self, sample_rate: u32, sofa: &[u8]) -> Option<Vec<u8>> {
        let stamp = u32::try_from(self.stamp.len()).ok()?;
        let mut h = Vec::with_capacity(48 + self.stamp.len());
        h.extend_from_slice(MAGIC);
        h.extend_from_slice(&VERSION.to_le_bytes());
        h.extend_from_slice(&stamp.to_le_bytes());
        h.extend_from_slice(self.stamp.as_bytes());
        h.extend_from_slice(&sample_rate.to_le_bytes());
        h.extend_from_slice(&u32::from(self.diffuse_field_eq).to_le_bytes());
        h.extend_from_slice(&(sofa.len() as u64).to_le_bytes());
        h.extend_from_slice(&fnv1a(sofa).to_le_bytes());
        Some(h)
    }
}

/// 64-bit FNV-1a: a stable identity for the SOFA bytes a grid came from.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
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
            stamp: "test build".into(),
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

    /// Another file, another engine build or another configuration does not
    /// get the grid; nor does a file cut short.
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
        let rebuilt = GridCache {
            stamp: "next build".into(),
            ..c.clone()
        };
        assert!(
            rebuilt.load(48_000, &sofa).is_none(),
            "another engine build"
        );
        assert!(c.load(96_000, &sofa).is_none(), "another rate");
        let flat = GridCache {
            diffuse_field_eq: false,
            ..c.clone()
        };
        assert!(flat.load(48_000, &sofa).is_none(), "another setting");

        let whole = std::fs::read(&c.path).unwrap();
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
}
