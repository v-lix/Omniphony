//! Exercise rate reporting and EOF across the C entry points with a decoder
//! that holds one access unit. No codec corpus, filesystem config or OSC port.
use std::os::raw::c_int;
use std::ptr;

use abi_stable::std_types::{ROption, RSlice, RStr, RString, RVec};
use abi_stable::{prefix_type::PrefixTypeTrait, sabi_trait::prelude::TD_Opaque};
use bridge_api::*;
use orender::{
    orender_channel_count, orender_decoded_sample_rate, orender_drain, orender_hrir_in_use,
    orender_process, orender_reset, OrenderRenderer,
};
use orender_engine::bridge_loader::LoadedBridge;
use orender_engine::renderer_build::{build_spatial_renderer, SpatialRendererParams};
use orender_engine::Engine;
use renderer::speaker_layout::SpeakerLayout;

#[derive(Default)]
struct HoldingBridge {
    pending: Option<RDecodedFrame>,
    fail: bool,
}

fn result(frame: Option<RDecodedFrame>) -> RPushResult {
    RPushResult {
        frames: frame.into_iter().collect(),
        error_message: RString::new(),
        did_reset: false,
    }
}

impl FormatBridge for HoldingBridge {
    fn push_packet(&mut self, data: RSlice<'_, u8>, _: RInputTransport, _: u8) -> RPushResult {
        self.fail = data[0] == 0;
        let frame = RDecodedFrame {
            sampling_frequency: u32::from(data[0]) * 1000,
            sample_count: 4,
            channel_count: 2,
            pcm: RVec::from(vec![1_000_000; 8]),
            channel_labels: RVec::from(vec![RChannelLabel::L, RChannelLabel::R]),
            metadata: RVec::new(),
            drc_gain: 1.0,
            drc_ramp_duration: 0,
            dialogue_level: ROption::RNone,
            is_new_segment: false,
        };
        result(self.pending.replace(frame))
    }
    fn reset(&mut self) {
        self.pending = None;
        self.fail = false;
    }
    fn is_ready(&self) -> bool {
        self.pending.is_some()
    }
    fn has_objects(&self) -> bool {
        false
    }
    fn configure(&mut self, _: RStr<'_>, _: RStr<'_>) -> bool {
        true
    }
    fn coordinate_format(&self) -> RCoordinateFormat {
        RCoordinateFormat::Cartesian
    }
    fn vbap_cartesian_defaults(&self) -> RVbapCartesianDefaults {
        RVbapCartesianDefaults {
            x_size: 3,
            y_size: 3,
            z_size: 3,
            allow_negative_z: false,
        }
    }
    fn preferred_vbap_table_mode(&self) -> RVbapTableMode {
        RVbapTableMode::Cartesian
    }
    fn supported_drc_modes(&self) -> RVec<RString> {
        RVec::new()
    }
    fn set_drc_mode(&mut self, _: RStr<'_>) -> bool {
        false
    }
    fn fixed_channel_poses(&self) -> RVec<RChannelPose> {
        RVec::new()
    }
    fn source_family(&self) -> RString {
        "pcm".into()
    }
    fn drain(&mut self) -> RPushResult {
        if self.fail {
            let mut error = result(None);
            error.error_message = "fixture decoder failure".into();
            error
        } else {
            result(self.pending.take())
        }
    }
}

extern "C" fn new_bridge(_: bool) -> FormatBridgeBox {
    FormatBridge_TO::from_value(HoldingBridge::default(), TD_Opaque)
}
extern "C" fn log_sink(_: usize) {}

fn engine() -> Box<Engine> {
    let bridge = new_bridge(false);
    let renderer = build_spatial_renderer(
        &SpatialRendererParams::from_render_config(None),
        SpeakerLayout::preset_stereo().unwrap(),
        48_000,
        bridge.vbap_cartesian_defaults(),
        bridge.preferred_vbap_table_mode(),
        None,
    )
    .unwrap();
    let lib = BridgeLib {
        new_bridge,
        set_host_log_sink: log_sink,
    }
    .leak_into_prefix();
    Box::new(Engine::new(LoadedBridge { lib, bridge }, renderer, 48_000))
}

unsafe fn feed(r: *mut OrenderRenderer, rate_khz: u8) -> (c_int, usize) {
    let mut out = [0.0; 64];
    let mut frames = usize::MAX;
    let rc = orender_process(
        r,
        &rate_khz,
        1,
        0,
        out.as_mut_ptr(),
        out.len(),
        &mut frames,
        ptr::null_mut(),
        ptr::null_mut(),
    );
    (rc, frames)
}

