# AHardwareBuffer: what the two consumers that matter actually call

Droidloom's vendor image and the forwarding server agree on the
AHardwareBuffer contract the guest's renderers follow. This document
records the contract as measured from the consumers' own sources —
ANGLE and the Android 13/17 framework (Skia and HWUI) — following the
nvidia-prolapse project's field work (`AHB-CONTRACT.md` there, Apache-2.0
and MIT licensed); the conclusions carry into the cell directly because
the same ANGLE build and the same framework versions run in both.

## Enabling

| Consumer | Condition |
| --- | --- |
| ANGLE | device lists both `VK_ANDROID_external_memory_android_hardware_buffer` and `VK_EXT_queue_family_foreign` |
| Skia | `VK_ANDROID_external_memory_android_hardware_buffer` at spec version ≥ 2 plus external memory and bindMemory2 |
| HWUI | enables every device extension the driver lists; fatal without swapchain |

Consequence: the HAL lists both extensions and every listed extension
must survive `vkCreateDevice` (HWUI and dEQP enable all of them).

## Import sequence (ANGLE, `EGL_NATIVE_BUFFER_ANDROID`)

1. `vkGetAndroidHardwareBufferPropertiesANDROID` with a format-properties
   struct chained: `format` decides the native-format path,
   `formatFeatures` the external one (SAMPLED_IMAGE, plus
   COLOR_ATTACHMENT for GPU_FRAMEBUFFER).
2. `AHardwareBuffer_acquire`, `AHardwareBuffer_describe`.
3. `vkCreateImage` with tiling OPTIMAL unconditionally (linear +
   INPUT_ATTACHMENT is unsupported on NVIDIA), usages per the AHB
   usage, MUTABLE_FORMAT with an explicit format list when both sRGB
   and linear twins share features, sharing mode EXCLUSIVE, initial
   layout UNDEFINED.
4. No `vkGetImageMemoryRequirements*`: requirements come from step 1.
5. `vkAllocateMemory` with a dedicated allocation importing the AHB.
6. `vkBindImageMemory` version 1, offset 0.
7. The image starts on the foreign queue family; barriers use
   `VK_QUEUE_FAMILY_FOREIGN_EXT` on both sides.
8. Views and commands go through the image handle; teardown releases the
   AHB.

For buffers: AHB blob format, external memory buffer create, properties
without a format struct, dedicated allocation import.

## What the forwarding server adds

NVIDIA does not accept linear multi-plane DMA-BUF imports, so the video
formats (NV12, YV12, P010) arrive as one linear image per plane, and the
first import of the second plane duplicates the fd. A foreign-queue take
copies each plane into the application's optimal image; handing back
copies nothing, because the application only samples the planes. ETC2
and ASTC, which desktop NVIDIA GPUs lack, decode on the GPU by compute
shader on the host side. See `gpu-forwarding.md` for the memory kinds
the shared allocation carries.

## The staging-lock contract (this series)

CPU locks of VRAM render targets go through a staged linear copy whose
stride equals the buffer's own: the handle's promise holds for every CPU
consumer. Read locks stage in; write locks stage out on flush; read-only
locks re-stage on invalidate. Fencing is the framework's acquire and
release fence pair around each lock; the staging copy itself adds no
fence of its own, because the DMA-BUF carries one read or one write at a
time by the buffer queue's contract.
