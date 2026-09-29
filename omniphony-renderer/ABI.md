# liborender C ABI contract

`orender_ffi` builds the engine's only stable C surface: `liborender.so.<major>`
(Linux) / `orender.dll` (Windows) / `liborender.dylib` (macOS), described by the
generated header `orender_ffi/include/orender.h`. Known consumers: mpv's
`ad_orender.c` decoder + `orender_overlay.c` overlay client, and the smoke test
`orender_ffi/examples/smoke.c`. The `orender` CLI does NOT use this ABI — it
links the engine as a Rust crate.

## Version numbers (who is who)

| Number | Where | Meaning |
|---|---|---|
| ABI major (`ORENDER_ABI_MAJOR`) | `orender_ffi/src/lib.rs`, `#define` in header, `orender_version_major()` | Breaking-change counter. Linux soname `liborender.so.<major>` derives from it (build.rs). |
| ABI minor (`ORENDER_ABI_MINOR`) | same | Additive-change counter. Logging/diagnostics only. |
| Crate version (`orender_ffi/Cargo.toml`) | crate, `orender_build_id()`, `liborender-v*` release tags, Arch `pkgver` | Package/release identity. Moves faster than the ABI pair. |
| Build fingerprint | `orender_build_id()`, `/omniphony/state/render/version` | git-describe + build time; identifies the exact build. |

The ABI pair and the crate version have different lifecycles on purpose: a
release with no header change bumps the crate version only.

## Change policy

- **Additive** (new exported function, new `orender_set_option` key, enum value
  **appended**): bump `ORENDER_ABI_MINOR`. Existing consumers keep working
  unchanged.
- **Breaking** (changing/removing a symbol or its semantics, touching a struct
  layout, reordering/removing enum values): bump `ORENDER_ABI_MAJOR`, reset
  minor to 0. The Linux soname follows automatically; Windows/macOS file names
  do not change — consumers there are protected only by the runtime check.

**`OrenderConfig` is frozen at ABI major 0.** It crosses the boundary by layout
with no size handshake. New knobs go through `orender_set_option` (post-create)
or the config YAML (create-time), never through new struct fields.

**`OrenderChannelLabel` is append-only** and must mirror
`bridge_api::RChannelLabel` exactly — a unit test in `orender_ffi` asserts
discriminant parity and breaks the build when `bridge_api` adds a variant.

## Consumer contract

At load time a consumer must:

1. Resolve `orender_version_major`/`orender_version_minor` first; reject the
   library if they are missing (pre-handshake build).
2. Reject the library if `orender_version_major() != ORENDER_ABI_MAJOR` it was
   compiled against.
3. Gate optional features on **symbol presence** (`dlsym`), not on the minor.
   The minor is for logs. This makes both skew directions degrade gracefully:
   an older library just lacks the newer optional symbols; a newer library
   keeps every old symbol working.
4. Log `orender_build_id()` (when present) and the path the library was loaded
   from.

Probing an `orender_set_option` key: a return of `-1` means "this build does
not know that key" — treat it as feature-unavailable, not as an error.

## Options

`orender_set_option` keys, in the order they were added:

