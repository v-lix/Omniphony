//! Host log sink forwarding, mirroring the reference bridge's logging contract.
//!
//! The host installs a callback via `set_host_log_sink`; the bridge forwards its
//! diagnostics through it so they land in the renderer's log pipeline. When no
//! sink is installed the messages fall back to stderr.

use abi_stable::std_types::RStr;
use bridge_api::{BridgeHostLogSink, RLogLevel};
use std::sync::Mutex;

static HOST_LOG_SINK: Mutex<Option<BridgeHostLogSink>> = Mutex::new(None);

/// Install (or clear, when `sink == 0`) the host-provided log callback.
pub(crate) extern "C" fn register_host_log_sink(sink: usize) {
    let mut slot = HOST_LOG_SINK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    *slot = if sink == 0 {
        None
    } else {
        // SAFETY: the host passes a valid `BridgeHostLogSink` function pointer
        // cast to `usize`, exactly as the reference bridge expects.
        Some(unsafe { std::mem::transmute::<usize, BridgeHostLogSink>(sink) })
    };
}

/// Forward one diagnostic line to the host (or stderr) at `level`.
pub(crate) fn bridge_diag_log(level: log::Level, message: &str) {
    let trimmed = message.trim_end_matches('\n');
    let sink = {
        let slot = HOST_LOG_SINK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *slot
    };
    if let Some(callback) = sink {
        callback(
            encode_log_level(level),
            RStr::from("pcm-bridge::diag"),
            RStr::from(trimmed),
        );
    } else {
        eprintln!("{trimmed}");
    }
}

fn encode_log_level(level: log::Level) -> RLogLevel {
    match level {
        log::Level::Error => RLogLevel::Error,
        log::Level::Warn => RLogLevel::Warn,
        log::Level::Info => RLogLevel::Info,
        log::Level::Debug => RLogLevel::Debug,
        log::Level::Trace => RLogLevel::Trace,
    }
}
