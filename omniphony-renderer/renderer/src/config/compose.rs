//! A host's generated config, overridden by a patch its user owns.
//!
//! A host that writes the renderer's config itself (a player can, from its
//! own settings, for every stream) can let an advanced user replace any of it
//! with a patch: a partial `config.yaml` whose values win over the host's.
//! The patch is the user's file; nothing here ever writes it.
//!
//! The patch's rules:
//!
//! * `null` inherits: a key set to `null` is as if absent, so a template
//!   with every key `null` changes nothing. `false`, `0`, `""` and `[]` are
//!   values. A mapping merges key by key; anything else replaces what the
//!   host wrote, a sequence whole.
//! * It is accepted whole or not at all. A key this build does not know, a
//!   value of the wrong type or out of its range, a key the host owns, or a
//!   file it points at that is not there rejects the whole patch, with the
//!   reason, and the host's config is used as it is.
//! * Keys the host owns are refused rather than ignored: the decoder, the
//!   input and output, OSC, and keys only the desktop player reads (see
//!   [`render_key`]).
//! * Relative paths start in the patch's own directory.
//!
//! [`compose`] does the work on text; [`compose_files`] reads the files and
//! bounds the patch's size first.

use std::path::{Path, PathBuf};

use serde_yaml_ng::{Mapping, Value};

use super::{CONFIG_SCHEMA_VERSION, Config};
use crate::binaural::HrirSource;
use crate::options::{self, OptionKind};

/// Largest patch read, bytes: a template with every key documented is a
/// few dozen kilobytes.
pub const MAX_PATCH_BYTES: u64 = 256 * 1024;

/// Largest SOFA file a patch may point a session at directly. Past it a room
/// is prepared ([`crate::binaural::brir::prepare_room`]) rather than parsed
/// at every session start.
pub const MAX_RAW_ROOM_BYTES: u64 = 64 * 1024 * 1024;

/// What became of a patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeStatus {
    /// No patch, or one that sets nothing: the host's config applies as is.
    None,
    /// The patch applies: [`ComposeReport::effective`] holds the result.
    Applied,
    /// The patch was refused whole ([`ComposeReport::reason`] says why): the
    /// host's config applies as is.
    Rejected,
}

impl ComposeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Applied => "applied",
            Self::Rejected => "rejected",
        }
    }
}

/// The outcome of [`compose`].
#[derive(Debug, Clone, PartialEq)]
pub struct ComposeReport {
    pub status: ComposeStatus,
    /// Values the patch sets (a sequence counts once).
    pub keys: usize,
    /// The patch sets `render.current_layout`: a host that passes a layout
    /// of its own beside the config must not, or it would win.
    pub layout_set: bool,
    /// The patch sets `render.decode_thread`: a host that picks the decode
    /// thread itself hands the choice to the option instead.
    pub decode_thread_set: bool,
    /// Why the patch was rejected.
    pub reason: Option<String>,
    /// The composed config, YAML, when the patch applies.
    pub effective: Option<String>,
}

impl ComposeReport {
    fn none() -> Self {
        Self {
            status: ComposeStatus::None,
            keys: 0,
            layout_set: false,
            decode_thread_set: false,
            reason: None,
            effective: None,
        }
    }

    fn rejected(reason: impl Into<String>) -> Self {
        Self {
            status: ComposeStatus::Rejected,
            reason: Some(reason.into()),
            ..Self::none()
        }
    }

    /// One line a host can log or forward: `status=… keys=… layout_set=0|1
    /// decode_thread_set=0|1`, then `reason=…` to the end of the line when
    /// rejected.
    pub fn line(&self) -> String {
        let mut line = format!(
            "status={} keys={} layout_set={} decode_thread_set={}",
            self.status.as_str(),
            self.keys,
            u8::from(self.layout_set),
            u8::from(self.decode_thread_set)
        );
        if let Some(reason) = &self.reason {
            line.push_str(" reason=");
            line.push_str(&reason.replace(['\n', '\r'], " "));
        }
        line
    }
}

/// Where a `render` key stands in a host that generates the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderKey {
    /// A rendering control: the patch may set it.
    Allowed,
    /// Refused, for the reason given.
    Refused(&'static str),
}

const HOST_OWNED: &str =
    "is the host's (the decoder, the input or output, or OSC), not a rendering control";
const DESKTOP_ONLY: &str = "is read by the desktop player only and does nothing here";
const LEGACY: &str = "is a legacy key read for migration only";

