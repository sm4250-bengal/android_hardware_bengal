# Bengal Vulkan Android WSI smoke

This is a source-only, manually launched NativeActivity diagnostic. D3-0126
proved its raw Vulkan renderer, and the isolated Ganesh prerequisite later
passed on D3-0127. It is not added to any product, init service,
policy, renderer selection, HWUI, or Skia path. Its inert `Android.bp` only
declares an arm64 APK and native library if a lead explicitly names the target.
The APK is `soc_specific` for a future `/vendor/app` diagnostic and is neither
privileged nor installed by this source directory.

## Acceptance and rendering contract

The app uses Android's real `libvulkan`, `VK_KHR_android_surface`, and
`VK_KHR_swapchain`, requests Vulkan 1.0 and one queue, and enables no optional
features. It refuses everything except the measured D3-0125 identity: an
integrated A610 with `vendorID=0x5143`, `deviceID=0x06010000`, physical API
major/minor exactly 1.0, Mesa Turnip driver properties, and exactly one queue
family containing exactly one graphics+present queue. SwiftShader cannot pass.
The raw API, driver ID/version/name, vendor/device IDs, extension counts, and
queue counts are printed.

The checked-in Soong declaration and the standalone `android` build remain the
raw renderer. Private Ganesh is an explicit standalone `android-ganesh` build
mode; selecting it does not change framework Skia, HWUI, another app, or a
global renderer property. It combines the isolated archive's closed
two-instance/seven-device extension set with the three WSI extensions, enables
only `samplerYcbcrConversion`, and still owns one queue.

`android-mesa26` is a separate raw diagnostic profile for the isolated Mesa26
candidate only. It requests Vulkan 1.3 and accepts only an integrated Qualcomm
Adreno 610 (`0x5143`/`0x06010000`) with physical API at least 1.3, Mesa Turnip
driver properties, and one selected graphics+present queue. It enables no
additional device features or extensions. The default raw `android` and private
`android-ganesh` profiles remain exact Vulkan 1.0 contracts; this profile does
not alter framework Skia, HWUI, Ganesh, or any global renderer selection.

The Mesa26 profile additionally accepts only the measured two-family A610 WSI
topology: one queue in the graphics+present family and one present-capable
sparse-binding-only queue. It selects and requests only the graphics+present
queue; a missing graphics/present queue, non-sparse extra family, or any queue
count other than one is rejected.

The private adapter retains one wrapped Ganesh surface per swapchain image so
Skia tracks the transition from initial `UNDEFINED` layout through
`PRESENT_SRC_KHR` and later reacquisition. Each frame queues the acquire
semaphore through `SkSurface::wait`, draws, flushes a unique render-finished
semaphore for present, calls `GrDirectContext::submit`, then submits an empty
same-queue batch with the existing finite fence. The existing frame-loop
failure and unsafe-teardown latches remain authoritative.

Android's local loader reports the native window default dimensions as
`currentExtent` and the window transform hint as `currentTransform`. The app
uses that exact extent and `preTransform=currentTransform`; its fragment shader
pre-transforms a logical upright test card for all eight Android rotation and
mirror cases. It submits at most four frames. The expected upright card is:

- top-left red, top-right green;
- bottom-left blue, bottom-right yellow;
- a gray 10-by-10 grid on a dark background;
- a magenta vertical center line and cyan horizontal center line.

Ganesh mode keeps those corner/grid/orientation tells and adds the already
proven red path, green clipped rectangle, yellow raster image, white procedural
glyph, and magenta/cyan center marks. This is presented private Ganesh content,
not framework HWUI/View support, typography, AHB, or performance proof.

For 90/270-degree transforms the implementation deliberately uses Android's
raw `currentExtent`; compositor scaling may compensate for the unswapped
dimensions. The card is an orientation tell, not a pixel-perfect, unscaled, or
optimal pre-rotation claim.

## Future manual protocol

Only after the lead creates a numbered guarded WSI probe:

1. Explicitly build/package the otherwise inert `BengalVulkanWsiSmoke` target
   and verify its hashes and that no product/default-renderer files changed.
2. Launch **Bengal Vulkan WSI Smoke** manually from the launcher. Do not change
   global graphics properties and do not select ANGLE.
3. Retain the complete `BengalVulkanWsi` log stream. Confirm the exact identity,
   four `GANESH_FRAME ... stage=vkQueueSubmit_empty_fence verdict=PASS` and
   `FRAME_FENCE` PASS tells in private mode, plus
   `FRAME_PRESENT_API ... ACCEPTED_NOT_OBSERVED`. A raw build instead retains
   the existing four `FRAME_SUBMIT` tells.
4. Independently record whether the card above is physically visible and
   upright. `vkQueuePresentKHR` acceptance is only a queue/API fact; the app
   always prints `OBSERVATION verdict=UNPROVEN` and never promotes it to a
   visual-pixel PASS itself.
5. Rotate once after the finite render and record the lifecycle tell. The
   bounded implementation does not recreate a swapchain: transform/extent or
   window termination prints `STOP_NO_RECREATE`/`STOP_NO_DESTROY` and requires
   a relaunch. Close with Back after recording the card.