#[test]
fn drain_retries_keep_audio_and_timestamps_without_decoding_twice() {
    let mut engine = engine();
    let r = (&mut *engine as *mut Engine).cast::<OrenderRenderer>();
    unsafe {
        let expected_channels = orender_channel_count(r);
        assert_eq!(orender_decoded_sample_rate(r), 0);
        assert_eq!(feed(r, 96), (0, 0));
        assert_eq!(orender_decoded_sample_rate(r), 0);
        assert_eq!(feed(r, 48), (0, 4));
        assert_eq!(orender_decoded_sample_rate(r), 96_000);
        let mut out = [f32::NAN; 64];
        let mut frames = usize::MAX;
        let mut channels = 0;
        let mut pts = -1;
        for _ in 0..2 {
            assert_eq!(
                orender_drain(r, out.as_mut_ptr(), 0, &mut frames, &mut channels, &mut pts),
                1
            );
            assert_eq!(frames, 0);
            assert!(out.iter().all(|s| s.is_nan()));
        }
        assert_eq!(orender_decoded_sample_rate(r), 48_000);
        assert!(
            feed(r, 48).0 < 0,
            "pending tail must not be overtaken by input"
        );
        assert_eq!(
            orender_drain(
                r,
                out.as_mut_ptr(),
                out.len(),
                &mut frames,
                &mut channels,
                &mut pts
            ),
            0
        );
        assert_eq!((frames, channels, pts), (4, expected_channels, 83));
        assert!(out[..frames * channels as usize]
            .iter()
            .all(|s| s.is_finite()));
        assert_eq!(
            orender_drain(
                r,
                out.as_mut_ptr(),
                out.len(),
                &mut frames,
                &mut channels,
                &mut pts
            ),
            0
        );
        assert_eq!(frames, 0, "the tail must be emitted only once");
        assert_eq!(feed(r, 48), (0, 0), "decoder remains usable after drain");
    }
}

#[test]
fn reset_discards_pending_tail_and_drain_reports_decoder_errors() {
    let mut engine = engine();
    let r = (&mut *engine as *mut Engine).cast::<OrenderRenderer>();
    unsafe {
        assert_eq!(feed(r, 96), (0, 0));
        let mut out = [0.0; 64];
        let mut frames = 0;
        let mut pts = -1;
        assert_eq!(
            orender_drain(
                r,
                out.as_mut_ptr(),
                0,
                &mut frames,
                ptr::null_mut(),
                &mut pts
            ),
            1
        );
        orender_reset(r);
        assert_eq!(
            orender_decoded_sample_rate(r),
            96_000,
            "same-stream seek retains the last known rate"
        );
        assert_eq!(
            orender_drain(
                r,
                out.as_mut_ptr(),
                out.len(),
                &mut frames,
                ptr::null_mut(),
                &mut pts
            ),
            0
        );
        assert_eq!(frames, 0);
        assert_eq!(feed(r, 48), (0, 0));
        assert_eq!(
            orender_drain(
                r,
                out.as_mut_ptr(),
                out.len(),
                &mut frames,
                ptr::null_mut(),
                &mut pts
            ),
            0
        );
        assert_eq!((frames, pts), (4, 0));
        assert_eq!(feed(r, 0), (0, 0));
        assert_eq!(
            orender_drain(
                r,
                out.as_mut_ptr(),
                out.len(),
                &mut frames,
                ptr::null_mut(),
                &mut pts
            ),
            -2
        );
    }
}

#[test]
fn hrir_in_use_names_the_convolved_set_with_the_label_convention() {
    let mut engine = engine();
    let r = (&mut *engine as *mut Engine).cast::<OrenderRenderer>();
    unsafe {
        assert_eq!(orender_hrir_in_use(ptr::null(), ptr::null_mut(), 0), 0);

        // Nothing configured: the embedded KEMAR set the engine starts with.
        let n = orender_hrir_in_use(r, ptr::null_mut(), 0);
        assert_eq!(n, 3);

        // cap == N writes nothing; cap > N writes the name and its NUL.
        let mut buf = [0x7f as std::os::raw::c_char; 8];
        assert_eq!(orender_hrir_in_use(r, buf.as_mut_ptr(), n), n);
        assert!(buf.iter().all(|&c| c == 0x7f));
        assert_eq!(orender_hrir_in_use(r, buf.as_mut_ptr(), n + 1), n);
        let name = std::ffi::CStr::from_ptr(buf.as_ptr()).to_str().unwrap();
        assert_eq!(name, "saf");
    }
}