/// What a patch may do with the `render` key `key`. Every key of the
/// schema is listed (a test checks it): one added upstream is refused until
/// it is reviewed here.
pub fn render_key(key: &str) -> Option<RenderKey> {
    use RenderKey::{Allowed, Refused};
    Some(match key {
        // The headphone stage, checked key by key in `check_binaural`.
        "binaural" => Allowed,
        // Gain, placement, layout and channel policy.
        "master_gain" | "placement" | "current_layout" | "channel_render_mode" => Allowed,
        // Panning backends and plugin parameters.
        "render_backend"
        | "backend_params"
        | "generator_params"
        | "phantom_extract_params"
        | "hybrid_external_backend"
        | "hybrid_internal_backend"
        | "hybrid_curve"
        | "hybrid_curve_smoothing"
        | "hybrid_metric"
        | "barycenter_localize" => Allowed,
        // Room warp of the virtual array.
        "room_width_m"
        | "room_front_m"
        | "room_rear_m"
        | "room_height_m"
        | "room_lower_m"
        | "room_ratio"
        | "room_ratio_rear"
        | "room_ratio_lower"
        | "room_ratio_center_blend" => Allowed,
        // Panning, evaluation, spread and distance models.
        "vbap_azimuth_resolution"
        | "vbap_elevation_resolution"
        | "vbap_spread"
        | "vbap_distance_res"
        | "vbap_distance_max"
        | "vbap_allow_negative_z"
        | "vbap_distance_model"
        | "vbap_spread_min"
        | "vbap_spread_max"
        | "render_evaluation_mode"
        | "evaluation_grid"
        | "render_evaluation_position_interpolation"
        | "evaluation_cartesian_x_size"
        | "evaluation_cartesian_y_size"
        | "evaluation_cartesian_z_size"
        | "evaluation_cartesian_z_neg_size"
        | "evaluation_object_size_intervals"
        | "spread_from_distance"
        | "spread_distance_range"
        | "spread_distance_curve"
        | "size_to_spread_mode"
        | "distance_diffuse"
        | "distance_diffuse_threshold"
        | "distance_diffuse_curve"
        | "distance_model_metric"
        | "distance_diffuse_metric"
        | "distance_diffuse_mirror_axes"
        | "experimental_distance_distance_floor"
        | "experimental_distance_min_active_speakers"
        | "experimental_distance_max_active_speakers"
        | "experimental_distance_position_error_floor"
        | "experimental_distance_position_error_nearest_scale"
        | "experimental_distance_position_error_span_scale" => Allowed,
        // Declared options (`options::declared`), all rendering controls but
        // the speaker output's channel wiring.
        "surround_placement"
        | "synthetic_objects_enabled"
        | "decode_thread"
        | "object_generator_id"
        | "phantom_extract_mode"
        | "crossover_type"
        | "crossover_fir_transition_ratio"
        | "auto_gain"
        | "auto_gain_ceiling_db"
        | "use_loudness"
        | "ramp_mode"
        | "sample_ramp_stride"
        | "drc_mode"
        | "drc_weight"
        | "dialogue_gain_db" => Allowed,
        "output_channel_mapping" => Refused(HOST_OWNED),
        // Input, output, transport and control surface.
        "input_mode"
        | "input_pipe"
        | "live_input"
        | "output_backend"
        | "output_file"
        | "output_file_format"
        | "output_device"
        | "output_sample_rate"
        | "latency_target"
        | "bridge_path"
        | "bridge_paths"
        | "osc"
        | "osc_metering"
        | "osc_rx_port"
        | "osc_host"
        | "osc_port"
        | "meter_rate"
        | "diag_rate"
        | "enable_adaptive_resampling" => Refused(HOST_OWNED),
        k if k.starts_with("adaptive_resampling_") => Refused(HOST_OWNED),
        "presentation" => Refused("is chosen by the decoder bridge in this host"),
        "speaker_layout" => Refused(
            "(a layout file) is read by the desktop player only; set current_layout instead",
        ),
        "enable_vbap" | "continuous" | "bed_conform" => Refused(DESKTOP_ONLY),
        "vbap_table" => Refused("names a precomputed table file, which this host does not use"),
        "virtual_bed" | "object_generator_params" | "phantom_enabled" | "phantom_params" => {
            Refused(LEGACY)
        }
        _ => return None,
    })
}

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

/// `render.master_gain` is decibels in the file; the registry's bound is the
/// linear gain's (1000).
const MAX_MASTER_GAIN_DB: f64 = 60.0;

