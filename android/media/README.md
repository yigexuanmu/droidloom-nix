# The Codec2 bridge over the forwarded GPU

This directory carries the hardware video decode bridge: a Codec2
component store whose H.264 decoder drives Vulkan Video over the cell's
forwarded GPU, through `droidloom-gpu-server` and the NVIDIA driver's
own video queues. The engine (`droidloom-video/`) is Rust, in the
workspace, and compiles on the host; the C2 component layer is C++ and
builds in the Soong pipeline with the pinned frameworks/av tree.

## What exists here now

- `droidloom-video/` — the engine: an Annex B splitter, SPS/PPS/slice
  parsers producing the `StdVideoH264*` structures the generated
  protocol crate carries, DPB bookkeeping, the Vulkan Video session over
  the forwarded driver (`session.rs`, opened the way
  `droidloom-video-probe` validated end to end on real hardware), and
  the C ABI the component drives (`ffi.h`).
- The capability surface the engine builds on comes from the forwarding
  protocol's video replay (see `docs/gpu-forwarding.md`), validated on
  an RTX 4060 Ti: the probe through `libdroidloom_vulkan.so` and
  `droidloom-gpu-server` reports the native driver's own decode
  capabilities and DPB formats.

## What lands with the device build

The C2 component layer (`droidloom-c2-service/`) is the piece this tree
does not carry yet, and deliberately so: it binds the engine's FFI into
`C2Component`/`C2ComponentStore` implementations and registers the
AIDL `IComponentStore/default` through the
`codec2/aidl/ComponentStore` wrapper the pinned frameworks/av tree
ships. That code compiles only inside the Soong pipeline, where the
pinned `android.hardware.media.c2-V1-ndk`, `libcodec2_vndk`, and
frameworks/av headers exist; written outside them it would be a guess
that fails at build time.

The shape the component layer takes is fixed by the engine's design and
follows the reference implementation's proven semantics:

- one work thread per component; queue_nb appends, drain_nb reports
  E_AGAIN while the DPB holds unreported pictures, flush_sm returns
  queued and pending work;
- CSD records cache and feed `droidloom_video_configure`; each frame
  feeds `droidloom_video_decode` with the output pool's graphic block;
- ordinals pass through to the output worklets;
- the release fence the copy carries waits inside the work loop, the
  way the reference implementation does, until the platform proves a
  cross-HAL C2Fence path;
- `media_codecs_c2.xml` is generated from the live capability query at
  build time and the service refuses to start when the file and the GPU
  disagree (a stale file would rank the bridge above the software
  decoders for formats it cannot decode).

The wiring when it lands: `Android.bp` gains
`cc_library_shared droidloom-c2` (engine FFI +
`android.hardware.media.c2-V1-ndk` + `libcodec2_vndk`) and
`cc_binary android.hardware.media.c2-service.droidloom`
(`relative_install_path: "hw"`), the device trees gain the service rc
and VINTF fragment (`interface aidl
android.hardware.media.c2.IComponentStore/default`), and
`tools/droidloom-update/src/android.rs` adds the module to TARGETS so
the vendor image carries it.
