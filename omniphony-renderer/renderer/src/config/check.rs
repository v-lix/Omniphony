//! Whether a config file is one this renderer reads as it is written.
//!
//! [`Config::load_or_default`] reads a file it cannot parse as the defaults,
//! and keeps a key it does not know - or a value of an enum it does not know -
//! for a later build to read; an option out of its range is clamped or passed
//! over when the session starts. That is right for the renderer's own file,
//! which a newer build may have written. A host that writes or merges a
//! config for a session - a player letting its user override its own - wants
//! to know instead, before the session starts, so that it can say why and
//! use another.
//!
//! [`check_config`] says: the file is one YAML document without tags, reads
//! as a config, holds no key or enum value this build does not know, no
//! number that is not finite or is outside the option registry's range, and
//! no choice the renderer would pass over; an HRIR source that needs a file
//! names one, and the files it names are there. It says nothing of whose
//! keys they are: which keys a host lets its user set is the host's to
//! decide.

use std::path::Path;

use serde_yaml_ng::{Mapping, Value};

use super::Config;
use crate::binaural::HrirSource;
use crate::options::{self, OptionKind};

/// Largest config read, bytes: a config with every key set is tens of
/// kilobytes.
pub const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// The registry option whose bounds a binaural value is checked against,
/// for the keys of `render.binaural` that have one (by path under it).
const BINAURAL_BOUNDS: &[(&[&str], &str)] = &[
    (&["unit_scale_m"], "binaural_unit_scale_m"),
    (&["head_radius_m"], "binaural_head_radius_m"),
    (&["brir_max_length_s"], "brir_max_length_s"),
    (&["brir_tail_floor_db"], "brir_tail_floor_db"),
    (&["ear_gains"], "binaural_ear_gains"),
    (&["reflections", "level"], "reflections_level"),
    (
        &["reflections", "wall_cutoff_hz"],
        "reflections_wall_cutoff_hz",
    ),
    (&["reflections", "room_width_m"], "reflections_room_width_m"),
    (&["reflections", "room_depth_m"], "reflections_room_depth_m"),
    (
        &["reflections", "room_height_m"],
        "reflections_room_height_m",
    ),
    (&["reverb", "level"], "reverb_level"),
    (&["reverb", "rt60_s"], "reverb_rt60_s"),
    (&["reverb", "predelay_ms"], "reverb_predelay_ms"),
    (&["reverb", "size"], "reverb_size"),
    (&["reverb", "rt60_low_ratio"], "reverb_rt60_low_ratio"),
    (&["reverb", "rt60_high_ratio"], "reverb_rt60_high_ratio"),
];

/// The registry's choices under `render.binaural`, by path: checked as the
/// ones directly under `render` are.
const BINAURAL_CHOICES: &[(&str, &str)] = &[
    ("mode", "binaural_mode"),
    ("hrir_update_lattice", "hrir_update_lattice"),
];

/// `render.master_gain` is decibels in the file; the registry's bound is the
/// linear gain's (1000).
const MAX_MASTER_GAIN_DB: f64 = 60.0;