/// Compose the files a host has: its generated config at `base`, the user's
/// patch at `patch` (absent = no patch), relative paths resolved from
/// `patch_dir` (the patch's directory when `None`).
pub fn compose_files(base: &Path, patch: &Path, patch_dir: Option<&Path>) -> ComposeReport {
    let size = match std::fs::metadata(patch) {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ComposeReport::none(),
        Err(e) => return ComposeReport::rejected(format!("cannot read the patch: {e}")),
    };
    if size > MAX_PATCH_BYTES {
        return ComposeReport::rejected(format!(
            "the patch is {size} bytes, more than the {MAX_PATCH_BYTES} read"
        ));
    }
    let patch_text = match std::fs::read_to_string(patch) {
        Ok(text) => text,
        Err(e) => return ComposeReport::rejected(format!("cannot read the patch: {e}")),
    };
    let base_text = match std::fs::read_to_string(base) {
        Ok(text) => text,
        Err(e) => {
            return ComposeReport::rejected(format!(
                "cannot read the base config {}: {e}",
                base.display()
            ));
        }
    };
    let dir = patch_dir
        .map(Path::to_path_buf)
        .or_else(|| patch.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    compose(&base_text, &patch_text, &dir)
}

/// Compose the host's config `base` (YAML) with the user's `patch` (YAML),
/// resolving the patch's relative paths from `patch_dir`.
pub fn compose(base: &str, patch: &str, patch_dir: &Path) -> ComposeReport {
    match try_compose(base, patch, patch_dir) {
        Ok(report) => report,
        Err(reason) => ComposeReport::rejected(reason),
    }
}

fn try_compose(base: &str, patch: &str, patch_dir: &Path) -> Result<ComposeReport, String> {
    if patch.len() as u64 > MAX_PATCH_BYTES {
        return Err(format!(
            "the patch is {} bytes, more than the {MAX_PATCH_BYTES} read",
            patch.len()
        ));
    }
    let patch = parse(patch).map_err(|e| format!("the patch is not valid YAML: {e}"))?;
    let mut render = match patch {
        Value::Null => return Ok(ComposeReport::none()),
        Value::Mapping(root) => patch_render(root)?,
        _ => return Err("the patch must be a mapping with a `render:` section".into()),
    };
    if render.is_empty() {
        return Ok(ComposeReport::none());
    }
    check_values(&Value::Mapping(render.clone()), "render")?;
    for (key, value) in &render {
        let key = key_str(key, "render")?;
        match render_key(key) {
            Some(RenderKey::Allowed) => {}
            Some(RenderKey::Refused(why)) => return Err(format!("render.{key} {why}")),
            None => return Err(format!("render.{key} is not a key this renderer knows")),
        }
        if key == "binaural" {
            check_binaural(value)?;
        }
    }
    resolve_paths(&mut render, patch_dir)?;
    check_bounds(&render)?;
    check_choices(&render)?;

    let report = ComposeReport {
        status: ComposeStatus::Applied,
        keys: count_values(&Value::Mapping(render.clone())),
        layout_set: render.contains_key("current_layout"),
        decode_thread_set: render.contains_key("decode_thread"),
        reason: None,
        effective: None,
    };

    let mut effective = match parse(base).map_err(|e| format!("the base config: {e}"))? {
        Value::Mapping(m) => m,
        Value::Null => Mapping::new(),
        _ => return Err("the base config is not a mapping".into()),
    };
    let target = effective
        .entry(Value::from("render"))
        .or_insert_with(|| Value::Mapping(Mapping::new()));
    if !target.is_mapping() {
        *target = Value::Mapping(Mapping::new());
    }
    merge(target, Value::Mapping(render.clone()));
    let effective = Value::Mapping(effective);

    let config: Config = serde_yaml_ng::from_value(effective.clone())
        .map_err(|e| format!("the patched config does not read: {e}"))?;
    check_known(&config)?;
    check_effective(&config, &render)?;

    let text = serde_yaml_ng::to_string(&effective).map_err(|e| e.to_string())?;
    Ok(ComposeReport {
        effective: Some(format!(
            "# Composed by the renderer from the host's config and the user's patch.\n\
             # Read-only: edit the patch, not this file.\n{text}"
        )),
        ..report
    })
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

fn key_str<'a>(key: &'a Value, at: &str) -> Result<&'a str, String> {
    key.as_str()
        .ok_or_else(|| format!("{at}: a key that is not a string ({key:?})"))
}

