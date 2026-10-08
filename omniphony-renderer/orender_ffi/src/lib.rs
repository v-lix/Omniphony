//! C ABI for the `orender` spatial audio renderer — built as `liborender.so`.
//!
//! A thin, panic-safe shim over [`orender_engine::Engine`]: the host (mpv via
//! `ad_orender.c`, or any C program) creates a session from a config, pushes
//! raw encoded packets, and receives interleaved multichannel `f32` PCM. No
//! audio output happens here — the host owns that.
//!
//! Every entry point catches Rust panics at the boundary (a panic crossing into
//! C is undefined behaviour) and the C caller owns all output buffers.

#![allow(clippy::missing_safety_doc)]
// Every raw-pointer access sits in its own `unsafe {}` block, also inside
// `unsafe extern "C" fn` bodies, so each one states why it is sound.
#![deny(unsafe_op_in_unsafe_fn)]

use orender_engine::{
    DecodeThreadMode, Engine, NoBridgeRuntime, NoBridgeSetup, OscOptions, OscOverrides, OscSettings,
};

use anyhow::Result;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

// Process-global decoder-less OSC reporter, brought up when the bridge can't be
// loaded so Studio can show a red banner (orender_create still returns NULL, so
// mpv falls back to its native decoder). One per process; lives until a real
// engine starts (which reclaims the OSC port) or the host exits. The runtime
// itself is the one the CLI idles on (`orender_engine::degraded`); keeping it
// alive is this host's part.
static DEGRADED_REPORTER: Mutex<Option<NoBridgeRuntime>> = Mutex::new(None);
static DEGRADED_ACTIVE: AtomicBool = AtomicBool::new(false);

// Build the no-bridge runtime and start its OSC server — never asking a
// holder of the port to yield: it is only a banner and must not evict a
// healthy standby renderer.
fn start_degraded_reporter(setup: NoBridgeSetup, opts: &OscOptions) -> Result<NoBridgeRuntime> {
    let mut runtime = NoBridgeRuntime::build(setup)?;
    runtime.start_osc(opts, None, false)?;
    Ok(runtime)
}

// Bring up the degraded reporter once. The renderer build (VBAP table) takes a
// moment, so do it on a detached thread — the caller returns NULL immediately
// and mpv falls back without waiting.
fn start_degraded_reporter_global(setup: NoBridgeSetup, opts: OscOptions) {
    // Claim the single slot; bail if a reporter is already active/starting.
    if DEGRADED_ACTIVE
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    std::thread::spawn(move || match start_degraded_reporter(setup, &opts) {
        Ok(reporter) => {
            // A real engine may have started while we were building; only
            // keep ours if the slot is still claimed (else drop → free port).
            if DEGRADED_ACTIVE.load(Ordering::SeqCst) {
                *DEGRADED_REPORTER.lock().unwrap() = Some(reporter);
            }
        }
        Err(e) => {
            eprintln!("degraded reporter failed to start: {e:#}");
            DEGRADED_ACTIVE.store(false, Ordering::SeqCst);
        }
    });
}

// Tear down the degraded reporter (releases its OSC port) when a real engine is
// coming up.
fn stop_degraded_reporter_global() {
    if DEGRADED_ACTIVE.swap(false, Ordering::SeqCst) {
        *DEGRADED_REPORTER.lock().unwrap() = None;
    }
}

// Resolve OSC options from the C override → config → environment → defaults,
// or None when OSC is off. Shared by the normal path and the degraded reporter,
// and — through `OscSettings::resolve` — with the CLI, so the two hosts cannot
// disagree on when OSC is up or where it listens.
//
// A zero/NULL field of the C struct defers to the config; `osc_enabled` can
// only force OSC on (0 means "follow the config", never "off"). When nothing
// decides, OSC is on iff no config file exists (`config_file_exists`): a first
// start in a player, where Studio is the only place a failure can show.
fn resolve_osc_opts(
    cfg: &OrenderConfig,
    render_cfg: Option<&orender_engine::RenderConfig>,
    config_file_exists: bool,
) -> Option<OscOptions> {
    let overrides = OscOverrides {
        enabled: (cfg.osc_enabled != 0).then_some(true),
        host: unsafe { opt_str(cfg.osc_host) }.map(str::to_string),
        port_out: (cfg.osc_port_out != 0).then_some(cfg.osc_port_out),
        port_in: (cfg.osc_port_in != 0).then_some(cfg.osc_port_in),
        metering: None,
    };
    let opts = OscSettings::resolve(render_cfg, &overrides, !config_file_exists).options();
    if opts.is_none() {
        log::info!(
            "OSC disabled: render.osc is unset/false in the config, no host override, \
             and no OMNIPHONY_OSC_PORT in the environment"
        );
    }
    opts
}

// Whether the config file the engine reads exists. Asked of the file itself,
// not of what was loaded: a live-handoff sidecar can stand in for a missing
// `config.yaml`, and no path at all (no home directory) is no config either.
fn config_file_exists(config_path: Option<&Path>) -> bool {
    config_path.is_some_and(Path::exists)
}

/// Opaque handle to a decode→render session. Created by `orender_create`,
/// freed by `orender_destroy`. Internally an engine session.
// Deliberately not `#[repr(C)]`: cbindgen then emits an incomplete type
// (`typedef struct OrenderRenderer OrenderRenderer;`) instead of a body with a
// zero-length array, which ISO C and C++ reject. Hosts only hold pointers.
pub struct OrenderRenderer {
    _private: [u8; 0],
}

/// Session configuration passed to `orender_create`. All `*const c_char`
/// fields are UTF-8, nul-terminated, and may be NULL (treated as "unset").
///
/// **FROZEN at ABI major 0** — never add, remove, reorder, or retype fields:
/// consumers compiled against an older header pass this struct by layout with
/// no size handshake, so any change here is silently breaking. New knobs go
/// through `orender_set_option` (post-create) or the config YAML
/// (create-time). See ABI.md.
#[repr(C)]
pub struct OrenderConfig {
    /// Output/host sample rate in Hz. 0 → 48000.
    pub sample_rate: u32,
    /// Path to the omniphony YAML config (drives bridge path, speaker layout +
    /// all render params). NULL → the shared default config used by the orender
    /// CLI + studio (`~/.config/omniphony/config.yaml`).
    pub config_yaml_path: *const c_char,
    /// Optional speaker-layout YAML path overriding the config. NULL → use the
    /// config's embedded layout, else the 7.1.4 preset. A config that renders
    /// a room (`hrir_source: brir`) is built on the room's loudspeakers
    /// whatever this says.
    pub speaker_layout_path: *const c_char,
    /// Optional decoder bridge plugins (the `*_bridge.so` files of the input
    /// formats' bridges) overriding the config: one path, or a path list in
    /// the platform's syntax (`:` on Unix, `;` on Windows), in load order.
    /// NULL → the config YAML's `render.bridge_path(s)`; when that is unset
    /// too, the engine loads every `*_bridge.{so,dll,dylib}` of the first
    /// folder holding one: next to the host executable, then
    /// `$ORENDER_BRIDGE_DIR`, then the system plugin directory
    /// (`/usr/lib/orender` on Unix) — for library hosts as for the CLI. A path
    /// given here or in the config must name an existing file (a relative one
    /// is tried against the working directory, then the executable's
    /// directory); it is never replaced by a discovered bridge.
    pub bridge_path: *const c_char,
    /// Codec identifier of the raw access units the host will feed (matches
    /// the bridge's supported codec IDs, e.g. as used in FFmpeg/IEC958).
    /// Disambiguates the bridge's raw transport (which carries no data-type
    /// byte). NULL → the bridge sniffs the sync word.
    pub codec: *const c_char,
    /// Force the OSC live-control server on (non-zero). 0 → follow the
    /// config's `render.osc`; when that is unset too, OSC defaults to on iff
    /// no config file exists (a first start: Studio can then see the engine)
    /// or `OMNIPHONY_OSC_PORT` is set (a workflow-assigned control port
    /// implies the engine must be reachable there). An existing config without
    /// the key keeps OSC off.
    pub osc_enabled: c_int,
    /// Incoming OSC port (0 → config `render.osc_rx_port`, else
    /// `OMNIPHONY_OSC_PORT`, else 9000).
    pub osc_port_in: u16,
    /// Outgoing/monitoring OSC port (0 → config `render.osc_port`, else
    /// `OMNIPHONY_OSC_PORT`, else 9000).
    pub osc_port_out: u16,
    /// OSC bind address (default "127.0.0.1").
    pub osc_bind: *const c_char,
    /// OSC monitoring target host.
    pub osc_host: *const c_char,
}

/// C-ABI major version of this library. A bump means a breaking change: the
/// Linux soname `liborender.so.<major>` follows automatically (see build.rs);
/// Windows/macOS consumers must gate on `orender_version_major` at load time
/// (their library file name does not change).
///
/// Exported into the generated header (as a `#define`) so a consumer can
/// compare the constants it was compiled against with the runtime values
/// reported by `orender_version_major`/`orender_version_minor`. Policy:
/// additive change (new symbol, new `orender_set_option` key, enum value
/// appended) bumps the minor; anything else (signature/struct/semantic change,
/// symbol removal, enum reorder) bumps the major. See ABI.md.
pub const ORENDER_ABI_MAJOR: u32 = 0;
/// C-ABI minor version: backwards-compatible additions only. Consumers should
/// gate optional features on symbol presence (dlsym), not on this value; it
/// exists for logging and diagnostics.
// 2: added orender_overlay_ass / orender_overlay_set_enabled (in-process overlay).
// 3: added orender_overlay_heatmap_bgra (BGRA energy-field bitmap for overlay-add).
// 4: added overlay toggles (labels/objects/trails/heatmap) + heatmap band/colormap cycling.
// 5: added orender_set_option, orender_build_id, exported ORENDER_ABI_* consts,
//    OrenderChannelLabel, and the /omniphony/state/render/abi OSC broadcast.
// 6: added orender_has_objects (live-fact semantics; orender_is_spatial is a
//    deprecated alias kept for older hosts).
// 7: added orender_output_latency_samples (constant DSP latency of the
//    rendered output, for host A/V sync compensation — non-zero when the
//    linear-phase FIR crossover is active).
// 8: appended the height-tier labels Lh/Rh/Ch/Lhs/Rhs to OrenderChannelLabel
//    (30° over the floor speaker of the same name — Auro-3D's height layer,
//    BS.2051 U+030/U+000/U+110). A host that predates them reads unknown
//    bytes for those channels; nothing existing moved.
// 9: added orender_source_label (the bridge's name for the presentation's
//    format — "DTS-HD MA + DTS:X 7.1.4", "Dolby TrueHD + Dolby Atmos" — for
//    the host's track info; 0/empty when the bridge states none).
// 10: added the `decode_thread` key of orender_set_option (decode on a thread
//     of its own, overlapping the render; a packet's audio may then come back
//     from a later call) and orender_drain (render what the engine still holds
//     at end of stream, one packet's audio per call).
// 11: added the `live` value of the `decode_thread` key (follow the live
//     option — config.yaml, Studio, OSC — switching at packet boundaries) and
//     orender_output_packet_pts (the host timestamp of the packet whose audio
//     the last call returned); orender_process now reads its pts_us argument.
// 12: added the `heard_us` key of orender_set_option (where the listener is,
//     relayed to OSC clients as /omniphony/playout/heard).
// 13: fork additions: orender_decoded_sample_rate, the bridge's actual output
//     rate so a host can detect a mismatch with its configured session rate;
//     orender_drain releasing a pending decoder access unit at EOF too;
//     orender_hrir_in_use to name the HRIR set the binaural path convolves
//     (`brir` while a room does); orender_brir_prepare (carrying the host's
//     source text in the room) and orender_brir_state for measured rooms; orender_sofa_describe, what a SOFA file or prepared
//     room holds and which stage takes it; and orender_compose_config for a
//     host's generated config overridden by a patch its user owns.
pub const ORENDER_ABI_MINOR: u32 = 13;

