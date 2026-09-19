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

## Bump checklist

1. Edit `ORENDER_ABI_MINOR` (or `MAJOR`) in `orender_ffi/src/lib.rs` and extend
   the changelog comment above it.
2. `cargo build -p orender_ffi` — regenerates `include/orender.h`; commit it.
3. If breaking: expect the soname to change; update packaging (`PKGBUILD`
   symlinks) and warn mpv-omniphony (bundled lib name changes).
4. `cargo test -p orender_ffi` + run `examples/smoke.c` (CI does both).

## Kodi fork additions (C ABI 0.10)

Upstream ABI 8 height-tier labels and ABI 9 `orender_source_label` retain
their values and contracts. This fork adds `orender_decoded_sample_rate`:
the last decoder output rate in Hz, zero before it is reported or for a NULL
handle. It survives a same-stream seek and updates with each decoded frame;
it is not the configured renderer rate. Hosts poll it to detect when the
renderer must be reopened at the source rate. Probe the symbol, not minor 10.

The Rust plugin interface remains `bridge_api` 0.4; its package version is
independent of the C ABI minor. Rebuild the matching renderer and plugins.

`orender_drain` releases pending decoder audio at end of input without
resetting the renderer. It is not a DSP/reverb-tail flush and must not be
called instead of reset on a seek. A successful repeated drain emits no
additional frames. A short output buffer returns 1 with zero output frames
and retains the rendered tail; retry drain with a larger buffer before
sending more input. Reset discards any retained tail.

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