/// [`check_config`] on the file at `path`.
pub fn check_config_file(path: &Path) -> Result<(), String> {
    let size = std::fs::metadata(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?
        .len();
    if size > MAX_CONFIG_BYTES {
        return Err(format!(
            "{} is {size} bytes, more than the {MAX_CONFIG_BYTES} read",
            path.display()
        ));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    check_config(&text)
}

/// Whether `text` is a config this renderer reads as written; the reason
/// when it is not.
pub fn check_config(text: &str) -> Result<(), String> {
    let value = parse(text).map_err(|e| format!("not valid YAML: {e}"))?;
    if value.is_null() {
        return Ok(());
    }
    if !value.is_mapping() {
        return Err("a config is a mapping".into());
    }
    check_values(&value, "the config")?;
    // Ranges and choices first: they name the key, where the config's own
    // reading names only what it expected.
    if let Some(Value::Mapping(render)) = value.get("render") {
        check_bounds(render)?;
        check_choices(render)?;
    }
    let config: Config =
        serde_yaml_ng::from_value(value).map_err(|e| format!("does not read as a config: {e}"))?;
    check_known(&config)?;
    check_sources(&config)
}

/// One YAML document; tags refused.
fn parse(text: &str) -> Result<Value, String> {
    // A file of comments and blank lines is no document at all.
    if text
        .lines()
        .all(|l| l.trim().is_empty() || l.trim_start().starts_with('#'))
    {
        return Ok(Value::Null);
    }
    let value: Value = serde_yaml_ng::from_str(text).map_err(|e| e.to_string())?;
    reject_tags(&value)?;
    Ok(value)
}

fn reject_tags(value: &Value) -> Result<(), String> {
    match value {
        Value::Tagged(t) => Err(format!("YAML tag {} is not accepted", t.tag)),
        Value::Mapping(m) => m.iter().try_for_each(|(k, v)| {
            reject_tags(k)?;
            reject_tags(v)
        }),
        Value::Sequence(s) => s.iter().try_for_each(reject_tags),
        _ => Ok(()),
    }
}

/// No non-finite number anywhere.
fn check_values(value: &Value, at: &str) -> Result<(), String> {
    match value {
        Value::Number(n) if n.as_f64().is_some_and(|v| !v.is_finite()) => {
            Err(format!("{at}: {n} is not a finite number"))
        }
        Value::Mapping(m) => m
            .iter()
            .try_for_each(|(k, v)| check_values(v, &format!("{at}.{}", k.as_str().unwrap_or("?")))),
        Value::Sequence(s) => s.iter().try_for_each(|v| check_values(v, at)),
        _ => Ok(()),
    }
}

/// Nothing the config kept for a later build: a key or an enum value this
/// build does not know lands in a section's `extra`.
fn check_known(config: &Config) -> Result<(), String> {
    let unknown = |section: &str, extra: &Mapping| -> Result<(), String> {
        match extra.keys().next() {
            None => Ok(()),
            Some(key) => Err(format!(
                "{section}{} is not a key (or value) this renderer knows",
                key.as_str().map_or("?".into(), |k| format!(".{k}"))
            )),
        }
    };
    unknown("the config", &config.extra)?;
    if let Some(global) = &config.global {
        unknown("global", &global.extra)?;
    }
    let Some(render) = &config.render else {
        return Ok(());
    };
    unknown("render", &render.extra)?;
    if let Some(placement) = &render.placement {
        for (family, own) in &placement.families {
            unknown(&format!("render.placement.{family}"), &own.extra)?;
        }
    }
    if let Some(bin) = &render.binaural {
        unknown("render.binaural", &bin.extra)?;
        if let Some(r) = &bin.reverb {
            unknown("render.binaural.reverb", &r.extra)?;
        }
        if let Some(r) = &bin.reflections {
            unknown("render.binaural.reflections", &r.extra)?;
        }
    }
    Ok(())
}

/// Numbers within the registry's bounds, where the registry has one for
/// the key: every option a `render` key names - declared, or registered by
/// hand as the room proportions, distance, evaluation and panner ones are -
/// the binaural keys of [`BINAURAL_BOUNDS`], and `master_gain`, which the
/// file holds in dB where the registry holds a linear gain.
fn check_bounds(render: &Mapping) -> Result<(), String> {
    for (key, value) in render {
        let Some(key) = key.as_str() else { continue };
        if key == "master_gain" {
            if let Some(db) = value.as_f64()
                && db > MAX_MASTER_GAIN_DB
            {
                return Err(format!(
                    "render.master_gain {db} dB is above the {MAX_MASTER_GAIN_DB} dB the \
                     renderer takes"
                ));
            }
        } else if let Some(spec) = options::find(key) {
            within(spec.kind, value, &format!("render.{key}"))?;
        }
    }
    if let Some(Value::Mapping(bin)) = render.get("binaural") {
        for (path, option) in BINAURAL_BOUNDS {
            let mut node = bin.get(path[0]);
            for step in &path[1..] {
                node = node.and_then(|v| v.as_mapping()).and_then(|m| m.get(*step));
            }
            if let Some(value) = node {
                let spec =
                    options::find(option).ok_or_else(|| format!("no registry option {option}"))?;
                within(
                    spec.kind,
                    value,
                    &format!("render.binaural.{}", path.join(".")),
                )?;
            }
        }
    }
    Ok(())
}

/// Choices the renderer takes: where the registry has a closed set for the
/// key, or the backends, the value must be one its own setter takes - the
/// parser the option is seeded with - so a value the renderer would pass
/// over at start is refused. Plugin backends are not known here: only the
/// built-in ones are taken.
fn check_choices(render: &Mapping) -> Result<(), String> {
    let check = |option: &str, value: &Value, at: String| -> Result<(), String> {
        let Some(spec) = options::find(option) else {
            return Ok(());
        };
        if !matches!(
            spec.kind,
            OptionKind::Enum(_) | OptionKind::DynamicEnum { .. }
        ) {
            return Ok(());
        }
        // Types are the config's to check.
        let Some(text) = value.as_str() else {
            return Ok(());
        };
        let mut live = crate::live_params::LiveParams::default();
        let taken = (spec.set)(
            &mut live,
            &options::RawOptionValue::Str(text),
            &options::OptionEnv::detached(),
        );
        match taken {
            Some(_) => Ok(()),
            None => Err(format!("{at}: {text:?} is not a value this renderer takes")),
        }
    };
    for (key, value) in render {
        if let Some(key) = key.as_str() {
            check(key, value, format!("render.{key}"))?;
        }
    }
    if let Some(Value::Mapping(bin)) = render.get("binaural") {
        for (key, option) in BINAURAL_CHOICES {
            if let Some(value) = bin.get(*key) {
                check(option, value, format!("render.binaural.{key}"))?;
            }
        }
        if let Some(value) = bin.get("hrir_source") {
            let text = value
                .as_str()
                .ok_or("render.binaural.hrir_source must be a string")?;
            if HrirSource::from_str(text).is_none() {
                return Err(format!(
                    "render.binaural.hrir_source {text:?} is not a source this renderer knows"
                ));
            }
        }
    }
    Ok(())
}

/// `value` within `kind`'s range, when `kind` has one and `value` is a
/// number (or, for an array kind, a sequence of them). Types are the
/// config's to check.
fn within(kind: OptionKind, value: &Value, at: &str) -> Result<(), String> {
    let out = |v: f64, min: f64, max: f64| Err(format!("{at}: {v} is outside {min}..{max}"));
    match kind {
        OptionKind::Float { min, max, .. } => {
            if let Some(v) = value.as_f64()
                && !(f64::from(min)..=f64::from(max)).contains(&v)
            {
                return out(v, f64::from(min), f64::from(max));
            }
        }
        OptionKind::Int { min, max } => {
            if let Some(v) = value.as_f64()
                && !((min as f64)..=(max as f64)).contains(&v)
            {
                return out(v, min as f64, max as f64);
            }
        }
        OptionKind::FloatArray { len, min, max, .. } => {
            if let Some(items) = value.as_sequence() {
                if items.len() != len {
                    return Err(format!("{at}: {} values, {len} expected", items.len()));
                }
                for v in items.iter().filter_map(Value::as_f64) {
                    if !(f64::from(min)..=f64::from(max)).contains(&v) {
                        return out(v, f64::from(min), f64::from(max));
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// An HRIR source that takes its file from a key names one, and the files
/// the binaural section names are there: either would otherwise leave the
/// session on the embedded set, with only a log line to say so.
fn check_sources(config: &Config) -> Result<(), String> {
    let Some(bin) = config.render.as_ref().and_then(|r| r.binaural.as_ref()) else {
        return Ok(());
    };
    match bin.hrir_source.as_deref().and_then(HrirSource::from_str) {
        Some(HrirSource::Sofa(p)) if p.is_empty() && bin.hrtf_sofa_path.is_none() => {
            return Err("render.binaural.hrir_source sofa needs hrtf_sofa_path".into());
        }
        Some(HrirSource::Brir(p)) if p.is_empty() && bin.brir_sofa_path.is_none() => {
            return Err("render.binaural.hrir_source brir needs brir_sofa_path".into());
        }
        _ => {}
    }
    for (key, path) in [
        ("hrtf_sofa_path", &bin.hrtf_sofa_path),
        ("brir_sofa_path", &bin.brir_sofa_path),
    ] {
        let Some(path) = path else { continue };
        let meta = std::fs::metadata(path)
            .map_err(|e| format!("render.binaural.{key}: {}: {e}", path.display()))?;
        if !meta.is_file() {
            return Err(format!(
                "render.binaural.{key}: {} is not a file",
                path.display()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejected(text: &str) -> String {
        check_config(text).expect_err(text)
    }

    fn fixture(name: &str) -> String {
        format!("{}/tests/sofa/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    /// A host's generated config, as a player writes it, reads.
    #[test]
    fn a_full_config_reads() {
        let sofa = fixture("tester.sofa");
        let config = format!(
            "render:\n  bridge_path: '/usr/lib/omniphony/libpcm_bridge.so'\n  \
             master_gain: -3.0\n  auto_gain: false\n  decode_thread: true\n  \
             binaural:\n    output_mode: binaural\n    mode: cascaded\n    \
             hrir_source: sofa\n    hrtf_sofa_path: '{sofa}'\n    \
             reverb: {{ enabled: true, level: 0.10 }}\n    \
             reflections: {{ enabled: false }}\n"
        );
        assert_eq!(check_config(&config), Ok(()));
        // Nothing, and a template of nulls, are no less a config.
        assert_eq!(check_config("# comments only\n\n"), Ok(()));
        assert_eq!(
            check_config("render:\n  master_gain: null\n  binaural:\n    reverb: null\n"),
            Ok(())
        );
    }

    #[test]
    fn what_does_not_parse_is_rejected() {
        assert!(rejected("render: [unclosed\n").contains("not valid YAML"));
        assert!(
            rejected("render:\n  auto_gain: true\n---\nrender:\n  auto_gain: false\n")
                .contains("not valid YAML")
        );
        let dup = rejected("render:\n  auto_gain: true\n  auto_gain: false\n");
        assert!(
            dup.contains("not valid YAML") || dup.contains("duplicate"),
            "{dup}"
        );
        assert!(rejected("render:\n  master_gain: !dB 3\n").contains("tag"));
        assert!(rejected("- render\n").contains("mapping"));
        assert!(rejected("render: 3\n").contains("does not read"));
    }

    /// A key or a value this build does not know is refused, never kept for
    /// later as the renderer's own file's would be, whatever the depth.
    #[test]
    fn what_this_build_does_not_know_is_rejected() {
        assert!(rejected("render:\n  reverbb: 0.2\n").contains("render.reverbb"));
        assert!(rejected("rendr:\n  auto_gain: true\n").contains("the config.rendr"));
        assert!(
            rejected("render:\n  binaural:\n    reverbb: 0.2\n")
                .contains("render.binaural.reverbb")
        );
        assert!(
            rejected("render:\n  binaural:\n    reverb:\n      levle: 0.2\n")
                .contains("render.binaural.reverb.levle")
        );
        assert!(
            rejected("render:\n  binaural:\n    reflections:\n      height: 3\n")
                .contains("render.binaural.reflections.height")
        );
        assert!(
            rejected("render:\n  placement:\n    generic:\n      moode: room\n")
                .contains("render.placement.generic.moode")
        );
        assert!(rejected("render:\n  ramp_mode: fastest\n").contains("ramp_mode"));
        assert!(rejected("render:\n  binaural:\n    hrir_source: kemur\n").contains("kemur"));
        // The wrong type.
        assert!(
            rejected("render:\n  binaural:\n    unit_scale_m: two\n").contains("does not read")
        );
        assert!(
            rejected("render:\n  binaural:\n    ear_mutes: [true]\n").contains("does not read")
        );
    }

    /// A choice the renderer would pass over at start is refused: never a
    /// gain applied beside an evaluation mode silently left at its default.
    #[test]
    fn a_choice_the_renderer_does_not_take_is_rejected() {
        for (config, what) in [
            (
                "render:\n  master_gain: -3\n  render_evaluation_mode: fastest\n",
                "render.render_evaluation_mode",
            ),
            (
                "render:\n  render_backend: nonesuch\n",
                "render.render_backend",
            ),
            (
                "render:\n  vbap_distance_model: far\n",
                "render.vbap_distance_model",
            ),
            (
                "render:\n  distance_diffuse_mirror_axes: q\n",
                "render.distance_diffuse_mirror_axes",
            ),
            (
                "render:\n  binaural:\n    mode: sideways\n",
                "render.binaural.mode",
            ),
            (
                "render:\n  binaural:\n    hrir_update_lattice: rough\n",
                "render.binaural.hrir_update_lattice",
            ),
        ] {
            assert!(rejected(config).contains(what), "{config}");
        }
        // Every value each choice lists is taken.
        for spec in options::LIVE_OPTIONS {
            let OptionKind::Enum(values) = spec.kind else {
                continue;
            };
            for value in values {
                let mut render = Mapping::new();
                render.insert(Value::from(spec.key), Value::from(*value));
                assert_eq!(check_choices(&render), Ok(()), "{} {value}", spec.key);
            }
        }
        let mut render = Mapping::new();
        render.insert(Value::from("render_backend"), Value::from("vbap"));
        assert_eq!(check_choices(&render), Ok(()));
    }

    #[test]
    fn numbers_out_of_range_are_rejected() {
        for (config, what) in [
            (
                "render:\n  binaural:\n    reverb:\n      rt60_s: 5\n",
                "rt60_s",
            ),
            (
                "render:\n  binaural:\n    reflections:\n      room_width_m: 0.5\n",
                "room_width_m",
            ),
            (
                "render:\n  binaural:\n    head_radius_m: 0.3\n",
                "head_radius_m",
            ),
            (
                "render:\n  binaural:\n    brir_max_length_s: 11\n",
                "brir_max_length_s",
            ),
            (
                "render:\n  binaural:\n    ear_gains: [5.0, 1.0]\n",
                "ear_gains",
            ),
            (
                "render:\n  binaural:\n    ear_gains: [1.0, 1.0, 1.0]\n",
                "ear_gains",
            ),
            ("render:\n  sample_ramp_stride: 64\n", "sample_ramp_stride"),
            ("render:\n  drc_weight: 1.5\n", "drc_weight"),
            // Options registered by hand rather than declared, which the
            // renderer would clamp at start rather than refuse.
            (
                "render:\n  room_ratio_center_blend: 2.0\n  auto_gain: false\n",
                "room_ratio_center_blend",
            ),
            ("render:\n  room_ratio_rear: 0\n", "room_ratio_rear"),
            (
                "render:\n  hybrid_curve_smoothing: 1.5\n",
                "hybrid_curve_smoothing",
            ),
            (
                "render:\n  evaluation_cartesian_x_size: 0\n",
                "evaluation_cartesian_x_size",
            ),
            ("render:\n  master_gain: 70\n", "master_gain"),
            ("render:\n  binaural:\n    unit_scale_m: .nan\n", "finite"),
            ("render:\n  master_gain: .inf\n", "finite"),
        ] {
            let why = rejected(config);
            assert!(why.contains(what), "{config}: {why}");
        }
        // At the bounds is within them.
        assert_eq!(
            check_config(
                "render:\n  binaural:\n    reverb:\n      rt60_s: 3.0\n    ear_gains: [0, 4]\n"
            ),
            Ok(())
        );
        assert_eq!(
            check_config("render:\n  room_ratio_center_blend: 1.0\n  master_gain: -12\n"),
            Ok(())
        );
    }

    /// A source that takes its file from a key needs the key, and the files
    /// named are there.
    #[test]
    fn a_source_needs_its_file_and_the_file_is_there() {
        assert!(
            rejected("render:\n  binaural:\n    hrir_source: sofa\n").contains("hrtf_sofa_path")
        );
        assert!(
            rejected("render:\n  binaural:\n    hrir_source: brir\n").contains("brir_sofa_path")
        );
        assert!(
            rejected("render:\n  binaural:\n    hrtf_sofa_path: /nonexistent/x.sofa\n")
                .contains("/nonexistent/x.sofa")
        );
        let dir = format!("{}/tests/sofa", env!("CARGO_MANIFEST_DIR"));
        assert!(
            rejected(&format!(
                "render:\n  binaural:\n    brir_sofa_path: '{dir}'\n"
            ))
            .contains("not a file")
        );
        let room = fixture("rows_multispeaker_brir.sofa");
        assert_eq!(
            check_config(&format!(
                "render:\n  binaural:\n    hrir_source: brir\n    brir_sofa_path: '{room}'\n"
            )),
            Ok(())
        );
        // A selector carrying its own file needs no key.
        assert_eq!(
            check_config("render:\n  binaural:\n    hrir_source: saf\n"),
            Ok(())
        );
    }

    #[test]
    fn a_file_too_large_is_not_read() {
        let dir = std::env::temp_dir().join(format!("config-check-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("big.yaml");
        std::fs::write(&big, "#".repeat(MAX_CONFIG_BYTES as usize + 1)).unwrap();
        assert!(check_config_file(&big).unwrap_err().contains("bytes"));
        assert!(
            check_config_file(&dir.join("missing.yaml"))
                .unwrap_err()
                .contains("cannot read")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The bounds table names options the registry holds, with bounds.
    #[test]
    fn every_binaural_bound_is_a_registry_option_with_a_range() {
        for (path, option) in BINAURAL_BOUNDS {
            let spec = options::find(option).unwrap_or_else(|| panic!("{option}"));
            assert!(
                matches!(
                    spec.kind,
                    OptionKind::Float { .. } | OptionKind::FloatArray { .. }
                ),
                "{path:?} → {option}: {:?}",
                spec.kind
            );
        }
    }
}
