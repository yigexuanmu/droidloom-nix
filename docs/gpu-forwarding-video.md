# Video decode over GPU forwarding

The forwarding protocol carries the thirteen commands of
`VK_KHR_video_queue` and `VK_KHR_video_decode_queue`, replayed onto the
host driver's own video queues. Video decode rides the forwarded queue
families (`vkGetPhysicalDeviceQueueFamilyProperties` no longer masks the
video decode bit), so a Codec2 decoder in the cell sees the same DPB
grants and DPB formats the host driver gives a native application.

## What the wire carries

- Capability queries (`vkGetPhysicalDeviceVideoCapabilitiesKHR`) return
  the host driver's own values: on an RTX 4060 Ti with driver 615,
  seventeen H.264 DPB slots, sixteen active references, 4096x4096 max
  coded extent, and a G8_B8R8_2PLANE_420_UNORM DPB format. Verified end
  to end: `droidloom-video-probe` through `libdroidloom_vulkan.so` and
  `droidloom-gpu-server` reports what the native driver reports.
- Session creation, parameter updates and memory binding carry their
  `VkResult` back to the caller. Creation failures are realistic (the
  host may not support the profile), so they propagate as errors instead
  of the abort-on-failure contract plain non-sync commands keep.
- The coding-scope commands (`vkCmdBeginVideoCodingKHR`,
  `vkCmdControlVideoCodingKHR`, `vkCmdEndVideoCodingKHR`,
  `vkCmdDecodeVideoKHR`) ride the command-buffer stream like every other
  `vkCmd*`; the structures reach the host driver byte-exact, with
  `StdVideoH264*` payload the guest's parser built.

## Versioned inputs

The video structures come from a second registry: the Vulkan headers
ship `StdVideo*` definitions in `video.xml` beside `vk.xml`, and
video.xml declares its API constants inside extension blocks. Both
default to the AOSP-pinned headers; `--video-registry` overrides. The
protocol hash covers the video commands, so a runtime built from this
tree refuses a vendor image built from an earlier one at the handshake —
both ends ship together in one package build, as every wire change does.

## The guest decoder

`android/media/droidloom-video` parses Annex B into the
`StdVideoH264*` structures (Vulkan Video never parses bitstreams), books
DPB slots from the slice headers' reference marking, and submits the
decode. Its state and boundaries live in `android/media/README.md`.

## NvkRm, the native alternative

An explicit `graphics_backend: "nvk_rm"` turns the cell over to
source-built Mesa NVK with the RM backend instead of forwarding: no
forwarding process, no IPC, hardware video decode through the driver's
own path. `auto` never selects it. The cell then sees the NVIDIA
character devices alongside the render node, each verified against its
kernel device identity. See `threat-model-v2.md` for the trust boundary.
