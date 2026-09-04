//! PCM decoder bridge for the orender spatial renderer.
//!
//! This plugin takes **PCM a host has already decoded** and presents it to the
//! engine as a channel bed, so a player that owns its own decoder — Kodi, say,
//! with FFmpeg — can hand the renderer any format it can decode and get a
//! binaural render back. It implements the [`bridge_api`] plugin ABI and is
//! loaded exactly like any other format bridge (`--bridge-path`, or
//! `render.bridge_path` in the config).
//!
//! Input is a byte stream: one [`OPCM` header](header) declaring the geometry,
//! then interleaved samples, in 24-bit-scaled `i32` or 32-bit float. Chunks may
//! split anywhere. [`FormatBridge::reset`] returns the bridge to the header
//! state, which is how a host announces a seek or a format change.
//!
//! # Why not `reference_bridge`
//!
//! `reference_bridge` also carries PCM, wrapped in WAV, but it has to infer
//! speaker positions from the channel count because that is all a plain WAV
//! header offers — six channels are labelled `L R C LFE Ls Rs` whether the
//! source used side or back surrounds. `WAVE_FORMAT_EXTENSIBLE`'s
//! `dwChannelMask` can carry more, at the cost of a lossy translation in both
//! directions: mask bit order is interleave order, so a host must re-sort its
//! layout to match, and positions with no bit (or no [`RChannelLabel`], as with
//! `TOP_BACK_CENTER`) cannot be expressed at all.
//!
//! A host that decoded the stream already knows what every channel is. This
//! bridge lets it simply say so.
//!
//! [`RChannelLabel`]: bridge_api::RChannelLabel
//! [`FormatBridge::reset`]: bridge_api::FormatBridge::reset

#![allow(non_local_definitions)]

mod bridge;
mod header;
mod logging;

use abi_stable::{
    export_root_module, prefix_type::PrefixTypeTrait, sabi_trait::prelude::TD_Opaque,
};
use bridge::PcmBridge;
use bridge_api::{BridgeLib, BridgeLibRef, FormatBridge_TO, FormatBridgeBox};

// `FormatBridge` is used through the proc-macro generated trait object impl.
#[allow(unused_imports)]
use bridge_api::FormatBridge as _FormatBridgeTrait;

/// Plugin entry point: export the root module so the host can load it.
#[export_root_module]
fn get_library() -> BridgeLibRef {
    BridgeLib {
        new_bridge: create_bridge,
        set_host_log_sink,
    }
    .leak_into_prefix()
}

extern "C" fn create_bridge(strict: bool) -> FormatBridgeBox {
    FormatBridge_TO::from_value(PcmBridge::new(strict), TD_Opaque)
}

extern "C" fn set_host_log_sink(sink: usize) {
    logging::register_host_log_sink(sink);
}