/// Speaker-position labels written by `orender_channel_layout` and
/// `orender_bed_layout` (one byte per channel). Mirrors the engine's
/// ABI-stable `bridge_api::RChannelLabel` exactly (a unit test asserts
/// discriminant parity); values are append-only per the ABI policy.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrenderChannelLabel {
    L = 0,
    R = 1,
    C = 2,
    Lfe = 3,
    Ls = 4,
    Rs = 5,
    Tfl = 6,
    Tfr = 7,
    Tsl = 8,
    Tsr = 9,
    Tbl = 10,
    Tbr = 11,
    Lsc = 12,
    Rsc = 13,
    Lb = 14,
    Rb = 15,
    Cb = 16,
    Tc = 17,
    Lsd = 18,
    Rsd = 19,
    Lw = 20,
    Rw = 21,
    Tfc = 22,
    Lfe2 = 23,
    /// The channel carries dynamic-object audio (position driven by metadata).
    Object = 24,
    /// Height tier: over the floor speaker of the same name at about 30° of
    /// elevation (BS.2051 `U+030`/`U-030`/`U+000`/`U+110`/`U-110`; DTS-HD
    /// `Lh`/`Rh`/`Ch`/`Lhs`/`Rhs`; Auro-3D's height layer). Distinct from the
    /// top tier (`Tfl`…), which means the ceiling corners.
    Lh = 25,
    Rh = 26,
    Ch = 27,
    Lhs = 28,
    Rhs = 29,
    Unknown = 255,
}

/// # Safety
/// `p` is NULL or a nul-terminated string that outlives `'a`.
unsafe fn opt_str<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    // SAFETY: non-null, and the caller guarantees the rest.
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

/// Diagnostics about how the *host* process (mpv) was launched, appended to the
/// degraded bridge-error so Studio can show it. liborender runs inside mpv, so
/// `current_dir()`/`args()` are mpv's own CWD and argv — exactly what's needed
/// to explain a relative `bridge_path` that resolves against the wrong folder.
///
/// Caveat: `args()` only sees the actual command line. Options coming from
/// `mpv.conf` / portable_config are read internally by mpv and won't appear
/// here — hence we surface the CWD too (which explains relative-path failures
/// regardless of where the path was set).
fn host_launch_diagnostics() -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    let cmdline = std::env::args().collect::<Vec<_>>().join(" ");
    format!("\n\nWorking dir: {cwd}\nCommand line: {cmdline}")
}

fn build_engine(cfg: &OrenderConfig) -> Result<Engine> {
    // Seed the machine-wide Windows config from the legacy per-user location
    // once (no-op on Linux/macOS), before resolving the default path.
    orender_engine::migrate_legacy_windows_config();

    // Optional override; NULL → taken from the config YAML's
    // render.bridge_path(s). A path list in the platform's syntax (`:` on
    // Unix, `;` on Windows), so a player names several bridges with one
    // option (mpv's `ad-orender-bridge-path`).
    let bridge_path = unsafe { opt_str(cfg.bridge_path) };
    let bridge_paths: Vec<PathBuf> = bridge_path
        .map(|list| {
            std::env::split_paths(list)
                .filter(|path| !path.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default();
    // NULL config → the shared omniphony config (same as the CLI + studio),
    // so one config drives all hosts.
    let config_path = unsafe { opt_str(cfg.config_yaml_path) }
        .map(PathBuf::from)
        .or_else(orender_engine::default_config_path);
    let layout_path = unsafe { opt_str(cfg.speaker_layout_path) };
    let codec = unsafe { opt_str(cfg.codec) };
    let sample_rate = if cfg.sample_rate == 0 {
        48_000
    } else {
        cfg.sample_rate
    };

    // Load the shared config once: drives OSC settings (C override → config →
    // defaults) and seeds the degraded reporter below. The `_with_live`
    // variant keeps this pre-load consistent with `Engine::from_paths` when a
    // live-handoff sidecar was already consumed in this process (the OSC
    // fields themselves are never live-modified, so either source is correct).
    let render_cfg = config_path
        .as_deref()
        .map(|p| orender_engine::Config::load_or_default_with_live(p).0)
        .and_then(|c| c.render);
    // Resolve OSC up front so we can also reach Studio with a degraded reporter
    // if the bridge fails. `None` when OSC is off.
    let osc_opts = resolve_osc_opts(
        cfg,
        render_cfg.as_ref(),
        config_file_exists(config_path.as_deref()),
    );

    // Settle OSC port ownership BEFORE `Engine::from_paths` reads the config:
    // a yieldable standby renderer writes its live-state sidecar while still
    // holding the port, so negotiating first guarantees `from_paths` sees the
    // handed-over state. Gated on a successful bridge pre-resolution (same
    // strict policy as `from_paths`): a host that is about to fall back to the
    // degraded reporter must not evict a healthy standby.
    let config_bridges = render_cfg
        .as_ref()
        .map(|render| render.bridges())
        .unwrap_or_default();
    let bridge_resolvable =
        orender_engine::bridge_loader::resolve_bridges(&bridge_paths, &config_bridges).is_ok();
    if bridge_resolvable {
        // A same-process degraded reporter may itself hold the port.
        stop_degraded_reporter_global();
        if let Some(opts) = osc_opts.as_ref()
            && !orender_engine::osc::negotiate_rx_port(opts.port_in)
        {
            log::warn!(
                "OSC RX port {} still busy after yield negotiation; \
                 the engine will run without an OSC listener",
                opts.port_in
            );
        }
    }

    let mut engine = match Engine::from_paths(
        config_path.as_deref(),
        layout_path.map(Path::new),
        &bridge_paths,
        codec,
        sample_rate,
    ) {
        Ok(engine) => engine,
        Err(e) => {
            // The decoder bridge couldn't be resolved/loaded. Returning the
            // error makes orender_create yield NULL, so mpv falls back to its
            // native decoder (audio keeps working). But bring up a decoder-less
            // OSC reporter so Studio can show *why* spatial didn't engage —
            // Studio registers normally, so its address is known (no guessing).
            if let Some(opts) = osc_opts {
                // The same inputs `Engine::from_paths` just used, so the
                // banner comes with the state the engine would have shown.
                start_degraded_reporter_global(
                    NoBridgeSetup::embedded(
                        config_path.clone(),
                        render_cfg,
                        layout_path.map(PathBuf::from),
                        bridge_paths.clone(),
                        sample_rate,
                        format!("{e:#}{}", host_launch_diagnostics()),
                        Some((ORENDER_ABI_MAJOR, ORENDER_ABI_MINOR)),
                    ),
                    opts,
                );
            }
            return Err(e);
        }
    };

    // A real engine is starting; release any degraded reporter holding the OSC
    // port before we bind it.
    stop_degraded_reporter_global();

    if let Some(opts) = osc_opts {
        engine.enable_osc(opts)?;
    }

    Ok(engine)
}

/// Initialise the `log` backend once, so the engine's `log::*` diagnostics
/// (bridge-load time, "Spatial renderer built in Xs", clip warnings, engine-ready
/// time) surface BOTH on stderr and over OSC to connected clients (Studio's log
/// panel).
///
/// This installs the engine's shared live-log logger — the same one the `orender`
/// CLI uses — rather than a plain `env_logger`, which only wrote to stderr and so
/// left the OSC log stream empty in the mpv/liborender build. Initial verbosity
/// comes from `RUST_LOG` (a bare level like `info`/`warn`/`debug`), defaulting to
/// `info`; it stays OSC-adjustable at runtime. Falls back to plain `env_logger`
/// only if the host already installed a global logger.
fn init_logging() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let level = std::env::var("RUST_LOG")
            .ok()
            .and_then(|s| s.trim().parse::<log::LevelFilter>().ok())
            .unwrap_or(log::LevelFilter::Info);
        if orender_engine::init_live_logging(level, false).is_err() {
            let _ =
                env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
                    .try_init();
        }
    });
}

/// Create a session. Returns NULL on failure (bad config, missing bridge, etc.).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_create(cfg: *const OrenderConfig) -> *mut OrenderRenderer {
    init_logging();
    catch_unwind(AssertUnwindSafe(|| {
        if cfg.is_null() {
            return ptr::null_mut();
        }
        // SAFETY: non-null (checked above); the caller passes a valid config.
        match build_engine(unsafe { &*cfg }) {
            Ok(engine) => {
                // Stamp the shim's C-ABI version so the live-state snapshot
                // broadcasts it (Studio About shows it next to the fingerprint).
                engine.set_host_abi(ORENDER_ABI_MAJOR, ORENDER_ABI_MINOR);
                // Arm the spatial overlay: it stays blank until a session exists,
                // so a host that loads the overlay shim without selecting
                // `--ad=orender` (no session created) draws nothing.
                orender_engine::overlay::session_started();
                Box::into_raw(Box::new(engine)) as *mut OrenderRenderer
            }
            Err(e) => {
                eprintln!("orender_create failed: {e:#}");
                ptr::null_mut()
            }
        }
    }))
    .unwrap_or(ptr::null_mut())
}

/// Free a session created by `orender_create`. NULL is ignored.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_destroy(r: *mut OrenderRenderer) {
    if r.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `r` is the boxed `Engine` from `orender_create`, freed once.
        drop(unsafe { Box::from_raw(r as *mut Engine) });
        // Disarm the overlay as this session goes away (clears the scene when it
        // was the last one), so the box can't linger past the stream.
        orender_engine::overlay::session_ended();
    }));
}

