# The two NVIDIA paths, measured and structural

Droidloom drives NVIDIA GPUs two ways, chosen per cell, never per app.

## GPU forwarding (the default)

The Android process loads `vulkan.droidloom`, which records every call
into a shared-memory ring and replays it on the host driver. Measured on
an RTX 5060 Ti: a draw call costs 36 ns against 34 ns direct, and a
timeline submit-and-wait 57 µs against 37.6 µs. The compiler is the
host driver's own, so shader throughput is the host driver's own.

Shadow images existed for buffers with CPU usage bits: the allocator
gate excluded anything the CPU touches, and NVIDIA cannot draw into a
linear colour image while a depth or stencil attachment is bound. The
allocator now stages those locks (see the minigbm staging change in the
patch series), so only buffers whose producer writes the bytes itself
(camera, video hardware, GPU data buffers, sensors, protected content,
cursors) stay linear.

## NvkRm (explicit)

Mesa NVK with the RM backend runs inside the cell, talking to the
NVIDIA kernel module directly: no IPC, native VRAM, hardware video
decode through the driver's own path, and no per-client server process.
The cost is the compiler: NAK compiles shaders to roughly 93 percent of
the host driver's throughput (475 against 508 FPS in 3DMark Wild Life
Extreme on an RTX 5090, measured by the upstream nvidia-prolapse
project this backend builds on). Driver revisions pin the RM adapter
work; `auto` never selects this backend.

## Choosing

Game-heavy workloads on a big card keep forwarding: the compiler gap
dwarfs the replay overhead. Latency-sensitive UI, video-heavy use, or
VRAM-constrained sizes gain from NvkRm — and the bridge's video decode
works on either path, since both reach the driver's video queues.