/// The patch's `render` section with its inherited (`null`) values taken
/// out, after checking the root holds nothing else.
fn patch_render(root: Mapping) -> Result<Mapping, String> {
    let mut render = Mapping::new();
    for (key, value) in root {
        match key_str(&key, "the patch")? {
            "schema_version" => match &value {
                Value::Null => {}
                Value::Number(n)
                    if n.as_u64()
                        .is_some_and(|v| v <= u64::from(CONFIG_SCHEMA_VERSION)) => {}
                _ => {
                    return Err(format!(
                        "schema_version {value:?}: this renderer reads up to {CONFIG_SCHEMA_VERSION}"
                    ));
                }
            },
            "render" => match value {
                Value::Null => {}
                Value::Mapping(m) => render = without_nulls(m),
                _ => return Err("render: must be a mapping".into()),
            },
            other => {
                return Err(format!(
                    "{other}: a patch holds `render:` (and `schema_version:`) only"
                ));
            }
        }
    }
    Ok(render)
}

/// `map` without its `null` values, recursively, and without the mappings
/// that leaves empty. Sequences are values: kept whole.
fn without_nulls(map: Mapping) -> Mapping {
    map.into_iter()
        .filter_map(|(k, v)| match v {
            Value::Null => None,
            Value::Mapping(m) => {
                let m = without_nulls(m);
                (!m.is_empty()).then_some((k, Value::Mapping(m)))
            }
            other => Some((k, other)),
        })
        .collect()
}