/// 1 while the current presentation carries dynamic objects, 0 while it is a
/// plain multichannel stream, <0 on error.
///
/// A live, observable fact about the stream (`docs/channel-object-contract.md`):
/// it may flip in either direction mid-stream and must not be latched. Before
/// the first decoded frame it reports the bridge's container-level guess.
/// Hosts keep object-bearing tracks on the renderer regardless of the channel
/// mode (a host cannot render objects); channel-based content follows
/// `orender_channel_mode`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_has_objects(r: *const OrenderRenderer) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        if r.is_null() {
            return -1;
        }
        // SAFETY: non-null (checked above) and a live `orender_create` handle.
        let engine = unsafe { &*(r as *const Engine) };
        if engine.has_objects() { 1 } else { 0 }
    }))
    .unwrap_or(-1)
}

/// Deprecated alias of `orender_has_objects`, kept for hosts compiled
/// against ABI minor < 6. Same values, same live semantics.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_is_spatial(r: *const OrenderRenderer) -> c_int {
    // SAFETY: same contract as `orender_has_objects`.
    unsafe { orender_has_objects(r) }
}

/// Dynamic object count of the last rendered frame (decoded channels minus the
/// bed channels) for object-based content, `0` for plain multichannel, `-1` on
/// a NULL handle / error. For the host's track info display. Meaningful after at
/// least one `orender_process` call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_object_count(r: *const OrenderRenderer) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        if r.is_null() {
            return -1;
        }
        // SAFETY: non-null (checked above) and a live `orender_create` handle.
        unsafe { &*(r as *const Engine) }.object_count() as c_int
    }))
    .unwrap_or(-1)
}

/// Dialogue normalisation level in dBFS (always ≤ 0) once the stream has
/// declared it, or `INT32_MIN` when unknown / not yet seen (also on a NULL
/// handle / error). For the host's track info display.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_dialnorm_db(r: *const OrenderRenderer) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        if r.is_null() {
            return c_int::MIN;
        }
        // SAFETY: non-null (checked above) and a live `orender_create` handle.
        match unsafe { &*(r as *const Engine) }.dialnorm_db() {
            Some(db) => db as c_int,
            None => c_int::MIN,
        }
    }))
    .unwrap_or(c_int::MIN)
}

/// Write the bed channel labels of the last object-based frame (one
/// `OrenderChannelLabel` byte per bed channel) so the host can show the bed
/// composition (e.g. "LFE+11 objects").
///
/// Same query/fill convention as `orender_channel_layout`: returns the bed
/// channel count `N`; if `out_labels` is non-NULL and `cap >= N`, the first `N`
/// bytes are filled (else nothing is written — call with `out_labels = NULL` to
/// query `N`). `0` for plain multichannel / no bed / NULL handle / error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_bed_layout(
    r: *const OrenderRenderer,
    out_labels: *mut u8,
    cap: u32,
) -> u32 {
    catch_unwind(AssertUnwindSafe(|| {
        if r.is_null() {
            return 0;
        }
        // SAFETY: non-null (checked above) and a live `orender_create` handle.
        let labels = unsafe { &*(r as *const Engine) }.bed_labels();
        let n = labels.len() as u32;
        if !out_labels.is_null() && cap >= n {
            // SAFETY: non-null, and the caller's buffer holds `cap >= n` bytes.
            let out = unsafe { std::slice::from_raw_parts_mut(out_labels, labels.len()) };
            for (dst, lbl) in out.iter_mut().zip(labels.iter()) {
                *dst = *lbl as u8;
            }
        }
        n
    }))
    .unwrap_or(0)
}

/// Write the name the bridge gives the current presentation's format, as a
/// NUL-terminated UTF-8 string — `DTS-HD MA + DTS:X 7.1.4`,
/// `DTS-HD MA + Auro-3D 11.1`, `Dolby TrueHD + Dolby Atmos`, `Dolby Digital
/// Plus` — for the host's track info display. Declaration-level: it follows
/// the channel labels (a lossy carrier whose spatial layer the bridge cannot
/// read is named as the carrier alone), so poll it with the other track-info
/// queries rather than latching it.
///
/// Query/fill convention: returns the label's length `N` in bytes (without the
/// terminator); if `out` is non-NULL and `cap > N`, the label and its NUL are
/// written (else nothing is written — call with `out = NULL` to query `N`).
/// `0` when the bridge states no name (the host composes its own), and on a
/// NULL handle / error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_source_label(
    r: *const OrenderRenderer,
    out: *mut c_char,
    cap: u32,
) -> u32 {
    catch_unwind(AssertUnwindSafe(|| {
        if r.is_null() {
            return 0;
        }
        // SAFETY: non-null (checked above) and a live `orender_create` handle.
        let label = unsafe { &*(r as *const Engine) }.source_label().as_bytes();
        let n = label.len() as u32;
        if !out.is_null() && cap > n {
            // SAFETY: non-null, and the caller's buffer holds `cap > n` bytes.
            let out = unsafe { std::slice::from_raw_parts_mut(out as *mut u8, label.len() + 1) };
            out[..label.len()].copy_from_slice(label);
            out[label.len()] = 0;
        }
        n
    }))
    .unwrap_or(0)
}

/// Write how the last frames reached the headphones, as a host shows it, as
/// a NUL-terminated string: `room:N` while a room of `N` loudspeakers
/// convolves, `cascade:N` while objects are panned onto `N` virtual
/// loudspeakers for the HRTF stage (a room's own while it loads), `direct`
/// when each object is convolved as a direction of its own, `speakers:N`
/// for speaker output. It follows the session, not the host's settings: a
/// config that chose a room or a mode is reported as rendered. Live, like
/// `orender_hrir_in_use`.
///
/// Query/fill convention as `orender_source_label`: returns the length `N`
/// without the terminator and writes only when `out` is non-NULL and
/// `cap > N`. 0 on a NULL handle / error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_render_path(
    r: *const OrenderRenderer,
    out: *mut c_char,
    cap: u32,
) -> u32 {
    catch_unwind(AssertUnwindSafe(|| {
        if r.is_null() {
            return 0;
        }
        // SAFETY: non-null (checked above) and a live `orender_create` handle.
        let engine = unsafe { &*(r as *const Engine) };
        let path = engine.render_path();
        let n = path.len() as u32;
        if !out.is_null() && cap > n {
            // SAFETY: non-null, and the caller's buffer holds `cap > n` bytes.
            let out = unsafe { std::slice::from_raw_parts_mut(out as *mut u8, path.len() + 1) };
            out[..path.len()].copy_from_slice(path.as_bytes());
            out[path.len()] = 0;
        }
        n
    }))
    .unwrap_or(0)
}

/// Write the selector of the HRIR set the binaural renderer is convolving
/// with — `saf` (the embedded KEMAR set), `sofa`, `brir`, `synthetic`,
/// `pinna` or `prtf`, the same words `hrir_source` takes in the config — as a
/// NUL-terminated string. It names the set in use, not the one configured: a
/// SOFA file that could not be loaded reports `saf`, the set the build fell
/// back to. Live: a configured set is requested with the first rendered block
/// and built off the audio thread, so the answer can move from `saf` to
/// `sofa` a moment into the stream; poll it with the other per-frame queries
/// rather than latching the first value.
///
/// `brir` means the last rendered frame was convolved with a room. While a
/// room loads, or when it could not be loaded, the HRTF stage renders its
/// virtual array on the embedded set and this reads `saf`;
/// `orender_brir_state` tells the two apart.
///
/// Query/fill convention as `orender_source_label`: returns the length `N`
/// without the terminator and writes only when `out` is non-NULL and
/// `cap > N`. 0 on a NULL handle / error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_hrir_in_use(
    r: *const OrenderRenderer,
    out: *mut c_char,
    cap: u32,
) -> u32 {
    catch_unwind(AssertUnwindSafe(|| {
        if r.is_null() {
            return 0;
        }
        // SAFETY: non-null (checked above) and a live `orender_create` handle.
        let engine = unsafe { &*(r as *const Engine) };
        let status = engine.hrir_status();
        let name = if engine.brir_rendering() {
            "brir".as_bytes()
        } else {
            status.effective.as_str().as_bytes()
        };
        let n = name.len() as u32;
        if !out.is_null() && cap > n {
            // SAFETY: non-null, and the caller's buffer holds `cap > n` bytes.
            let out = unsafe { std::slice::from_raw_parts_mut(out as *mut u8, name.len() + 1) };
            out[..name.len()].copy_from_slice(name);
            out[name.len()] = 0;
        }
        n
    }))
    .unwrap_or(0)
}

/// Where the headphone session's room (a `brir` HRIR source) stands: 0 no
/// room selected (or the output is not binaural), 1 loading, 2 resident, 3
/// refused — the reason is in the log. While it loads and after it is
/// refused, the HRTF stage renders the virtual array on the embedded set
/// (`orender_hrir_in_use` reads `saf`). Live, like `orender_hrir_in_use`: a
/// room is requested with the first rendered block and loaded off the audio
/// thread. -1 on a NULL handle or an internal error.
///
/// # Safety
/// `r` is NULL or a live `orender_create` handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_brir_state(r: *const OrenderRenderer) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        if r.is_null() {
            return -1;
        }
        // SAFETY: non-null (checked above) and a live `orender_create` handle.
        unsafe { &*(r as *const Engine) }.brir_state() as c_int
    }))
    .unwrap_or(-1)
}

/// Write `text` into a caller's buffer of `cap` bytes, NUL-terminated and cut
/// at a character boundary when it does not fit. Nothing for a NULL buffer
/// or a zero capacity.
///
/// # Safety
/// `out` is NULL or holds `cap` writable bytes.
unsafe fn write_text(out: *mut c_char, cap: u32, text: &str) {
    if out.is_null() || cap == 0 {
        return;
    }
    let mut n = text.len().min(cap as usize - 1);
    while !text.is_char_boundary(n) {
        n -= 1;
    }
    // SAFETY: non-null, and the caller's buffer holds `cap > n` bytes.
    let out = unsafe { std::slice::from_raw_parts_mut(out as *mut u8, n + 1) };
    out[..n].copy_from_slice(&text.as_bytes()[..n]);
    out[n] = 0;
}

