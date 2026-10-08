//! A session on a prepared room, as a host without head tracking runs one:
//! built on the room's loudspeakers from the start, whatever their number,
//! reporting the room only once it convolves, the same with the decode
//! thread on or off, and leaving nothing of the room's tail across a reset.
//!
//! Runs the reference bridge on the bundled demo (see `common`). The room is
//! synthetic: thirteen loudspeakers placed as BS.2051's 9+4 listening room
//! (the BBC set's layout), one more than the 7.1.4 layout a session is
//! otherwise built on.

mod common;

use std::path::{Path, PathBuf};

use common::{PACKET, collect};
use orender_engine::Engine;
use renderer::binaural::BrirState;
use renderer::binaural::brir::{ExtractedRoom, OrientationSelection, RawRoomIr};

const RATE: u32 = 48_000;
/// Taps per response: a short room keeps the test quick.
const TAPS: usize = 2400;

/// SOFA azimuth (left positive) and elevation of each loudspeaker, degrees.
const ROOM: [(f32, f32); 13] = [
    (0.0, 0.0),
    (30.0, 0.0),
    (-30.0, 0.0),
    (45.0, 0.0),
    (-45.0, 0.0),
    (90.0, 0.0),
    (-90.0, 0.0),
    (135.0, 0.0),
    (-135.0, 0.0),
    (45.0, 40.0),
    (-45.0, 40.0),
    (110.0, 40.0),
    (-110.0, 40.0),
];

/// The thirteen-loudspeaker room, prepared, in a file of its own.
fn prepared_room(dir: &Path) -> PathBuf {
    let sph = |az: f32, el: f32| {
        let (az, el) = (az.to_radians(), el.to_radians());
        [
            2.0 * el.cos() * az.cos(),
            2.0 * el.cos() * az.sin(),
            2.0 * el.sin(),
        ]
    };
    let e = ROOM.len();
    let mut ir = vec![0.0f32; 2 * e * TAPS];
    for ear in 0..2 {
        for k in 0..e {
            let base = (ear * e + k) * TAPS;
            // A direct sound, then a decaying tail: a room, not a delay.
            ir[base + 100 + k] = if ear == 0 { 0.8 } else { 0.6 };
            for t in 200..TAPS {
                let n = ((t * 2_654_435_761 + k * 97 + ear) % 1000) as f32 / 1000.0 - 0.5;
                ir[base + t] = 0.05 * n * (-(t as f32) / 600.0).exp();
            }
        }
    }
    let emitters: Vec<f32> = ROOM.iter().flat_map(|&(az, el)| sph(az, el)).collect();
    let raw = RawRoomIr {
        conventions: "MultiSpeakerBRIR",
        sample_rate: RATE as f32,
        m: 1,
        r: 2,
        e,
        n: TAPS,
        source_position: &[0.0, 0.0, 0.0],
        emitter_position: &emitters,
        listener_position: &[0.0, 0.0, 0.0],
        listener_view: &[1.0, 0.0, 0.0],
        data_ir: &ir,
        ir_first: 0,
        data_delay: &[],
    };
    let room = ExtractedRoom::extract(&raw, OrientationSelection::FrontOnly).unwrap();
    let path = dir.join("brir.room");
    std::fs::write(&path, room.to_prepared()).unwrap();
    path
}

