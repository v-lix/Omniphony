use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use renderer::backend_files;
use renderer::backend_params::ParamValue;
use renderer::live_params::RendererControl;
use renderer::options::OptionSpec;
use rosc::{OscMessage, OscType};
use runtime_control::HostControlHandler;
use runtime_control::command::{RuntimeCommand, parse_process_command};
use runtime_control::context::RuntimeControlContext;
use runtime_control::osc::{
    BroadcastUpdate, BroadcastValue, ControlEffects, Notify, apply_simple_osc_control,
    gaintable_chunk_broadcasts,
};
use runtime_control::osc::{
    parse_bool_arg, parse_f32_arg, parse_nonnegative_u32_arg, parse_positive_u32_arg,
    parse_string_arg,
};
use runtime_control::osc_contract;

use super::client_registry::OscClientRegistry;
use super::export::{build_live_state, export_current_layout, save_live_config};
use super::gaintable::GaintableCache;
use super::recompute::trigger_layout_recompute;
use super::transport::{
    broadcast_blob, broadcast_fff, broadcast_float, broadcast_int, broadcast_string,
    resolve_register_addr, send_diag_state, send_message_to_client, send_metering_state,
    send_update_to_client,
};

#[derive(Default)]
pub(crate) struct RealtimeSeqState {
    pub master_gain: Option<i32>,
    pub speaker_gain: HashMap<usize, i32>,
}