/// Prepare a measured room for this engine, once, so a session loads it in a
/// fraction of the time and memory the SOFA file takes.
///
/// `sofa` holds `len` bytes of a room-response SOFA file (`MultiSpeakerBRIR`,
/// or a per-direction set with room-length responses). The head orientation
/// nearest straight ahead is kept, which is what a session without head
/// tracking renders, and the result is checked to load and to make a speaker
/// layout. It is written to `out_path` through `out_path.part`, renamed into
/// place, so a failure leaves any previous file there untouched.
///
/// A session given the prepared file as its `brir_sofa_path` renders it
/// exactly as it renders the SOFA file without head tracking, and builds its
/// virtual array on the room's loudspeakers from the start.
///
/// `source` is a text of the host's carried in the prepared room: what the
/// room was made from, in whatever form the host compares later - the
/// file's path, size and time, say. The engine stores it verbatim, cut
/// to 4096 bytes, and never interprets it; a host reads it back from the
/// room's header (see ABI.md) to tell whether a room already there is the
/// one this file would prepare, with no note beside it.
///
/// `summary` (NULL allowed) receives a NUL-terminated line, cut to `cap`
/// bytes. On success: `emitters=13 orientations=1 seconds=0.512 rate=48000
/// bytes=1712345 names=C,FL,FR,… conventions=MultiSpeakerBRIR` (conventions
/// last, spaces in it replaced by `_`). On failure: the reason.
///
/// Returns 0 on success, -1 when the bytes are not a usable room response
/// (or this build has no SOFA support), -2 when the prepared file cannot be
/// written, -3 on a NULL argument (a `source` that is not UTF-8 included) or
/// an internal error.
///
/// # Safety
/// `sofa` holds `len` readable bytes; `out_path` is a NUL-terminated path;
/// `source` is a NUL-terminated UTF-8 string; `summary` is NULL or holds
/// `cap` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_brir_prepare(
    sofa: *const u8,
    len: usize,
    out_path: *const c_char,
    source: *const c_char,
    summary: *mut c_char,
    cap: u32,
) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: NULL or a nul-terminated string (caller contract).
        let Some(source) = (unsafe { opt_str(source) }) else {
            unsafe { write_text(summary, cap, "no source text, or not UTF-8") };
            return -3;
        };
        // SAFETY: NULL or a nul-terminated string (caller contract).
        let Some(out_path) = (unsafe { opt_str(out_path) }) else {
            unsafe { write_text(summary, cap, "no output path") };
            return -3;
        };
        if sofa.is_null() {
            unsafe { write_text(summary, cap, "no input") };
            return -3;
        }
        // SAFETY: non-null, and the caller's buffer holds `len` bytes.
        let bytes = unsafe { std::slice::from_raw_parts(sofa, len) };
        match prepare_room_file(bytes, Path::new(out_path), source) {
            Ok(line) => {
                unsafe { write_text(summary, cap, &line) };
                0
            }
            Err((code, reason)) => {
                unsafe { write_text(summary, cap, &reason) };
                code
            }
        }
    }))
    .unwrap_or(-3)
}

/// [`orender_brir_prepare`] on safe types: the summary line, or the return
/// code and the reason.
#[cfg(feature = "sofa")]
fn prepare_room_file(
    bytes: &[u8],
    out: &Path,
    source: &str,
) -> std::result::Result<String, (c_int, String)> {
    let mut prepared =
        renderer::binaural::brir::prepare_room(bytes).map_err(|e| (-1, format!("{e:#}")))?;
    prepared.room = prepared.room.with_source(source);
    let image = prepared.room.to_prepared();
    let mut part = out.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    std::fs::write(&part, &image)
        .and_then(|()| std::fs::rename(&part, out))
        .map_err(|e| {
            let _ = std::fs::remove_file(&part);
            (-2, format!("write {}: {e}", out.display()))
        })?;
    let room = &prepared.room;
    Ok(format!(
        "emitters={} orientations={} seconds={:.3} rate={} bytes={} names={} conventions={}",
        room.emitters().len(),
        room.orientations().len(),
        prepared.seconds,
        room.file_rate(),
        image.len(),
        prepared.speaker_names.join(","),
        room.conventions().replace(char::is_whitespace, "_"),
    ))
}

#[cfg(not(feature = "sofa"))]
fn prepare_room_file(
    _bytes: &[u8],
    _out: &Path,
    _source: &str,
) -> std::result::Result<String, (c_int, String)> {
    Err((
        -1,
        "SOFA support is not built into this library (the 'sofa' feature)".to_string(),
    ))
}

/// Say what a SOFA file or a prepared room holds, and which binaural stage
/// takes it, before a host copies or prepares anything: its shape and
/// geometry are read, never its responses, so a room set of hundreds of MB
/// is described in a moment.
///
/// `sofa` holds `len` bytes of the file. `out` (NULL allowed) receives a
/// NUL-terminated line, cut to `cap` bytes:
/// `hrtf=yes|no room=yes|no prepared=yes|no conventions=… measurements=M
/// receivers=R emitters=E samples=N rate=…`, then, for a room,
/// `orientations=… speakers=S names=C,FL,FR,…`, and `reason=…` to the end
/// of the line: why the stage that does not take the file refuses it (the
/// HRTF stage's reason for a file that suits neither and holds one emitter
/// per measurement, the room stage's otherwise). Spaces in `conventions` are
/// replaced by `_`.
///
/// The HRTF stage (`hrtf_sofa_path`) takes one direction per measurement
/// and convolves the first few milliseconds of each; the room stage
/// (`brir_sofa_path`, or `orender_brir_prepare`) takes up to 64 loudspeakers
/// measured with their room. A multi-speaker room suits only the second, a
/// free-field set of hundreds of directions only the first.
///
/// Returns 1 when the HRTF stage takes the file, 2 when the room stage does,
/// 3 when both do, 0 when neither does (the reason is in `out`), -1 when the
/// bytes are neither a SOFA file this engine reads nor a prepared room, -3
/// on a NULL `sofa` or an internal error.
///
/// # Safety
/// `sofa` holds `len` readable bytes; `out` is NULL or holds `cap` writable
/// bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_sofa_describe(
    sofa: *const u8,
    len: usize,
    out: *mut c_char,
    cap: u32,
) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        if sofa.is_null() {
            unsafe { write_text(out, cap, "reason=no input") };
            return -3;
        }
        // SAFETY: non-null, and the caller's buffer holds `len` bytes.
        let bytes = unsafe { std::slice::from_raw_parts(sofa, len) };
        match renderer::binaural::brir::describe_room_file(bytes) {
            Ok(contents) => {
                let (line, code) = describe_line(&contents);
                unsafe { write_text(out, cap, &line) };
                code
            }
            Err(e) => {
                unsafe { write_text(out, cap, &format!("reason={e:#}")) };
                -1
            }
        }
    }))
    .unwrap_or(-3)
}

/// [`orender_sofa_describe`]'s line and return code for `c`.
fn describe_line(c: &renderer::binaural::brir::SofaContents) -> (String, c_int) {
    let yes = |b: bool| if b { "yes" } else { "no" };
    let hrtf = c.hrtf_refusal.is_none();
    let room = c.room.as_ref().ok();
    let mut line = format!(
        "hrtf={} room={} prepared={} conventions={} measurements={} receivers={} emitters={} \
         samples={} rate={}",
        yes(hrtf),
        yes(room.is_some()),
        yes(c.prepared),
        c.conventions.replace(char::is_whitespace, "_"),
        c.measurements,
        c.receivers,
        c.emitters,
        c.samples,
        c.rate,
    );
    if let Some(room) = room {
        line += &format!(
            " orientations={} speakers={} names={}",
            room.orientations,
            room.speakers.len(),
            room.speakers.join(",")
        );
    }
    let reason = match (&c.hrtf_refusal, &c.room) {
        (Some(why), Err(_)) if c.emitters == 1 => Some(why.as_str()),
        (_, Err(why)) => Some(why.as_str()),
        (Some(why), Ok(_)) => Some(why.as_str()),
        (None, Ok(_)) => None,
    };
    if let Some(reason) = reason {
        line += &format!(" reason={reason}");
    }
    let code = c_int::from(hrtf) | if room.is_some() { 2 } else { 0 };
    (line, code)
}

/// Compose a host's generated config with a patch its user owns, for a host
/// that writes the config itself and lets an advanced user override it.
///
/// `base_path` is the host's config for this session; `patch_path` the
/// user's partial config (absent = no patch); `patch_dir` the directory the
/// patch's relative paths start in (NULL = the patch's own). In the patch,
/// `null` inherits the host's value, mappings merge key by key and anything
/// else replaces; it is applied whole or not at all, and a key the host owns
/// (the decoder, input, output, OSC) is refused rather than ignored. The
/// patch is only read, never written.
///
/// When the patch applies, the composed config is written to `out_path`
/// (through `out_path.part`, renamed into place) for `orender_create` -
/// unless `out_path` already holds exactly that text, which is left as it
/// is: a host can keep the composed config between sessions, and it is
/// rewritten only when the base or the patch changes what it says.
///
/// `report` (NULL allowed) receives a NUL-terminated line, cut to `cap`
/// bytes: `status=none|applied|rejected keys=N layout_set=0|1
/// decode_thread_set=0|1`, and `reason=…` to the end of the line when
/// rejected. `layout_set`: the patch sets `current_layout`, so a host that
/// also passes `speaker_layout_path` must not, or it would win.
/// `decode_thread_set`: the patch sets `decode_thread`, so a host that picks
/// the decode thread itself should hand the choice to the option instead
/// (`orender_set_option(r, "decode_thread", "live")`).
///
/// Returns 1 when the patch applies (`out_path` holds it), 0 when there is no
/// patch or it sets nothing (nothing written: use `base_path`), -1 when the
/// patch is rejected (nothing written: use `base_path`), -2 when the
/// composed config cannot be written, -3 on a NULL argument or an internal
/// error.
///
/// # Safety
/// `base_path`, `patch_path` and `out_path` are NUL-terminated paths;
/// `patch_dir` is NULL or one; `report` is NULL or holds `cap` writable
/// bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_compose_config(
    base_path: *const c_char,
    patch_path: *const c_char,
    patch_dir: *const c_char,
    out_path: *const c_char,
    report: *mut c_char,
    cap: u32,
) -> c_int {
    use renderer::config::compose::{ComposeStatus, compose_files};
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: NULL or nul-terminated strings (caller contract).
        let (Some(base), Some(patch), Some(out)) = (unsafe { opt_str(base_path) }, unsafe {
            opt_str(patch_path)
        }, unsafe {
            opt_str(out_path)
        }) else {
            unsafe { write_text(report, cap, "status=rejected keys=0 layout_set=0 decode_thread_set=0 reason=a path is missing") };
            return -3;
        };
        let dir = unsafe { opt_str(patch_dir) }.map(Path::new);
        let composed = compose_files(Path::new(base), Path::new(patch), dir);
        let code = match composed.status {
            ComposeStatus::None => 0,
            ComposeStatus::Rejected => -1,
            ComposeStatus::Applied => {
                let text = composed.effective.as_deref().unwrap_or_default();
                if std::fs::read(out).is_ok_and(|held| held == text.as_bytes()) {
                    unsafe { write_text(report, cap, &composed.line()) };
                    return 1;
                }
                let mut part = std::ffi::OsString::from(out);
                part.push(".part");
                let part = PathBuf::from(part);
                match std::fs::write(&part, text).and_then(|()| std::fs::rename(&part, out)) {
                    Ok(()) => 1,
                    Err(e) => {
                        let _ = std::fs::remove_file(&part);
                        let line = format!(
                            "{} reason=cannot write {out}: {e}",
                            composed.line().replacen("status=applied", "status=rejected", 1)
                        );
                        unsafe { write_text(report, cap, &line) };
                        return -2;
                    }
                }
            }
        };
        unsafe { write_text(report, cap, &composed.line()) };
        code
    }))
    .unwrap_or(-3)
}

