#include "frame_loop.h"

namespace bengal_wsi {
namespace {

bool IsSuccessOrSuboptimal(int32_t result) {
  return result == kFrameSuccess || result == kFrameSuboptimal;
}

FrameLoopOutcome FailFrame(FrameLoopOutcome outcome, uint32_t frame,
                           FrameOperation operation, int32_t result,
                           const FrameLoopCallbacks &callbacks) {
  outcome.failed_frame = frame;
  outcome.failed_operation = operation;
  outcome.raw_result = result;
  if (!FrameOutcomeUnsafe(outcome) && callbacks.safe_cleanup != nullptr) {
    callbacks.safe_cleanup(callbacks.context);
    outcome.safe_cleanup_called = true;
  }
  return outcome;
}

} // namespace

const char *FrameOperationName(FrameOperation operation) {
  switch (operation) {
  case FrameOperation::kAcquire:
    return "vkAcquireNextImageKHR";
  case FrameOperation::kImageIndexValidation:
    return "acquired_image_index_validation";
  case FrameOperation::kResetFence:
    return "vkResetFences";
  case FrameOperation::kSubmit:
    return "vkQueueSubmit";
  case FrameOperation::kWaitFence:
    return "vkWaitForFences";
  case FrameOperation::kPresent:
    return "vkQueuePresentKHR";
  case FrameOperation::kNone:
  default:
    return "none";
  }
}

bool FrameOutcomeUnsafe(const FrameLoopOutcome &outcome) {
  return outcome.present_attempted_ever || outcome.acquire_signal_pending ||
         outcome.submitted_work_indeterminate ||
         outcome.raw_result == kFrameDeviceLost;
}

FrameLoopOutcome RunBoundedFrameLoop(uint32_t frame_count,
                                     uint32_t swapchain_image_count,
                                     const FrameLoopCallbacks &callbacks) {
  FrameLoopOutcome outcome;
  if (callbacks.acquire == nullptr || callbacks.reset_fence == nullptr ||
      callbacks.submit == nullptr || callbacks.wait_fence == nullptr ||
      callbacks.present == nullptr || swapchain_image_count == 0) {
    return FailFrame(outcome, 0, FrameOperation::kImageIndexValidation, -1,
                     callbacks);
  }

  for (uint32_t frame = 0; frame < frame_count; ++frame) {
    uint32_t image_index = UINT32_MAX;
    int32_t result = callbacks.acquire(callbacks.context, frame, &image_index);
    if (!IsSuccessOrSuboptimal(result)) {
      return FailFrame(outcome, frame, FrameOperation::kAcquire, result,
                       callbacks);
    }

    // A successful/suboptimal acquire signals the supplied binary semaphore.
    // Keep this sticky until a successful submit and completed fence prove the
    // queue consumed it.
    outcome.acquire_signal_pending = true;
    if (image_index >= swapchain_image_count) {
      return FailFrame(outcome, frame, FrameOperation::kImageIndexValidation,
                       result, callbacks);
    }

    result = callbacks.reset_fence(callbacks.context, frame);
    if (result != kFrameSuccess) {
      return FailFrame(outcome, frame, FrameOperation::kResetFence, result,
                       callbacks);
    }

    outcome.submitted_work_indeterminate = true;
    result = callbacks.submit(callbacks.context, frame, image_index);
    if (result != kFrameSuccess) {
      return FailFrame(outcome, frame, FrameOperation::kSubmit, result,
                       callbacks);
    }

    result = callbacks.wait_fence(callbacks.context, frame);
    if (result != kFrameSuccess) {
      return FailFrame(outcome, frame, FrameOperation::kWaitFence, result,
                       callbacks);
    }
    outcome.submitted_work_indeterminate = false;
    outcome.acquire_signal_pending = false;

    // Sticky by design and set before the call. Even an error return cannot
    // prove that the presentation engine did not retain the swapchain image.
    outcome.present_attempted_ever = true;
    result = callbacks.present(callbacks.context, frame, image_index);
    if (!IsSuccessOrSuboptimal(result)) {
      return FailFrame(outcome, frame, FrameOperation::kPresent, result,
                       callbacks);
    }
    ++outcome.presented_count;
    if (result == kFrameSuboptimal) {
      outcome.stopped_suboptimal = true;
      outcome.completed = true;
      return outcome;
    }
  }

  outcome.completed = true;
  return outcome;
}

} // namespace bengal_wsi
