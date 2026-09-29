#pragma once

#include <vulkan/vulkan.h>

#include <cstdint>

namespace bengal_wsi {

enum class GaneshStage {
  kNone,
  kContext,
  kSceneResources,
  kWrapSwapchainImage,
  kAcquireWait,
  kDraw,
  kFlushSignal,
  kSubmit,
  kFenceSubmit,
};

const char *GaneshStageName(GaneshStage stage);

struct GaneshCreateInfo {
  VkInstance instance = VK_NULL_HANDLE;
  VkPhysicalDevice physical_device = VK_NULL_HANDLE;
  VkDevice device = VK_NULL_HANDLE;
  VkQueue queue = VK_NULL_HANDLE;
  uint32_t queue_family = UINT32_MAX;
  const VkPhysicalDeviceFeatures2 *enabled_features = nullptr;
  const char *const *instance_extensions = nullptr;
  uint32_t instance_extension_count = 0;
  const char *const *device_extensions = nullptr;
  uint32_t device_extension_count = 0;
  const VkImage *images = nullptr;
  uint32_t image_count = 0;
  VkFormat format = VK_FORMAT_UNDEFINED;
  VkImageUsageFlags usage = 0;
  uint32_t width = 0;
  uint32_t height = 0;
  uint32_t pre_transform = 0;
};

struct GaneshFrameResult {
  VkResult result = VK_ERROR_INITIALIZATION_FAILED;
  GaneshStage stage = GaneshStage::kNone;
};

// Private adapter over the isolated Bengal Skia archive. The caller owns all
// Vulkan handles and must destroy this object before them only when no acquired
// signal, submitted work, or presentation can remain. Unsafe paths deliberately
// leak it until dedicated-process exit.
class PrivateGaneshRenderer {
public:
  static PrivateGaneshRenderer *Create(const GaneshCreateInfo &info,
                                       GaneshStage *failed_stage);
  ~PrivateGaneshRenderer();

  PrivateGaneshRenderer(const PrivateGaneshRenderer &) = delete;
  PrivateGaneshRenderer &operator=(const PrivateGaneshRenderer &) = delete;

  GaneshFrameResult RenderAndSubmit(uint32_t frame, uint32_t image_index,
                                    VkSemaphore acquire_semaphore,
                                    VkSemaphore render_finished,
                                    VkFence completion_fence);

private:
  struct Impl;
  explicit PrivateGaneshRenderer(Impl *impl);
  Impl *impl_ = nullptr;
};

} // namespace bengal_wsi