/// Constant DSP latency of the rendered output, in samples at the engine
/// sample rate: PCM fed to `orender_process` emerges this many samples later
/// in the rendered stream. 0 for the default filters; non-zero when the
/// linear-phase FIR crossover sits on the rendered path. The host should
/// subtract `latency / sample_rate` from the presentation timestamps of
/// rendered frames (or delay video by the same amount) to preserve A/V sync.
/// May change mid-stream (live crossover / output-mode switch), so poll it
/// per rendered frame; meaningful after the first `orender_process` call.
/// 0 on a NULL handle / error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_output_latency_samples(r: *const OrenderRenderer) -> u64 {
    catch_unwind(AssertUnwindSafe(|| {
        if r.is_null() {
            return 0;
        }
        // SAFETY: non-null (checked above) and a live `orender_create` handle.
        unsafe { &*(r as *const Engine) }.output_latency_samples()
    }))
    .unwrap_or(0)
}

/// Configured render mode for channel-based (non-object) content:
/// 0 = host, 1 = spatial; <0 on error. When this is `host` (0) and
/// `orender_has_objects` reports 0, the host should decline this track and fall
/// back to its native decoder. Meaningful once the renderer is created (the mode
/// comes from config / live params, not from the stream).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_channel_mode(r: *const OrenderRenderer) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        if r.is_null() {
            return -1;
        }
        // SAFETY: non-null (checked above) and a live `orender_create` handle.
        unsafe { &*(r as *const Engine) }.channel_render_mode_code() as c_int
    }))
    .unwrap_or(-1)
}

/// Override the channel render mode for non-object content at runtime (a
/// per-host override of the config value): 0 = host, 1 = spatial. No-op on a
/// NULL handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_set_channel_mode(r: *mut OrenderRenderer, mode: c_int) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if r.is_null() {
            return;
        }
        // SAFETY: non-null (checked above) and a live `orender_create` handle.
        unsafe { &mut *(r as *mut Engine) }.set_channel_render_mode_code(mode as i32);
    }));
}

/// Output channel mapping: 0 = by_index (positionless — output port N carries
/// layout speaker N), 1 = by_name (positional — each channel tagged with its
/// speaker position). <0 on error. The host uses this to choose between a
/// positionless and a positional channel map.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_channel_mapping(r: *const OrenderRenderer) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        if r.is_null() {
            return -1;
        }
        // SAFETY: non-null (checked above) and a live `orender_create` handle.
        unsafe { &*(r as *const Engine) }.output_channel_mapping_code() as c_int
    }))
    .unwrap_or(-1)
}

/// Override the output channel mapping at runtime: 0 = by_index, 1 = by_name.
/// No-op on a NULL handle or an unknown code.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_set_channel_mapping(r: *mut OrenderRenderer, mode: c_int) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if r.is_null() {
            return;
        }
        // SAFETY: non-null (checked above) and a live `orender_create` handle.
        unsafe { &mut *(r as *mut Engine) }.set_output_channel_mapping_code(mode as i32);
    }));
}

/// Number of output channels (speakers) the renderer produces, 0 on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_channel_count(r: *const OrenderRenderer) -> u32 {
    catch_unwind(AssertUnwindSafe(|| {
        if r.is_null() {
            return 0;
        }
        // SAFETY: non-null (checked above) and a live `orender_create` handle.
        unsafe { &*(r as *const Engine) }.channel_count()
    }))
    .unwrap_or(0)
}

/// Write the active output layout's per-channel labels (one
/// `OrenderChannelLabel` byte per speaker, in render order) so the host can
/// build a channel map.
///
/// Returns the channel count `N`. If `out_labels` is non-NULL and `cap >= N`,
/// the first `N` bytes are filled with label discriminants; otherwise nothing is
/// written — call with `out_labels = NULL` to query `N`, size a buffer, then
/// call again. Each byte is an `OrenderChannelLabel` value (255 = Unknown).
/// Returns 0 on error/NULL handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_channel_layout(
    r: *const OrenderRenderer,
    out_labels: *mut u8,
    cap: u32,
) -> u32 {
    catch_unwind(AssertUnwindSafe(|| {
        if r.is_null() {
            return 0;
        }
        // SAFETY: non-null (checked above) and a live `orender_create` handle.
        let labels = unsafe { &*(r as *const Engine) }.channel_layout();
        let n = labels.len() as u32;
        if !out_labels.is_null() && cap >= n {
            // SAFETY: non-null, and the caller's buffer holds `cap >= n` bytes.
            let out = unsafe { std::slice::from_raw_parts_mut(out_labels, labels.len()) };
            for (dst, lbl) in out.iter_mut().zip(labels.iter()) {
                *dst = *lbl as u8;
            }
        }
        n
    }))
    .unwrap_or(0)
}

/// Sampling frequency (Hz) of the last decoded frame, or 0 before a rate is
/// reported and on a NULL handle / error. Retained across same-stream seeks.
/// This is distinct from the session rate the host configured: an extension
/// such as DTS XLL can decode at 96 kHz over a 48 kHz core, and the renderer
/// follows the stream's rate, so the audio comes back at this one. Poll after
/// processing and reopen at this rate if needed, before playing mismatched-rate
/// output.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_decoded_sample_rate(r: *const OrenderRenderer) -> u32 {
    catch_unwind(AssertUnwindSafe(|| {
        if r.is_null() {
            return 0;
        }
        // SAFETY: non-null (checked above) and a live `orender_create` handle.
        unsafe { &*(r as *const Engine) }.decoded_sample_rate()
    }))
    .unwrap_or(0)
}

/// Reset after a seek/discontinuity (flushes decoder + renderer state, keeps
/// live params).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_reset(r: *mut OrenderRenderer) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if r.is_null() {
            return;
        }
        // SAFETY: non-null (checked above) and a live `orender_create` handle.
        unsafe { &mut *(r as *mut Engine) }.reset();
    }));
}

/// Push one raw encoded packet and render whatever frames it yields.
///
/// The caller owns `out` (capacity `out_cap_samples` floats). On success the
/// rendered interleaved samples are written there and `*out_frames` /
/// `*out_channels` / `*out_pts_us` are set.
///
/// Returns: 0 = OK (may be 0 frames — need more data), >0 = output buffer too
/// small (nothing written; call again with the same packet and a larger
/// buffer), <0 = error.
///
/// The packet is decoded before its size is known, so a >0 return keeps the
/// rendered audio and the retry hands it back without decoding the packet a
/// second time. A host that moves on to the next packet instead loses this
/// packet's audio, but the stream stays in step.
///
/// `*out_pts_us` is where the returned audio sits in the stream, from the
/// samples decoded since `orender_create` or the last `orender_reset`;
/// `orender_output_packet_pts` gives the `pts_us` passed with the packet it
/// was decoded from, carried through untouched (ABI.md, "Output timestamps").
///
/// With the `decode_thread` option on (see `orender_set_option`) a packet's
/// audio comes back from a later call - one packet's per call, about 30 ms of
/// audio behind, or one packet if that is longer; occasionally two while the
/// queue shrinks, so size `out` for two packets' audio (a smaller buffer gets
/// the >0 return above and a retry) - or from `orender_drain`: take the
/// timestamps from one of those two, not from the packet just passed in, and
/// drain at end of stream.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_process(
    r: *mut OrenderRenderer,
    pkt: *const u8,
    pkt_len: usize,
    pts_us: i64,
    out: *mut f32,
    out_cap_samples: usize,
    out_frames: *mut usize,
    out_channels: *mut u32,
    out_pts_us: *mut i64,
) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        if r.is_null() || pkt.is_null() || out.is_null() {
            return -1;
        }
        // SAFETY: non-null (checked above) and a live `orender_create` handle.
        let engine = unsafe { &mut *(r as *mut Engine) };
        // SAFETY: non-null, and the caller passes `pkt_len` readable bytes.
        let data = unsafe { std::slice::from_raw_parts(pkt, pkt_len) };

        engine.set_input_pts(Some(pts_us));
        let chunks = match engine.process_raw_within(data, out_cap_samples) {
            Ok(Some(c)) => c,
            Ok(None) => {
                if !out_frames.is_null() {
                    // SAFETY: non-null out-parameter supplied by the caller.
                    unsafe { *out_frames = 0 };
                }
                return 1; // buffer too small; the engine holds the audio for the retry
            }
            Err(e) => {
                eprintln!("orender_process error: {e:#}");
                return -2;
            }
        };

        // SAFETY: `out` is non-null (checked above) and the caller sized it
        // for `out_cap_samples` floats; the out-parameters may be NULL.
        unsafe {
            emit_chunks(
                engine,
                chunks,
                out,
                out_cap_samples,
                out_frames,
                out_channels,
                out_pts_us,
            )
        }
    }))
    .unwrap_or(-100)
}

/// Render what the engine still holds, because the stream is over: with the
/// `decode_thread` option on, the packets it has been handed and not returned
/// yet, and then whatever the decoder is still holding. A bridge cannot always
/// decide an access unit on arrival — an E-AC-3 independent substream may be
/// the first half of a presentation, and only the unit after it says whether
/// it is — so one is held back when the input ends and no further
/// `orender_process` call is coming to release it. One packet's audio per call,
/// as `orender_process` returns it, so a buffer that fits one packet's audio
/// fits a drain too: after the last packet, call it until it returns 0 frames,
/// and play what each call returns.
///
/// Not a reset: the renderer keeps its state, because this audio continues
/// what came before, and a seek, which is meant to discard the audio it seeks
/// away from, calls `orender_reset` instead. Once it has returned 0 frames it
/// keeps returning 0 until new input, and the engine stays usable afterwards,
/// so a host may drain and keep pushing. `out` and the out-parameters are as
/// for `orender_process`.
///
/// Returns: 0 = OK (0 frames: nothing is left), >0 = output buffer too small
/// (nothing written; call drain again with a larger buffer before sending
/// more input — the audio is kept for it, and `orender_process` refuses
/// input until it has been collected), <0 = error. `orender_reset`
/// discards it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_drain(
    r: *mut OrenderRenderer,
    out: *mut f32,
    out_cap_samples: usize,
    out_frames: *mut usize,
    out_channels: *mut u32,
    out_pts_us: *mut i64,
) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        if r.is_null() || out.is_null() {
            return -1;
        }
        // SAFETY: non-null (checked above) and a live `orender_create` handle.
        let engine = unsafe { &mut *(r as *mut Engine) };

        let chunks = match engine.drain_with_capacity(out_cap_samples) {
            Ok(Some(c)) => c,
            Ok(None) => {
                if !out_frames.is_null() {
                    // SAFETY: non-null out-parameter supplied by the caller.
                    unsafe { *out_frames = 0 };
                }
                return 1; // buffer too small; the engine keeps the audio for the retry
            }
            Err(e) => {
                eprintln!("orender_drain error: {e:#}");
                return -2;
            }
        };

        // SAFETY: `out` is non-null (checked above) and the caller sized it
        // for `out_cap_samples` floats; the out-parameters may be NULL.
        unsafe {
            emit_chunks(
                engine,
                chunks,
                out,
                out_cap_samples,
                out_frames,
                out_channels,
                out_pts_us,
            )
        }
    }))
    .unwrap_or(-100)
}

