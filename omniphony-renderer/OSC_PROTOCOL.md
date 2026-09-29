# OSC Protocol

This document describes the OSC messages exchanged between `orender`, `omniphony-studio`,
and compatible metadata producers such as `adm-player`.

## Overview

`orender` can:

- broadcast decoded spatial metadata
- broadcast live renderer state
- accept control messages for gain, mute, spread, room ratio, and speaker layout edits
- expose an OSC registration endpoint for dynamic clients

## Ports

| CLI option | Default | Purpose |
|---|---|---|
| `--osc-host` | `127.0.0.1` | Fixed OSC client target |
| `--osc-port` | `9000` | Fixed OSC client port |
| `--osc-rx-port` | `9000` | `orender` receive port for registration and control |

The fixed client defined by `--osc-host:--osc-port` always receives broadcasts.
Additional clients can register dynamically. Port `0` (`render.osc_port: 0`)
means no fixed client: nothing is sent until a client registers.

## Managed hosts

`render.managed_host: <name>` declares that an application owns the engine and
writes its config for every stream — the Kodi fork sets `kodi`. Such an engine:

- reports `<name>` as `host` in `/omniphony/state/capabilities` (the
  `variant` is unchanged, so clients behave as for any embedded host);
- keeps live edits for the current stream only: nothing is saved, written
  back to the config (view state such as the monitoring cadences and the head
  recenter included), or handed to the next instance, overlay display
  preferences are neither read nor written, and a leftover live-handoff sidecar
  next to the config is deleted unapplied;
- refuses a change to the `output_mode`, `binaural_mode` and `decode_thread`
  options and a FIR `crossover_type`, under any spelling the option takes,
  whether sent to the option's own address, through `/omniphony/control/option`
  or in a `/omniphony/control/options` batch (a whole batch is refused for one
  such pair; a pair that leaves the option as it is goes through);
  `save_config`, `reload_config`, `restart`, `quit`, `profile/*`,
  `layout/export`, `backend/file/put` and `binaural/hrtf_upload/*`;
  `render/bridge_path` and `render/input_pipe`; `speaker_test*` and
  `object_test*`; a `backend/param` setting a backend's `Path` or `File`
  parameter (names trimmed, a blank backend meaning the active one, as the
  setter reads them); and an `hrir_source` naming a `sofa:`/`brir:` file other
  than the one in use or the one the host's config chose, by any of the same
  routes. A bare `sofa` means the host's SOFA file when it has one;
- reports the decode thread the host forced (`orender_set_option`
  `decode_thread` `on`/`off`) as the live `decode_thread` option, since its
  clients cannot change it;
- answers each refusal with a warning in the log stream, a fresh state
  broadcast so the client shows what holds, and then
  `/omniphony/state/config/save_error s "Not changed: <Name> manages <what>"`,
  the reason a client shows by its save indicator until the next state
  update;
- never makes the host wait for `--osc-rx-port`. A host may open a successor
  before it closes the engine it replaces, in another process that neither
  yields nor releases the port until that open returns. The successor starts
  at once without a listener and takes the port within 250 ms of it freeing;
  registered clients re-register as usual.

## Registration

### `/omniphony/register`

Sent by a client to `--osc-rx-port`.

Arguments:

| Name | Type | Optional | Description |
|---|---|---|---|
| `listen_port` | `i32` | Yes | Client receive port if different from the UDP source port |

After registration, `orender` sends:

1. a state bundle with the current live renderer state, including `state/layout` and `state/speakers`

### `/omniphony/heartbeat`

Arguments:

| Name | Type | Optional | Description |
|---|---|---|---|
| `listen_port` | `i32` | Yes | Same convention as `/omniphony/register` |

Responses:

- `/omniphony/heartbeat/ack`
- `/omniphony/heartbeat/unknown`

Dynamic clients should send heartbeats periodically to stay registered.

## Messages Sent by orender

### Serialized State

#### `/omniphony/state/layout`

Serialized JSON layout snapshot with speaker geometry and static metadata.

#### `/omniphony/state/speakers`

Serialized JSON speaker runtime/config snapshot with per-speaker `gain`, `delayMs`, and `muted`.

### Spatial Metadata

These, `/omniphony/timestamp` and the meter bundles describe a block of audio
and are sent as it is rendered. An embedded host that says where its listener
is (`orender_set_option` `heard_us`, see ABI.md) has them held until the
listener reaches the block instead, in order: Kodi buffers up to two seconds
or more of rendered audio, which a client would otherwise show that far ahead
of the sound.

#### `/omniphony/spatial/frame`

| Argument | Type | Description |
|---|---|---|
| `sample_pos` | `i64` | Sample position from start of stream |
| `generation` | `i64` | Monotonic content generation ID |
| `object_count` | `i32` | Number of active objects in this frame |
| `coordinate_format` | `i32` | `0=cartesian`, `1=polar` |

#### `/omniphony/object/{idx}/xyz`

| Argument | Type | Description |
|---|---|---|
| `x` | `f32` | ADM X coordinate |
| `y` | `f32` | ADM Y coordinate |
| `z` | `f32` | ADM Z coordinate |
| `gain_db` | `i32` | Per-object gain in dBFS |
| `priority` | `f32` | Object priority |
| `divergence` | `f32` | Object divergence |
| `ramp_duration` | `i32` | Ramp duration in audio frames |
| `generation` | `i64` | Monotonic content generation ID |
| `name` | `string` | Object or bed label |

