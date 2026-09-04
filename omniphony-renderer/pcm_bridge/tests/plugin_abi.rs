//! Load the built `.so` the way the engine does and drive it across the ABI.
//!
//! The unit tests exercise `PcmBridge` as a Rust type. They cannot show that
//! the plugin is loadable, that the root module is exported where the host
//! looks for it, or that the trait object survives the FFI boundary — which is
//! the part that breaks when an ABI drifts. This test does what
//! `orender_engine::bridge_loader` does: `BridgeLibRef::load_from_file`, then
//! `new_bridge`, then push bytes and read frames back.

use std::path::PathBuf;

use abi_stable::library::RootModule;
use abi_stable::std_types::RSlice;
use bridge_api::{BridgeLibRef, RChannelLabel, RInputTransport};

/// Where cargo left the cdylib for this test run, building it if needed.
///
/// `cargo test` builds the rlib the test links against, not the cdylib the
/// host would `dlopen`, so the artefact this needs may legitimately be absent.
/// Building it here keeps the test self-sufficient rather than silently
/// passing — or silently skipping — when the thing under test is missing.
fn plugin_path() -> PathBuf {
    // The test binary sits in target/<profile>/deps/; the cdylib is one up.
    let mut dir = std::env::current_exe().expect("test binary path");
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    let release = dir.file_name().is_some_and(|n| n == "release");
    let path = dir.join(format!(
        "{}pcm_bridge{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));

    // Build unconditionally rather than returning an existing file. `cargo
    // test` builds the rlib the test links against, not the cdylib a host
    // dlopens, so the artefact on disk can be older than the source being
    // tested - and a test that passes against last week's library is worse than
    // no test. This is a no-op once cargo has nothing to do.
    let mut cargo = std::process::Command::new(env!("CARGO"));
    cargo.args(["build", "-p", "pcm_bridge"]);
    if release {
        cargo.arg("--release");
    }
    let status = cargo.status().expect("running cargo build for the cdylib");
    assert!(status.success(), "cargo build -p pcm_bridge failed");
    assert!(path.exists(), "cargo built no cdylib at {}", path.display());
    path
}

fn header(labels: &[u8], sample_rate: u32, fmt: u8) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"OPCM");
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&(labels.len() as u16).to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.push(fmt);
    buf.push(0);
    buf.extend_from_slice(labels);
    buf
}

fn i32_pcm(frames: &[Vec<i32>]) -> Vec<u8> {
    let mut out = Vec::new();
    for frame in frames {
        for &s in frame {
            out.extend_from_slice(&s.to_le_bytes());
        }
    }
    out
}

#[test]
fn the_built_plugin_loads_and_decodes_over_the_abi() {
    use RChannelLabel::*;

    let path = plugin_path();
    assert!(
        path.exists(),
        "cdylib not built at {}: run `cargo test -p pcm_bridge`",
        path.display()
    );

    let lib = BridgeLibRef::load_from_file(&path)
        .unwrap_or_else(|e| panic!("loading {}: {e}", path.display()));

    // `false`, as the host constructs it: bridge_loader passes that always, so
    // it is the only mode worth proving across the boundary.
    let mut bridge = lib.new_bridge()(false);

    // 5.1 with back surrounds — the layout a channel count alone cannot
    // distinguish from 5.1 with side surrounds.
    let mut stream = header(&[0, 1, 2, 3, 14, 15], 48_000, 0);
    stream.extend(i32_pcm(&[
        vec![1, 2, 3, 4, 5, 6],
        vec![7, 8, 9, 10, 11, 12],
    ]));

    let result = bridge.push_packet(RSlice::from_slice(&stream), RInputTransport::Raw, 0);
    assert!(
        result.error_message.is_empty(),
        "{}",
        result.error_message.as_str()
    );
    assert!(!result.did_reset);
    assert_eq!(result.frames.len(), 1);

    let frame = &result.frames[0];
    assert_eq!(frame.sample_count, 2);
    assert_eq!(frame.channel_count, 6);
    assert_eq!(frame.sampling_frequency, 48_000);
    assert_eq!(
        frame.channel_labels.as_slice(),
        &[L, R, C, LFE, Lb, Rb],
        "the host's labels must cross the boundary unchanged"
    );
    assert_eq!(
        frame.pcm.as_slice(),
        &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
    );
    assert!(
        frame.metadata.is_empty(),
        "a bed frame must carry no metadata, or the renderer takes it for objects"
    );

    assert!(bridge.is_ready());
    assert!(!bridge.has_objects());

    // The seek path: reset, then a header of a different shape.
    bridge.reset();
    let mut second = header(&[0, 1], 96_000, 1);
    second.extend(1.0f32.to_le_bytes());
    second.extend((-1.0f32).to_le_bytes());
    let result = bridge.push_packet(RSlice::from_slice(&second), RInputTransport::Raw, 0);
    assert!(
        result.error_message.is_empty(),
        "{}",
        result.error_message.as_str()
    );
    assert_eq!(result.frames.len(), 1);
    let frame = &result.frames[0];
    assert_eq!(frame.channel_count, 2);
    assert_eq!(frame.sampling_frequency, 96_000);
    assert_eq!(frame.channel_labels.as_slice(), &[L, R]);
    assert_eq!(frame.pcm.as_slice(), &[8_388_607, -8_388_607]);
}