/// Copy rendered blocks into the caller's buffer and report their geometry:
/// the tail shared by `orender_process` and `orender_drain`. Both have
/// checked the capacity by then.
///
/// # Safety
/// `out` must be non-null and valid for `out_cap_samples` floats; the three
/// out-parameters are written only when non-null.
unsafe fn emit_chunks(
    engine: &mut Engine,
    chunks: Vec<orender_engine::engine::RenderedAudio>,
    out: *mut f32,
    out_cap_samples: usize,
    out_frames: *mut usize,
    out_channels: *mut u32,
    out_pts_us: *mut i64,
) -> c_int {
    // SAFETY: guaranteed by the caller (see above).
    let out_slice = unsafe { std::slice::from_raw_parts_mut(out, out_cap_samples) };
    let mut written = 0usize;
    let mut total_frames = 0usize;
    let mut n_channels = engine.channel_count();
    let mut first_sample_pos: Option<u64> = None;
    for chunk in &chunks {
        // An output-mode switch can land between blocks of one packet; a
        // mixed-layout copy would corrupt the frame geometry. Keep the
        // call single-layout and drop the tail (sub-millisecond of audio,
        // once per switch) — the next call carries the new layout.
        if total_frames > 0 && chunk.n_channels != n_channels {
            break;
        }
        out_slice[written..written + chunk.samples.len()].copy_from_slice(&chunk.samples);
        written += chunk.samples.len();
        total_frames += chunk.n_frames;
        n_channels = chunk.n_channels;
        first_sample_pos.get_or_insert(chunk.sample_pos);
    }
    // Copied out (or deliberately skipped, on a layout change): the sample
    // buffers go back to the engine to be filled again next packet, instead
    // of being freed and reallocated ~1200 times a second.
    engine.recycle(chunks);

    // SAFETY (the three writes): non-null out-parameters from the caller.
    if !out_frames.is_null() {
        unsafe { *out_frames = total_frames };
    }
    if !out_channels.is_null() {
        unsafe { *out_channels = n_channels };
    }
    if !out_pts_us.is_null() {
        let sr = engine.sample_rate().max(1) as i64;
        unsafe {
            *out_pts_us = first_sample_pos
                .map(|p| (p as i64) * 1_000_000 / sr)
                .unwrap_or(0)
        };
    }
    0
}

/// The `pts_us` the host passed to `orender_process` with the packet whose
/// audio the last `orender_process` or `orender_drain` call returned.
///
/// Inline, that is the packet the call was given. With the `decode_thread`
/// option on, a packet's audio comes back a few calls later, and this says
/// which packet it was, so a host that stamps its output with its input
/// timestamps stamps it right. When a call returns two packets' audio, it is
/// the first one's.
///
/// Returns 1 and writes `*pts_us` when the last call returned audio; 0 when it
/// returned none (nothing ready yet, a short buffer, end of drain) and after
/// `orender_reset`; -1 on a NULL argument.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_output_packet_pts(
    r: *const OrenderRenderer,
    pts_us: *mut i64,
) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        if r.is_null() || pts_us.is_null() {
            return -1;
        }
        // SAFETY: non-null (checked above) and a live `orender_create` handle.
        let engine = unsafe { &*(r as *const Engine) };
        match engine.last_output_input_pts() {
            Some(pts) => {
                // SAFETY: non-null (checked above) out-parameter.
                unsafe { *pts_us = pts };
                1
            }
            None => 0,
        }
    }))
    .unwrap_or(-1)
}

/// Render the spatial overlay for the given OSD resolution and copy the ASS
/// `osd-overlay` payload into `out` (UTF-8, not nul-terminated).
///
/// This *is* the overlay redraw: each call rebuilds the scene and advances the
/// motion trails, so the host (the mpv Lua shim) must call it exactly once per
/// redraw — typically on a periodic timer and on OSD resize. It also marks the
/// overlay "active" so the engine starts feeding it (the engine does no overlay
/// work until the first pull).
///
/// Returns the number of bytes the payload needs. If `out` is non-NULL and
/// `cap >= len`, the first `len` bytes are written; otherwise nothing is written
/// (the host should grow its buffer and skip this redraw — the next one fits).
/// A handful of KiB is always enough; the output is bounded. Returns 0 when the
/// overlay is disabled, the resolution is zero, or there is nothing to draw.
///
/// Handle-less by design: the overlay is a process-global singleton, and the Lua
/// shim has no session handle (it `ffi.load`s this already-loaded library).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_overlay_ass(
    res_x: u32,
    res_y: u32,
    out: *mut u8,
    cap: usize,
) -> usize {
    catch_unwind(AssertUnwindSafe(|| {
        let ass = orender_engine::overlay::build_ass(res_x, res_y);
        let bytes = ass.as_bytes();
        let n = bytes.len();
        if !out.is_null() && cap >= n {
            // SAFETY: non-null, and the caller's buffer holds `cap >= n` bytes.
            let dst = unsafe { std::slice::from_raw_parts_mut(out, n) };
            dst.copy_from_slice(bytes);
        }
        n
    }))
    .unwrap_or(0)
}

/// Enable or disable the overlay (host keybind / script message). Disabling also
/// makes the engine stop feeding it. `0` = off, non-zero = on.
#[unsafe(no_mangle)]
pub extern "C" fn orender_overlay_set_enabled(enabled: c_int) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        orender_engine::overlay::set_enabled(enabled != 0);
    }));
}

/// Drop all overlay scene state (object positions, levels, trails, labels)
/// without touching the master enable. Used by a host that stops feeding the
/// overlay — e.g. mpv routing channel audio to its native decoder in host mode —
/// so the spatial overlay clears immediately instead of lingering on the last
/// frame until the trails decay. The next pull after feeding resumes shows the
/// live scene again; the user's overlay on/off preference is preserved.
#[unsafe(no_mangle)]
pub extern "C" fn orender_overlay_clear() {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        orender_engine::overlay::clear();
    }));
}

/// Suppress or resume *all* overlay drawing — the wireframe cube included — for a
/// live session, independent of the master enable. A host that keeps the engine
/// alive but is not spatial-rendering (mpv in host mode, decoding channel audio
/// natively) sets `0` so the whole overlay disappears, and `1` when it resumes
/// spatial rendering. `0` = not rendering (blank), non-zero = rendering.
#[unsafe(no_mangle)]
pub extern "C" fn orender_overlay_set_rendering(rendering: c_int) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        orender_engine::overlay::set_rendering(rendering != 0);
    }));
}

// ── overlay toggles (host keybinds) ──────────────────────────────────────────
//
// Each flips the matching control inside the renderer and returns the *new*
// state (1 = on, 0 = off; the heatmap band/colormap variants return the new
// numeric value). Returning the result lets the mpv shim show it in the OSD
// without keeping a mirror that could drift from Studio's OSC pushes. On a panic
// the catch returns a safe default (0).

/// Flip the master enable and return the new state (1 = on, 0 = off).
#[unsafe(no_mangle)]
pub extern "C" fn orender_overlay_toggle() -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        orender_engine::overlay::toggle_enabled() as c_int
    }))
    .unwrap_or(0)
}

/// Flip object-label visibility and return the new state (1 = on, 0 = off).
#[unsafe(no_mangle)]
pub extern "C" fn orender_overlay_toggle_labels() -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        orender_engine::overlay::toggle_labels() as c_int
    }))
    .unwrap_or(0)
}

/// Flip object visibility (markers + labels + trails + depth lines) and return
/// the new state (1 = on, 0 = off).
#[unsafe(no_mangle)]
pub extern "C" fn orender_overlay_toggle_objects() -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        orender_engine::overlay::toggle_objects() as c_int
    }))
    .unwrap_or(0)
}

/// Flip whether motion trails are drawn and return the new state (1 = on,
/// 0 = off). Clears the trail buffers when disabling.
#[unsafe(no_mangle)]
pub extern "C" fn orender_overlay_toggle_trails() -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        orender_engine::overlay::toggle_trails() as c_int
    }))
    .unwrap_or(0)
}

/// Flip the object energy heatmap and return the new state (1 = on, 0 = off).
#[unsafe(no_mangle)]
pub extern "C" fn orender_overlay_toggle_heatmap() -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        orender_engine::overlay::toggle_heatmap() as c_int
    }))
    .unwrap_or(0)
}

/// Advance the heatmap colour gradient to the next index (wraps 0..=4) and return
/// the new index.
#[unsafe(no_mangle)]
pub extern "C" fn orender_overlay_cycle_heatmap_colormap() -> u32 {
    catch_unwind(AssertUnwindSafe(|| {
        orender_engine::overlay::cycle_heatmap_colormap() as u32
    }))
    .unwrap_or(0)
}

/// Step the heatmap depth-plane count by `delta` (clamped to 1..=12) and return
/// the new count.
#[unsafe(no_mangle)]
pub extern "C" fn orender_overlay_adjust_heatmap_bands(delta: i32) -> u32 {
    catch_unwind(AssertUnwindSafe(|| {
        orender_engine::overlay::adjust_heatmap_bands(delta) as u32
    }))
    .unwrap_or(0)
}

