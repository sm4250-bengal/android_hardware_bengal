#pragma once

#include <cstdint>

namespace bengal_wsi {

constexpr int32_t kFrameSuccess = 0;
constexpr int32_t kFrameTimeout = 2;
constexpr int32_t kFrameDeviceLost = -4;
constexpr int32_t kFrameSuboptimal = 1000001003;
constexpr int32_t kFrameOutOfDate = -1000001004;

enum class FrameOperation {
  kNone,
  kAcquire,
  kImageIndexValidation,
  kResetFence,
  kSubmit,
  kWaitFence,
  kPresent,
};

struct FrameLoopOutcome {
  bool completed = false;
  bool stopped_suboptimal = false;
  bool present_attempted_ever = false;
  bool acquire_signal_pending = false;
  bool submitted_work_indeterminate = false;
  bool safe_cleanup_called = false;
  uint32_t presented_count = 0;
  uint32_t failed_frame = 0;
  FrameOperation failed_operation = FrameOperation::kNone;
  int32_t raw_result = kFrameSuccess;
};

struct FrameLoopCallbacks {
  void *context = nullptr;
  int32_t (*acquire)(void *context, uint32_t frame,
                     uint32_t *image_index) = nullptr;
  int32_t (*reset_fence)(void *context, uint32_t frame) = nullptr;
  int32_t (*submit)(void *context, uint32_t frame,
                    uint32_t image_index) = nullptr;
  int32_t (*wait_fence)(void *context, uint32_t frame) = nullptr;
  int32_t (*present)(void *context, uint32_t frame,
                     uint32_t image_index) = nullptr;
  void (*safe_cleanup)(void *context) = nullptr;
};

const char *FrameOperationName(FrameOperation operation);
bool FrameOutcomeUnsafe(const FrameLoopOutcome &outcome);

// Runs the same acquire/reset/submit/fence/present transition sequence used by
// the Android Vulkan path. On failure it invokes safe_cleanup only when no
// earlier present, pending acquire signal, or indeterminate submit remains.
FrameLoopOutcome RunBoundedFrameLoop(uint32_t frame_count,
                                     uint32_t swapchain_image_count,
                                     const FrameLoopCallbacks &callbacks);

} // namespace bengal_wsi