pub(crate) fn handle_control_message(
    msg: &OscMessage,
    src: SocketAddr,
    control: &Arc<RendererControl>,
    host: Option<&Arc<dyn HostControlHandler>>,
    realtime_seq: &mut RealtimeSeqState,
    socket: &Arc<UdpSocket>,
    clients: &Arc<OscClientRegistry>,
    gaintable_cache: &Arc<GaintableCache>,
) {
    let addr = msg.addr.as_str();
    let runtime_ctx = RuntimeControlContext::new(Arc::clone(control));

    // A managed host owns this engine's config, output and process; see
    // `managed_host_refusal`. The client that sent the refused change may
    // already show it, so everyone gets the state that actually holds, and
    // then the reason, as a config save error: the one message upstream Studio
    // shows in plain view, in red by its Save indicator, until the next state
    // update clears it - which is why it goes after the state, not before. An
    // upload's chunks are refused quietly: the begin was already refused and
    // said so, and a file arrives in hundreds of them.
    let rewritten;
    let msg = match managed_host_rewrite(msg, control.host_hrir_source().as_ref()) {
        Some(message) => {
            rewritten = message;
            &rewritten
        }
        None => msg,
    };
    if let Some(manager) = control.managed_host() {
        let refusal = {
            let (output_mode, binaural_mode, decode_thread, current_hrir, active_backend) = {
                let live = control.live.read();
                (
                    live.binaural.output_mode,
                    live.binaural.mode,
                    live.decode_thread,
                    live.binaural.hrir_source.clone(),
                    live.backend_id().to_string(),
                )
            };
            let host_hrir = control.host_hrir_source();
            let state = ManagedState {
                output_mode,
                binaural_mode,
                decode_thread,
                hrir: &current_hrir,
                host_hrir: host_hrir.as_ref(),
            };
            managed_host_refusal(msg, &state, |backend, key| {
                control.is_backend_path_param(backend.unwrap_or(&active_backend), key)
            })
        };
        if let Some(what) = refusal {
            if addr == osc_contract::CONTROL_BINAURAL_HRTF_UPLOAD_CHUNK {
                log::debug!("OSC {addr} refused: {manager} manages {what}");
            } else {
                log::warn!("OSC {addr} refused: {manager} manages {what}");
                build_live_state(control, host).broadcast(socket, clients);
                broadcast_string(
                    socket,
                    clients,
                    osc_contract::STATE_CONFIG_SAVE_ERROR,
                    &managed_host_refusal_notice(&manager, what),
                );
            }
            return;
        }
    }

    // Pure live-state writes (declared live options, monitoring cadences,
    // generator/phantom params, placement): validated and applied by the core;
    // notified and persisted here.
    if let Some(effects) = runtime_control::live_control::apply_live_control(
        msg,
        &runtime_ctx,
        host.map(|h| h.as_ref()),
    ) {
        apply_control_effects(effects, control, host, socket, clients, gaintable_cache);
        return;
    }

    // mpv overlay configuration. The overlay itself is generated in-process by
    // the `overlay` module and pulled over FFI; Studio only configures it here
    // (it no longer transports overlay frames). These are view state
    // (docs/persistence-policy.md): the enabled, labels and trails switches
    // are written to `overlay-prefs.conf` as they change, the rest is
    // transient, and none of them ever marks the config dirty.
    if addr == osc_contract::CONTROL_OVERLAY_ENABLED {
        let enabled = match parse_bool_arg(msg.args.first()) {
            Some(v) => v,
            None => return,
        };
        crate::overlay::set_enabled(enabled);
        return;
    }
    if addr == osc_contract::CONTROL_OVERLAY_LABELS {
        let enabled = match parse_bool_arg(msg.args.first()) {
            Some(v) => v,
            None => return,
        };
        crate::overlay::set_labels_enabled(enabled);
        return;
    }
    if addr == osc_contract::CONTROL_OVERLAY_OBJECTS {
        let visible = match parse_bool_arg(msg.args.first()) {
            Some(v) => v,
            None => return,
        };
        crate::overlay::set_objects_visible(visible);
        return;
    }
    if addr == osc_contract::CONTROL_OVERLAY_HEATMAP_ENABLED {
        let enabled = match parse_bool_arg(msg.args.first()) {
            Some(v) => v,
            None => return,
        };
        crate::overlay::set_heatmap_enabled(enabled);
        return;
    }
    if addr == osc_contract::CONTROL_OVERLAY_HEATMAP_CUSTOM_STOPS {
        // Flat [pos, r, g, b, …] floats → grouped stops for the custom gradient.
        let flat: Vec<f32> = msg
            .args
            .iter()
            .filter_map(|a| match a {
                OscType::Float(f) => Some(*f),
                OscType::Int(i) => Some(*i as f32),
                _ => None,
            })
            .collect();
        let stops: Vec<[f32; 4]> = flat
            .chunks_exact(4)
            .map(|c| [c[0], c[1], c[2], c[3]])
            .collect();
        crate::overlay::set_heatmap_custom_stops(stops);
        return;
    }
    if addr == osc_contract::CONTROL_OVERLAY_HEATMAP_BANDS {
        let count = match parse_positive_u32_arg(msg.args.first()) {
            Some(v) => v as usize,
            None => return,
        };
        crate::overlay::set_heatmap_bands(count);
        return;
    }
    if addr == osc_contract::CONTROL_OVERLAY_HEATMAP_COLORMAP {
        let idx = match parse_nonnegative_u32_arg(msg.args.first()) {
            Some(v) => v as usize,
            None => return,
        };
        crate::overlay::set_heatmap_colormap(idx);
        return;
    }
    if addr == osc_contract::CONTROL_OVERLAY_TRAILS {
        // Args mirror Studio's former wire fields: enabled, ttl_ms, mode, teleport.
        let enabled = match parse_bool_arg(msg.args.first()) {
            Some(v) => v,
            None => return,
        };
        let ttl_ms = parse_nonnegative_u32_arg(msg.args.get(1)).unwrap_or(7000);
        let diffuse = matches!(
            msg.args.get(2),
            Some(OscType::String(s)) if s.eq_ignore_ascii_case("diffuse")
        );
        let teleport = parse_f32_arg(msg.args.get(3)).unwrap_or(0.0) as f64;
        crate::overlay::set_trail_config(enabled, ttl_ms, diffuse, teleport);
        return;
    }
    if addr == osc_contract::CONTROL_OVERLAY_TAG {
        // [id, tag]: tag "A"/"B" sets an override colour, anything else clears it.
        let Some(id) = msg.args.first().and_then(|a| match a {
            OscType::Int(v) if *v >= 0 => Some(*v as u32),
            OscType::Float(v) if *v >= 0.0 => Some(*v as u32),
            OscType::String(s) => s.parse::<u32>().ok(),
            _ => None,
        }) else {
            return;
        };
        let tag = match msg.args.get(1) {
            Some(OscType::String(s)) => s
                .chars()
                .next()
                .filter(|c| matches!(c, 'A' | 'a' | 'B' | 'b')),
            _ => None,
        };
        crate::overlay::set_tag(id, tag);
        return;
    }
    // Speaker gain-table pub/sub. A client subscribes for one speaker (the heatmap
    // shows one), carrying the version it has cached; the renderer pushes that
    // speaker's per-band field only if the version differs, and keeps pushing on
    // every topology rebuild while subscribed (see `recompute.rs`). Args:
    // [Int have_version, Int speaker_index].
    if addr == osc_contract::CONTROL_DEBUG_SPEAKER_GAINTABLE_SUBSCRIBE {
        let have_version = parse_nonnegative_u32_arg(msg.args.first());
        // A negative index selects an all-speaker derived field, not an error:
        // the global heatmaps subscribe through the same path as a per-speaker
        // one. Known sentinels pass through; an unknown negative falls back to
        // the energy field so an older client never gets a field it can't read.
        let speaker = match msg.args.get(1) {
            Some(OscType::Int(i)) if *i >= 0 => *i as i64,
            Some(OscType::Int(i))
                if matches!(
                    *i as i64,
                    renderer::band_gaintable::GAIN_DISCONTINUITY_INDEX
                        | renderer::band_gaintable::CENTROID_JUMP_INDEX
                ) =>
            {
                *i as i64
            }
            Some(OscType::Int(_)) => renderer::band_gaintable::GLOBAL_ENERGY_INDEX,
            _ => 0,
        };
        let client = resolve_register_addr(src, &[]);
        // Ensure the client exists in the registry (refreshes liveness) so the
        // subscribe flag sticks and the 5 s heartbeat keeps it alive.
        clients.register(client);
        clients.set_gaintable(client, true);
        // Additive: a client showing several heatmaps subscribes once per
        // target, and each must keep receiving pushes.
        clients.add_gaintable_target(client, speaker);
        push_gaintable_subscribe(
            socket,
            clients,
            gaintable_cache,
            &runtime_ctx,
            client,
            speaker,
            have_version,
        );
        return;
    }

    if addr == osc_contract::CONTROL_DEBUG_SPEAKER_GAINTABLE_UNSUBSCRIBE {
        let client = resolve_register_addr(src, &[]);
        clients.set_gaintable(client, false);
        // Drop the targets too: the next subscribe declares what it wants, and
        // keeping them would push fields nobody is displaying any more.
        clients.clear_gaintable_targets(client);
        return;
    }

    if addr == osc_contract::CONTROL_DEBUG_SPEAKER_GAINTABLE_NACK {
        // Args: Int version, Int missing_index… — resend just the lost chunks for
        // the client's subscribed speaker.
        let mut ints = msg.args.iter().filter_map(|a| match a {
            OscType::Int(i) if *i >= 0 => Some(*i as u32),
            _ => None,
        });
        if let Some(version) = ints.next() {
            let missing: Vec<u32> = ints.collect();
            if !missing.is_empty() {
                let client = resolve_register_addr(src, &[]);
                // Resolve the target from the version the client is missing
                // chunks for, so a NACK is answered with the right field even
                // when several transfers are in flight.
                let target = clients
                    .gaintable_target_for_version(client, version)
                    .unwrap_or(0);
                if let Some((_v, bytes)) = gaintable_cache.bytes_for_target(&runtime_ctx, target) {
                    for update in gaintable_chunk_broadcasts(&bytes, Some((version, missing))) {
                        send_update_to_client(socket, client, &update);
                    }
                }
            }
        }
        return;
    }

    if addr == osc_contract::CONTROL_METERING {
        let enabled = match parse_bool_arg(msg.args.first()) {
            Some(v) => v,
            None => return,
        };
        let client = resolve_register_addr(src, &[]);
        if clients.set_metering(client, enabled) {
            send_metering_state(socket, client, enabled);
        }
        return;
    }

    if addr == osc_contract::CONTROL_DIAG_ENABLED {
        let enabled = match parse_bool_arg(msg.args.first()) {
            Some(v) => v,
            None => return,
        };
        let client = resolve_register_addr(src, &[]);
        if clients.set_diag(client, enabled) {
            send_diag_state(socket, client, enabled);
        }
        return;
    }

    if addr == osc_contract::CONTROL_INPUT_REFRESH {
        build_live_state(control, host).broadcast(socket, clients);
        log::info!("OSC: input state refresh requested");
        return;
    }

    if addr == osc_contract::CONTROL_REALTIME_MASTER_GAIN {
        let Some(value) = msg.args.first().and_then(|arg| match arg {
            OscType::Float(v) => Some(*v),
            OscType::Int(v) => Some(*v as f32),
            _ => None,
        }) else {
            return;
        };
        let Some(seq) = msg.args.get(1).and_then(|arg| match arg {
            OscType::Int(v) => Some(*v),
            _ => None,
        }) else {
            return;
        };
        if realtime_seq.master_gain.is_some_and(|last| seq < last) {
            return;
        }
        // Same setter as `/control/gain`: one validation, one field.
        let Some(value) = runtime_control::osc::set_master_gain(control, value) else {
            return;
        };
        realtime_seq.master_gain = Some(seq);
        // The realtime echo below is for the sender's own sequencing; the
        // other clients read the gain from the live-state bundle, coalesced
        // because a gain slider drag is a burst of writes.
        notify_changed(control, host, socket, clients, Notify::CoalescedSnapshot);
        if let Ok(bytes) = rosc::encoder::encode(&rosc::OscPacket::Message(rosc::OscMessage {
            addr: osc_contract::STATE_REALTIME_MASTER_GAIN.to_string(),
            args: vec![OscType::Float(value), OscType::Int(seq)],
        })) {
            super::transport::send_raw(socket, clients, &bytes);
        }
        return;
    }

    if addr == osc_contract::CONTROL_REALTIME_SPEAKER_GAIN {
        let Some(idx) = msg.args.first().and_then(|arg| match arg {
            OscType::Int(v) if *v >= 0 => Some(*v as usize),
            OscType::Float(v) if *v >= 0.0 => Some(*v as usize),
            _ => None,
        }) else {
            return;
        };
        let Some(value) = msg.args.get(1).and_then(|arg| match arg {
            OscType::Float(v) => Some(*v),
            OscType::Int(v) => Some(*v as f32),
            _ => None,
        }) else {
            return;
        };
        let Some(seq) = msg.args.get(2).and_then(|arg| match arg {
            OscType::Int(v) => Some(*v),
            _ => None,
        }) else {
            return;
        };
        if realtime_seq
            .speaker_gain
            .get(&idx)
            .copied()
            .is_some_and(|last| seq < last)
        {
            return;
        }
        if !value.is_finite() || value < 0.0 {
            log::warn!("OSC speaker gain: rejected value {value}");
            return;
        }
        realtime_seq.speaker_gain.insert(idx, seq);
        control.live.write().speakers.entry(idx).or_default().gain = value;
        control.mark_speaker_params_dirty();
        notify_changed(control, host, socket, clients, Notify::CoalescedSnapshot);
        if let Ok(bytes) = rosc::encoder::encode(&rosc::OscPacket::Message(rosc::OscMessage {
            addr: osc_contract::STATE_REALTIME_SPEAKER_GAIN.to_string(),
            args: vec![
                OscType::Int(idx as i32),
                OscType::Float(value),
                OscType::Int(seq),
            ],
        })) {
            super::transport::send_raw(socket, clients, &bytes);
        }
        return;
    }

    if addr == osc_contract::CONTROL_RENDER_BRIDGE_PATH {
        let value = match msg.args.first() {
            Some(OscType::String(s)) => s.trim(),
            _ => return,
        };
        let next = if value.is_empty() {
            None
        } else {
            Some(std::path::PathBuf::from(value))
        };
        if control.bridge_path() != next {
            control.set_bridge_path(next.clone());
            notify_changed(control, host, socket, clients, Notify::DirtyOnly);
            let state_value = next
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default();
            broadcast_string(
                socket,
                clients,
                osc_contract::STATE_RENDER_BRIDGE_PATH,
                &state_value,
            );
            log::info!(
                "OSC: render.bridge_path → {}",
                next.as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "<auto>".to_string())
            );
        }
        return;
    }

    if addr == osc_contract::CONTROL_RENDER_INPUT_PIPE {
        let value = match msg.args.first() {
            Some(OscType::String(s)) => s.trim(),
            _ => return,
        };
        let next = if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        };
        if control.input_path() != next {
            control.set_input_path(next.clone());
            notify_changed(control, host, socket, clients, Notify::DirtyOnly);
            broadcast_string(
                socket,
                clients,
                osc_contract::STATE_INPUT_PIPE,
                &next.clone().unwrap_or_default(),
            );
            log::info!(
                "OSC: render.input_pipe → {}",
                next.as_deref().unwrap_or("<default>")
            );
        }
        return;
    }

    // Named config profiles: switch / create / delete / rename
    // (docs/config-profiles.md). Handled before the process commands so the
    // profile addresses never fall through to the host.
    if super::profiles::handle_profile_message(msg, control, host, socket, clients, gaintable_cache)
    {
        return;
    }

    if let Some(command) = parse_process_command(msg) {
        match command {
            RuntimeCommand::SaveConfig => save_live_config(control, host, socket, clients),
            RuntimeCommand::ReloadConfig => {
                log::info!("OSC reload_config requested");
                if sys::shutdown::is_restartable() {
                    sys::shutdown::request_restart_from_config();
                } else {
                    super::profiles::reload_config_in_place(
                        control,
                        host,
                        socket,
                        clients,
                        gaintable_cache,
                    );
                }
            }
            RuntimeCommand::Restart => {
                if sys::shutdown::is_restartable() {
                    log::info!("OSC restart requested (live state kept)");
                    sys::shutdown::request_restart_keeping_live();
                } else {
                    // An embedded host owns the pipeline's lifecycle: a new
                    // bridge takes effect when it restarts the renderer.
                    log::info!("OSC restart ignored (embedded host)");
                }
            }
            RuntimeCommand::Quit => {
                log::info!("OSC quit requested");
                sys::shutdown::request_shutdown();
            }
            RuntimeCommand::YieldPort => {
                if sys::shutdown::is_yieldable() {
                    // Instead of shutting down, allocate a dynamic resume port,
                    // tell the requester (mpv) about it, and enter standby: the
                    // render loop releases the RX port + audio output and idles
                    // until a `resume` arrives on that port (mpv exit).
                    match crate::osc::prepare_standby_resume_port() {
                        Some(resume_port) => {
                            let reply = OscMessage {
                                addr: crate::osc::STANDBY_RESUME_REPLY.to_string(),
                                args: vec![OscType::Int(resume_port as i32)],
                            };
                            if let Ok(bytes) =
                                rosc::encoder::encode(&rosc::OscPacket::Message(reply))
                            {
                                let _ = socket.send_to(&bytes, src);
                            }
                            log::info!(
                                "OSC yield_port: entering standby; resume port {resume_port}"
                            );
                            sys::shutdown::request_standby();
                        }
                        None => {
                            log::warn!(
                                "OSC yield_port: could not allocate a resume port; shutting down"
                            );
                            sys::shutdown::request_shutdown();
                        }
                    }
                } else {
                    log::info!("OSC yield_port ignored (instance not yieldable)");
                }
            }
            RuntimeCommand::Resume => {
                log::info!("OSC resume requested");
                sys::shutdown::request_resume();
            }
            RuntimeCommand::SetLogLevel(requested) => {
                live_log::set_runtime_level(requested);
                broadcast_string(
                    socket,
                    clients,
                    osc_contract::STATE_LOG_LEVEL,
                    live_log::current_runtime_level_name(),
                );
                log::info!(
                    "OSC: log_level → {}",
                    live_log::current_runtime_level_name()
                );
            }
        }
        return;
    }

    // Editable backend files (e.g. the scriptable backend's `.lua`). The content
    // is owned by the renderer, so the editor reads/writes it here over OSC; these
    // reply point-to-point to the requester (`src`) rather than broadcasting.
    if addr == osc_contract::CONTROL_BACKEND_FILE_GET {
        handle_backend_file_get(msg, src, control, socket);
        return;
    }
    if addr == osc_contract::CONTROL_BACKEND_FILE_LIST {
        handle_backend_file_list(msg, src, control, socket);
        return;
    }
    if addr == osc_contract::CONTROL_BACKEND_FILE_PUT {
        handle_backend_file_put(msg, src, control, host, socket, clients, gaintable_cache);
        return;
    }

    if let Some(effects) = apply_simple_osc_control(msg, &runtime_ctx) {
        apply_control_effects(effects, control, host, socket, clients, gaintable_cache);
        return;
    }

    // Core didn't handle it — delegate to the host (audio output/input).
    if let Some(effects) = host.and_then(|h| h.handle(addr, msg)) {
        apply_control_effects(effects, control, host, socket, clients, gaintable_cache);
        return;
    }

    if addr == osc_contract::CONTROL_LAYOUT_EXPORT {
        let requested_name = match msg.args.first() {
            Some(OscType::String(s)) if !s.trim().is_empty() => Some(s.trim()),
            _ => None,
        };
        export_current_layout(control, requested_name);
        return;
    }
}

