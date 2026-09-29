# Bengal Vulkan offscreen smoke

## Prior-art census

The four-tree census in
`kernel/out/.tmp/sf-ab-lead/vulkan-prior-art-census.txt` shows that device 1
previously rendered through Turnip's KGSL backend, including real A610 identity
and several Android loader/gralloc traps. That is an oracle for the required
identity and for refusing SwiftShader, not a recipe for device 3: device 3 keeps
the existing strict-GKI `drm/msm` path. The source-backed device-3 assessment in
`docs/research/2026-09-06-turnip-a610-feasibility.md` establishes the expected
physical identity (`vendorID=0x5143`, `deviceID=0x06010000`, integrated A610,
Mesa Turnip), physical Vulkan 1.0, and one graphics queue. This tool requires
that exact physical 1.0 version as a guard against the known `noconform`
version inflation and reuses the other exact
discriminators and does not inherit KGSL, `noconform`, fake Vulkan versions, or
two-queue assumptions.

## What this proves

The default invocation requires the Android Vulkan loader to enumerate an
integrated Qualcomm A610 backed by the Mesa Turnip driver. It prints bounded
device extensions, selected Vulkan 1.0 features, every bounded queue family,
raw physical API/driver/device/vendor values, and driver properties when
`VK_KHR_driver_properties` can truthfully be queried through
`VK_KHR_get_physical_device_properties2`.

After identity acceptance it creates exactly one queue and enables no optional
features or device extensions. It draws one shader triangle into a 64x64
`VK_FORMAT_R8G8B8A8_UNORM` offscreen image, copies 16384 bytes to host-visible
memory, waits once on a five-second fence, and checks a green interior pixel at
(32,32) plus the black clear color at exterior pixel (2,2). A clear/copy alone
is not reported as shader proof. The command stream follows the local Vulkan
CTS readback pattern in
`lineage-src/external/deqp/external/vulkancts/modules/vulkan/util/vktDrawUtil.cpp`:
the image-to-buffer copy is followed by a TRANSFER_WRITE/TRANSFER-stage to
HOST_READ/HOST-stage buffer memory barrier before submission completes.

Expected hardware tells are:

```text
IDENTITY device=N verdict=ACCEPTED_A610 reason=exact_a610_turnip
DEVICE_CREATE verdict=PASS requestedQueueCount=1 enabledFeatureCount=0 enabledExtensionCount=0
SUBMIT verdict=PASS vk=0 name=VK_SUCCESS queueCount=1
FENCE verdict=PASS timeoutNs=5000000000
READBACK verdict=PASS ... inside_rgba=0,255,0,255 ... outside_rgba=0,0,0,255
RESULT HARDWARE_A610_PASS shader_render=PASS submit=PASS readback=PASS
```

Standard output is line-buffered before the first tell, including when output
is redirected, so an outer timeout retains all completed newline-terminated
identity, submit, fence, and cleanup records.

SwiftShader or any non-A610 device is a default failure. The explicit
`--allow-software-control` mode may validate the same shaders, submit, and pixel
checks using forced CPU Vulkan on a host, but it prints `NOT_HARDWARE` and exits
3 even when rendering succeeds. It can never produce hardware PASS.

## Builds and desk controls

Use Bash and an absolute private scratch directory:

```bash
export TMPDIR=/home/meinte/android/bengal/kernel/out/.tmp/vulkan-offscreen-smoke
export BENGAL_DESK_TMP="$TMPDIR"
bash tools/build.sh android
bash tools/build.sh host
bash tools/test-host.sh
```

To add the host software-rendering controls without touching the host GPU, set
`BENGAL_SOFTWARE_VULKAN_ICD` to an absolute SwiftShader or lavapipe ICD JSON.
The test wrapper forces that single ICD and places a 20-second outer timeout
around each Vulkan invocation.

The inert `Android.bp` declares a 64-bit vendor binary linked to Android's
actual `libvulkan` and platform libc++; nothing adds it to `PRODUCT_PACKAGES`,
init, or policy. The standalone NDK API-35 desk artifact statically links and
hides its C++ runtime, while its ELF must retain `DT_NEEDED libvulkan.so`.

Shader provenance is fixed in `generated/triangle_spv.h`. Both shaders are
compiled with:

```text
glslangValidator --target-env vulkan1.0 -V -S <stage> -o <output> <source>
```

`tools/verify-shaders.sh` recompiles them, verifies the recorded source and
SPIR-V SHA-256 values, reconstructs the embedded arrays, compares all bytes,
and runs `spirv-val --target-env vulkan1.0` when available.

## Limits and statuses

Counts are capped at 16 physical devices, 256 instance extensions, 256 device
extensions, and 32 queue families. The renderer makes two Vulkan allocations,
each capped at 16 MiB. Normal completion destroys all owned resources without
`vkQueueWaitIdle`. After fence timeout, device loss, or an indeterminate fence
failure it deliberately skips both device/resource teardown and
`vkDestroyInstance`, because the instance may still own a live device and
destroying possibly in-use objects or waiting idle could block indefinitely;
the process boundary releases userspace state. Injected self-test callbacks
verify that timeout/device-loss paths call neither cleanup function.
The local Khronos valid-usage registry at
`lineage-src/external/vulkan-headers/registry/validusage.json` records
`VUID-vkDestroyInstance-instance-00629` (children must be destroyed first),
and the local validation layer enforces it in
`lineage-src/external/vulkan-validation-layers/layers/object_tracker_utils.cpp`.

For non-coherent readback memory, the code maps the whole allocation and
invalidates that same `VK_WHOLE_SIZE` range before inspecting only the first
16384 bytes. This keeps an allocation padded beyond the logical buffer inside
the mapped range; the synthetic control uses a 20480-byte padded allocation.
This also satisfies the local registry's `VkMappedMemoryRange` valid-usage
rules 00685/00686 and 01389/01390 without hand-computing atom-aligned padding.

The five-second fence bounds cooperative submitted work, but a kernel driver
stuck inside an ioctl, device creation, pipeline compilation, or process exit
can still be uninterruptible. A caller should retain an outer timeout and
collect kernel GPU-fault/device-lost evidence. This program does not prove
Android WSI/AHardwareBuffer, presentation, pre-rotation, Skia/HWUI, absence of
GPU resets, performance, or physical A610 behavior until root deploys and runs
it through a numbered guarded probe.

Exit status: 0 hardware A610 shader/readback PASS; 2 usage; 3 successful
explicit `NOT_HARDWARE` control; 10 instance failure; 20 no Vulkan device; 21
identity rejection; 30 setup failure; 40 submit/wait failure; 41 fence timeout;
42 device lost; 50 pixel/readback mismatch; 60 internal self-test failure.