/// Values a patch sets: every leaf, a sequence once.
fn count_values(value: &Value) -> usize {
    match value {
        Value::Mapping(m) => m.values().map(count_values).sum(),
        _ => 1,
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

/// What a patch may set under `render.binaural`.
fn check_binaural(value: &Value) -> Result<(), String> {
    let Value::Mapping(bin) = value else {
        return Err("render.binaural must be a mapping".into());
    };
    for (key, value) in bin {
        let key = key_str(key, "render.binaural")?;
        match key {
            "output_mode" => {
                let binaural = value
                    .as_str()
                    .and_then(crate::live_params::OutputMode::from_str)
                    == Some(crate::live_params::OutputMode::Binaural);
                if !binaural {
                    return Err(
                        "render.binaural.output_mode: this host renders to headphones only \
                         (binaural)"
                            .into(),
                    );
                }
            }
            "hrir_source" => {
                let text = value
                    .as_str()
                    .ok_or("render.binaural.hrir_source must be a string")?;
                let lower = text.trim().to_ascii_lowercase();
                if lower.starts_with("sofa:") || lower.starts_with("brir:") {
                    return Err(format!(
                        "render.binaural.hrir_source {text:?}: name the file in \
                         hrtf_sofa_path or brir_sofa_path"
                    ));
                }
                if HrirSource::from_str(text).is_none() {
                    return Err(format!(
                        "render.binaural.hrir_source {text:?} is not a source this renderer knows"
                    ));
                }
            }
            "brir_head_tracking" => {
                if value.as_bool() == Some(true) {
                    return Err(
                        "render.binaural.brir_head_tracking: this host has no head tracker, \
                         and every orientation of a room would be kept in memory"
                            .into(),
                    );
                }
            }
            "head_tracking" => {
                return Err("render.binaural.head_tracking: this host has no head tracker".into());
            }
            "hrtf_grid_cache" => {
                return Err(
                    "render.binaural.hrtf_grid_cache: where the HRIR grid is kept is the host's"
                        .into(),
                );
            }
            "mode"
            | "ear_gains"
            | "ear_mutes"
            | "unit_scale_m"
            | "head_radius_m"
            | "hrtf_sofa_path"
            | "brir_sofa_path"
            | "brir_max_length_s"
            | "brir_tail_floor_db"
            | "reflections"
            | "reverb"
            | "air_absorption"
            | "diffuse_field_eq"
            | "hrir_update_lattice" => {}
            other => {
                return Err(format!(
                    "render.binaural.{other} is not a key this renderer knows"
                ));
            }
        }
    }
    Ok(())
}

/// Resolve the patch's file paths from `dir`, and check the files are there.
fn resolve_paths(render: &mut Mapping, dir: &Path) -> Result<(), String> {
    let Some(Value::Mapping(bin)) = render.get_mut("binaural") else {
        return Ok(());
    };
    for (key, room) in [("hrtf_sofa_path", false), ("brir_sofa_path", true)] {
        let Some(value) = bin.get_mut(key) else {
            continue;
        };
        let text = value
            .as_str()
            .ok_or_else(|| format!("render.binaural.{key} must be a path"))?;
        if text.contains("://") {
            return Err(format!(
                "render.binaural.{key} {text:?}: a local path is needed (choose network \
                 files through the host)"
            ));
        }
        let path = PathBuf::from(text);
        let path = if path.is_absolute() {
            path
        } else {
            dir.join(path)
        };
        let meta = std::fs::metadata(&path)
            .map_err(|e| format!("render.binaural.{key}: {}: {e}", path.display()))?;
        if !meta.is_file() {
            return Err(format!(
                "render.binaural.{key}: {} is not a file",
                path.display()
            ));
        }
        if room && meta.len() > MAX_RAW_ROOM_BYTES {
            let prepared = crate::binaural::brir::prepared_room_emitters(&path)
                .map_err(|e| format!("render.binaural.{key}: {}: {e:#}", path.display()))?;
            if prepared.is_none() {
                return Err(format!(
                    "render.binaural.{key}: {} is a {} MB SOFA file; prepare it first \
                     (choose it as the room in the host) so it is not read at every start",
                    path.display(),
                    meta.len() / 1_000_000
                ));
            }
        }
        *value = Value::from(path.to_string_lossy().into_owned());
    }
    Ok(())
}

/// Numbers within the registry's bounds, where the registry has one for
/// the key: the declared options, the binaural keys of [`BINAURAL_BOUNDS`],
/// and `master_gain`.
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
        } else if options::DECLARED_KEYS.contains(&key)
            && let Some(spec) = options::find(key)
        {
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

/// The registry's choices under `render.binaural`, by path: checked as the
/// ones directly under `render` are.
const BINAURAL_CHOICES: &[(&str, &str)] = &[
    ("mode", "binaural_mode"),
    ("hrir_update_lattice", "hrir_update_lattice"),
];

/// Choices the renderer takes: where the registry has a closed set for the
/// key, or the backends, the value must be one its own setter takes - the
/// parser the option is seeded with - so a value the renderer would pass
/// over at start is refused here, not reported as applied. Plugin backends
/// are not known here: only the built-in ones are taken.
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

/// Mappings merge key by key; anything else replaces.
fn merge(into: &mut Value, patch: Value) {
    match (into, patch) {
        (Value::Mapping(into), Value::Mapping(patch)) => {
            for (key, value) in patch {
                match into.get_mut(&key) {
                    Some(slot) if slot.is_mapping() && value.is_mapping() => merge(slot, value),
                    _ => {
                        into.insert(key, value);
                    }
                }
            }
        }
        (into, patch) => *into = patch,
    }
}

/// Nothing the config kept for a later build: a key or an enum value this
/// build does not know lands in a section's `extra`.
fn check_known(config: &Config) -> Result<(), String> {
    let unknown = |section: &str, extra: &Mapping| -> Result<(), String> {
        match extra.keys().next() {
            None => Ok(()),
            Some(key) => Err(format!(
                "{section}.{} is not a key (or value) this renderer knows",
                key.as_str().unwrap_or("?")
            )),
        }
    };
    unknown("the patch", &config.extra)?;
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

/// Rules across keys, on the composed config.
fn check_effective(config: &Config, patch: &Mapping) -> Result<(), String> {
    let Some(render) = &config.render else {
        return Ok(());
    };
    if render.channel_render_mode == Some(crate::live_params::ChannelRenderMode::Host) {
        return Err(
            "render.channel_render_mode: host hands channel-based content back to the host, \
             which this host does not take; spatial renders it"
                .into(),
        );
    }
    let Some(bin) = &render.binaural else {
        return Ok(());
    };
    let selector = bin.hrir_source.as_deref().and_then(HrirSource::from_str);
    match selector {
        Some(HrirSource::Sofa(p)) if p.is_empty() && bin.hrtf_sofa_path.is_none() => {
            return Err("render.binaural.hrir_source sofa needs hrtf_sofa_path".into());
        }
        Some(HrirSource::Brir(p)) if p.is_empty() && bin.brir_sofa_path.is_none() => {
            return Err("render.binaural.hrir_source brir needs brir_sofa_path".into());
        }
        _ => {}
    }
    if matches!(bin.effective_hrir_source(), Some(HrirSource::Brir(_)))
        && patch.contains_key("current_layout")
    {
        return Err(
            "render.current_layout: a room renders on its own measured loudspeakers".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a host generates (abridged): the keys a patch merges into.
    const BASE: &str = "\
render:
  bridge_path: \"/usr/lib/omniphony/libharletty_dolby_bridge.so\"
  master_gain: -12.50
  placement:
    generic:
      layout:
        speakers:
          - { name: LFE, coord_mode: cartesian, x: 0, y: 1, z: 0, spatialize: false, gain_db: 0.0 }
          - { name: LFE2, coord_mode: cartesian, x: 0, y: 1, z: 0, spatialize: false, gain_db: 0.0 }
  auto_gain: false
  binaural:
    output_mode: binaural
    hrir_source: saf
    unit_scale_m: 2.00
    head_radius_m: 0.0875
    air_absorption: true
    diffuse_field_eq: true
    reflections: { enabled: true, room_width_m: 4.00, room_depth_m: 5.00,
                   room_height_m: 2.70, level: 0.5 }
    reverb: { enabled: true, level: 0.10, rt60_s: 0.35, predelay_ms: 20 }
";

    fn dir() -> PathBuf {
        std::env::temp_dir()
    }

    fn applied(patch: &str) -> (ComposeReport, Config) {
        let report = compose(BASE, patch, &dir());
        assert_eq!(report.status, ComposeStatus::Applied, "{report:?}");
        let text = report.effective.clone().expect("effective config");
        let config: Config = serde_yaml_ng::from_str(&text).expect("the effective config reads");
        (report, config)
    }

    fn rejected(patch: &str) -> String {
        let report = compose(BASE, patch, &dir());
        assert_eq!(
            report.status,
            ComposeStatus::Rejected,
            "{patch}\n{report:?}"
        );
        assert!(report.effective.is_none());
        report.reason.unwrap()
    }

    fn binaural(config: &Config) -> &crate::config::BinauralConfig {
        config.render.as_ref().unwrap().binaural.as_ref().unwrap()
    }

    #[test]
    fn nothing_set_is_no_patch() {
        for patch in [
            "",
            "\n\n",
            "## only comments\n# and more\n",
            "render: null\n",
            "render:\n  binaural:\n    hrir_source: null\n    reverb:\n      level: null\n",
            "schema_version: null\nrender:\n  master_gain: null\n  placement:\n    generic:\n      mode: null\n",
            "render: {}\n",
        ] {
            let report = compose(BASE, patch, &dir());
            assert_eq!(report.status, ComposeStatus::None, "{patch:?}: {report:?}");
            assert_eq!(report.keys, 0);
            assert!(report.effective.is_none());
        }
    }

    #[test]
    fn a_patch_replaces_what_it_sets_and_inherits_the_rest() {
        let (report, config) = applied(
            "render:\n  binaural:\n    reverb:\n      level: 0.25\n      enabled: null\n    \
             reflections:\n      enabled: false\n    air_absorption: false\n    ear_gains: [0.8, 1.0]\n  \
             auto_gain_ceiling_db: 0\n",
        );
        assert_eq!(report.keys, 5);
        assert!(!report.layout_set && !report.decode_thread_set);
        let bin = binaural(&config);
        let reverb = bin.reverb.as_ref().unwrap();
        assert_eq!(reverb.level, Some(0.25));
        assert_eq!(
            reverb.enabled,
            Some(true),
            "a null inherits the host's value"
        );
        assert_eq!(reverb.rt60_s, Some(0.35), "an unset sibling is the host's");
        let reflections = bin.reflections.as_ref().unwrap();
        assert_eq!(reflections.enabled, Some(false), "false is a value");
        assert_eq!(reflections.room_width_m, Some(4.0));
        assert_eq!(bin.air_absorption, Some(false));
        assert_eq!(bin.ear_gains, Some([0.8, 1.0]));
        assert_eq!(bin.unit_scale_m, Some(2.0));
        let render = config.render.as_ref().unwrap();
        assert_eq!(
            render.options.auto_gain_ceiling_db,
            Some(0.0),
            "zero is a value"
        );
        assert_eq!(render.master_gain, Some(-12.5));
        assert_eq!(
            render.bridge_path.as_deref(),
            Some(Path::new("/usr/lib/omniphony/libharletty_dolby_bridge.so")),
            "the host's own keys stay"
        );
    }

    /// A sequence is replaced whole, never merged by position.
    #[test]
    fn a_sequence_is_replaced_whole() {
        let (_, config) = applied(
            "render:\n  placement:\n    generic:\n      layout:\n        speakers:\n          \
             - { name: LFE, coord_mode: cartesian, x: 0, y: 1, z: 0, spatialize: false, gain_db: 3.0 }\n",
        );
        let placement = config.render.unwrap().placement.unwrap();
        let generic = placement.get("generic").unwrap();
        let speakers = generic.layout.as_ref().unwrap().speaker_names();
        assert_eq!(
            speakers,
            ["LFE"],
            "LFE2 is not carried over from the host's list"
        );
    }

    #[test]
    fn the_layout_and_decode_thread_are_reported() {
        let (report, _) = applied(
            "render:\n  decode_thread: false\n  current_layout:\n    name: two\n    speakers:\n      \
             - { name: FL, coord_mode: polar, azimuth: -30.0, elevation: 0.0 }\n      \
             - { name: FR, coord_mode: polar, azimuth: 30.0, elevation: 0.0 }\n",
        );
        assert!(report.layout_set && report.decode_thread_set);
        assert_eq!(
            report.line(),
            format!(
                "status=applied keys={} layout_set=1 decode_thread_set=1",
                report.keys
            )
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
        assert!(rejected("render:\n  1: true\n").contains("not a string"));
        assert!(rejected("- render\n").contains("mapping"));
        assert!(rejected("render: 3\n").contains("mapping"));
        assert!(rejected("profiles: {}\n").contains("render"));
        assert!(rejected("active_profile: x\n").contains("render"));
        assert!(rejected("schema_version: 2\n").contains("schema_version"));
        assert_eq!(
            compose(
                BASE,
                "schema_version: 1\nrender:\n  auto_gain: true\n",
                &dir()
            )
            .status,
            ComposeStatus::Applied
        );
    }

    /// A key or a value this build does not know is refused, never kept for
    /// later as a config file's would be, whatever the depth.
    #[test]
    fn what_this_build_does_not_know_is_rejected() {
        assert!(rejected("render:\n  reverbb: 0.2\n").contains("render.reverbb"));
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

    /// A choice the renderer would pass over at start is refused, with the
    /// rest of the patch: never a gain applied beside an evaluation mode
    /// silently left at its default.
    #[test]
    fn a_choice_the_renderer_does_not_take_is_rejected() {
        for (patch, what) in [
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
            assert!(rejected(patch).contains(what), "{patch}");
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
        for (patch, what) in [
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
            ("render:\n  master_gain: 70\n", "master_gain"),
            ("render:\n  binaural:\n    unit_scale_m: .nan\n", "finite"),
            ("render:\n  master_gain: .inf\n", "finite"),
        ] {
            let why = rejected(patch);
            assert!(why.contains(what), "{patch}: {why}");
        }
        // At the bounds is within them.
        applied("render:\n  binaural:\n    reverb:\n      rt60_s: 3.0\n    ear_gains: [0, 4]\n");
    }

    #[test]
    fn keys_the_host_owns_are_rejected_with_the_reason() {
        for (patch, what) in [
            ("render:\n  bridge_path: /x.so\n", "host"),
            ("render:\n  osc: true\n", "host"),
            ("render:\n  osc_rx_port: 9000\n", "host"),
            ("render:\n  output_sample_rate: 96000\n", "host"),
            ("render:\n  adaptive_resampling_kp_near: 1.0\n", "host"),
            ("render:\n  output_channel_mapping: wave\n", "host"),
            ("render:\n  presentation: 1\n", "bridge"),
            ("render:\n  speaker_layout: my.yaml\n", "current_layout"),
            ("render:\n  enable_vbap: false\n", "desktop"),
            ("render:\n  virtual_bed: { name: old }\n", "legacy"),
            (
                "render:\n  binaural:\n    output_mode: speaker\n",
                "headphones",
            ),
            ("render:\n  channel_render_mode: host\n", "spatial"),
            (
                "render:\n  binaural:\n    brir_head_tracking: true\n",
                "head tracker",
            ),
            (
                "render:\n  binaural:\n    head_tracking:\n      osc_address: /x\n",
                "head tracker",
            ),
            ("global:\n  loglevel: debug\n", "render"),
            (
                "render:\n  binaural:\n    hrtf_grid_cache:\n      path: /tmp/g\n      \
                 sample_rate: 48000\n      diffuse_field_eq: true\n",
                "host's",
            ),
        ] {
            let why = rejected(patch);
            assert!(why.contains(what), "{patch}: {why}");
        }
        // What the host would write anyway is no conflict.
        applied(
            "render:\n  binaural:\n    output_mode: binaural\n    brir_head_tracking: false\n  \
                 channel_render_mode: spatial\n",
        );
    }

    #[test]
    fn a_response_needs_its_file_and_paths_start_in_the_patchs_directory() {
        let dir = std::env::temp_dir().join(format!("compose-paths-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("rooms")).unwrap();
        std::fs::write(dir.join("rooms/my-room.sofa"), b"stand-in").unwrap();
        std::fs::write(dir.join("my-hrtf.sofa"), b"stand-in").unwrap();

        let report = compose(
            BASE,
            "render:\n  binaural:\n    hrir_source: brir\n    brir_sofa_path: \"rooms/my-room.sofa\"\n",
            &dir,
        );
        assert_eq!(report.status, ComposeStatus::Applied, "{report:?}");
        let config: Config = serde_yaml_ng::from_str(report.effective.as_deref().unwrap()).unwrap();
        assert_eq!(
            binaural(&config).brir_sofa_path.as_deref(),
            Some(dir.join("rooms/my-room.sofa").as_path()),
            "resolved from the patch's directory"
        );
        let absolute = format!(
            "render:\n  binaural:\n    hrir_source: sofa\n    hrtf_sofa_path: '{}'\n",
            dir.join("my-hrtf.sofa").display()
        );
        assert_eq!(
            compose(BASE, &absolute, Path::new("/")).status,
            ComposeStatus::Applied
        );

        let reason = |patch: &str| compose(BASE, patch, &dir).reason.unwrap_or_default();
        assert!(reason("render:\n  binaural:\n    hrir_source: brir\n").contains("brir_sofa_path"));
        assert!(reason("render:\n  binaural:\n    hrir_source: sofa\n").contains("hrtf_sofa_path"));
        assert!(
            reason("render:\n  binaural:\n    brir_sofa_path: missing.sofa\n")
                .contains("missing.sofa")
        );
        assert!(reason("render:\n  binaural:\n    brir_sofa_path: rooms\n").contains("not a file"));
        assert!(
            reason("render:\n  binaural:\n    hrtf_sofa_path: \"special://profile/x.sofa\"\n")
                .contains("local path")
        );
        assert!(
            reason("render:\n  binaural:\n    hrir_source: \"brir:rooms/my-room.sofa\"\n")
                .contains("brir_sofa_path")
        );
        let layout = "render:\n  binaural:\n    hrir_source: brir\n    brir_sofa_path: rooms/my-room.sofa\n  \
                      current_layout:\n    name: two\n    speakers:\n      \
                      - { name: FL, coord_mode: polar, azimuth: -30.0, elevation: 0.0 }\n";
        assert!(reason(layout).contains("measured loudspeakers"));

        // A large SOFA file must be prepared first; a prepared one of any size
        // is taken.
        let big = dir.join("big.sofa");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_RAW_ROOM_BYTES + 1)
            .unwrap();
        assert!(
            reason("render:\n  binaural:\n    brir_sofa_path: big.sofa\n").contains("prepare it")
        );
        let mut prepared = crate::binaural::brir::PREPARED_ROOM_MAGIC.to_vec();
        prepared.extend_from_slice(&1u32.to_le_bytes()); // version
        prepared.extend_from_slice(&48_000u32.to_le_bytes());
        prepared.extend_from_slice(&1u32.to_le_bytes()); // one emitter
        prepared.extend_from_slice(&1u32.to_le_bytes()); // one orientation
        prepared.extend_from_slice(&0u32.to_le_bytes()); // no conventions text
        for v in [0.0f32, 2.0, 0.0] {
            prepared.extend_from_slice(&v.to_le_bytes());
        }
        let big_room = dir.join("big.room");
        std::fs::write(&big_room, &prepared).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&big_room)
            .unwrap()
            .set_len(MAX_RAW_ROOM_BYTES + 1)
            .unwrap();
        assert_eq!(
            compose(
                BASE,
                "render:\n  binaural:\n    brir_sofa_path: big.room\n",
                &dir
            )
            .status,
            ComposeStatus::Applied
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_patch_too_large_is_not_read() {
        let mut patch = String::from("render:\n  auto_gain: true\n");
        while patch.len() as u64 <= MAX_PATCH_BYTES {
            patch.push_str("## padding padding padding padding padding padding padding\n");
        }
        assert!(rejected(&patch).contains("bytes"));
    }

    /// Every key of `render` this build declares is classified: one added
    /// upstream fails here until it is reviewed in `render_key`.
    #[test]
    fn every_render_key_is_classified() {
        let source = include_str!("../config.rs");
        let start = source
            .find("pub struct RenderConfig {")
            .expect("RenderConfig");
        let body = &source[start..];
        let body = &body[..body.find("\n}\n").expect("end of RenderConfig")];
        let fields: Vec<&str> = body
            .lines()
            .filter_map(|l| l.trim().strip_prefix("pub "))
            .filter_map(|l| l.split(':').next())
            .filter(|name| !matches!(*name, "struct RenderConfig {" | "options" | "extra"))
            .collect();
        assert!(fields.len() > 100, "{} fields read", fields.len());
        for field in fields
            .iter()
            .copied()
            .chain(options::DECLARED_KEYS.iter().copied())
        {
            assert!(
                render_key(field).is_some(),
                "render.{field} is not classified"
            );
        }
        assert_eq!(render_key("not_a_key"), None);
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