/// What a managed host keeps, as [`managed_host_refusal`] compares a write
/// against it.
pub(crate) struct ManagedState<'a> {
    pub output_mode: renderer::live_params::OutputMode,
    pub binaural_mode: renderer::live_params::BinauralMode,
    pub decode_thread: bool,
    /// The HRIR source in use.
    pub hrir: &'a renderer::binaural::HrirSource,
    /// The one the host's config chose.
    pub host_hrir: Option<&'a renderer::binaural::HrirSource>,
}

/// What an engine with a managed host (`render.managed_host`: Kodi) will not
/// let a client change, named for the log, or `None` for a message it acts on.
///
/// The host writes the whole config for every stream and reads back only
/// stereo: its helper frames each block as two channels and ends the stream on
/// anything else, so the output mode stays. The binaural render mode stays
/// too: the host moves a stream to the cascade when it carries more objects
/// than direct rendering can keep up with, and switching back would undo that.
/// A FIR crossover delays the sound by its latency, which the host does not
/// take off its timestamps, so only the zero-latency LR4 is accepted. The
/// decode thread is the host's to force, and the live option only matters
/// where it does not. These are registry options, reached by their own
/// addresses, `/control/option` and the batch `/control/options` alike, so
/// they are judged by key, as the registry reads each value; a write that
/// leaves one as it is goes through. Saving, profiles, layout export, backend
/// file writes and HRTF uploads would write into the host's directory on the
/// device; quitting, reloading, restarting, and a new bridge or input pipe
/// belong to the host's process. Test signals would play into whatever is
/// showing. A file-backed HRIR source or a backend's path parameter reads a
/// path the host did not choose: the HRIR source already in use, or the one
/// the host configured, may still be sent. `is_path_param(backend, key)`
/// answers for a backend's schema, `None` meaning the active backend. Every
/// other control is live and lasts until the host's next stream.
pub(crate) fn managed_host_refusal(
    msg: &OscMessage,
    state: &ManagedState,
    is_path_param: impl Fn(Option<&str>, &str) -> bool,
) -> Option<&'static str> {
    if let Some(what) = option_writes(msg)
        .into_iter()
        .find_map(|(spec, value)| managed_option_refusal(spec, &msg.args[value], state))
    {
        return Some(what);
    }
    match msg.addr.as_str() {
        osc_contract::CONTROL_BACKEND_PARAM => {
            // `[backend, key, value]`, or `[key, value]` for the active backend,
            // read as the handler reads them: trimmed, a blank backend meaning
            // the active one.
            let (backend, key) = if msg.args.len() >= 3 {
                (
                    parse_string_arg(msg.args.first()),
                    parse_string_arg(msg.args.get(1)),
                )
            } else {
                (None, parse_string_arg(msg.args.first()))
            };
            key.filter(|key| is_path_param(backend.as_deref(), key))
                .map(|_| "which files are read")
        }
        osc_contract::CONTROL_SAVE_CONFIG
        | osc_contract::CONTROL_PROFILE_SWITCH
        | osc_contract::CONTROL_PROFILE_CREATE
        | osc_contract::CONTROL_PROFILE_DELETE
        | osc_contract::CONTROL_PROFILE_RENAME
        | osc_contract::CONTROL_LAYOUT_EXPORT
        | osc_contract::CONTROL_BACKEND_FILE_PUT
        | osc_contract::CONTROL_BINAURAL_HRTF_UPLOAD_BEGIN
        | osc_contract::CONTROL_BINAURAL_HRTF_UPLOAD_CHUNK
        | osc_contract::CONTROL_BINAURAL_HRTF_UPLOAD_END => {
            Some("the configuration and the files on the device")
        }
        osc_contract::CONTROL_QUIT
        | osc_contract::CONTROL_RELOAD_CONFIG
        | osc_contract::CONTROL_RESTART
        | osc_contract::CONTROL_RENDER_BRIDGE_PATH
        | osc_contract::CONTROL_RENDER_INPUT_PIPE => Some("the engine's process and its decoder"),
        osc_contract::CONTROL_SPEAKER_TEST
        | osc_contract::CONTROL_SPEAKER_TEST_IDLE_FEED
        | osc_contract::CONTROL_OBJECT_TEST
        | osc_contract::CONTROL_OBJECT_TEST_CLIP
        | osc_contract::CONTROL_OBJECT_TEST_ROTATION => Some("what plays"),
        _ => None,
    }
}

