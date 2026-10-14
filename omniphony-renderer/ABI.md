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
| Crate version (the release version, `[workspace.package]`) | crate, `orender_build_id()`, the `vX.Y.Z` release and its `liborender-vX.Y.Z-<platform>.zip` assets, Arch `pkgver` | Package/release identity, shared with Studio and `orender` (#676). Moves faster than the ABI pair. |
| Build fingerprint | `orender_build_id()`, `/omniphony/state/render/version` | git-describe + build time; identifies the exact build. |

The ABI pair and the crate version have different lifecycles on purpose: a
release with no header change bumps the crate version only. Until 0.6.0 the
library had its own `liborender-v*` releases; it now ships as assets of every
`v*` release, and the README's compatibility table (and the release's
`omniphony-<tag>-manifest.json`) names the ABI pair each release carries.

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
| `heard_us` | a decimal integer | 0.12 | Where the listener is, in the microseconds `*out_pts_us` counts (see [Output timestamps](#output-timestamps)), so from 0 after `orender_reset`. A host that buffers what it is handed plays it later than it is rendered — Kodi banks seconds of it — and only the host knows how much. Reported as the audio plays (a few dozen times a second is plenty), it reaches OSC clients as `/omniphony/playout/heard`, together with `/omniphony/playout/block` markers naming the block each stream message describes, so a client such as Studio can show each block when it is heard. The engine holds nothing back; a host that never sets it changes nothing. |

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

## Fork additions (C ABI 0.13)

Upstream ABI 8 to 12 entry points retain their values and contracts. This
fork adds `orender_decoded_sample_rate`: the last decoder output rate in Hz,
zero before it is reported or for a NULL handle. It survives a same-stream
seek and updates with each decoded frame; it is not the session rate the host
configured. The renderer follows the stream's rate, so this is the rate the
audio comes back at: hosts poll it to detect when they must reopen at the
source rate. Probe the symbol, not minor 13.

`orender_hrir_in_use` names the HRIR set the binaural path is convolving
with, using the `hrir_source` selectors (`saf`, `sofa`, `brir`, ...), with the
`orender_source_label` query/fill convention. It reports the set in use, not
the one configured: a SOFA file that failed to load reads as `saf`. A
configured set is requested with the first rendered block and built off the
audio thread, so the answer is live and can change shortly after a stream
starts. It reads `brir` only once a frame was convolved with a room; while a
room loads, or after it was refused, the HRTF stage renders its virtual
array on the embedded set and it reads `saf`.

### Measured rooms

`orender_brir_prepare(sofa, len, kept_path, out_path, summary, cap)`
prepares a room once, so that a session does not open a room-response SOFA
file (hundreds of megabytes) every time it starts. It first describes the
bytes as `orender_sofa_describe` does, in the same pass over them, so a host
that chooses a room asks this alone: what the room stage does not take (an
HRTF set, a prepared room, or neither) returns 2 with that description, and
nothing is prepared. When `kept_path` (NULL allowed) is the room a host
keeps for the file, of this engine's format and prepared from these same
bytes, it returns 1 and writes nothing: the kept room stays. It reads the
file's geometry, then only
the measurements of the head orientation nearest straight ahead (what a
session without head tracking renders): the BBC 7.1.4 set (274 MB) prepares
in 0.4 s on x86, its memory little more than the bytes passed in. It keeps
responses up to 10 s past their common lead, checks the result loads and
makes a speaker layout, and writes a prepared room to `out_path`
through `out_path.part`; name it `<room>.room` (`bbcrdlr_systemG.room`): it
is not a SOFA file, and the loader tells the two apart by content. A session
given that file as `brir_sofa_path` renders it exactly as it renders the SOFA
file without head tracking, and is built on the room's loudspeakers from the
start, whatever their number (up to 64), rather than on the 7.1.4 layout a
13-loudspeaker room does not fit. So is a session given the SOFA file itself
(from its geometry, the file read once more), and either way the room's
loudspeakers come before a `speaker_layout_path` the host passes. The
summary line is the file's description (its loudspeakers, rate and
identity among it), with the prepared room's `seconds=` (its longest
response) and `bytes=` before its `reason=` when it was prepared. Returns 0
(prepared), 1 (kept), 2 (not a room to prepare), -1 (not a usable room, or
no SOFA support), -2 (cannot write), -3 (NULL argument or internal error).

A prepared room keeps the room its loudspeakers were measured in, when the
file states it (its `RoomType` and corners), so that it pans in the same
measured room as its file, and the identity of the SOFA file it was made
from: the file's length and the 64-bit FNV-1a hash of its bytes, which
`orender_sofa_describe` reports for the room and for a SOFA file alike as
`source=<length>-<16 hex digits>`. A host matches a file against the room
it keeps for it by that, and prepares it again only when they differ. A
prepared room is of one format, which this engine writes and reads and no
other: a room of another format, an older engine's, is refused where it is
loaded or described ("prepare it again from its SOFA file"), so that a room
is never played as something it is not; nothing reads a room any other way.
Its layout is the engine's own (`ExtractedRoom::to_prepared`); a host reads
what it needs through `orender_sofa_describe`.

`orender_brir_state(r)`: 0 no room selected, 1 loading, 2 resident, 3
refused (the reason is in the log), -1 for a NULL handle. Live, like
`orender_hrir_in_use`.