/// Render the object energy heatmap as a single flattened BGRA bitmap
/// (premultiplied alpha) for mpv's `overlay-add`, drawn *under* the ASS overlay.
///
/// On success copies `w*h*4` BGRA bytes into `out` and writes the geometry into
/// `geom` (6 × i32: `[x, y, w, h, dw, dh]` — top-left position, source size, and
/// the on-screen display size mpv scales the source to), then returns the number
/// of bytes written. Returns 0 — and writes nothing — when the overlay is
/// disabled, the resolution is zero, the buffers are too small, or there is no
/// audible object. The bitmap is bounded (`FIELD_BITMAP_MAX²·4` ≈ 256 KiB).
///
/// Read-only with respect to the scene: unlike `orender_overlay_ass`, this does
/// not advance trails or the pull clock (the ASS pull already does), so the host
/// may call it alongside the ASS redraw.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_overlay_heatmap_bgra(
    res_x: u32,
    res_y: u32,
    out: *mut u8,
    cap: usize,
    geom: *mut i32,
) -> usize {
    catch_unwind(AssertUnwindSafe(|| {
        if out.is_null() || geom.is_null() {
            return 0;
        }
        let Some(bmp) = orender_engine::overlay::build_heatmap(res_x, res_y) else {
            return 0;
        };
        let n = bmp.pixels.len();
        if n == 0 || n > cap {
            return 0;
        }
        // SAFETY: both non-null (checked above); the caller sized `out` for
        // `cap >= n` bytes and `geom` for 6 `i32`s.
        unsafe { std::slice::from_raw_parts_mut(out, n) }.copy_from_slice(&bmp.pixels);
        let g = unsafe { std::slice::from_raw_parts_mut(geom, 6) };
        g[0] = bmp.x;
        g[1] = bmp.y;
        g[2] = bmp.w;
        g[3] = bmp.h;
        g[4] = bmp.dw;
        g[5] = bmp.dh;
        n
    }))
    .unwrap_or(0)
}

/// ABI major version of the loaded library (see `ORENDER_ABI_MAJOR`). A
/// consumer must refuse a library whose major differs from the one it was
/// compiled against.
#[unsafe(no_mangle)]
pub extern "C" fn orender_version_major() -> u32 {
    ORENDER_ABI_MAJOR
}

/// ABI minor version of the loaded library (backwards-compatible additions;
/// see `ORENDER_ABI_MINOR`). For logging — gate features on symbol presence.
#[unsafe(no_mangle)]
pub extern "C" fn orender_version_minor() -> u32 {
    ORENDER_ABI_MINOR
}

#[cfg(test)]
mod tests {
    use super::OrenderChannelLabel;
    use bridge_api::RChannelLabel;

    /// Exhaustive by construction: a new `RChannelLabel` variant breaks this
    /// match at compile time, forcing the FFI mirror (and thus the generated C
    /// header) to be updated in the same change.
    fn mirror(label: RChannelLabel) -> OrenderChannelLabel {
        match label {
            RChannelLabel::L => OrenderChannelLabel::L,
            RChannelLabel::R => OrenderChannelLabel::R,
            RChannelLabel::C => OrenderChannelLabel::C,
            RChannelLabel::LFE => OrenderChannelLabel::Lfe,
            RChannelLabel::Ls => OrenderChannelLabel::Ls,
            RChannelLabel::Rs => OrenderChannelLabel::Rs,
            RChannelLabel::Tfl => OrenderChannelLabel::Tfl,
            RChannelLabel::Tfr => OrenderChannelLabel::Tfr,
            RChannelLabel::Tsl => OrenderChannelLabel::Tsl,
            RChannelLabel::Tsr => OrenderChannelLabel::Tsr,
            RChannelLabel::Tbl => OrenderChannelLabel::Tbl,
            RChannelLabel::Tbr => OrenderChannelLabel::Tbr,
            RChannelLabel::Lsc => OrenderChannelLabel::Lsc,
            RChannelLabel::Rsc => OrenderChannelLabel::Rsc,
            RChannelLabel::Lb => OrenderChannelLabel::Lb,
            RChannelLabel::Rb => OrenderChannelLabel::Rb,
            RChannelLabel::Cb => OrenderChannelLabel::Cb,
            RChannelLabel::Tc => OrenderChannelLabel::Tc,
            RChannelLabel::Lsd => OrenderChannelLabel::Lsd,
            RChannelLabel::Rsd => OrenderChannelLabel::Rsd,
            RChannelLabel::Lw => OrenderChannelLabel::Lw,
            RChannelLabel::Rw => OrenderChannelLabel::Rw,
            RChannelLabel::Tfc => OrenderChannelLabel::Tfc,
            RChannelLabel::LFE2 => OrenderChannelLabel::Lfe2,
            RChannelLabel::Object => OrenderChannelLabel::Object,
            RChannelLabel::Lh => OrenderChannelLabel::Lh,
            RChannelLabel::Rh => OrenderChannelLabel::Rh,
            RChannelLabel::Ch => OrenderChannelLabel::Ch,
            RChannelLabel::Lhs => OrenderChannelLabel::Lhs,
            RChannelLabel::Rhs => OrenderChannelLabel::Rhs,
            RChannelLabel::Unknown => OrenderChannelLabel::Unknown,
        }
    }

    #[test]
    fn channel_label_discriminants_match_bridge_api() {
        let all = [
            RChannelLabel::L,
            RChannelLabel::R,
            RChannelLabel::C,
            RChannelLabel::LFE,
            RChannelLabel::Ls,
            RChannelLabel::Rs,
            RChannelLabel::Tfl,
            RChannelLabel::Tfr,
            RChannelLabel::Tsl,
            RChannelLabel::Tsr,
            RChannelLabel::Tbl,
            RChannelLabel::Tbr,
            RChannelLabel::Lsc,
            RChannelLabel::Rsc,
            RChannelLabel::Lb,
            RChannelLabel::Rb,
            RChannelLabel::Cb,
            RChannelLabel::Tc,
            RChannelLabel::Lsd,
            RChannelLabel::Rsd,
            RChannelLabel::Lw,
            RChannelLabel::Rw,
            RChannelLabel::Tfc,
            RChannelLabel::LFE2,
            RChannelLabel::Lh,
            RChannelLabel::Rh,
            RChannelLabel::Ch,
            RChannelLabel::Lhs,
            RChannelLabel::Rhs,
            RChannelLabel::Unknown,
        ];
        for label in all {
            assert_eq!(
                label as u8,
                mirror(label) as u8,
                "discriminant mismatch for {label:?}"
            );
        }
    }

    mod resolve_osc {
        use crate::{OrenderConfig, config_file_exists, resolve_osc_opts};
        use orender_engine::RenderConfig;
        use std::ptr;

        /// The env is process-global; serialise the tests that touch it and
        /// restore the previous value so parallel tests never observe a
        /// half-set variable.
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        fn with_env_port<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
            let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let name = "OMNIPHONY_OSC_PORT";
            let previous = std::env::var(name).ok();
            match value {
                Some(v) => unsafe { std::env::set_var(name, v) },
                None => unsafe { std::env::remove_var(name) },
            }
            let out = f();
            match previous {
                Some(v) => unsafe { std::env::set_var(name, v) },
                None => unsafe { std::env::remove_var(name) },
            }
            out
        }

        fn host_cfg() -> OrenderConfig {
            OrenderConfig {
                sample_rate: 0,
                config_yaml_path: ptr::null(),
                speaker_layout_path: ptr::null(),
                bridge_path: ptr::null(),
                codec: ptr::null(),
                osc_enabled: 0,
                osc_port_in: 0,
                osc_port_out: 0,
                osc_bind: ptr::null(),
                osc_host: ptr::null(),
            }
        }

        /// A config file that exists but says nothing about OSC keeps it off,
        /// as before: a save with OSC off leaves the key out.
        #[test]
        fn off_when_an_existing_config_does_not_enable_it() {
            with_env_port(None, || {
                assert!(resolve_osc_opts(&host_cfg(), None, true).is_none());
                let render = RenderConfig::default();
                assert!(resolve_osc_opts(&host_cfg(), Some(&render), true).is_none());
            });
        }

        /// No config file: a first start in a player. OSC comes up on the
        /// built-in rendezvous port, so Studio sees the engine.
        #[test]
        fn on_when_no_config_file_exists() {
            with_env_port(None, || {
                let opts = resolve_osc_opts(&host_cfg(), None, false)
                    .expect("no config file must bring OSC up");
                assert_eq!(opts.port_in, 9000);
                assert_eq!(opts.port_out, 9000);
                assert_eq!(opts.host, "127.0.0.1");
                assert!(!opts.metering);
            });
        }

        /// What the config says still wins over the missing-file default (a
        /// live-handoff sidecar can stand in for a missing `config.yaml`),
        /// and the workflow port still picks the port.
        #[test]
        fn config_and_environment_still_apply_without_a_config_file() {
            with_env_port(Some("9010"), || {
                let off = RenderConfig {
                    osc: Some(false),
                    ..RenderConfig::default()
                };
                assert!(resolve_osc_opts(&host_cfg(), Some(&off), false).is_none());
                let opts = resolve_osc_opts(&host_cfg(), None, false).expect("OSC on");
                assert_eq!((opts.port_in, opts.port_out), (9010, 9010));
            });
        }

