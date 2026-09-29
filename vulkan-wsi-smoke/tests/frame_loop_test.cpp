#include "frame_loop.h"

#include <cstdio>

namespace {

using bengal_wsi::FrameLoopCallbacks;
using bengal_wsi::FrameLoopOutcome;

enum class InjectAt {
  kNone,
  kAcquire,
  kInvalidImageIndex,
  kReset,
  kSubmit,
  kWait,
  kPresent,
};

struct Script {
  uint32_t failure_frame = UINT32_MAX;
  InjectAt inject_at = InjectAt::kNone;
  int32_t injected_result = -1;
  uint32_t vulkan_destroy_calls = 0;
};

int32_t Acquire(void *opaque, uint32_t frame, uint32_t *image_index) {
  auto *script = static_cast<Script *>(opaque);
  if (frame == script->failure_frame &&
      script->inject_at == InjectAt::kAcquire) {
    return script->injected_result;
  }
  *image_index = frame % 2;
  if (frame == script->failure_frame &&
      script->inject_at == InjectAt::kInvalidImageIndex) {
    *image_index = 2;
  }
  return bengal_wsi::kFrameSuccess;
}

int32_t Reset(void *opaque, uint32_t frame) {
  auto *script = static_cast<Script *>(opaque);
  return frame == script->failure_frame && script->inject_at == InjectAt::kReset
             ? script->injected_result
             : bengal_wsi::kFrameSuccess;
}

int32_t Submit(void *opaque, uint32_t frame, uint32_t) {
  auto *script = static_cast<Script *>(opaque);
  return frame == script->failure_frame &&
                 script->inject_at == InjectAt::kSubmit
             ? script->injected_result
             : bengal_wsi::kFrameSuccess;
}

int32_t Wait(void *opaque, uint32_t frame) {
  auto *script = static_cast<Script *>(opaque);
  return frame == script->failure_frame && script->inject_at == InjectAt::kWait
             ? script->injected_result
             : bengal_wsi::kFrameSuccess;
}

int32_t Present(void *opaque, uint32_t frame, uint32_t) {
  auto *script = static_cast<Script *>(opaque);
  return frame == script->failure_frame &&
                 script->inject_at == InjectAt::kPresent
             ? script->injected_result
             : bengal_wsi::kFrameSuccess;
}

void SafeCleanup(void *opaque) {
  // This callback occupies the same seam as SafeFrameCleanup, the only frame
  // failure route that calls Cleanup and therefore any vkDestroy* function.
  ++static_cast<Script *>(opaque)->vulkan_destroy_calls;
}

FrameLoopOutcome Run(Script *script) {
  const FrameLoopCallbacks callbacks{script, Acquire, Reset,      Submit,
                                     Wait,   Present, SafeCleanup};
  return bengal_wsi::RunBoundedFrameLoop(4, 2, callbacks);
}

bool UnsafeWithNoDestroy(Script script) {
  const FrameLoopOutcome outcome = Run(&script);
  return !outcome.completed && bengal_wsi::FrameOutcomeUnsafe(outcome) &&
         !outcome.safe_cleanup_called && script.vulkan_destroy_calls == 0;
}

bool StickyPresentFailureWithNoDestroy(Script script) {
  const FrameLoopOutcome outcome = Run(&script);
  return !outcome.completed && outcome.present_attempted_ever &&
         bengal_wsi::FrameOutcomeUnsafe(outcome) &&
         !outcome.safe_cleanup_called && script.vulkan_destroy_calls == 0;
}

} // namespace

int main() {
  bool pass = true;

  Script second_acquire_timeout{1, InjectAt::kAcquire,
                                bengal_wsi::kFrameTimeout};
  const FrameLoopOutcome timeout = Run(&second_acquire_timeout);
  pass &= !timeout.completed && timeout.present_attempted_ever &&
          timeout.failed_frame == 1 &&
          timeout.failed_operation == bengal_wsi::FrameOperation::kAcquire &&
          timeout.raw_result == bengal_wsi::kFrameTimeout &&
          second_acquire_timeout.vulkan_destroy_calls == 0;

  pass &= StickyPresentFailureWithNoDestroy(
      {1, InjectAt::kAcquire, bengal_wsi::kFrameOutOfDate});
  pass &= StickyPresentFailureWithNoDestroy({1, InjectAt::kReset, -13});
  pass &= StickyPresentFailureWithNoDestroy(
      {1, InjectAt::kAcquire, bengal_wsi::kFrameDeviceLost});

  // Before any present, a successful acquire still leaves the acquire
  // semaphore signaled if validation/reset aborts before queue submission.
  pass &= UnsafeWithNoDestroy({0, InjectAt::kInvalidImageIndex, -1});
  pass &= UnsafeWithNoDestroy({0, InjectAt::kReset, -13});
  pass &= UnsafeWithNoDestroy({0, InjectAt::kSubmit, -13});
  pass &= UnsafeWithNoDestroy({0, InjectAt::kWait, bengal_wsi::kFrameTimeout});
  pass &= UnsafeWithNoDestroy(
      {0, InjectAt::kAcquire, bengal_wsi::kFrameDeviceLost});

  Script first_acquire_timeout{0, InjectAt::kAcquire,
                               bengal_wsi::kFrameTimeout};
  const FrameLoopOutcome safe = Run(&first_acquire_timeout);
  pass &= !safe.completed && !bengal_wsi::FrameOutcomeUnsafe(safe) &&
          safe.safe_cleanup_called &&
          first_acquire_timeout.vulkan_destroy_calls == 1;

  Script complete;
  const FrameLoopOutcome success = Run(&complete);
  pass &= success.completed && success.present_attempted_ever &&
          success.presented_count == 4 && complete.vulkan_destroy_calls == 0;

  std::printf("FRAME_LOOP_TEST verdict=%s sticky_present_failures=4 "
              "pending_or_work_failures=4 device_lost_control=1 "
              "vulkanDestroyCallsOnUnsafe=0 safe_cleanup_control=1 "
              "complete=1\n",
              pass ? "PASS" : "FAIL");
  return pass ? 0 : 1;
}