/// The registry option writes `msg` carries, however it sends them - one pair
/// on `/control/option`, several on `/control/options`, or an option's own
/// address - as the registry reads them: the option, and where its value is in
/// `msg.args`. Empty for any other message, and for a batch the registry
/// refuses whole (an unknown key, a missing value).
fn option_writes(msg: &OscMessage) -> Vec<(&'static OptionSpec, std::ops::Range<usize>)> {
    let addr = msg.addr.as_str();
    if let Some(spec) = renderer::options::find_by_legacy_addr(addr) {
        let arity = spec.kind.arity();
        return if msg.args.len() >= arity {
            vec![(spec, 0..arity)]
        } else {
            Vec::new()
        };
    }
    let single = addr == osc_contract::CONTROL_OPTION;
    if !single && addr != osc_contract::CONTROL_OPTIONS {
        return Vec::new();
    }
    let mut writes = Vec::new();
    let mut at = 0;
    while let Some(key) = msg.args.get(at) {
        let OscType::String(key) = key else {
            return Vec::new();
        };
        let Some(spec) = renderer::options::find(key) else {
            return Vec::new();
        };
        let value = at + 1..at + 1 + spec.kind.arity();
        if value.end > msg.args.len() {
            return Vec::new();
        }
        at = value.end;
        writes.push((spec, value));
        if single {
            break;
        }
    }
    writes
}