`orender_render_path(r, out, cap)` says how the last frames reached the
headphones: `room:N` while a room of `N` loudspeakers convolves,
`cascade:N` while objects are panned onto `N` virtual loudspeakers for the
HRTF stage (a room's own while it loads), `direct` when each object is
convolved as a direction of its own, `speakers:N` for speaker output. It
follows the session rather than the host's settings, so a config that chose
a room or a mode reads as rendered. Live, with the fill convention of
`orender_source_label`.

`orender_sofa_describe(sofa, len, out, cap)` says what a SOFA file or a
prepared room holds, and which binaural stage takes it, before a host
copies or prepares anything; of a SOFA file only the shape and geometry are
read, so a room set of hundreds of MB is described in a moment (a prepared
room, a few MB, is read whole). The HRTF stage takes
one direction per measurement and convolves the first few milliseconds of
each; the room stage takes up to 64 loudspeakers measured with their room.
A multi-speaker room (`MultiSpeakerBRIR`) suits only the room stage: cut to
the HRTF stage's few milliseconds, the room in it is gone. Nor does the HRTF
stage take a set any measurement of which has its `SourcePosition` at the
listener, fewer than three distinct directions, more than 2²⁴ values of
`Data.IR`, or a file over 144 MB: those are refused before a response is
read, by the stage itself too (see `hrtf_sofa_path` in BINAURAL.md).
Returns a mask, 1 HRTF and 2 room (0 neither), -1 for bytes that are
neither a SOFA file nor a prepared room of this engine's format, -3 on a
NULL argument or an internal error. The line is `hrtf=yes|no room=yes|no
prepared=yes|no conventions=… measurements=… receivers=… emitters=…
samples=… rate=… source=…`, then `orientations=… speakers=… names=…` for a
room, and `reason=…` to its end for the stage that does not take the file -
for a file neither takes, `as an HRTF set, …; as a room, …`. `source` is
the file's identity, or for a prepared room the identity of the file it was
made from (see *Measured rooms*).

`orender_hrtf_prepare(sofa_path, grid_path, sample_rate, diffuse_field_eq,
summary, cap)` builds the finished HRIR grid of a SOFA set now and keeps it
in `grid_path`, exactly as a session whose `binaural.hrtf_grid_cache` is
`{ path: grid_path, sample_rate, diffuse_field_eq }` would at its start (a
`grid_path` naming `{khz}` or `{rate}` is the file for `sample_rate`), so
that a host can do it when the set is chosen and the first session plays the
set from its first block. A grid is of one format, which this engine writes
and reads and no other, and carries the identity of the SOFA file it was
built from (as `orender_sofa_describe` reports it): nothing is built when
the file already holds this set's grid in this format, and a session builds
over a grid of another set or format rather than play it. The summary line
is `grid=built seconds=… bytes=…` or `grid=kept bytes=…`. Returns 0
(built), 1 (already kept), -1 (unreadable or not a set the HRTF stage loads,
or no SOFA support), -2 (cannot write), -3 (NULL path).

A seek (`orender_reset`) now leaves nothing of the previous stream's room:
the BRIR stage's histories, the reflections and the late reverb are cleared
in place with the rest of the per-stream state.

### Config check

`orender_check_config(path, report, cap)` checks a config file strictly
(`renderer::config::check`), for a host that builds the config itself -
for instance merging a file its user edits over its own - and falls back to
its own config when the result is refused. `orender_create` reads a config
leniently (a file that does not parse becomes the defaults, unknown keys are
kept unused, out-of-range options are clamped or ignored); the check refuses
instead: one YAML mapping that reads as a config, every key known to this
build, every number finite, every option within the option registry's
bounds and among its choices, and a `sofa` / `brir` source naming a file
that is there. Which keys a host leaves to its user is the host's policy and is not
checked. The file is only read; one over 1 MiB is refused unread. Returns 0
(accepted, `report` empty), -1 (refused or unreadable, the reason in
`report`), -3 (NULL path). `orender_create` itself is unchanged.

The Rust plugin interface is `bridge_api` 0.7, one minor past upstream's 0.6
for the method described below; its package version is independent of the C
ABI minor. Rebuild the matching renderer and plugins.

`orender_drain` (see End of stream) also releases pending decoder audio in
this fork: once the decode thread holds nothing more, what a bridge kept back
to see what follows it (an E-AC-3 independent substream, or the DTS frames
held while an Auro-3D carrier is looked for), as the last call before 0
frames. The contract is unchanged — not a reset, not a DSP/reverb-tail
flush, one packet's audio per call until 0 frames, a short buffer keeps the
audio for the retry, and reset discards it — but what a bridge held can be
more than one packet's audio, so a host must take that retry.

`FormatBridge::drain` is appended after upstream's methods, with an empty
successful default, so source implementations without a tail can omit it.
It grows the vtable, so under upstream's versioning policy (BRIDGE_API.md,
"Versioning") it bumps `bridge_api` to 0.7: the loader refuses a bridge built
against upstream's 0.6 by version, naming both, and an upstream host refuses
this fork's bridges the same way. Rebuild the matching renderer and plugins
together. Bridges that hold an access unit must override drain to release
it; PCM and WAV bridges emit complete sample frames immediately and have no
decoder tail to release.