        #[test]
        fn config_file_exists_asks_the_file() {
            let dir = std::env::temp_dir()
                .join(format!("orender-ffi-config-exists-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("temp dir");
            let path = dir.join("config.yaml");
            let _ = std::fs::remove_file(&path);
            assert!(!config_file_exists(None), "no path is no config");
            assert!(!config_file_exists(Some(&path)));
            std::fs::write(&path, "render: {}\n").expect("write config");
            assert!(config_file_exists(Some(&path)));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn workflow_port_enables_osc_and_supplies_both_ports() {
            with_env_port(Some("9010"), || {
                let opts = resolve_osc_opts(&host_cfg(), None, true)
                    .expect("OMNIPHONY_OSC_PORT alone must bring OSC up");
                assert_eq!(opts.port_in, 9010);
                assert_eq!(opts.port_out, 9010);
            });
        }

        #[test]
        fn explicit_config_off_beats_the_workflow_port() {
            with_env_port(Some("9010"), || {
                let render = RenderConfig {
                    osc: Some(false),
                    ..RenderConfig::default()
                };
                assert!(resolve_osc_opts(&host_cfg(), Some(&render), true).is_none());
            });
        }

        #[test]
        fn host_port_override_beats_the_workflow_port() {
            with_env_port(Some("9010"), || {
                let cfg = OrenderConfig {
                    osc_port_in: 9020,
                    ..host_cfg()
                };
                let opts = resolve_osc_opts(&cfg, None, true).expect("env enables OSC");
                assert_eq!(opts.port_in, 9020);
                assert_eq!(opts.port_out, 9010);
            });
        }

        #[test]
        fn config_port_beats_the_workflow_port() {
            with_env_port(Some("9010"), || {
                let render = RenderConfig {
                    osc: Some(true),
                    osc_rx_port: Some(9005),
                    ..RenderConfig::default()
                };
                let opts =
                    resolve_osc_opts(&host_cfg(), Some(&render), true).expect("config enables OSC");
                assert_eq!(opts.port_in, 9005);
                assert_eq!(opts.port_out, 9010);
            });
        }
    }
}

/// Human-readable build identifier of the loaded library:
/// `"<crate-version> <git-describe> (built <timestamp>)"`. Static storage,
/// never NULL (a fixed placeholder if the identifier cannot be built) — for
/// host logs, so "which engine did I actually load" is one
/// log line instead of a debugging session.
#[unsafe(no_mangle)]
pub extern "C" fn orender_build_id() -> *const c_char {
    use std::ffi::CString;
    use std::sync::OnceLock;
    static BUILD_ID: OnceLock<CString> = OnceLock::new();
    catch_unwind(AssertUnwindSafe(|| {
        BUILD_ID
            .get_or_init(|| {
                let id = format!(
                    "{} {}",
                    env!("CARGO_PKG_VERSION"),
                    runtime_control::build_fingerprint()
                );
                CString::new(id).unwrap_or_default()
            })
            .as_ptr()
    }))
    .unwrap_or(c"unknown".as_ptr())
}

/// Set a named runtime option on a session — the additive evolution path for
/// the frozen `OrenderConfig`: new knobs get a string key here instead of a
/// struct field, so consumers compiled against older headers keep working and
/// newer consumers can probe.
///
/// Returns 0 on success, -1 for an unknown key (also how a consumer probes
/// whether this build supports a key), -2 for an invalid value, -3 on a NULL
/// handle/argument or internal error.
///
/// Keys:
///
/// - `decode_thread` = `on` | `off` | `live` (ABI 0.10, `live` since 0.11;
///   default `off`): decode on a thread of its own, overlapping the render, so
///   the two share the work across two cores. With it on, a packet's audio
///   comes back from a later `orender_process` call (one packet's per call,
///   about 30 ms of audio behind, or one packet if that is longer;
///   occasionally two while the queue shrinks) or from
///   `orender_drain`, so only a host that takes its timestamps from what the
///   call returns (`*out_pts_us` or `orender_output_packet_pts`, see
///   `orender_process`) and drains at end of stream should turn it on.
///   `on` and `off` force it: switch them while nothing is in flight — right
///   after `orender_create`, after `orender_reset`, or once
///   `orender_drain` has returned 0 frames; turning it off with packets
///   still on the thread returns -2. `live` hands the choice to the user's
///   `render.decode_thread` option (config.yaml, Studio, OSC), which the
///   engine then follows at packet boundaries, winding the thread down a
///   packet per call when it is turned off mid-stream.
/// - `heard_us` = a decimal integer (ABI 0.12): where the listener is, in the
///   microseconds `*out_pts_us` counts — so from 0 after `orender_reset`. A
///   host that buffers the rendered audio plays it later than it renders it;
///   reported as the audio plays, it reaches OSC clients as
///   `/omniphony/playout/heard`, so a client such as Studio can show each block
///   when it is heard rather than when it was rendered. The engine holds
///   nothing back. A host that never sets it changes nothing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn orender_set_option(
    r: *mut OrenderRenderer,
    key: *const c_char,
    value: *const c_char,
) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        if r.is_null() {
            return -3;
        }
        // SAFETY: NULL or nul-terminated strings (caller contract).
        let (Some(key), Some(value)) = (unsafe { opt_str(key) }, unsafe { opt_str(value) }) else {
            return -3;
        };
        // SAFETY: non-null (checked above) and a live `orender_create` handle.
        let engine = unsafe { &mut *(r as *mut Engine) };
        match key {
            "decode_thread" => {
                let mode = match value {
                    "on" => DecodeThreadMode::On,
                    "off" => DecodeThreadMode::Off,
                    "live" => DecodeThreadMode::Live,
                    _ => return -2,
                };
                match engine.set_decode_thread_mode(mode) {
                    Ok(()) => 0,
                    Err(e) => {
                        eprintln!("orender_set_option decode_thread={value}: {e:#}");
                        -2
                    }
                }
            }
            "heard_us" => match value.trim().parse::<i64>() {
                Ok(us) => {
                    engine.set_heard_us(us);
                    0
                }
                Err(_) => -2,
            },
            _ => -1,
        }
    }))
    .unwrap_or(-3)
}

/// Held by every test that creates a session or waits on the degraded
/// reporter: a session that starts stops the process-wide reporter, which a
/// concurrent test may be waiting on.
#[cfg(test)]
static SESSION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod live_handle_tests;

#[cfg(test)]
mod source_label_tests {
    use super::*;

    #[test]
    fn source_label_reports_nothing_without_a_session() {
        let mut buf = [0x7fu8 as c_char; 8];
        unsafe {
            assert_eq!(orender_source_label(ptr::null(), ptr::null_mut(), 0), 0);
            assert_eq!(orender_source_label(ptr::null(), buf.as_mut_ptr(), 8), 0);
        }
        assert_eq!(buf[0], 0x7f, "nothing written for a NULL handle");
    }
}

#[cfg(test)]
mod degraded_reporter_tests {
    use super::*;
    use std::ffi::CString;
    use std::net::UdpSocket;
    use std::time::{Duration, Instant};

    /// Far above the no-bridge renderer build, even in a debug build.
    const READY_DEADLINE: Duration = Duration::from_secs(120);
    const BRIDGE: &str = "/nonexistent/libnone_bridge.so";

    fn free_port() -> u16 {
        UdpSocket::bind("127.0.0.1:0")
            .and_then(|socket| socket.local_addr())
            .expect("free port")
            .port()
    }

    /// `/omniphony/register <port>`, OSC-encoded by hand (this crate does not
    /// link an OSC library).
    fn register_message(port: u16) -> Vec<u8> {
        let mut out = b"/omniphony/register\0".to_vec();
        out.extend_from_slice(b",i\0\0");
        out.extend_from_slice(&i32::from(port).to_be_bytes());
        out
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    /// A bridge that cannot be loaded: `orender_create` still returns NULL
    /// (mpv falls back to its native decoder), and the shared no-bridge
    /// runtime comes up behind it, publishing the bridge error with the host's
    /// launch diagnostics, the bridge path it was asked for and the C-ABI, and
    /// answering a registration over OSC. A real engine start tears it down.
    #[test]
    fn an_unloadable_bridge_returns_null_and_keeps_the_degraded_reporter() {
        let _session = SESSION_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("orender-ffi-degraded-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let config_path = dir.join("config.yaml");
        // A small evaluation grid, so the renderer builds fast in a debug run.
        std::fs::write(
            &config_path,
            "render:\n  evaluation_cartesian_x_size: 9\n  evaluation_cartesian_y_size: 9\n  evaluation_cartesian_z_size: 5\n",
        )
        .expect("write config");
        let config = CString::new(config_path.to_str().expect("utf-8 path")).unwrap();
        let bridge = CString::new(BRIDGE).unwrap();
        let port_in = free_port();
        let cfg = OrenderConfig {
            sample_rate: 48_000,
            config_yaml_path: config.as_ptr(),
            speaker_layout_path: ptr::null(),
            bridge_path: bridge.as_ptr(),
            codec: ptr::null(),
            osc_enabled: 1,
            osc_port_in: port_in,
            osc_port_out: free_port(),
            osc_bind: ptr::null(),
            osc_host: ptr::null(),
        };

        // SAFETY: `cfg` and the strings it points to outlive the call.
        let handle = unsafe { orender_create(&cfg) };
        assert!(handle.is_null(), "no session without a bridge");

        let started = Instant::now();
        let control = loop {
            if let Some(runtime) = DEGRADED_REPORTER.lock().unwrap().as_ref() {
                break runtime.control();
            }
            assert!(
                DEGRADED_ACTIVE.load(Ordering::SeqCst),
                "the degraded reporter failed to start"
            );
            assert!(
                started.elapsed() < READY_DEADLINE,
                "no degraded reporter after {READY_DEADLINE:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        let error = control.bridge_error().expect("bridge error published");
        assert!(error.contains(BRIDGE), "{error}");
        assert!(error.contains("Working dir:"), "{error}");
        assert_eq!(control.bridge_paths(), [PathBuf::from(BRIDGE)]);
        assert_eq!(
            control.host_abi(),
            Some((ORENDER_ABI_MAJOR, ORENDER_ABI_MINOR))
        );
        assert_eq!(control.config_path(), Some(config_path));

        let client = UdpSocket::bind("127.0.0.1:0").expect("client socket");
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let client_port = client.local_addr().unwrap().port();
        client
            .send_to(&register_message(client_port), ("127.0.0.1", port_in))
            .expect("register");
        let mut buf = vec![0u8; 65_536];
        let mut answered = false;
        for _ in 0..64 {
            let Ok((len, _)) = client.recv_from(&mut buf) else {
                break;
            };
            if contains(&buf[..len], b"/omniphony/state/render/bridge_error")
                && contains(&buf[..len], BRIDGE.as_bytes())
            {
                answered = true;
                break;
            }
        }
        assert!(answered, "no bridge error in the registration answer");

        stop_degraded_reporter_global();
        assert!(DEGRADED_REPORTER.lock().unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod describe_tests {
    use super::describe_line;
    use renderer::binaural::brir::{RoomContents, SofaContents};

    fn contents(hrtf: Option<&str>, room: Result<usize, &str>, emitters: usize) -> SofaContents {
        SofaContents {
            conventions: "Some Convention".to_string(),
            prepared: false,
            rate: 44100,
            measurements: 5,
            receivers: 2,
            emitters,
            samples: 9600,
            hrtf_refusal: hrtf.map(str::to_string),
            room: room
                .map(|n| RoomContents {
                    speakers: (0..n).map(|i| format!("E{}", i + 1)).collect(),
                    orientations: 1,
                })
                .map_err(str::to_string),
        }
    }

    /// A few room-length responses, one direction each, suit both stages,
    /// with no reason; each refusal is the one a host needs for the stage
    /// the file does not suit.
    #[test]
    fn the_line_names_both_stages_and_the_reason_that_matters() {
        let (line, code) = describe_line(&contents(None, Ok(2), 1));
        assert_eq!(code, 3);
        assert_eq!(
            line,
            "hrtf=yes room=yes prepared=no conventions=Some_Convention measurements=5 \
             receivers=2 emitters=1 samples=9600 rate=44100 orientations=1 speakers=2 names=E1,E2"
        );

        let (line, code) = describe_line(&contents(Some("no ears"), Err("no room"), 1));
        assert_eq!(code, 0);
        assert!(line.ends_with(" reason=no ears"), "{line}");
        let (line, code) = describe_line(&contents(Some("no ears"), Err("no room"), 4));
        assert_eq!(code, 0);
        assert!(line.ends_with(" reason=no room"), "{line}");
        let (line, code) = describe_line(&contents(None, Err("too many"), 1));
        assert_eq!(code, 1);
        assert!(line.ends_with(" reason=too many"), "{line}");
    }
}