| Key | Values | Since | Meaning |
|---|---|---|---|
| `decode_thread` | `on`, `off` (default), `live` (0.11) | 0.10 | Decode on a thread of its own, overlapping the render, so the two share the work across two cores. A packet's audio then comes back from a later `orender_process` call — one packet's per call, about 30 ms of audio behind, or one packet if that is longer; occasionally two while the queue shrinks, so size the buffer for two — or from `orender_drain`, so only a host that takes its timestamps from what the call returns (see [Output timestamps](#output-timestamps)) and drains at end of stream should turn it on. `on`/`off` force it: switch them while nothing is in flight — right after `orender_create`, after `orender_reset`, or once `orender_drain` has returned 0 frames; turning it off with packets still on the thread returns -2. `live` hands the choice to the user's `render.decode_thread` option (config.yaml, Studio, OSC); the engine follows it at packet boundaries, and when it is turned off mid-stream the thread winds down a packet per call before decoding goes back inline. |
| `heard_us` | a decimal integer | 0.12 | Where the listener is, in the microseconds `*out_pts_us` counts (see [Output timestamps](#output-timestamps)), so from 0 after `orender_reset`. From the first report on, what OSC clients are told about each block — the spatial frame and its objects, the timestamp, the meters — is held until the listener reaches that block, so a client such as Studio shows what is being heard rather than what was just rendered for a buffer ahead of it. Report `0` right after `orender_create` to hold from the first block, then report as the audio plays; `orender_reset` rewinds it to 0 and drops what is held. A host that never sets it gets them as it renders. What is held is bounded: past a few megabytes the oldest goes out early. |

## Output timestamps

A call's audio can be placed two ways, both right whether `decode_thread` is on
or off. Neither is the packet just passed in once the thread is on: its audio
comes back later.

- `*out_pts_us` (`orender_process`, `orender_drain`): where that audio sits in
  the stream, from the samples decoded since `orender_create` or the last
  `orender_reset`, so it starts again from 0 after a reset. For a host that
  keeps its own clock from the start of playback.
- `orender_output_packet_pts` (0.11): the `pts_us` the host passed to
  `orender_process` with the packet that audio was decoded from (the first
  one's, when a call returns two), or return 0 when the call returned none.
  For a host that stamps its output with its demuxer timestamps, as mpv does.
  `orender_process` carries `pts_us` through untouched (read since 0.11), so a
  host with no timestamp for a packet passes a value of its choosing and gets
  it back; mpv uses `INT64_MIN`.

## End of stream

`orender_drain` renders what the engine still holds when the input ends: with
`decode_thread` on, the packets still on the thread. One packet's audio per
call, as `orender_process` returns it, so a buffer that fits one packet's audio
fits a drain too: call it until it returns 0 frames. It is not a reset, and not
a DSP/reverb-tail flush; call `orender_reset` on a seek. A short output buffer
returns 1 with zero frames and keeps the audio: call drain again with a larger
buffer before sending more input — `orender_process` refuses input until it has
been collected. `orender_reset` discards it.

## Bump checklist

1. Edit `ORENDER_ABI_MINOR` (or `MAJOR`) in `orender_ffi/src/lib.rs` and extend
   the changelog comment above it.
2. `cargo build -p orender_ffi` — regenerates `include/orender.h`; commit it.
3. If breaking: expect the soname to change; update packaging (`PKGBUILD`
   symlinks) and warn mpv-omniphony (bundled lib name changes).
4. `cargo test -p orender_ffi` + run `examples/smoke.c` (CI does both).

## Kodi fork additions (C ABI 0.12)

Upstream ABI 8 height-tier labels and ABI 9 `orender_source_label` retain
their values and contracts. This fork adds `orender_decoded_sample_rate`:
the last decoder output rate in Hz, zero before it is reported or for a NULL
handle. It survives a same-stream seek and updates with each decoded frame;
it is not the session rate the host configured. The renderer follows the
stream's rate, so this is the rate the audio comes back at: hosts poll it to
detect when they must reopen at the source rate. Probe the symbol, not minor
12.

`orender_hrir_in_use` names the HRIR set the binaural path is convolving
with, using the `hrir_source` selectors (`saf`, `sofa`, `brir`, ...), with the
`orender_source_label` query/fill convention. It reports the set in use, not
the one configured: a SOFA file that failed to load reads as `saf`. A
configured set is requested with the first rendered block and built off the
audio thread, so the answer is live and can change shortly after a stream
starts.

The fork also adds the `heard_us` option (see Options). Kodi buffers what it
is handed - the codec's reserve, then its audio engine and sink - so the
render runs up to two seconds or more ahead of what is heard, and Studio drew
the objects that far early; the codec reports the position the sink is playing
through its helper instead. Probe the key, not minor 12.

The Rust plugin interface remains `bridge_api` 0.4; its package version is
independent of the C ABI minor. Rebuild the matching renderer and plugins.

`orender_drain` (see End of stream) also releases pending decoder audio in
this fork: once the decode thread holds nothing more, the access unit a bridge
kept back to see what follows it, as the last call before 0 frames. The
contract is unchanged — not a reset, not a DSP/reverb-tail flush, one packet's
audio per call until 0 frames, a short buffer keeps the audio for the retry,
and reset discards it.

`FormatBridge::drain` is appended after upstream 0.4's method prefix and
optional family/label methods, with an empty successful default. Source
implementations without a tail can omit it when rebuilt against this API.
An already-built upstream 0.4 plugin has a shorter method table and is rejected
by the new host's `abi_stable` layout check; the default does not make that
binary loadable. The reverse direction (upstream host loading a rebuilt plugin)
passes the layout check. Keep those checks enabled and rebuild the matching
renderer and plugins together. Bridges that hold an access unit must override
drain to release it; PCM and WAV bridges emit complete sample frames
immediately and have no decoder tail to release.