/// One registry write, judged as [`managed_host_refusal`] describes, with the
/// value read the way the option's own setter reads it - so every spelling it
/// takes (FIR as `linear_phase`, the output as `speakers`) is caught, and a
/// value it would reject is left for it to reject.
fn managed_option_refusal(
    spec: &OptionSpec,
    args: &[OscType],
    state: &ManagedState,
) -> Option<&'static str> {
    use renderer::binaural::HrirSource;
    use renderer::live_params::{BinauralMode, CrossoverType, OutputMode};
    use renderer::options::{raw_bool, raw_str};
    let value = runtime_control::live_control::WireValue::from_args(spec.kind, args);
    let raw = value.raw()?;
    match spec.key {
        "output_mode" => OutputMode::from_str(raw_str(&raw)?)
            .filter(|mode| *mode != state.output_mode)
            .map(|_| "the output format"),
        "binaural_mode" => BinauralMode::from_str(raw_str(&raw)?)
            .filter(|mode| *mode != state.binaural_mode)
            .map(|_| "the render mode (direct or cascaded)"),
        "decode_thread" => raw_bool(&raw)
            .filter(|on| *on != state.decode_thread)
            .map(|_| "the decode thread"),
        "crossover_type" => (CrossoverType::from_str(raw_str(&raw)?) == Some(CrossoverType::Fir))
            .then_some("the crossover: FIR would put the sound behind the picture"),
        "hrir_source" => {
            let requested = HrirSource::from_str(raw_str(&raw)?)?;
            match &requested {
                HrirSource::Sofa(path) | HrirSource::Brir(path)
                    if !path.trim().is_empty()
                        && &requested != state.hrir
                        && Some(&requested) != state.host_hrir =>
                {
                    Some("which files are read")
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// A managed host's SOFA file for a client's bare `sofa`, by whichever route
/// the HRIR source is sent. Studio's source list sends the bare name, which the
/// live control would take as a SOFA with no file - the built-in set - where
/// the config's `sofa` means the host's own file. `None` leaves the message as
/// it is.
pub(crate) fn managed_host_rewrite(
    msg: &OscMessage,
    host_hrir: Option<&renderer::binaural::HrirSource>,
) -> Option<OscMessage> {
    use renderer::binaural::HrirSource;
    let Some(HrirSource::Sofa(path)) = host_hrir else {
        return None;
    };
    if path.trim().is_empty() {
        return None;
    }
    let mut rewritten: Option<OscMessage> = None;
    for (spec, value) in option_writes(msg) {
        let bare = matches!(msg.args.get(value.start), Some(OscType::String(s))
            if HrirSource::from_str(s) == Some(HrirSource::Sofa(String::new())));
        if spec.key == "hrir_source" && bare {
            rewritten.get_or_insert_with(|| msg.clone()).args[value.start] =
                OscType::String(format!("sofa:{path}"));
        }
    }
    rewritten
}

/// The refusal as a client shows it: `Not changed: Kodi manages the output
/// format`.
pub(crate) fn managed_host_refusal_notice(manager: &str, what: &str) -> String {
    let mut name = manager.trim().chars();
    let manager: String = name
        .next()
        .map(|first| first.to_uppercase().chain(name).collect())
        .unwrap_or_default();
    format!("Not changed: {manager} manages {what}")
}

/// The one notification path for a control write that changed config-backed
/// live state: mark the config dirty, light every client's Save button
/// (`/state/config/saved = 0`), then publish the new value to every client the
/// way `notify` says (see [`Notify`]). Registry options, the core handlers'
/// `ControlEffects`, the host handler's, and the engine-side writes below all
/// land here, so no write can reach one client and leave the others stale.
fn notify_changed(
    control: &Arc<RendererControl>,
    host: Option<&Arc<dyn HostControlHandler>>,
    socket: &Arc<UdpSocket>,
    clients: &Arc<OscClientRegistry>,
    notify: Notify,
) {
    control.mark_dirty();
    broadcast_int(socket, clients, osc_contract::STATE_CONFIG_SAVED, 0);
    publish_changed(control, host, socket, clients, notify);
}

/// Publish a changed live value to every client the way `notify` says. The
/// tail of [`notify_changed`], and the whole of the announcement for a change
/// no Save is for (view or transient state), which leaves the config clean.
fn publish_changed(
    control: &Arc<RendererControl>,
    host: Option<&Arc<dyn HostControlHandler>>,
    socket: &Arc<UdpSocket>,
    clients: &Arc<OscClientRegistry>,
    notify: Notify,
) {
    match notify {
        Notify::Snapshot => build_live_state(control, host).broadcast(socket, clients),
        // Picked up by the OSC loop's live-state generation poll.
        Notify::CoalescedSnapshot => control.bump_live_state(),
        Notify::DirtyOnly => {}
    }
}

fn apply_control_effects(
    effects: ControlEffects,
    control: &Arc<RendererControl>,
    host: Option<&Arc<dyn HostControlHandler>>,
    socket: &Arc<UdpSocket>,
    clients: &Arc<OscClientRegistry>,
    gaintable_cache: &Arc<GaintableCache>,
) {
    // Persist before notifying, so a client reacting to the notification by
    // reading the config finds the change already there.
    runtime_control::persist::persist_ops(control, &effects.persist);
    if effects.mark_dirty {
        notify_changed(control, host, socket, clients, effects.notify);
    } else if effects.publish_only {
        publish_changed(control, host, socket, clients, effects.notify);
    }
    for update in effects.broadcasts {
        match update.value {
            BroadcastValue::Int(value) => broadcast_int(socket, clients, &update.addr, value),
            BroadcastValue::Float(value) => broadcast_float(socket, clients, &update.addr, value),
            BroadcastValue::Fff(a, b, c) => broadcast_fff(socket, clients, &update.addr, a, b, c),
            BroadcastValue::String(value) => {
                broadcast_string(socket, clients, &update.addr, &value)
            }
            BroadcastValue::Blob(bytes) => broadcast_blob(socket, clients, &update.addr, &bytes),
        }
    }
    if let Some(message) = effects.log_message {
        log::info!("{message}");
    }
    if effects.trigger_layout_recompute {
        // A change that affects the backend geometry (triangulation / decorator
        // metrics) bumps the geometry generation so the upcoming recompute rebuilds
        // the gain models. Evaluation-only changes (mode / grid resolution) leave it
        // untouched, letting the recompute reuse the existing models and rebuild
        // only the evaluation wrapper. Bump BEFORE triggering so the plan captures
        // the new generation.
        if !effects.evaluation_only {
            control.bump_geometry_generation();
        }
        trigger_layout_recompute(control, socket, clients, gaintable_cache);
    }
}

/// Max bytes for an editable backend file carried in one OSC datagram. Scripts
/// are tiny, so a save/load stays a single all-or-nothing message (no chunk
/// reassembly), well under the UDP datagram limit.
const BACKEND_FILE_MAX_BYTES: usize = 60_000;

fn str_arg(msg: &OscMessage, index: usize) -> Option<String> {
    match msg.args.get(index) {
        Some(OscType::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// The directory holding the YAML config, used to root the managed file store.
fn backend_file_config_dir(control: &RendererControl) -> Option<PathBuf> {
    control
        .config_path()
        .and_then(|path| path.parent().map(|dir| dir.to_path_buf()))
}

/// Optional opaque request tag. Older clients omit it; malformed tags never
/// grow a reply unboundedly and are treated as absent.
fn backend_file_request_id(msg: &OscMessage, index: usize) -> Option<String> {
    str_arg(msg, index).filter(|id| !id.is_empty() && id.len() <= 64)
}
fn backend_file_reply(mut args: Vec<OscType>, request_id: Option<&str>) -> Vec<OscType> {
    if let Some(id) = request_id {
        args.push(OscType::String(id.to_owned()));
    }
    args
}

fn send_backend_file_error(
    socket: &UdpSocket,
    src: SocketAddr,
    backend_id: &str,
    key: &str,
    request_id: Option<&str>,
    message: impl Into<String>,
) {
    let message = message.into();
    log::warn!("backend file {backend_id}.{key}: {message}");
    send_message_to_client(
        socket,
        src,
        osc_contract::STATE_BACKEND_FILE_ERROR,
        backend_file_reply(
            vec![
                OscType::String(backend_id.to_string()),
                OscType::String(key.to_string()),
                OscType::String(message),
            ],
            request_id,
        ),
    );
}

/// `get [backend_id, key, name?]` → read a file's content on the renderer and
/// reply STATE_BACKEND_FILE_CONTENT to the requester. With an explicit `name` the
/// editor previews any managed-store file; without it, the param's current handle
/// is read. An absolute handle is only honoured for a loopback caller (see
/// [`backend_files::resolve`]).
fn handle_backend_file_get(
    msg: &OscMessage,
    src: SocketAddr,
    control: &Arc<RendererControl>,
    socket: &UdpSocket,
) {
    let (Some(backend_id), Some(key)) = (str_arg(msg, 0), str_arg(msg, 1)) else {
        return;
    };
    let request_id = backend_file_request_id(msg, 3);
    let request_id = request_id.as_deref();
    let handle = match str_arg(msg, 2) {
        Some(name) if !name.trim().is_empty() => name,
        _ => control
            .with_plugin_params(|params| {
                params
                    .get(renderer::plugin::PluginKind::Backend, &backend_id, &key)
                    .and_then(|value| value.as_str().map(str::to_string))
            })
            .unwrap_or_default(),
    };
    let config_dir = backend_file_config_dir(control);
    let allow_absolute = src.ip().is_loopback();
    let Some(path) =
        backend_files::resolve(config_dir.as_deref(), &backend_id, &handle, allow_absolute)
    else {
        send_backend_file_error(
            socket,
            src,
            &backend_id,
            &key,
            request_id,
            "no file selected",
        );
        return;
    };
    match std::fs::read_to_string(&path) {
        Ok(content) => send_message_to_client(
            socket,
            src,
            osc_contract::STATE_BACKEND_FILE_CONTENT,
            backend_file_reply(
                vec![
                    OscType::String(backend_id),
                    OscType::String(key),
                    OscType::String(handle),
                    OscType::String(content),
                ],
                request_id,
            ),
        ),
        Err(e) => send_backend_file_error(
            socket,
            src,
            &backend_id,
            &key,
            request_id,
            format!("read failed: {e}"),
        ),
    }
}

/// `list [backend_id]` → reply STATE_BACKEND_FILE_LIST with the managed store's
/// file names as a JSON array, so the editor can offer them when the renderer is
/// remote (no native Browse).
fn handle_backend_file_list(
    msg: &OscMessage,
    src: SocketAddr,
    control: &Arc<RendererControl>,
    socket: &UdpSocket,
) {
    let Some(backend_id) = str_arg(msg, 0) else {
        return;
    };
    let config_dir = backend_file_config_dir(control);
    let names = backend_files::list(config_dir.as_deref(), &backend_id);
    let json = serde_json::to_string(&names).unwrap_or_else(|_| "[]".to_string());
    send_message_to_client(
        socket,
        src,
        osc_contract::STATE_BACKEND_FILE_LIST,
        vec![OscType::String(backend_id), OscType::String(json)],
    );
}

/// `put [backend_id, key, name, content]` → write the content into the managed
/// store (or, for a loopback caller, an absolute path), persist the handle, and
/// rebuild the backend. Replies STATE_BACKEND_FILE_CONTENT as a save ack; build
/// errors surface through the usual recompute-error banner.
fn handle_backend_file_put(
    msg: &OscMessage,
    src: SocketAddr,
    control: &Arc<RendererControl>,
    host: Option<&Arc<dyn HostControlHandler>>,
    socket: &Arc<UdpSocket>,
    clients: &Arc<OscClientRegistry>,
    gaintable_cache: &Arc<GaintableCache>,
) {
    let (Some(backend_id), Some(key), Some(name)) =
        (str_arg(msg, 0), str_arg(msg, 1), str_arg(msg, 2))
    else {
        return;
    };
    let request_id = backend_file_request_id(msg, 4);
    let request_id = request_id.as_deref();
    let content = str_arg(msg, 3).unwrap_or_default();
    if content.len() > BACKEND_FILE_MAX_BYTES {
        send_backend_file_error(
            socket,
            src,
            &backend_id,
            &key,
            request_id,
            format!(
                "file too large ({} bytes, max {BACKEND_FILE_MAX_BYTES})",
                content.len()
            ),
        );
        return;
    }
    let config_dir = backend_file_config_dir(control);
    let allow_absolute = src.ip().is_loopback();
    let Some(path) =
        backend_files::resolve(config_dir.as_deref(), &backend_id, &name, allow_absolute)
    else {
        send_backend_file_error(
            socket,
            src,
            &backend_id,
            &key,
            request_id,
            "invalid file name",
        );
        return;
    };
    // The handle we persist must resolve back to `path` at build time (which
    // always allows absolute paths): keep an allowed absolute name as-is,
    // otherwise the safe store basename.
    let stored_handle = if allow_absolute && Path::new(name.trim()).is_absolute() {
        name.trim().to_string()
    } else {
        match backend_files::sanitize_name(&name) {
            Some(basename) => basename,
            None => {
                send_backend_file_error(
                    socket,
                    src,
                    &backend_id,
                    &key,
                    request_id,
                    "invalid file name",
                );
                return;
            }
        }
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            send_backend_file_error(
                socket,
                src,
                &backend_id,
                &key,
                request_id,
                format!("cannot create store dir: {e}"),
            );
            return;
        }
    }
    if let Err(e) = std::fs::write(&path, content.as_bytes()) {
        send_backend_file_error(
            socket,
            src,
            &backend_id,
            &key,
            request_id,
            format!("write failed: {e}"),
        );
        return;
    }
    control.set_backend_param(&backend_id, &key, ParamValue::Text(stored_handle.clone()));
    // Ack the save back to the editor.
    send_message_to_client(
        socket,
        src,
        osc_contract::STATE_BACKEND_FILE_CONTENT,
        backend_file_reply(
            vec![
                OscType::String(backend_id.clone()),
                OscType::String(key.clone()),
                OscType::String(stored_handle),
                OscType::String(content),
            ],
            request_id,
        ),
    );
    // Republish state and rebuild the backend with the new content; a bad script
    // surfaces via the recompute-error path like any other build failure.
    apply_control_effects(
        ControlEffects {
            mark_dirty: true,
            trigger_layout_recompute: true,
            log_message: Some(format!("OSC: backend file {backend_id}.{key} saved")),
            ..Default::default()
        },
        control,
        host,
        socket,
        clients,
        gaintable_cache,
    );
}

/// Reply to a gain-table subscribe: push the full chunked table if the client's
/// cached `have_version` is stale (or absent), ack `uptodate` if it already has
/// the current version, or `unavailable` if the active backend has no table.
fn push_gaintable_subscribe(
    socket: &UdpSocket,
    clients: &OscClientRegistry,
    gaintable_cache: &GaintableCache,
    ctx: &RuntimeControlContext,
    client: SocketAddr,
    speaker: i64,
    have_version: Option<u32>,
) {
    match gaintable_cache.bytes_for_target(ctx, speaker) {
        Some((version, bytes)) => {
            if have_version == Some(version) {
                send_update_to_client(
                    socket,
                    client,
                    &BroadcastUpdate {
                        addr: osc_contract::STATE_DEBUG_SPEAKER_GAINTABLE_UPTODATE.to_string(),
                        value: BroadcastValue::Int(version as i32),
                    },
                );
            } else {
                for update in gaintable_chunk_broadcasts(&bytes, None) {
                    send_update_to_client(socket, client, &update);
                }
                clients.set_gaintable_version(client, speaker, version);
            }
        }
        None => send_update_to_client(
            socket,
            client,
            &BroadcastUpdate {
                addr: osc_contract::STATE_DEBUG_SPEAKER_GAINTABLE_UNAVAILABLE.to_string(),
                value: BroadcastValue::String(
                    "{\"reason\":\"no precomputed gain table for the active backend\"}".to_string(),
                ),
            },
        ),
    }
}

#[cfg(test)]
mod backend_file_request_tests {
    use super::*;
    #[test]
    fn request_ids_are_optional_bounded_and_echoed_on_error() {
        let msg = OscMessage {
            addr: String::new(),
            args: vec![OscType::String("id-a".into())],
        };
        assert_eq!(backend_file_request_id(&msg, 0).as_deref(), Some("id-a"));
        assert!(backend_file_request_id(&msg, 1).is_none());
        let too_long = OscMessage {
            addr: String::new(),
            args: vec![OscType::String("x".repeat(65))],
        };
        assert!(backend_file_request_id(&too_long, 0).is_none());
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        for id in [None, Some("id-a")] {
            send_backend_file_error(
                &socket,
                receiver.local_addr().unwrap(),
                "script",
                "file",
                id,
                "test failure",
            );
            let mut bytes = [0; 1024];
            let count = receiver.recv(&mut bytes).unwrap();
            let (_, rosc::OscPacket::Message(reply)) =
                rosc::decoder::decode_udp(&bytes[..count]).unwrap()
            else {
                panic!("expected message");
            };
            assert_eq!(reply.addr, osc_contract::STATE_BACKEND_FILE_ERROR);
            assert_eq!(reply.args.len(), if id.is_some() { 4 } else { 3 });
            if let Some(id) = id {
                assert_eq!(reply.args[3], OscType::String(id.into()));
            }
        }
        let content = vec![
            OscType::String("script".into()),
            OscType::String("file".into()),
            OscType::String("file.lua".into()),
            OscType::String("return 1".into()),
        ];
        assert_eq!(backend_file_reply(content.clone(), None), content);
        assert_eq!(
            backend_file_reply(content, Some("id-b"))[4],
            OscType::String("id-b".into())
        );
    }
}

#[cfg(test)]
mod notify_tests {
    use super::*;
    use renderer::live_params::{LiveEvaluationMode, PreferredEvaluationMode};
    use renderer::spatial_renderer::SpatialRenderer;
    use renderer::spatial_vbap::{DistanceModel, VbapTableMode};
    use renderer::speaker_layout::SpeakerLayout;
    use std::time::Duration;

    /// A real `RendererControl` on 7.1.4 with a trivial cartesian grid (the
    /// live-options conformance fixture).
    fn fixture_control() -> Arc<RendererControl> {
        let layout = SpeakerLayout::preset("7.1.4").expect("7.1.4 preset");
        SpatialRenderer::new(
            layout,
            48_000,
            1,
            1,
            0.0,
            2.0,
            VbapTableMode::Cartesian {
                x_size: 5,
                y_size: 5,
                z_size: 3,
                z_neg_size: 3,
            },
            false,
            true,
            DistanceModel::Linear,
            false,
            1.0,
            1.0,
            0.0,
            1.0,
            false,
            [1.0, 1.0, 1.0],
            1.0,
            1.0,
            0.0,
            0.0,
            false,
            false,
            false,
            1.0,
            1.0,
            PreferredEvaluationMode::PrecomputedCartesian,
            LiveEvaluationMode::PrecomputedCartesian,
            5,
            5,
            3,
            3,
        )
        .expect("fixture renderer")
        .renderer_control()
    }

    /// The engine socket, a registry with two clients (the one that writes
    /// and a bystander), and the bystander's socket.
    struct Wire {
        engine: Arc<UdpSocket>,
        clients: Arc<OscClientRegistry>,
        writer: SocketAddr,
        bystander: UdpSocket,
        gaintable_cache: Arc<GaintableCache>,
    }

    fn wire() -> Wire {
        let engine = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        let writer = UdpSocket::bind("127.0.0.1:0").unwrap();
        let bystander = UdpSocket::bind("127.0.0.1:0").unwrap();
        bystander
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let clients = Arc::new(OscClientRegistry::new(Duration::from_secs(60)));
        clients.register(writer.local_addr().unwrap());
        clients.register(bystander.local_addr().unwrap());
        Wire {
            engine,
            clients,
            writer: writer.local_addr().unwrap(),
            bystander,
            gaintable_cache: Arc::new(GaintableCache::new()),
        }
    }

    fn send(wire: &Wire, control: &Arc<RendererControl>, addr: &str, args: Vec<OscType>) {
        handle_control_message(
            &OscMessage {
                addr: addr.to_string(),
                args,
            },
            wire.writer,
            control,
            None,
            &mut RealtimeSeqState::default(),
            &wire.engine,
            &wire.clients,
            &wire.gaintable_cache,
        );
    }

    /// Every message the bystander receives until the socket goes quiet.
    fn received(socket: &UdpSocket) -> Vec<OscMessage> {
        fn flatten(packet: rosc::OscPacket, out: &mut Vec<OscMessage>) {
            match packet {
                rosc::OscPacket::Message(msg) => out.push(msg),
                rosc::OscPacket::Bundle(bundle) => {
                    for inner in bundle.content {
                        flatten(inner, out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        let mut buf = vec![0u8; 70_000];
        socket
            .set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        while let Ok(len) = socket.recv(&mut buf) {
            let (_, packet) = rosc::decoder::decode_udp(&buf[..len]).expect("valid OSC");
            flatten(packet, &mut out);
        }
        out
    }

    fn state_json(messages: &[OscMessage], addr: &str) -> Option<serde_json::Value> {
        messages.iter().rev().find(|m| m.addr == addr).map(|m| {
            let Some(OscType::String(json)) = m.args.first() else {
                panic!("{addr} carries no JSON");
            };
            serde_json::from_str(json).expect("valid JSON")
        })
    }

    fn saw_dirty(messages: &[OscMessage]) -> bool {
        messages
            .iter()
            .any(|m| m.addr == osc_contract::STATE_CONFIG_SAVED && m.args == [OscType::Int(0)])
    }

    #[test]
    fn a_generator_param_write_reaches_the_other_clients() {
        let control = fixture_control();
        control.live.write().object_generator_id = "pad".to_string();
        let wire = wire();
        let generation = control.live_state_generation();
        send(
            &wire,
            &control,
            osc_contract::CONTROL_OBJECT_GENERATOR_PARAM,
            vec![OscType::String("strength".into()), OscType::Float(0.25)],
        );
        // The Save button lights on every client right away …
        assert!(saw_dirty(&received(&wire.bystander)));
        // … and the value is queued for the OSC loop's next live-state
        // bundle, which is what the loop broadcasts on a generation change.
        assert_ne!(control.live_state_generation(), generation);
        build_live_state(&control, None).broadcast(&wire.engine, &wire.clients);
        let renderer = state_json(&received(&wire.bystander), osc_contract::STATE_RENDERER)
            .expect("bundle carries /state/renderer");
        assert_eq!(
            renderer["objectGeneratorParamValuesById"]["pad"]["strength"],
            0.25
        );
    }

    #[test]
    fn a_monitoring_rate_write_broadcasts_the_bundle_to_the_other_clients() {
        let control = fixture_control();
        let wire = wire();
        send(
            &wire,
            &control,
            osc_contract::CONTROL_METERING_RATE_HZ,
            vec![OscType::Float(12.0)],
        );
        let messages = received(&wire.bystander);
        // A cadence is view state: it never lights the Save button.
        assert!(!saw_dirty(&messages));
        assert!(
            !control
                .config_dirty
                .load(std::sync::atomic::Ordering::Relaxed)
        );
        let monitoring = state_json(&messages, osc_contract::STATE_MONITORING)
            .expect("the bundle went out with the write");
        assert_eq!(monitoring["meterRateHz"], 12.0);
    }

    #[test]
    fn a_registry_option_write_waits_for_save_and_reaches_the_other_clients() {
        let control = fixture_control();
        let wire = wire();
        let dir = std::env::temp_dir().join(format!(
            "orender-dispatch-option-persist-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        std::fs::write(&path, "render:\n  some_future_key: 42\n").unwrap();
        control.set_config_path(path.clone());

        // The legacy dedicated address is an alias of `/control/option`.
        send(
            &wire,
            &control,
            osc_contract::CONTROL_SURROUND_PLACEMENT,
            vec![OscType::String("back".into())],
        );
        let messages = received(&wire.bystander);
        assert!(saw_dirty(&messages));
        let renderer = state_json(&messages, osc_contract::STATE_RENDERER)
            .expect("the bundle went out with the write");
        assert_eq!(renderer["options"]["surround_placement"], "back");
        // An option changes what is heard: it waits for the Save button.
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, "render:\n  some_future_key: 42\n");

        // The same value again changes nothing, so it lights nothing.
        control.mark_clean();
        send(
            &wire,
            &control,
            osc_contract::CONTROL_SURROUND_PLACEMENT,
            vec![OscType::String("back".into())],
        );
        assert!(!saw_dirty(&received(&wire.bystander)));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn both_master_gain_addresses_share_one_validation() {
        let control = fixture_control();
        let wire = wire();
        control.live.write().master_gain = 0.5;
        send(
            &wire,
            &control,
            osc_contract::CONTROL_REALTIME_MASTER_GAIN,
            vec![OscType::Float(f32::NAN), OscType::Int(1)],
        );
        send(
            &wire,
            &control,
            osc_contract::CONTROL_GAIN,
            vec![OscType::Float(-1.0)],
        );
        assert_eq!(control.live.read().master_gain, 0.5);
        assert!(!saw_dirty(&received(&wire.bystander)));

        let generation = control.live_state_generation();
        send(
            &wire,
            &control,
            osc_contract::CONTROL_REALTIME_MASTER_GAIN,
            vec![OscType::Float(0.75), OscType::Int(2)],
        );
        assert_eq!(control.live.read().master_gain, 0.75);
        let messages = received(&wire.bystander);
        assert!(saw_dirty(&messages));
        assert!(
            messages
                .iter()
                .any(|m| m.addr == osc_contract::STATE_REALTIME_MASTER_GAIN)
        );
        assert_ne!(control.live_state_generation(), generation);
    }
}

#[cfg(test)]
mod managed_host_tests {
    use super::*;
    use renderer::binaural::HrirSource;
    use renderer::live_params::{BinauralMode, OutputMode};

    const OPTION: &str = osc_contract::CONTROL_OPTION;
    const OPTIONS: &str = osc_contract::CONTROL_OPTIONS;

    fn msg(addr: &str, args: Vec<OscType>) -> OscMessage {
        OscMessage {
            addr: addr.to_string(),
            args,
        }
    }

    fn s(value: &str) -> OscType {
        OscType::String(value.into())
    }

    /// Kodi's usual state: binaural output, direct rendering, the decode thread
    /// forced on (TrueHD), on `hrir`, with `host_hrir` configured.
    fn judged(
        msg: &OscMessage,
        hrir: &HrirSource,
        host_hrir: Option<&HrirSource>,
    ) -> Option<&'static str> {
        let state = ManagedState {
            output_mode: OutputMode::Binaural,
            binaural_mode: BinauralMode::Direct,
            decode_thread: true,
            hrir,
            host_hrir,
        };
        managed_host_refusal(msg, &state, |_, _| false)
    }

    fn refused(addr: &str, args: Vec<OscType>) -> bool {
        judged(&msg(addr, args), &HrirSource::SafKemar, None).is_some()
    }

    #[test]
    fn output_mode_is_refused_by_every_route() {
        for (addr, args) in [
            (osc_contract::CONTROL_OUTPUT_MODE, vec![s("speakers")]),
            (osc_contract::CONTROL_OUTPUT_MODE, vec![s(" VBAP ")]),
            (OPTION, vec![s("output_mode"), s("speaker")]),
            (
                OPTIONS,
                vec![
                    s("reverb_level"),
                    OscType::Float(0.2),
                    s("output_mode"),
                    s("speaker"),
                ],
            ),
        ] {
            assert!(
                refused(addr, args.clone()),
                "{addr} {args:?} must be refused"
            );
        }
        // Sending the output it already has changes nothing, so it goes through
        // - a batch applying a whole group may well carry it.
        for (addr, args) in [
            (osc_contract::CONTROL_OUTPUT_MODE, vec![s("binaural")]),
            (OPTION, vec![s("output_mode"), s("headphones")]),
            (
                OPTIONS,
                vec![
                    s("output_mode"),
                    s("binaural"),
                    s("reverb_level"),
                    OscType::Float(0.2),
                ],
            ),
        ] {
            assert!(
                !refused(addr, args.clone()),
                "{addr} {args:?} must stay live"
            );
        }
    }

    #[test]
    fn config_files_process_and_test_signals_are_refused() {
        for addr in [
            osc_contract::CONTROL_SAVE_CONFIG,
            osc_contract::CONTROL_RELOAD_CONFIG,
            osc_contract::CONTROL_RESTART,
            osc_contract::CONTROL_QUIT,
            osc_contract::CONTROL_PROFILE_SWITCH,
            osc_contract::CONTROL_PROFILE_CREATE,
            osc_contract::CONTROL_PROFILE_DELETE,
            osc_contract::CONTROL_PROFILE_RENAME,
            osc_contract::CONTROL_LAYOUT_EXPORT,
            osc_contract::CONTROL_BACKEND_FILE_PUT,
            osc_contract::CONTROL_BINAURAL_HRTF_UPLOAD_BEGIN,
            osc_contract::CONTROL_BINAURAL_HRTF_UPLOAD_CHUNK,
            osc_contract::CONTROL_BINAURAL_HRTF_UPLOAD_END,
            osc_contract::CONTROL_RENDER_BRIDGE_PATH,
            osc_contract::CONTROL_RENDER_INPUT_PIPE,
            osc_contract::CONTROL_SPEAKER_TEST,
            osc_contract::CONTROL_SPEAKER_TEST_IDLE_FEED,
            osc_contract::CONTROL_OBJECT_TEST,
            osc_contract::CONTROL_OBJECT_TEST_CLIP,
            osc_contract::CONTROL_OBJECT_TEST_ROTATION,
        ] {
            assert!(refused(addr, vec![]), "{addr} must be refused");
        }
    }

    #[test]
    fn render_mode_decode_thread_and_a_fir_crossover_are_refused() {
        for (addr, args) in [
            (osc_contract::CONTROL_BINAURAL_MODE, vec![s("cascaded")]),
            (OPTION, vec![s("binaural_mode"), s(" Cascade ")]),
            (OPTIONS, vec![s("binaural_mode"), s("virtual_speakers")]),
            (osc_contract::CONTROL_DECODE_THREAD, vec![OscType::Int(0)]),
            (OPTION, vec![s("decode_thread"), OscType::Bool(false)]),
            (OPTIONS, vec![s("decode_thread"), OscType::Float(0.0)]),
            (osc_contract::CONTROL_CROSSOVER_TYPE, vec![s("fir")]),
            (osc_contract::CONTROL_CROSSOVER_TYPE, vec![s(" FIR ")]),
            (
                osc_contract::CONTROL_CROSSOVER_TYPE,
                vec![s("linear_phase")],
            ),
            (OPTION, vec![s("crossover_type"), s("fir")]),
            (OPTION, vec![s("crossover_type"), s(" Linear_Phase ")]),
            (
                OPTIONS,
                vec![
                    s("crossover_fir_transition_ratio"),
                    OscType::Float(0.5),
                    s("crossover_type"),
                    s("linear_phase"),
                ],
            ),
        ] {
            assert!(
                refused(addr, args.clone()),
                "{addr} {args:?} must be refused"
            );
        }
        // What leaves them as they are, the zero-latency crossover, and other
        // options stay available.
        for (addr, args) in [
            (osc_contract::CONTROL_BINAURAL_MODE, vec![s("direct")]),
            (OPTION, vec![s("binaural_mode"), s("objects")]),
            (osc_contract::CONTROL_DECODE_THREAD, vec![OscType::Int(1)]),
            (OPTION, vec![s("decode_thread"), OscType::Bool(true)]),
            (osc_contract::CONTROL_CROSSOVER_TYPE, vec![s("lr4")]),
            (OPTION, vec![s("crossover_type"), s("lr4")]),
            (OPTION, vec![s("crossover_type"), s("iir")]),
            (
                OPTIONS,
                vec![
                    s("crossover_type"),
                    s("lr4"),
                    s("crossover_fir_transition_ratio"),
                    OscType::Float(0.5),
                ],
            ),
            (
                OPTION,
                vec![s("synthetic_objects_enabled"), OscType::Bool(true)],
            ),
        ] {
            assert!(
                !refused(addr, args.clone()),
                "{addr} {args:?} must stay live"
            );
        }
    }

    #[test]
    fn a_batch_the_registry_refuses_whole_is_not_judged() {
        // An unknown key makes the registry drop the whole batch, so nothing in
        // it changes and there is nothing to refuse.
        let args = vec![s("nope"), OscType::Int(1), s("output_mode"), s("speaker")];
        assert!(option_writes(&msg(OPTIONS, args.clone())).is_empty());
        assert!(!refused(OPTIONS, args));
        // Nor is a value missing its key's arguments.
        assert!(option_writes(&msg(OPTION, vec![s("output_mode")])).is_empty());
    }

    #[test]
    fn backend_path_params_are_refused_and_others_stay_live() {
        let param = osc_contract::CONTROL_BACKEND_PARAM;
        let is_path = |backend: Option<&str>, key: &str| {
            backend.unwrap_or("script") == "script" && key == "path"
        };
        let state = ManagedState {
            output_mode: OutputMode::Binaural,
            binaural_mode: BinauralMode::Direct,
            decode_thread: true,
            hrir: &HrirSource::SafKemar,
            host_hrir: None,
        };
        let check =
            |args: Vec<OscType>| managed_host_refusal(&msg(param, args), &state, is_path).is_some();
        // Named backend, and the active one (here the script backend).
        assert!(check(vec![s("script"), s("path"), s("/dev/zero")]));
        assert!(check(vec![s("path"), s("/dev/zero")]));
        // Names are trimmed before they are stored, and a blank backend is the
        // active one, so neither slips past.
        for args in [
            vec![s("script"), s("path "), s("/dev/zero")],
            vec![s(" script\t"), s("path"), s("/dev/zero")],
            vec![s(""), s(" path"), s("/dev/zero")],
            vec![s("  "), s("path"), s("/dev/zero")],
            vec![s(" path "), s("/dev/zero")],
        ] {
            assert!(check(args.clone()), "{args:?} must be refused");
        }
        // A non-path key, or the same key on a backend without a path param.
        assert!(!check(vec![s("script"), s("gain"), OscType::Float(0.5)]));
        assert!(!check(vec![s("vbap"), s("path"), s("x")]));
    }

    #[test]
    fn rendering_controls_stay_live() {
        for (addr, args) in [
            (
                osc_contract::CONTROL_REALTIME_MASTER_GAIN,
                vec![OscType::Float(0.5), OscType::Int(1)],
            ),
            (
                osc_contract::CONTROL_BINAURAL_UNIT_SCALE,
                vec![OscType::Float(2.5)],
            ),
            (
                osc_contract::CONTROL_BINAURAL_REVERB_LEVEL,
                vec![OscType::Float(0.2)],
            ),
            (
                osc_contract::CONTROL_BINAURAL_REFLECTIONS_ENABLED,
                vec![OscType::Int(0)],
            ),
            (
                osc_contract::CONTROL_BINAURAL_EAR_GAIN,
                vec![OscType::Int(0), OscType::Float(1.0)],
            ),
            (osc_contract::CONTROL_HEAD_RECENTER, vec![]),
            (osc_contract::CONTROL_LOUDNESS, vec![OscType::Int(1)]),
            (osc_contract::CONTROL_METERING, vec![OscType::Int(1)]),
            (osc_contract::CONTROL_INPUT_REFRESH, vec![]),
            (osc_contract::CONTROL_BACKEND_FILE_GET, vec![]),
            (osc_contract::CONTROL_CONFIG_LAYOUT, vec![s("{}")]),
            (OPTIONS, vec![s("reverb_level"), OscType::Float(0.2)]),
        ] {
            assert!(!refused(addr, args), "{addr} must stay live");
        }
    }

    #[test]
    fn hrir_source_may_not_name_a_new_file() {
        let hrir = osc_contract::CONTROL_BINAURAL_HRIR_SOURCE;
        // Built-in and parametric sets read no file.
        for source in ["saf", "synthetic", "pinna", "prtf", "sofa", "brir"] {
            assert!(!refused(hrir, vec![s(source)]), "{source} reads no path");
        }
        for (addr, args) in [
            (hrir, vec![s("sofa:/storage/other.sofa")]),
            (hrir, vec![s("brir:/storage/room.sofa")]),
            (
                OPTION,
                vec![s("hrir_source"), s("sofa:/storage/other.sofa")],
            ),
            (
                OPTIONS,
                vec![
                    s("reverb_level"),
                    OscType::Float(0.2),
                    s("hrir_source"),
                    s(" brir:/storage/room.sofa"),
                ],
            ),
        ] {
            assert!(
                refused(addr, args.clone()),
                "{addr} {args:?} must be refused"
            );
        }

        // The set in use may be sent back as it is.
        let kodi = "/storage/.kodi/userdata/omniphony/hrtf.sofa";
        let current = HrirSource::Sofa(kodi.into());
        let same = msg(hrir, vec![s(&format!("sofa:{kodi}"))]);
        assert!(judged(&same, &current, None).is_none());
        let other = msg(hrir, vec![s("sofa:/storage/other.sofa")]);
        assert!(judged(&other, &current, None).is_some());
    }

    #[test]
    fn the_host_sofa_stays_reachable_after_switching_away() {
        let hrir = osc_contract::CONTROL_BINAURAL_HRIR_SOURCE;
        let kodi = "/storage/.kodi/userdata/omniphony/hrtf.sofa";
        let host = HrirSource::Sofa(kodi.into());
        // Now on the built-in set, the host's file is still allowed back ...
        let back = msg(hrir, vec![s(&format!("sofa:{kodi}"))]);
        assert!(judged(&back, &HrirSource::SafKemar, Some(&host)).is_none());
        // ... and another file still is not.
        let other = msg(hrir, vec![s("sofa:/storage/other.sofa")]);
        assert!(judged(&other, &HrirSource::SafKemar, Some(&host)).is_some());
    }

    #[test]
    fn a_bare_sofa_means_the_host_file() {
        let hrir = osc_contract::CONTROL_BINAURAL_HRIR_SOURCE;
        let kodi = "/storage/.kodi/userdata/omniphony/hrtf.sofa";
        let host = HrirSource::Sofa(kodi.into());
        let file = s(&format!("sofa:{kodi}"));
        let rewritten = managed_host_rewrite(&msg(hrir, vec![s("sofa")]), Some(&host))
            .expect("a bare sofa names the host file");
        assert_eq!(rewritten.addr, hrir);
        assert_eq!(rewritten.args, vec![file.clone()]);
        // By the other routes too, in the value's own place.
        let option =
            managed_host_rewrite(&msg(OPTION, vec![s("hrir_source"), s("sofa")]), Some(&host))
                .expect("through /control/option");
        assert_eq!(option.args, vec![s("hrir_source"), file.clone()]);
        let batch = vec![
            s("reverb_level"),
            OscType::Float(0.2),
            s("hrir_source"),
            s("sofa"),
        ];
        let options = managed_host_rewrite(&msg(OPTIONS, batch), Some(&host))
            .expect("through /control/options");
        assert_eq!(
            options.args,
            vec![
                s("reverb_level"),
                OscType::Float(0.2),
                s("hrir_source"),
                file
            ]
        );

        // Anything else is left alone.
        assert!(managed_host_rewrite(&msg(hrir, vec![s("sofa:/x.sofa")]), Some(&host)).is_none());
        assert!(managed_host_rewrite(&msg(hrir, vec![s("saf")]), Some(&host)).is_none());
        assert!(managed_host_rewrite(&msg(hrir, vec![s("sofa")]), None).is_none());
        let built_in = HrirSource::SafKemar;
        assert!(managed_host_rewrite(&msg(hrir, vec![s("sofa")]), Some(&built_in)).is_none());
        let other = osc_contract::CONTROL_OUTPUT_MODE;
        assert!(managed_host_rewrite(&msg(other, vec![s("sofa")]), Some(&host)).is_none());
    }

    #[test]
    fn the_notice_names_the_host_and_what_it_manages() {
        assert_eq!(
            managed_host_refusal_notice("kodi", "the output format"),
            "Not changed: Kodi manages the output format"
        );
    }
}