#### `/omniphony/object/{idx}/remove`

Sent when an object's slot goes away — the frame's `object_count` shrank past
it, or the content changed.

| Argument | Type | Description |
|---|---|---|
| `generation` | `i64` | Monotonic content generation ID |

A slot going away is also signalled the older way, by zeroing its position,
`/size` and `/meta`, and that is still sent for clients that predate this
message. Both are emitted for the same slot: the zeroed triple first, then this.

A client that only watches `object_count` has to infer which slots are gone,
which is what leaves ghost objects behind after a seek — the count can stay the
same while the objects behind it change.

### Metering

Pre-enabled for the fixed client with `--osc-metering`; a registered client
subscribes itself with `/omniphony/control/metering i 1`.

#### `/omniphony/meter/object/{idx}`

| Argument | Type | Description |
|---|---|---|
| `peak_dbfs` | `f32` | Object peak level |
| `rms_dbfs` | `f32` | Object RMS level |

#### `/omniphony/meter/object/{idx}/gains`

Variable-length list of linear gains, one value per output speaker.

#### `/omniphony/meter/object/{idx}/band/{band}/gains`

Per-band gains, the same shape, when the layout has a crossover.

#### `/omniphony/meter/speaker/{idx}`

| Argument | Type | Description |
|---|---|---|
| `peak_dbfs` | `f32` | Speaker peak level |
| `rms_dbfs` | `f32` | Speaker RMS level |

#### `/omniphony/meter/ear/{idx}`

Headphone ear levels (`0` left, `1` right) in binaural output mode, same
arguments.

### Timestamp

#### `/omniphony/timestamp`

| Argument | Type | Description |
|---|---|---|
| `sample_pos` | `i64` | Sample position |
| `seconds` | `f64` | Time from start of stream |

### Live State

The control and state surface — every `/omniphony/control/…` address a client
can send and every `/omniphony/state/…` address the engine publishes, with
argument types and semantics — is documented in
[`docs/osc-control-contract.md`](../docs/osc-control-contract.md), generated
against the `osc-contract` crate that names each address. This file only covers
the session handshake and the streams above.

In short: a newly registered client receives the full live-state snapshot (one
OSC bundle, or several consecutive ones when it would not fit a datagram, always
ending with `/omniphony/state/snapshot_complete`). The serialized domain
messages are `/omniphony/state/{capabilities,renderer,layout,speakers,input,
loudness,monitoring}` (`s <json>`), plus `/omniphony/state/audio` when the host
owns audio output.

### Log Stream

#### `/omniphony/log`

| Argument | Type | Description |
|---|---|---|
| `seq` | `i64` | Monotonic log sequence number |
| `level` | `string` | `error`, `warn`, `info`, `debug` or `trace` |
| `target` | `string` | Rust log target/module |
| `message` | `string` | Log message text |

## Messages Sent to orender

All control messages are sent to `--osc-rx-port`; the full list is in
[`docs/osc-control-contract.md`](../docs/osc-control-contract.md). Two usage
patterns are worth knowing:

- **Realtime controls** carry a trailing sequence number and use latest-wins
  semantics: `/omniphony/control/realtime/master_gain [f32 value, i32 seq]`
  and `/omniphony/control/realtime/speaker_gain [i32 id, f32 value, i32 seq]`,
  acknowledged on `/omniphony/state/realtime/{master_gain,speaker_gain}`. Fast
  gain drags should use them.
- **Config-domain controls** take a JSON patch and are staged, then applied:
  `/omniphony/control/config/{audio,input,layout} s <json>` followed by
  `/omniphony/control/config/{audio,input,layout}/apply`, and
  `/omniphony/control/config/speakers s <json>` for runtime speaker edits
  (`muted`, `delayMs`). Speaker topology and metadata edits go through
  `config/layout`.

### Live Input Control for Studio

The live-input surface is designed for staged editing from a controller such as
Studio:

1. send one or more staged values under `/omniphony/control/input/…` (`mode`,
   `live/{backend,node,description,layout,layout_import,channels,sample_rate,
   clock_mode,map,lfe_mode}`),
2. send `/omniphony/control/input/apply`,
3. observe `/omniphony/state/input`, the serialized input domain carrying both
   the staged and the active runtime values.

`/omniphony/control/input/refresh` makes `orender` rebroadcast the full state
bundle, for a client that reconnects without sending `/omniphony/register`.

## Speaker Recompute Flow

Speaker position edits are staged with `/omniphony/control/config/layout` and
applied atomically with `/omniphony/control/config/layout/apply`.

During recompute, `orender` broadcasts:

- `/omniphony/state/speakers/recomputing i 1`

When the new topology is published, it broadcasts:

- `/omniphony/state/speakers/recomputing i 0`
- updated `/omniphony/state/layout s <json>`
- updated `/omniphony/state/speakers s <json>`

A failed rebuild is reported on `/omniphony/state/speakers/recompute_error`.

## Notes

- Speaker gains and mutes apply after VBAP mixing.
- Object controls address PCM channel indices.
- Layout recompute requires runtime VBAP support and is not available when using a precomputed VBAP table.
- room geometry and distance-diffuse settings live in `state/renderer`.

## Recommended Next Step

The bridge API is documented separately in
[BRIDGE_API.md](BRIDGE_API.md). This file
only describes the OSC surface exposed by `orender`.