## Desk checks

Use Bash and the assigned absolute scratch directory; builds are single-job
compiler invocations:

```bash
export TMPDIR=/home/meinte/android/bengal/kernel/out/.tmp/vulkan-wsi-smoke
export BENGAL_DESK_TMP="$TMPDIR"
bash tools/test-host.sh
bash tools/build.sh android
bash tools/build.sh android-ganesh
```

`android-ganesh` verifies before compiling that its read-only inputs are the
clean Skia source at
`2b8abbdec5fc4b1350fc196689e4d330cc918fe1` and `libskia.a` at
17,073,132 bytes / SHA-256
`3ac3f4eb10271ba5feef90043a639b1fbf17f996e29ea017326c1719267a567d`.
They default to the measured project paths and may be overridden only with
`SKIA_SOURCE_ROOT` and `SKIA_STATIC_ARCHIVE` carrying those exact identities.
The build uses the matching standalone `SkUserConfig.h`, static C++ runtime,
and NDK r28c/API 35 configuration. It does not run Soong or package an APK.

The host tests cover exact identity rejection, Android extent/pre-transform
planning, all eight transform mappings, and the actual bounded frame-loop
dispatcher. Scripted frame 0 success followed by frame 1 acquire timeout,
out-of-date, device-lost, or fence-reset failure proves zero cleanup callbacks
after any presentation attempt. Separate first-frame controls prove that an
acquired but unconsumed semaphore, indeterminate submitted work, and device
loss also suppress cleanup. They never load Vulkan or touch a host GPU. The
Android output is an un-packaged NDK
API-35 arm64 shared library for ELF/entrypoint inspection; no Soong build or
device action is performed. Shader source and Vulkan-1.0 SPIR-V SHA-256 values
are pinned and byte-verified by `tools/verify-shaders.sh`.

`android:hasCode="false"` is intentional: the APK has no DEX, while
`android.app.NativeActivity` is supplied by the framework and loads the named
JNI library. Android's manifest parser otherwise defaults `hasCode` to true.

## Bounds and teardown limits

Counts are capped at 16 physical devices, 256 instance extensions, 256 device
extensions, 32 queue families, 64 surface formats, 16 present modes, and 8
swapchain images. Extent is capped at 4096x4096 pixels. There are 4 frames, a
1-second acquire timeout per frame, and a 5-second submit fence per frame.
Each frame has a one-shot render-finished semaphore so no presentation wait
semaphore is reused without a presentation-completion primitive. The acquire
semaphore is reused only after the render fence proves its queue wait consumed
the signal. The frame path makes no direct `vkQueueWaitIdle` or
`vkDeviceWaitIdle` call. Ganesh may use an internal idle wait when its context is
destroyed on an otherwise-safe pre-presentation failure; unsafe paths skip that
destruction and leave process exit as the cleanup boundary.

Those API timeouts only bound a cooperative Vulkan implementation. Instance,
device, pipeline/context creation, Ganesh wait/flush/submit, acquire, raw/empty
queue submit, present, context destruction on an otherwise-safe early failure,
and process exit may still block in a kernel/driver/BufferQueue call; a guarded
caller must retain an outer timeout and collect device-lost/GPU-fault evidence.

Before presentation, normal resources are destroyed only when no work was
submitted or acquired. A successful acquire that is not consumed by a queue
submission leaves its binary semaphore signaled, so image-index validation,
fence-reset, or submit failure conservatively skips every Vulkan destroy call.
Timeout, device loss, or an indeterminate submit does the same. Once
`vkQueuePresentKHR` is attempted,
even a completed render fence does not prove that the presentation engine has
released the swapchain image under strict Vulkan 1.0. Presentation-attempt
state is sticky across every later frame and failure. The app therefore keeps
all Vulkan objects alive, ignores later windows, and exits its
manifest-isolated dedicated process immediately when the activity is
destroyed; process exit is the cleanup boundary. This deliberately avoids an
unbounded idle wait and unsafe rotation/window-close teardown, at the cost of
requiring relaunch for every new window. A larger recreate/present-completion
framework is out of scope for this proving tool.

`RunSmoke` is synchronous and does not pump NativeActivity lifecycle callbacks
while it renders. In the fully cooperative timeout cases, four 1-second
acquires plus four 5-second fence waits can total about 24 seconds; setup,
submit, present, and process exit still have no Vulkan/API deadline. Back,
rotation, or window close therefore has no prompt-cancellation guarantee during
that interval. The guarded run needs an external supervisor and must tolerate
the dedicated process being ended if a driver or kernel call stalls.

Desk work cannot establish ANativeWindow creation, A610 surface support,
app-domain access, gralloc interop, actual Ganesh presentation, physical pixel
colors, rotation behavior, compositor release, absence of device loss/GPU
reset, or performance. The D3-0127 prerequisite was measured from a
vendor-mapped root test path; it did not prove ordinary-app permission
equivalence. Those remain guarded hardware observations.