/// An engine on the reference bridge whose config renders `room` on the
/// headphones, and the demo stream's bytes.
fn room_engine(room: &Path, decode_thread: bool) -> (Engine, Vec<u8>, PathBuf) {
    let (bridge, sample) = common::source();
    let dir = room.parent().unwrap().to_path_buf();
    let config = dir.join(format!("config-{decode_thread}.yaml"));
    std::fs::write(
        &config,
        format!(
            "render:\n  osc: false\n  evaluation_cartesian_x_size: 9\n  \
             evaluation_cartesian_y_size: 9\n  evaluation_cartesian_z_size: 5\n  \
             binaural:\n    output_mode: binaural\n    hrir_source: brir\n    \
             brir_sofa_path: '{}'\n",
            room.display()
        ),
    )
    .unwrap();
    let mut engine = Engine::from_paths(
        Some(&config),
        None,
        std::slice::from_ref(&bridge),
        None,
        RATE,
    )
    .expect("build engine");
    engine
        .set_decode_thread(decode_thread)
        .expect("decode thread");
    let data = std::fs::read(&sample).unwrap();
    (engine, data, config)
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("brir-room-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Feed packets until `done` holds (or `limit` packets go by); the frames
/// rendered meanwhile.
fn render_until(
    engine: &mut Engine,
    data: &[u8],
    limit: usize,
    done: impl Fn(&Engine) -> bool,
) -> usize {
    let mut out = Vec::new();
    let mut frames = 0;
    for (i, p) in data.chunks(PACKET).cycle().enumerate() {
        let chunks = engine
            .process_raw_within(p, usize::MAX)
            .expect("process")
            .expect("an unbounded buffer always fits");
        frames += collect(engine, chunks, &mut out);
        if done(engine) || i >= limit {
            break;
        }
        // The room loads, and the layout follows it, on workers: give them
        // the time a real stream would.
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    frames
}

#[test]
fn a_session_is_built_on_a_prepared_rooms_loudspeakers_and_reports_it() {
    let dir = scratch("built");
    let room = prepared_room(&dir);
    for decode_thread in [false, true] {
        let (mut engine, data, _) = room_engine(&room, decode_thread);
        let control = engine.renderer_control();
        // Built on the room: 13 loudspeakers and the LFE, before any audio.
        let topology = control.active_topology();
        assert_eq!(topology.num_speakers, 14, "decode_thread {decode_thread}");
        assert_eq!(topology.speaker_layout.speaker_names().last(), Some(&"LFE"));
        assert_eq!(engine.brir_state(), BrirState::Loading);
        assert!(!engine.brir_rendering());
        // Panned onto the room's loudspeakers for the stand-in meanwhile.
        assert_eq!(engine.render_path(), "cascade:13");

        let frames = render_until(&mut engine, &data, 2000, Engine::brir_rendering);
        assert!(frames > 0, "the stream renders");
        assert!(
            engine.brir_rendering(),
            "the room convolves (decode_thread {decode_thread})"
        );
        assert_eq!(engine.brir_state(), BrirState::Ready);
        assert_eq!(engine.render_path(), "room:13");
        // The follower then moves the session onto the room's own topology:
        // the same loudspeakers, so nothing it was built on is too narrow.
        render_until(&mut engine, &data, 2000, |e| {
            e.renderer_control().active_topology().brir_layout
        });
        let topology = control.active_topology();
        assert!(topology.brir_layout, "the session pans onto the room");
        assert_eq!(topology.num_speakers, 14);
        assert!(engine.brir_rendering(), "and still convolves");
        // The room's block is the reported latency (no crossover here).
        assert_eq!(engine.output_latency_samples(), 127);
        // The HRTF stage is not what convolves: its grid is only the stand-in.
        assert_eq!(engine.hrir_status().effective.as_str(), "saf");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The room's loudspeakers come before a layout the host names: a host
/// passes one for its own render mode (a cascade, say) and cannot know a
/// config has since chosen a room, which would not fit the host's twelve.
#[test]
fn a_room_comes_before_the_hosts_layout() {
    let dir = scratch("host-layout");
    let room = prepared_room(&dir);
    let (_, _, config) = room_engine(&room, false);
    let layout = dir.join("cascade-12.yaml");
    renderer::speaker_layout::SpeakerLayout::preset("cascade-12")
        .unwrap()
        .save_to_file(&layout)
        .unwrap();
    let (bridge, _) = common::source();
    let engine = Engine::from_paths(
        Some(&config),
        Some(&layout),
        std::slice::from_ref(&bridge),
        None,
        RATE,
    )
    .expect("build engine");
    assert_eq!(engine.renderer_control().active_topology().num_speakers, 14);

    // Without a room the host's layout stands.
    let plain = dir.join("plain.yaml");
    std::fs::write(&plain, "render:\n  osc: false\n").unwrap();
    let engine = Engine::from_paths(
        Some(&plain),
        Some(&layout),
        std::slice::from_ref(&bridge),
        None,
        RATE,
    )
    .expect("build engine");
    assert_eq!(engine.renderer_control().active_topology().num_speakers, 13);
    assert_eq!(engine.render_path(), "speakers:12");
    // On the headphones each object is its own direction unless cascaded.
    std::fs::write(
        &plain,
        "render:\n  osc: false\n  binaural:\n    output_mode: binaural\n",
    )
    .unwrap();
    let engine = Engine::from_paths(
        Some(&plain),
        Some(&layout),
        std::slice::from_ref(&bridge),
        None,
        RATE,
    )
    .expect("build engine");
    assert_eq!(engine.render_path(), "direct");
    std::fs::write(
        &plain,
        "render:\n  osc: false\n  binaural:\n    output_mode: binaural\n    mode: cascaded\n",
    )
    .unwrap();
    let engine = Engine::from_paths(
        Some(&plain),
        Some(&layout),
        std::slice::from_ref(&bridge),
        None,
        RATE,
    )
    .expect("build engine");
    assert_eq!(engine.render_path(), "cascade:12");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A room given as its SOFA file is built on its loudspeakers too, from the
/// file's geometry: the vendored room's three and the LFE, not the 7.1.4
/// layout's twelve.
#[cfg(feature = "sofa")]
#[test]
fn a_session_is_built_on_a_sofa_rooms_loudspeakers() {
    let room = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../renderer/tests/sofa/chunked_multispeaker_brir.sofa");
    let dir = scratch("sofa");
    let copy = dir.join("room.sofa");
    std::fs::copy(&room, &copy).unwrap();
    let (engine, _, _) = room_engine(&copy, false);
    let topology = engine.renderer_control().active_topology();
    assert_eq!(topology.num_speakers, 4);
    assert_eq!(topology.speaker_layout.speaker_names().last(), Some(&"LFE"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_room_that_cannot_be_loaded_is_reported_and_the_stand_in_renders() {
    let dir = scratch("missing");
    let room = dir.join("missing.room");
    std::fs::write(&room, b"OMNIROOM").unwrap(); // the magic, then nothing
    let (mut engine, data, _) = room_engine(&room, false);
    // Not readable as a prepared room: built on the default layout.
    assert_eq!(engine.renderer_control().active_topology().num_speakers, 12);
    let mut state = engine.brir_state();
    for p in data.chunks(PACKET).take(400) {
        let chunks = engine.process_raw_within(p, usize::MAX).unwrap().unwrap();
        engine.recycle(chunks);
        state = engine.brir_state();
        if state == BrirState::Failed {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert_eq!(state, BrirState::Failed);
    assert!(!engine.brir_rendering());
    let status = engine.renderer_control().binaural_brir_status();
    assert!(
        status
            .error
            .as_deref()
            .unwrap_or("")
            .contains("prepared room"),
        "{status:?}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The decode thread changes when a packet's audio comes out, never what,
/// on a room as on the HRTF stage: each engine settles on the room, then a
/// reset (a seek) starts both from silence, and the same stream renders the
/// same blocks, sample for sample.
#[test]
fn the_decode_thread_changes_when_a_rooms_audio_comes_out_not_what() {
    let dir = scratch("thread");
    let room = prepared_room(&dir);
    let run = |thread: bool| -> common::Blocks {
        let (mut engine, data, _) = room_engine(&room, thread);
        render_until(&mut engine, &data, 2000, |e| {
            e.brir_rendering() && e.renderer_control().active_topology().brir_layout
        });
        // Let the bands for the room's topology land as well.
        render_until(&mut engine, &data, 100, |_| false);
        engine.reset();
        let mut out = common::Blocks::new();
        for p in data.chunks(PACKET) {
            let chunks = engine
                .process_raw_within(p, usize::MAX)
                .expect("process")
                .expect("an unbounded buffer always fits");
            collect(&mut engine, chunks, &mut out);
        }
        loop {
            let tail = engine.drain().expect("drain");
            if collect(&mut engine, tail, &mut out) == 0 {
                break;
            }
        }
        assert!(
            engine.brir_rendering(),
            "thread {thread}: still on the room"
        );
        out
    };
    let off = run(false);
    let on = run(true);
    assert!(!off.is_empty());
    assert_eq!(off.len(), on.len(), "as many blocks");
    for (i, (a, b)) in off.iter().zip(&on).enumerate() {
        assert_eq!(a.0, b.0, "block {i}: position");
        assert!(a.1 == b.1, "block {i}: samples differ");
    }
    assert!(
        off.iter().any(|(_, s)| s.iter().any(|&v| v != 0.0)),
        "the room renders sound"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
