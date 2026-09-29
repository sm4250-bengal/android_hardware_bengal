#define VK_USE_PLATFORM_ANDROID_KHR 1

#include <vulkan/vulkan.h>

#include <android/log.h>
#include <android_native_app_glue.h>

#include <algorithm>
#include <cinttypes>
#include <cstdarg>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#ifdef BENGAL_WSI_PRIVATE_GANESH
#include <iterator>
#endif
#include <limits>
#include <string>
#include <vector>

#include "frame_loop.h"
#include "grid_spv.h"
#include "logic.h"
#ifdef BENGAL_WSI_PRIVATE_GANESH
#include "ganesh_renderer.h"
#endif

namespace {

using bengal_wsi::Extent;
using bengal_wsi::FrameLoopCallbacks;
using bengal_wsi::FrameLoopOutcome;
using bengal_wsi::FrameOperation;
using bengal_wsi::GeometryPlan;
using bengal_wsi::IdentityInput;
using bengal_wsi::LifecycleAction;
using bengal_wsi::LifecycleEvent;
using bengal_wsi::TeardownDecision;

constexpr char kLogTag[] = "BengalVulkanWsi";
constexpr uint32_t kMaxPhysicalDevices = 16;
constexpr uint32_t kMaxInstanceExtensions = 256;
constexpr uint32_t kMaxDeviceExtensions = 256;
constexpr uint32_t kMaxQueueFamilies = 32;
constexpr uint32_t kMaxSurfaceFormats = 64;
constexpr uint32_t kMaxPresentModes = 16;
constexpr uint32_t kMaxSwapchainImages = 8;
constexpr uint64_t kMaxSwapchainPixels = 4096ULL * 4096ULL;
constexpr uint32_t kFrameCount = 4;
constexpr uint64_t kAcquireTimeoutNs = 1'000'000'000ULL;
constexpr uint64_t kFenceTimeoutNs = 5'000'000'000ULL;

#ifdef BENGAL_WSI_PRIVATE_GANESH
constexpr const char *kGaneshInstanceExtensions[] = {
    VK_KHR_SURFACE_EXTENSION_NAME,
    VK_KHR_ANDROID_SURFACE_EXTENSION_NAME,
    VK_KHR_GET_PHYSICAL_DEVICE_PROPERTIES_2_EXTENSION_NAME,
    VK_KHR_EXTERNAL_MEMORY_CAPABILITIES_EXTENSION_NAME,
};
constexpr const char *kGaneshDeviceExtensions[] = {
    VK_KHR_SWAPCHAIN_EXTENSION_NAME,
    VK_KHR_GET_MEMORY_REQUIREMENTS_2_EXTENSION_NAME,
    VK_KHR_BIND_MEMORY_2_EXTENSION_NAME,
    VK_KHR_DEDICATED_ALLOCATION_EXTENSION_NAME,
    VK_KHR_MAINTENANCE1_EXTENSION_NAME,
    VK_KHR_MAINTENANCE2_EXTENSION_NAME,
    VK_KHR_MAINTENANCE3_EXTENSION_NAME,
    VK_KHR_SAMPLER_YCBCR_CONVERSION_EXTENSION_NAME,
};

#endif

static_assert(VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU == 1);
static_assert(VK_PHYSICAL_DEVICE_TYPE_CPU == 4);
static_assert(VK_DRIVER_ID_MESA_TURNIP == 18);
static_assert(VK_SUCCESS == bengal_wsi::kFrameSuccess);
static_assert(VK_TIMEOUT == bengal_wsi::kFrameTimeout);
static_assert(VK_ERROR_DEVICE_LOST == bengal_wsi::kFrameDeviceLost);
static_assert(VK_SUBOPTIMAL_KHR == bengal_wsi::kFrameSuboptimal);
static_assert(VK_ERROR_OUT_OF_DATE_KHR == bengal_wsi::kFrameOutOfDate);
static_assert(VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR ==
              bengal_wsi::kTransformIdentity);
static_assert(VK_SURFACE_TRANSFORM_HORIZONTAL_MIRROR_ROTATE_270_BIT_KHR ==
              bengal_wsi::kTransformHorizontalMirrorRotate270);

enum class RunResult {
  kNotRun,
  kPresentedUnobserved,
  kIdentityRejected,
  kSetupFailed,
  kAcquireTimeout,
  kDeviceLost,
  kSubmitFailed,
  kFenceTimeout,
  kPresentFailed,
};

struct AppState {
  android_app *app = nullptr;
  bool window_ready = false;
  bool run_started = false;
  bool run_finished = false;
  bool lifecycle_changed_after_start = false;
  bool unsafe_teardown = false;
  RunResult result = RunResult::kNotRun;
};

struct Identity {
  VkPhysicalDeviceProperties properties{};
  bool driver_properties_available = false;
  VkDriverId driver_id = static_cast<VkDriverId>(0);
  char driver_name[VK_MAX_DRIVER_NAME_SIZE]{};
  char driver_info[VK_MAX_DRIVER_INFO_SIZE]{};
};

struct Resources {
  VkInstance instance = VK_NULL_HANDLE;
  VkSurfaceKHR surface = VK_NULL_HANDLE;
  VkDevice device = VK_NULL_HANDLE;
  VkSwapchainKHR swapchain = VK_NULL_HANDLE;
  VkRenderPass render_pass = VK_NULL_HANDLE;
  VkPipelineLayout pipeline_layout = VK_NULL_HANDLE;
  VkPipeline pipeline = VK_NULL_HANDLE;
  VkShaderModule vert = VK_NULL_HANDLE;
  VkShaderModule frag = VK_NULL_HANDLE;
  VkCommandPool command_pool = VK_NULL_HANDLE;
  VkSemaphore image_available = VK_NULL_HANDLE;
  std::vector<VkSemaphore> render_finished;
  VkFence fence = VK_NULL_HANDLE;
  std::vector<VkImage> images;
  std::vector<VkImageView> views;
  std::vector<VkFramebuffer> framebuffers;
  std::vector<VkCommandBuffer> command_buffers;
#ifdef BENGAL_WSI_PRIVATE_GANESH
  bengal_wsi::PrivateGaneshRenderer *ganesh = nullptr;
#endif
};

void Tell(const char *format, ...) __attribute__((format(printf, 1, 2)));

void Tell(const char *format, ...) {
  char line[1536];
  va_list args;
  va_start(args, format);
  std::vsnprintf(line, sizeof(line), format, args);
  va_end(args);
  std::printf("%s\n", line);
  __android_log_print(ANDROID_LOG_INFO, kLogTag, "%s", line);
}

const char *VkResultName(VkResult result) {
  switch (result) {
  case VK_SUCCESS:
    return "VK_SUCCESS";
  case VK_NOT_READY:
    return "VK_NOT_READY";
  case VK_TIMEOUT:
    return "VK_TIMEOUT";
  case VK_INCOMPLETE:
    return "VK_INCOMPLETE";
  case VK_SUBOPTIMAL_KHR:
    return "VK_SUBOPTIMAL_KHR";
  case VK_ERROR_OUT_OF_HOST_MEMORY:
    return "VK_ERROR_OUT_OF_HOST_MEMORY";
  case VK_ERROR_OUT_OF_DEVICE_MEMORY:
    return "VK_ERROR_OUT_OF_DEVICE_MEMORY";
  case VK_ERROR_INITIALIZATION_FAILED:
    return "VK_ERROR_INITIALIZATION_FAILED";
  case VK_ERROR_DEVICE_LOST:
    return "VK_ERROR_DEVICE_LOST";
  case VK_ERROR_LAYER_NOT_PRESENT:
    return "VK_ERROR_LAYER_NOT_PRESENT";
  case VK_ERROR_EXTENSION_NOT_PRESENT:
    return "VK_ERROR_EXTENSION_NOT_PRESENT";
  case VK_ERROR_FEATURE_NOT_PRESENT:
    return "VK_ERROR_FEATURE_NOT_PRESENT";
  case VK_ERROR_INCOMPATIBLE_DRIVER:
    return "VK_ERROR_INCOMPATIBLE_DRIVER";
  case VK_ERROR_SURFACE_LOST_KHR:
    return "VK_ERROR_SURFACE_LOST_KHR";
  case VK_ERROR_NATIVE_WINDOW_IN_USE_KHR:
    return "VK_ERROR_NATIVE_WINDOW_IN_USE_KHR";
  case VK_ERROR_OUT_OF_DATE_KHR:
    return "VK_ERROR_OUT_OF_DATE_KHR";
  default:
    return "VK_RESULT_OTHER";
  }
}

bool HasExtension(const std::vector<VkExtensionProperties> &extensions,
                  const char *name) {
  return std::any_of(extensions.begin(), extensions.end(),
                     [name](const auto &extension) {
                       return std::strcmp(extension.extensionName, name) == 0;
                     });
}

#ifdef BENGAL_WSI_PRIVATE_GANESH
template <size_t N>
bool HasEveryExtension(const std::vector<VkExtensionProperties> &available,
                       const char *const (&required)[N]) {
  return std::all_of(std::begin(required), std::end(required),
                     [&available](const char *name) {
                       return HasExtension(available, name);
                     });
}
#endif

bool EnumerateInstanceExtensions(
    std::vector<VkExtensionProperties> *extensions) {
  uint32_t count = 0;
  VkResult result =
      vkEnumerateInstanceExtensionProperties(nullptr, &count, nullptr);
  if (result != VK_SUCCESS || count > kMaxInstanceExtensions) {
    Tell("INSTANCE_EXTENSIONS verdict=FAIL vk=%d name=%s count=%u cap=%u",
         result, VkResultName(result), count, kMaxInstanceExtensions);
    return false;
  }
  extensions->resize(count);
  result = count == 0 ? VK_SUCCESS
                      : vkEnumerateInstanceExtensionProperties(
                            nullptr, &count, extensions->data());
  if (result != VK_SUCCESS || count > extensions->size()) {
    Tell("INSTANCE_EXTENSIONS verdict=FAIL vk=%d name=%s reason=second_query",
         result, VkResultName(result));
    return false;
  }
  extensions->resize(count);
  Tell("INSTANCE_EXTENSIONS verdict=PASS count=%u cap=%u", count,
       kMaxInstanceExtensions);
  return true;
}

bool EnumerateDeviceExtensions(VkPhysicalDevice physical, uint32_t device_index,
                               std::vector<VkExtensionProperties> *extensions) {
  uint32_t count = 0;
  VkResult result =
      vkEnumerateDeviceExtensionProperties(physical, nullptr, &count, nullptr);
  if (result != VK_SUCCESS || count > kMaxDeviceExtensions) {
    Tell("DEVICE_EXTENSIONS device=%u verdict=FAIL vk=%d name=%s count=%u "
         "cap=%u",
         device_index, result, VkResultName(result), count,
         kMaxDeviceExtensions);
    return false;
  }
  extensions->resize(count);
  result = count == 0 ? VK_SUCCESS
                      : vkEnumerateDeviceExtensionProperties(
                            physical, nullptr, &count, extensions->data());
  if (result != VK_SUCCESS || count > extensions->size()) {
    Tell("DEVICE_EXTENSIONS device=%u verdict=FAIL vk=%d name=%s "
         "reason=second_query",
         device_index, result, VkResultName(result));
    return false;
  }
  extensions->resize(count);
  Tell("DEVICE_EXTENSIONS device=%u verdict=PASS count=%u cap=%u", device_index,
       count, kMaxDeviceExtensions);
  return true;
}

Identity
ReadIdentity(VkInstance instance, VkPhysicalDevice physical,
             const std::vector<VkExtensionProperties> &device_extensions,
             bool properties2_enabled) {
  Identity identity;
  vkGetPhysicalDeviceProperties(physical, &identity.properties);
  if (!properties2_enabled ||
      !HasExtension(device_extensions,
                    VK_KHR_DRIVER_PROPERTIES_EXTENSION_NAME)) {
    return identity;
  }
  auto get_properties2 =
      reinterpret_cast<PFN_vkGetPhysicalDeviceProperties2KHR>(
          vkGetInstanceProcAddr(instance, "vkGetPhysicalDeviceProperties2KHR"));
  if (get_properties2 == nullptr)
    return identity;
  VkPhysicalDeviceDriverPropertiesKHR driver{};
  driver.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES_KHR;
  VkPhysicalDeviceProperties2KHR properties2{};
  properties2.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2_KHR;
  properties2.pNext = &driver;
  get_properties2(physical, &properties2);
  identity.properties = properties2.properties;
  identity.driver_properties_available = true;
  identity.driver_id = driver.driverID;
  std::snprintf(identity.driver_name, sizeof(identity.driver_name), "%s",
                driver.driverName);
  std::snprintf(identity.driver_info, sizeof(identity.driver_info), "%s",
                driver.driverInfo);
  return identity;
}

void PrintIdentity(uint32_t index, const Identity &identity) {
  const auto &p = identity.properties;
  Tell("DEVICE index=%u api_raw=%u api=%u.%u.%u driverVersion_raw=%u "
       "vendorID=0x%08x deviceID=0x%08x deviceType_raw=%u name=\"%s\"",
       index, p.apiVersion, VK_VERSION_MAJOR(p.apiVersion),
       VK_VERSION_MINOR(p.apiVersion), VK_VERSION_PATCH(p.apiVersion),
       p.driverVersion, p.vendorID, p.deviceID, p.deviceType, p.deviceName);
  if (identity.driver_properties_available) {
    Tell("DRIVER_PROPERTIES device=%u available=1 driverID_raw=%u "
         "driverName=\"%s\" driverInfo=\"%s\"",
         index, static_cast<uint32_t>(identity.driver_id), identity.driver_name,
         identity.driver_info);
  } else {
    Tell("DRIVER_PROPERTIES device=%u available=0 driverID_raw=NA", index);
  }
}

struct QueueTopology {
  uint32_t family_count = 0;
  uint32_t total_queue_count = 0;
  uint32_t graphics_present_family = UINT32_MAX;
  bool exact_one = false;
};

QueueTopology ReadQueues(VkPhysicalDevice physical, VkSurfaceKHR surface,
                         uint32_t device_index) {
  QueueTopology topology;
  vkGetPhysicalDeviceQueueFamilyProperties(physical, &topology.family_count,
                                           nullptr);
  if (topology.family_count == 0 || topology.family_count > kMaxQueueFamilies) {
    Tell("QUEUES device=%u verdict=FAIL familyCount=%u cap=%u", device_index,
         topology.family_count, kMaxQueueFamilies);
    return topology;
  }
  std::vector<VkQueueFamilyProperties> queues(topology.family_count);
  uint32_t returned_count = topology.family_count;
  vkGetPhysicalDeviceQueueFamilyProperties(physical, &returned_count,
                                           queues.data());
  if (returned_count > queues.size()) {
    Tell("QUEUES device=%u verdict=FAIL reason=count_changed count=%u",
         device_index, returned_count);
    topology.family_count = 0;
    return topology;
  }
  topology.family_count = returned_count;
#ifdef BENGAL_WSI_MESA26_PROFILE
  std::vector<bengal_wsi::QueueFamilyInput> profile_families;
  profile_families.reserve(returned_count);
#endif
  for (uint32_t i = 0; i < returned_count; ++i) {
    VkBool32 present = VK_FALSE;
    const VkResult result =
        vkGetPhysicalDeviceSurfaceSupportKHR(physical, i, surface, &present);
    if (result != VK_SUCCESS) {
      Tell("QUEUE_FAMILY device=%u index=%u verdict=FAIL surfaceSupportVk=%d "
           "name=%s",
           device_index, i, result, VkResultName(result));
      continue;
    }
    topology.total_queue_count += queues[i].queueCount;
    const bool graphics = (queues[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) != 0;
#ifdef BENGAL_WSI_MESA26_PROFILE
    profile_families.push_back({queues[i].queueCount, queues[i].queueFlags,
                                graphics, present == VK_TRUE});
#endif
    Tell("QUEUE_FAMILY device=%u index=%u queueCount=%u flags_raw=0x%08x "
         "graphics=%u present=%u",
         device_index, i, queues[i].queueCount, queues[i].queueFlags, graphics,
         present);
    if (topology.graphics_present_family == UINT32_MAX && graphics && present &&
        queues[i].queueCount > 0) {
      topology.graphics_present_family = i;
    }
  }
#ifdef BENGAL_WSI_MESA26_PROFILE
  topology.exact_one = bengal_wsi::ValidateMesa26QueueTopology(
      profile_families.data(), static_cast<uint32_t>(profile_families.size()),
      &topology.graphics_present_family);
  Tell("QUEUES device=%u verdict=%s familyCount=%u totalQueueCount=%u "
       "selectedFamily=%u mesa26GraphicsPlusSparse=%u sparseQueueUnused=1",
       device_index, topology.exact_one ? "PASS" : "FAIL",
       topology.family_count, topology.total_queue_count,
       topology.graphics_present_family, topology.exact_one);
#else
  topology.exact_one = topology.family_count == 1 &&
                       topology.total_queue_count == 1 &&
                       topology.graphics_present_family != UINT32_MAX;
  Tell("QUEUES device=%u verdict=%s familyCount=%u totalQueueCount=%u "
       "selectedFamily=%u exactOne=%u",
       device_index,
       topology.graphics_present_family == UINT32_MAX ? "FAIL" : "PASS",
       topology.family_count, topology.total_queue_count,
       topology.graphics_present_family, topology.exact_one);
#endif
  return topology;
}

void Cleanup(Resources *r) {
  if (r->device != VK_NULL_HANDLE) {
#ifdef BENGAL_WSI_PRIVATE_GANESH
    delete r->ganesh;
    r->ganesh = nullptr;
#endif
    if (r->fence != VK_NULL_HANDLE)
      vkDestroyFence(r->device, r->fence, nullptr);
    for (VkSemaphore semaphore : r->render_finished)
      vkDestroySemaphore(r->device, semaphore, nullptr);
    if (r->image_available != VK_NULL_HANDLE)
      vkDestroySemaphore(r->device, r->image_available, nullptr);
    if (r->command_pool != VK_NULL_HANDLE)
      vkDestroyCommandPool(r->device, r->command_pool, nullptr);
    for (VkFramebuffer framebuffer : r->framebuffers)
      vkDestroyFramebuffer(r->device, framebuffer, nullptr);
    if (r->pipeline != VK_NULL_HANDLE)
      vkDestroyPipeline(r->device, r->pipeline, nullptr);
    if (r->pipeline_layout != VK_NULL_HANDLE)
      vkDestroyPipelineLayout(r->device, r->pipeline_layout, nullptr);
    if (r->vert != VK_NULL_HANDLE)
      vkDestroyShaderModule(r->device, r->vert, nullptr);
    if (r->frag != VK_NULL_HANDLE)
      vkDestroyShaderModule(r->device, r->frag, nullptr);
    if (r->render_pass != VK_NULL_HANDLE)
      vkDestroyRenderPass(r->device, r->render_pass, nullptr);
    for (VkImageView view : r->views)
      vkDestroyImageView(r->device, view, nullptr);
    if (r->swapchain != VK_NULL_HANDLE)
      vkDestroySwapchainKHR(r->device, r->swapchain, nullptr);
    vkDestroyDevice(r->device, nullptr);
  }
  if (r->surface != VK_NULL_HANDLE && r->instance != VK_NULL_HANDLE)
    vkDestroySurfaceKHR(r->instance, r->surface, nullptr);
  if (r->instance != VK_NULL_HANDLE)
    vkDestroyInstance(r->instance, nullptr);
  *r = {};
}

RunResult Fail(Resources *resources, const char *operation, VkResult result,
               bool present_attempted, bool work_indeterminate,
               bool *unsafe_teardown) {
  Tell("FAIL operation=%s vk=%d name=%s", operation, result,
       VkResultName(result));
  const TeardownDecision decision = bengal_wsi::DecideTeardown(
      present_attempted, work_indeterminate || result == VK_ERROR_DEVICE_LOST);
  *unsafe_teardown = decision == TeardownDecision::kSkipAllVulkan;
  if (*unsafe_teardown) {
    Tell("CLEANUP verdict=SKIPPED_UNSAFE reason=%s presentAttempted=%u "
         "workIndeterminate=%u noQueueWaitIdle=1 allVulkanDestroyCalls=0",
         result == VK_ERROR_DEVICE_LOST
             ? "device_lost"
             : (present_attempted ? "presentation_release_unproven"
                                  : "submitted_work_indeterminate"),
         present_attempted, work_indeterminate);
  } else {
    Cleanup(resources);
    Tell("CLEANUP verdict=PASS reason=no_submit_or_completed_fence "
         "noQueueWaitIdle=1");
  }
  if (result == VK_ERROR_DEVICE_LOST)
    return RunResult::kDeviceLost;
  if (std::strcmp(operation, "vkAcquireNextImageKHR") == 0 &&
      result == VK_TIMEOUT) {
    return RunResult::kAcquireTimeout;
  }
  if (std::strcmp(operation, "vkWaitForFences") == 0 && result == VK_TIMEOUT) {
    return RunResult::kFenceTimeout;
  }
  if (std::strcmp(operation, "vkQueueSubmit") == 0)
    return RunResult::kSubmitFailed;
  if (std::strcmp(operation, "vkQueuePresentKHR") == 0)
    return RunResult::kPresentFailed;
  return RunResult::kSetupFailed;
}

bool EnumerateSurfaceFormats(VkPhysicalDevice physical, VkSurfaceKHR surface,
                             std::vector<VkSurfaceFormatKHR> *formats) {
  uint32_t count = 0;
  VkResult result =
      vkGetPhysicalDeviceSurfaceFormatsKHR(physical, surface, &count, nullptr);
  if (result != VK_SUCCESS || count == 0 || count > kMaxSurfaceFormats) {
    Tell("SURFACE_FORMATS verdict=FAIL vk=%d name=%s count=%u cap=%u", result,
         VkResultName(result), count, kMaxSurfaceFormats);
    return false;
  }
  formats->resize(count);
  result = vkGetPhysicalDeviceSurfaceFormatsKHR(physical, surface, &count,
                                                formats->data());
  if (result != VK_SUCCESS || count > formats->size())
    return false;
  formats->resize(count);
  Tell("SURFACE_FORMATS verdict=PASS count=%u cap=%u", count,
       kMaxSurfaceFormats);
  return true;
}

bool EnumeratePresentModes(VkPhysicalDevice physical, VkSurfaceKHR surface,
                           std::vector<VkPresentModeKHR> *modes) {
  uint32_t count = 0;
  VkResult result = vkGetPhysicalDeviceSurfacePresentModesKHR(physical, surface,
                                                              &count, nullptr);
  if (result != VK_SUCCESS || count == 0 || count > kMaxPresentModes) {
    Tell("PRESENT_MODES verdict=FAIL vk=%d name=%s count=%u cap=%u", result,
         VkResultName(result), count, kMaxPresentModes);
    return false;
  }
  modes->resize(count);
  result = vkGetPhysicalDeviceSurfacePresentModesKHR(physical, surface, &count,
                                                     modes->data());
  if (result != VK_SUCCESS || count > modes->size())
    return false;
  modes->resize(count);
  Tell("PRESENT_MODES verdict=PASS count=%u cap=%u fifo=%u", count,
       kMaxPresentModes,
       std::find(modes->begin(), modes->end(), VK_PRESENT_MODE_FIFO_KHR) !=
           modes->end());
  return true;
}

VkSurfaceFormatKHR
ChooseSurfaceFormat(const std::vector<VkSurfaceFormatKHR> &formats) {
  constexpr VkFormat preferred[] = {VK_FORMAT_R8G8B8A8_UNORM,
                                    VK_FORMAT_B8G8R8A8_UNORM};
  for (VkFormat format : preferred) {
    for (const auto &candidate : formats) {
      if (candidate.format == format &&
          candidate.colorSpace == VK_COLOR_SPACE_SRGB_NONLINEAR_KHR) {
        return candidate;
      }
    }
  }
  return {VK_FORMAT_UNDEFINED, VK_COLOR_SPACE_SRGB_NONLINEAR_KHR};
}

#ifndef BENGAL_WSI_PRIVATE_GANESH
VkResult CreateShader(VkDevice device, const uint32_t *words, size_t bytes,
                      VkShaderModule *module) {
  VkShaderModuleCreateInfo info{};
  info.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO;
  info.codeSize = bytes;
  info.pCode = words;
  return vkCreateShaderModule(device, &info, nullptr, module);
}
#endif

struct VulkanFrameContext {
  Resources *resources = nullptr;
  VkDevice device = VK_NULL_HANDLE;
  VkSwapchainKHR swapchain = VK_NULL_HANDLE;
  VkQueue queue = VK_NULL_HANDLE;
};

int32_t AcquireFrame(void *opaque, uint32_t frame, uint32_t *image_index) {
  auto *context = static_cast<VulkanFrameContext *>(opaque);
  const VkResult result = vkAcquireNextImageKHR(
      context->device, context->swapchain, kAcquireTimeoutNs,
      context->resources->image_available, VK_NULL_HANDLE, image_index);
  Tell("FRAME_ACQUIRE frame=%u verdict=%s vk=%d name=%s timeoutNs=%" PRIu64
       " image=%u",
       frame,
       result == VK_SUCCESS || result == VK_SUBOPTIMAL_KHR ? "PASS" : "FAIL",
       result, VkResultName(result), kAcquireTimeoutNs, *image_index);
  return result;
}

int32_t ResetFrameFence(void *opaque, uint32_t frame) {
  auto *context = static_cast<VulkanFrameContext *>(opaque);
  const VkResult result =
      vkResetFences(context->device, 1, &context->resources->fence);
  Tell("FRAME_RESET_FENCE frame=%u verdict=%s vk=%d name=%s", frame,
       result == VK_SUCCESS ? "PASS" : "FAIL", result, VkResultName(result));
  return result;
}

int32_t SubmitFrame(void *opaque, uint32_t frame, uint32_t image_index) {
  auto *context = static_cast<VulkanFrameContext *>(opaque);
#ifdef BENGAL_WSI_PRIVATE_GANESH
  const bengal_wsi::GaneshFrameResult ganesh =
      context->resources->ganesh->RenderAndSubmit(
          frame, image_index, context->resources->image_available,
          context->resources->render_finished[frame],
          context->resources->fence);
  Tell("GANESH_FRAME frame=%u image=%u stage=%s verdict=%s vk=%d name=%s "
       "acquireWaitOwnedBySkia=1 presentSignalOwnedByFrame=1 "
       "emptyFenceSubmit=%u",
       frame, image_index, bengal_wsi::GaneshStageName(ganesh.stage),
       ganesh.result == VK_SUCCESS ? "PASS" : "FAIL", ganesh.result,
       VkResultName(ganesh.result),
       ganesh.stage == bengal_wsi::GaneshStage::kFenceSubmit &&
           ganesh.result == VK_SUCCESS);
  return ganesh.result;
#else
  const VkPipelineStageFlags wait_stage =
      VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
  VkSubmitInfo submit{};
  submit.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO;
  submit.waitSemaphoreCount = 1;
  submit.pWaitSemaphores = &context->resources->image_available;
  submit.pWaitDstStageMask = &wait_stage;
  submit.commandBufferCount = 1;
  submit.pCommandBuffers = &context->resources->command_buffers[image_index];
  submit.signalSemaphoreCount = 1;
  submit.pSignalSemaphores = &context->resources->render_finished[frame];
  const VkResult result =
      vkQueueSubmit(context->queue, 1, &submit, context->resources->fence);
  Tell("FRAME_SUBMIT frame=%u verdict=%s vk=%d name=%s queueCount=1 "
       "drawVertices=3",
       frame, result == VK_SUCCESS ? "PASS" : "FAIL", result,
       VkResultName(result));
  return result;
#endif
}

int32_t WaitFrameFence(void *opaque, uint32_t frame) {
  auto *context = static_cast<VulkanFrameContext *>(opaque);
  const VkResult result = vkWaitForFences(
      context->device, 1, &context->resources->fence, VK_TRUE, kFenceTimeoutNs);
  Tell("FRAME_FENCE frame=%u verdict=%s vk=%d name=%s timeoutNs=%" PRIu64,
       frame, result == VK_SUCCESS ? "PASS" : "FAIL", result,
       VkResultName(result), kFenceTimeoutNs);
  return result;
}

int32_t PresentFrame(void *opaque, uint32_t frame, uint32_t image_index) {
  auto *context = static_cast<VulkanFrameContext *>(opaque);
  VkPresentInfoKHR present{};
  present.sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR;
  present.waitSemaphoreCount = 1;
  present.pWaitSemaphores = &context->resources->render_finished[frame];
  present.swapchainCount = 1;
  present.pSwapchains = &context->swapchain;
  present.pImageIndices = &image_index;
  const VkResult result = vkQueuePresentKHR(context->queue, &present);
  Tell("FRAME_PRESENT_API frame=%u verdict=%s vk=%d name=%s "
       "presentedPixelsObserved=0",
       frame,
       result == VK_SUCCESS || result == VK_SUBOPTIMAL_KHR
           ? "ACCEPTED_NOT_OBSERVED"
           : "FAIL",
       result, VkResultName(result));
  return result;
}

void SafeFrameCleanup(void *opaque) {
  auto *context = static_cast<VulkanFrameContext *>(opaque);
  Cleanup(context->resources);
  Tell("CLEANUP verdict=PASS reason=no_present_no_pending_acquire_no_"
       "indeterminate_submit noQueueWaitIdle=1");
}

RunResult FrameFailureResult(FrameOperation operation, VkResult result) {
  if (result == VK_ERROR_DEVICE_LOST)
    return RunResult::kDeviceLost;
  if (operation == FrameOperation::kAcquire && result == VK_TIMEOUT)
    return RunResult::kAcquireTimeout;
  if (operation == FrameOperation::kWaitFence && result == VK_TIMEOUT)
    return RunResult::kFenceTimeout;
  if (operation == FrameOperation::kSubmit)
    return RunResult::kSubmitFailed;
  if (operation == FrameOperation::kPresent)
    return RunResult::kPresentFailed;
  return RunResult::kSetupFailed;
}

RunResult RunSmoke(ANativeWindow *window, bool *unsafe_teardown) {
  Resources r;
  *unsafe_teardown = false;
#ifdef BENGAL_WSI_MESA26_PROFILE
  Tell("MODE hardwareRequired=1 softwareControl=0 api=1.3 "
       "renderer=raw_mesa26_diagnostic frames=%u "
       "acquireTimeoutNs=%" PRIu64 " fenceTimeoutNs=%" PRIu64
       " maxImages=%u maxPixels=%" PRIu64,
       kFrameCount, kAcquireTimeoutNs, kFenceTimeoutNs, kMaxSwapchainImages,
       kMaxSwapchainPixels);
#elif defined(BENGAL_WSI_PRIVATE_GANESH)
  Tell("MODE hardwareRequired=1 softwareControl=0 api=1.0 "
       "renderer=private_ganesh frames=%u "
       "acquireTimeoutNs=%" PRIu64 " fenceTimeoutNs=%" PRIu64
       " maxImages=%u maxPixels=%" PRIu64,
       kFrameCount, kAcquireTimeoutNs, kFenceTimeoutNs, kMaxSwapchainImages,
       kMaxSwapchainPixels);
#else
  Tell("MODE hardwareRequired=1 softwareControl=0 api=1.0 frames=%u "
       "acquireTimeoutNs=%" PRIu64 " fenceTimeoutNs=%" PRIu64
       " maxImages=%u maxPixels=%" PRIu64,
       kFrameCount, kAcquireTimeoutNs, kFenceTimeoutNs, kMaxSwapchainImages,
       kMaxSwapchainPixels);
#endif
  Tell("WINDOW nativeWidth=%d nativeHeight=%d", ANativeWindow_getWidth(window),
       ANativeWindow_getHeight(window));

  std::vector<VkExtensionProperties> instance_extensions;
  if (!EnumerateInstanceExtensions(&instance_extensions))
    return Fail(&r, "vkEnumerateInstanceExtensionProperties",
                VK_ERROR_INITIALIZATION_FAILED, false, false, unsafe_teardown);
#ifdef BENGAL_WSI_PRIVATE_GANESH
  if (!HasEveryExtension(instance_extensions, kGaneshInstanceExtensions)) {
    return Fail(&r, "required_Ganesh_WSI_instance_extensions",
                VK_ERROR_EXTENSION_NOT_PRESENT, false, false, unsafe_teardown);
  }
  constexpr bool properties2 = true;
  const char *const *enabled_extensions = kGaneshInstanceExtensions;
  const uint32_t enabled_extension_count =
      static_cast<uint32_t>(std::size(kGaneshInstanceExtensions));
#else
  if (!HasExtension(instance_extensions, VK_KHR_SURFACE_EXTENSION_NAME) ||
      !HasExtension(instance_extensions,
                    VK_KHR_ANDROID_SURFACE_EXTENSION_NAME)) {
    return Fail(&r, "required_WSI_instance_extensions",
                VK_ERROR_EXTENSION_NOT_PRESENT, false, false, unsafe_teardown);
  }
  const bool properties2 =
      HasExtension(instance_extensions,
                   VK_KHR_GET_PHYSICAL_DEVICE_PROPERTIES_2_EXTENSION_NAME);
  const char *enabled_extensions[3] = {
      VK_KHR_SURFACE_EXTENSION_NAME,
      VK_KHR_ANDROID_SURFACE_EXTENSION_NAME,
      VK_KHR_GET_PHYSICAL_DEVICE_PROPERTIES_2_EXTENSION_NAME,
  };
#endif
  VkApplicationInfo application{};
  application.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO;
  application.pApplicationName = "bengal-vulkan-wsi-smoke";
  application.applicationVersion = 1;
  application.pEngineName = "none";
#ifdef BENGAL_WSI_MESA26_PROFILE
  application.apiVersion = VK_API_VERSION_1_3;
#else
  application.apiVersion = VK_API_VERSION_1_0;
#endif
  VkInstanceCreateInfo instance_info{};
  instance_info.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO;
  instance_info.pApplicationInfo = &application;
#ifdef BENGAL_WSI_PRIVATE_GANESH
  instance_info.enabledExtensionCount = enabled_extension_count;
#else
  instance_info.enabledExtensionCount = properties2 ? 3u : 2u;
#endif
  instance_info.ppEnabledExtensionNames = enabled_extensions;
  VkResult result = vkCreateInstance(&instance_info, nullptr, &r.instance);
  if (result != VK_SUCCESS)
    return Fail(&r, "vkCreateInstance", result, false, false, unsafe_teardown);
#ifdef BENGAL_WSI_MESA26_PROFILE
  Tell("INSTANCE_CREATE verdict=PASS requestedApiRaw=%u properties2=%u "
       "layers=0 extensions=%u",
       application.apiVersion, properties2, instance_info.enabledExtensionCount);
#else
  Tell("INSTANCE_CREATE verdict=PASS requestedApiRaw=%u properties2=%u "
       "layers=0 extensions=%u",
       VK_API_VERSION_1_0, properties2, instance_info.enabledExtensionCount);
#endif

  VkAndroidSurfaceCreateInfoKHR surface_info{};
  surface_info.sType = VK_STRUCTURE_TYPE_ANDROID_SURFACE_CREATE_INFO_KHR;
  surface_info.window = window;
  result =
      vkCreateAndroidSurfaceKHR(r.instance, &surface_info, nullptr, &r.surface);
  if (result != VK_SUCCESS)
    return Fail(&r, "vkCreateAndroidSurfaceKHR", result, false, false,
                unsafe_teardown);
  Tell("ANDROID_SURFACE_CREATE verdict=PASS nativeWindow=1");

  uint32_t physical_count = 0;
  result = vkEnumeratePhysicalDevices(r.instance, &physical_count, nullptr);
  if (result != VK_SUCCESS || physical_count == 0 ||
      physical_count > kMaxPhysicalDevices) {
    return Fail(&r, "vkEnumeratePhysicalDevices", result, false, false,
                unsafe_teardown);
  }
  std::vector<VkPhysicalDevice> physical_devices(physical_count);
  result = vkEnumeratePhysicalDevices(r.instance, &physical_count,
                                      physical_devices.data());
  if (result != VK_SUCCESS || physical_count > physical_devices.size())
    return Fail(&r, "vkEnumeratePhysicalDevices_second", result, false, false,
                unsafe_teardown);
  physical_devices.resize(physical_count);
  Tell("PHYSICAL_DEVICES verdict=PASS count=%u cap=%u", physical_count,
       kMaxPhysicalDevices);

  VkPhysicalDevice selected = VK_NULL_HANDLE;
  uint32_t queue_family = UINT32_MAX;
  for (uint32_t i = 0; i < physical_count; ++i) {
    std::vector<VkExtensionProperties> device_extensions;
    if (!EnumerateDeviceExtensions(physical_devices[i], i,
                                   &device_extensions)) {
      continue;
    }
    const Identity identity = ReadIdentity(r.instance, physical_devices[i],
                                           device_extensions, properties2);
    PrintIdentity(i, identity);
    const QueueTopology topology =
        ReadQueues(physical_devices[i], r.surface, i);
    IdentityInput input;
    input.device_type = identity.properties.deviceType;
    input.vendor_id = identity.properties.vendorID;
    input.device_id = identity.properties.deviceID;
    input.api_major = VK_VERSION_MAJOR(identity.properties.apiVersion);
    input.api_minor = VK_VERSION_MINOR(identity.properties.apiVersion);
    input.device_name = identity.properties.deviceName;
    input.driver_properties_available = identity.driver_properties_available;
    input.driver_id = static_cast<uint32_t>(identity.driver_id);
    input.driver_name = identity.driver_name;
    input.queue_family_count = topology.family_count;
    input.total_queue_count = topology.total_queue_count;
    input.has_one_graphics_present_queue = topology.exact_one;
    const char *rejection = bengal_wsi::RejectIdentity(input);
#ifdef BENGAL_WSI_PRIVATE_GANESH
    if (!HasEveryExtension(device_extensions, kGaneshDeviceExtensions)) {
      rejection = "required_private_Ganesh_device_extensions_unavailable";
    }
#else
    if (!HasExtension(device_extensions, VK_KHR_SWAPCHAIN_EXTENSION_NAME)) {
      rejection = "VK_KHR_swapchain_unavailable";
    }
#endif
    Tell("IDENTITY device=%u verdict=%s reason=%s", i,
         rejection == nullptr ? "ACCEPTED_A610" : "REJECTED",
         rejection == nullptr ? "exact_a610_turnip_one_queue" : rejection);
    if (selected == VK_NULL_HANDLE && rejection == nullptr) {
      selected = physical_devices[i];
      queue_family = topology.graphics_present_family;
    }
  }
  if (selected == VK_NULL_HANDLE) {
    Cleanup(&r);
    Tell("RESULT HARDWARE_A610_FAIL "
         "reason=no_exact_a610_turnip_one_graphics_present_queue");
    return RunResult::kIdentityRejected;
  }

  const float priority = 1.0f;
  VkDeviceQueueCreateInfo queue_info{};
  queue_info.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO;
  queue_info.queueFamilyIndex = queue_family;
  queue_info.queueCount = 1;
  queue_info.pQueuePriorities = &priority;
#ifdef BENGAL_WSI_PRIVATE_GANESH
  auto get_features2 = reinterpret_cast<PFN_vkGetPhysicalDeviceFeatures2KHR>(
      vkGetInstanceProcAddr(r.instance, "vkGetPhysicalDeviceFeatures2KHR"));
  if (get_features2 == nullptr)
    return Fail(&r, "vkGetPhysicalDeviceFeatures2KHR_entrypoint",
                VK_ERROR_EXTENSION_NOT_PRESENT, false, false, unsafe_teardown);
  VkPhysicalDeviceSamplerYcbcrConversionFeatures supported_ycbcr{};
  supported_ycbcr.sType =
      VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_SAMPLER_YCBCR_CONVERSION_FEATURES;
  VkPhysicalDeviceFeatures2 supported_features{};
  supported_features.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2;
  supported_features.pNext = &supported_ycbcr;
  get_features2(selected, &supported_features);
  if (supported_ycbcr.samplerYcbcrConversion != VK_TRUE)
    return Fail(&r, "samplerYcbcrConversion_support",
                VK_ERROR_FEATURE_NOT_PRESENT, false, false, unsafe_teardown);
  VkPhysicalDeviceSamplerYcbcrConversionFeatures enabled_ycbcr{};
  enabled_ycbcr.sType =
      VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_SAMPLER_YCBCR_CONVERSION_FEATURES;
  enabled_ycbcr.samplerYcbcrConversion = VK_TRUE;
  VkPhysicalDeviceFeatures2 enabled_features{};
  enabled_features.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2;
  enabled_features.pNext = &enabled_ycbcr;
#else
  const char *swapchain_extension = VK_KHR_SWAPCHAIN_EXTENSION_NAME;
#endif
  VkDeviceCreateInfo device_info{};
  device_info.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO;
#ifdef BENGAL_WSI_PRIVATE_GANESH
  device_info.pNext = &enabled_features;
#endif
  device_info.queueCreateInfoCount = 1;
  device_info.pQueueCreateInfos = &queue_info;
#ifdef BENGAL_WSI_PRIVATE_GANESH
  device_info.enabledExtensionCount =
      static_cast<uint32_t>(std::size(kGaneshDeviceExtensions));
  device_info.ppEnabledExtensionNames = kGaneshDeviceExtensions;
#else
  device_info.enabledExtensionCount = 1;
  device_info.ppEnabledExtensionNames = &swapchain_extension;
#endif
  device_info.pEnabledFeatures = nullptr;
  result = vkCreateDevice(selected, &device_info, nullptr, &r.device);
  if (result != VK_SUCCESS)
    return Fail(&r, "vkCreateDevice", result, false, false, unsafe_teardown);
#ifdef BENGAL_WSI_PRIVATE_GANESH
  Tell("DEVICE_CREATE verdict=PASS requestedQueueCount=1 "
       "enabledCoreFeatureCount=0 samplerYcbcrConversion=1 "
       "enabledExtensions=8 closedPrivateGaneshProfile=1");
#else
  Tell("DEVICE_CREATE verdict=PASS requestedQueueCount=1 enabledFeatureCount=0 "
       "enabledExtensions=VK_KHR_swapchain");
#endif
  VkQueue queue = VK_NULL_HANDLE;
  vkGetDeviceQueue(r.device, queue_family, 0, &queue);
  if (queue == VK_NULL_HANDLE)
    return Fail(&r, "vkGetDeviceQueue", VK_ERROR_INITIALIZATION_FAILED, false,
                false, unsafe_teardown);

  VkSurfaceCapabilitiesKHR capabilities{};
  result = vkGetPhysicalDeviceSurfaceCapabilitiesKHR(selected, r.surface,
                                                     &capabilities);
  if (result != VK_SUCCESS)
    return Fail(&r, "vkGetPhysicalDeviceSurfaceCapabilitiesKHR", result, false,
                false, unsafe_teardown);
  const GeometryPlan geometry = bengal_wsi::PlanAndroidGeometry(
      {capabilities.currentExtent.width, capabilities.currentExtent.height},
      {capabilities.minImageExtent.width, capabilities.minImageExtent.height},
      {capabilities.maxImageExtent.width, capabilities.maxImageExtent.height},
      capabilities.currentTransform, kMaxSwapchainPixels);
  Tell("SURFACE_CAPS currentExtent=%ux%u minExtent=%ux%u maxExtent=%ux%u "
       "currentTransformRaw=0x%08x currentTransform=%s minImages=%u "
       "maxImages=%u usageRaw=0x%08x",
       capabilities.currentExtent.width, capabilities.currentExtent.height,
       capabilities.minImageExtent.width, capabilities.minImageExtent.height,
       capabilities.maxImageExtent.width, capabilities.maxImageExtent.height,
       capabilities.currentTransform,
       bengal_wsi::TransformName(capabilities.currentTransform),
       capabilities.minImageCount, capabilities.maxImageCount,
       capabilities.supportedUsageFlags);
  if (!geometry.valid ||
      !(capabilities.supportedUsageFlags &
        VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT) ||
      !(capabilities.supportedCompositeAlpha &
        VK_COMPOSITE_ALPHA_INHERIT_BIT_KHR)) {
    return Fail(&r, "surface_geometry_or_usage", VK_ERROR_INITIALIZATION_FAILED,
                false, false, unsafe_teardown);
  }

  std::vector<VkSurfaceFormatKHR> formats;
  std::vector<VkPresentModeKHR> modes;
  if (!EnumerateSurfaceFormats(selected, r.surface, &formats) ||
      !EnumeratePresentModes(selected, r.surface, &modes)) {
    return Fail(&r, "surface_format_or_present_mode_enumeration",
                VK_ERROR_INITIALIZATION_FAILED, false, false, unsafe_teardown);
  }
  const VkSurfaceFormatKHR surface_format = ChooseSurfaceFormat(formats);
  if (surface_format.format == VK_FORMAT_UNDEFINED ||
      std::find(modes.begin(), modes.end(), VK_PRESENT_MODE_FIFO_KHR) ==
          modes.end()) {
    return Fail(&r, "required_RGBA8_FIFO_surface_support",
                VK_ERROR_FORMAT_NOT_SUPPORTED, false, false, unsafe_teardown);
  }
  uint32_t image_count = capabilities.minImageCount;
  if (image_count == 0)
    image_count = 2;
  if (capabilities.maxImageCount != 0 &&
      image_count > capabilities.maxImageCount) {
    return Fail(&r, "swapchain_image_count_surface_limit",
                VK_ERROR_INITIALIZATION_FAILED, false, false, unsafe_teardown);
  }
  if (image_count > kMaxSwapchainImages) {
    return Fail(&r, "swapchain_image_count_local_cap",
                VK_ERROR_TOO_MANY_OBJECTS, false, false, unsafe_teardown);
  }
  VkSwapchainCreateInfoKHR swapchain_info{};
  swapchain_info.sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR;
  swapchain_info.surface = r.surface;
  swapchain_info.minImageCount = image_count;
  swapchain_info.imageFormat = surface_format.format;
  swapchain_info.imageColorSpace = surface_format.colorSpace;
  swapchain_info.imageExtent = {geometry.extent.width, geometry.extent.height};
  swapchain_info.imageArrayLayers = 1;
  swapchain_info.imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT;
  swapchain_info.imageSharingMode = VK_SHARING_MODE_EXCLUSIVE;
  swapchain_info.preTransform =
      static_cast<VkSurfaceTransformFlagBitsKHR>(geometry.pre_transform);
  swapchain_info.compositeAlpha = VK_COMPOSITE_ALPHA_INHERIT_BIT_KHR;
  swapchain_info.presentMode = VK_PRESENT_MODE_FIFO_KHR;
  swapchain_info.clipped = VK_TRUE;
  result =
      vkCreateSwapchainKHR(r.device, &swapchain_info, nullptr, &r.swapchain);
  if (result != VK_SUCCESS)
    return Fail(&r, "vkCreateSwapchainKHR", result, false, false,
                unsafe_teardown);
  Tell("SWAPCHAIN_CREATE verdict=PASS requestedImages=%u extent=%ux%u "
       "formatRaw=%u colorSpaceRaw=%u preTransformRaw=0x%08x preTransform=%s "
       "presentMode=FIFO",
       image_count, geometry.extent.width, geometry.extent.height,
       surface_format.format, surface_format.colorSpace, geometry.pre_transform,
       bengal_wsi::TransformName(geometry.pre_transform));

  uint32_t returned_images = 0;
  result =
      vkGetSwapchainImagesKHR(r.device, r.swapchain, &returned_images, nullptr);
  if (result != VK_SUCCESS || returned_images == 0 ||
      returned_images > kMaxSwapchainImages) {
    return Fail(&r, "vkGetSwapchainImagesKHR", result, false, false,
                unsafe_teardown);
  }
  r.images.resize(returned_images);
  result = vkGetSwapchainImagesKHR(r.device, r.swapchain, &returned_images,
                                   r.images.data());
  if (result != VK_SUCCESS || returned_images > r.images.size())
    return Fail(&r, "vkGetSwapchainImagesKHR_second", result, false, false,
                unsafe_teardown);
  r.images.resize(returned_images);
  Tell("SWAPCHAIN_IMAGES verdict=PASS count=%u cap=%u", returned_images,
       kMaxSwapchainImages);

#ifndef BENGAL_WSI_PRIVATE_GANESH
  r.views.reserve(r.images.size());
  for (VkImage image : r.images) {
    VkImageViewCreateInfo view_info{};
    view_info.sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO;
    view_info.image = image;
    view_info.viewType = VK_IMAGE_VIEW_TYPE_2D;
    view_info.format = surface_format.format;
    view_info.subresourceRange.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT;
    view_info.subresourceRange.levelCount = 1;
    view_info.subresourceRange.layerCount = 1;
    VkImageView view = VK_NULL_HANDLE;
    result = vkCreateImageView(r.device, &view_info, nullptr, &view);
    if (result != VK_SUCCESS)
      return Fail(&r, "vkCreateImageView", result, false, false,
                  unsafe_teardown);
    r.views.push_back(view);
  }

  VkAttachmentDescription attachment{};
  attachment.format = surface_format.format;
  attachment.samples = VK_SAMPLE_COUNT_1_BIT;
  attachment.loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR;
  attachment.storeOp = VK_ATTACHMENT_STORE_OP_STORE;
  attachment.stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE;
  attachment.stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE;
  attachment.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED;
  attachment.finalLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
  VkAttachmentReference color_reference{
      0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL};
  VkSubpassDescription subpass{};
  subpass.pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS;
  subpass.colorAttachmentCount = 1;
  subpass.pColorAttachments = &color_reference;
  VkSubpassDependency dependency{};
  dependency.srcSubpass = VK_SUBPASS_EXTERNAL;
  dependency.dstSubpass = 0;
  dependency.srcStageMask = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
  dependency.dstStageMask = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
  dependency.dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT;
  VkRenderPassCreateInfo render_pass_info{};
  render_pass_info.sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO;
  render_pass_info.attachmentCount = 1;
  render_pass_info.pAttachments = &attachment;
  render_pass_info.subpassCount = 1;
  render_pass_info.pSubpasses = &subpass;
  render_pass_info.dependencyCount = 1;
  render_pass_info.pDependencies = &dependency;
  result =
      vkCreateRenderPass(r.device, &render_pass_info, nullptr, &r.render_pass);
  if (result != VK_SUCCESS)
    return Fail(&r, "vkCreateRenderPass", result, false, false,
                unsafe_teardown);

  result = CreateShader(r.device, bengal_wsi::kGridVertSpv,
                        sizeof(bengal_wsi::kGridVertSpv), &r.vert);
  if (result == VK_SUCCESS)
    result = CreateShader(r.device, bengal_wsi::kGridFragSpv,
                          sizeof(bengal_wsi::kGridFragSpv), &r.frag);
  if (result != VK_SUCCESS)
    return Fail(&r, "vkCreateShaderModule", result, false, false,
                unsafe_teardown);

  VkPushConstantRange push_range{};
  push_range.stageFlags = VK_SHADER_STAGE_FRAGMENT_BIT;
  push_range.size = 3 * sizeof(uint32_t);
  VkPipelineLayoutCreateInfo layout_info{};
  layout_info.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO;
  layout_info.pushConstantRangeCount = 1;
  layout_info.pPushConstantRanges = &push_range;
  result = vkCreatePipelineLayout(r.device, &layout_info, nullptr,
                                  &r.pipeline_layout);
  if (result != VK_SUCCESS)
    return Fail(&r, "vkCreatePipelineLayout", result, false, false,
                unsafe_teardown);

  VkPipelineShaderStageCreateInfo stages[2]{};
  stages[0].sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO;
  stages[0].stage = VK_SHADER_STAGE_VERTEX_BIT;
  stages[0].module = r.vert;
  stages[0].pName = "main";
  stages[1].sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO;
  stages[1].stage = VK_SHADER_STAGE_FRAGMENT_BIT;
  stages[1].module = r.frag;
  stages[1].pName = "main";
  VkPipelineVertexInputStateCreateInfo vertex_input{};
  vertex_input.sType =
      VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO;
  VkPipelineInputAssemblyStateCreateInfo assembly{};
  assembly.sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO;
  assembly.topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST;
  VkViewport viewport{0.0f,
                      0.0f,
                      static_cast<float>(geometry.extent.width),
                      static_cast<float>(geometry.extent.height),
                      0.0f,
                      1.0f};
  VkRect2D scissor{{0, 0}, {geometry.extent.width, geometry.extent.height}};
  VkPipelineViewportStateCreateInfo viewport_state{};
  viewport_state.sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO;
  viewport_state.viewportCount = 1;
  viewport_state.pViewports = &viewport;
  viewport_state.scissorCount = 1;
  viewport_state.pScissors = &scissor;
  VkPipelineRasterizationStateCreateInfo rasterization{};
  rasterization.sType =
      VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO;
  rasterization.polygonMode = VK_POLYGON_MODE_FILL;
  rasterization.cullMode = VK_CULL_MODE_NONE;
  rasterization.frontFace = VK_FRONT_FACE_COUNTER_CLOCKWISE;
  rasterization.lineWidth = 1.0f;
  VkPipelineMultisampleStateCreateInfo multisample{};
  multisample.sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO;
  multisample.rasterizationSamples = VK_SAMPLE_COUNT_1_BIT;
  VkPipelineColorBlendAttachmentState blend_attachment{};
  blend_attachment.colorWriteMask =
      VK_COLOR_COMPONENT_R_BIT | VK_COLOR_COMPONENT_G_BIT |
      VK_COLOR_COMPONENT_B_BIT | VK_COLOR_COMPONENT_A_BIT;
  VkPipelineColorBlendStateCreateInfo blend{};
  blend.sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO;
  blend.attachmentCount = 1;
  blend.pAttachments = &blend_attachment;
  VkGraphicsPipelineCreateInfo pipeline_info{};
  pipeline_info.sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO;
  pipeline_info.stageCount = 2;
  pipeline_info.pStages = stages;
  pipeline_info.pVertexInputState = &vertex_input;
  pipeline_info.pInputAssemblyState = &assembly;
  pipeline_info.pViewportState = &viewport_state;
  pipeline_info.pRasterizationState = &rasterization;
  pipeline_info.pMultisampleState = &multisample;
  pipeline_info.pColorBlendState = &blend;
  pipeline_info.layout = r.pipeline_layout;
  pipeline_info.renderPass = r.render_pass;
  result = vkCreateGraphicsPipelines(r.device, VK_NULL_HANDLE, 1,
                                     &pipeline_info, nullptr, &r.pipeline);
  if (result != VK_SUCCESS)
    return Fail(&r, "vkCreateGraphicsPipelines", result, false, false,
                unsafe_teardown);
  Tell("PIPELINE verdict=PASS vertexShader=full_screen_triangle "
       "fragmentShader=transformed_grid_corners pushConstantBytes=12");

  r.framebuffers.reserve(r.views.size());
  for (VkImageView view : r.views) {
    VkFramebufferCreateInfo framebuffer_info{};
    framebuffer_info.sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO;
    framebuffer_info.renderPass = r.render_pass;
    framebuffer_info.attachmentCount = 1;
    framebuffer_info.pAttachments = &view;
    framebuffer_info.width = geometry.extent.width;
    framebuffer_info.height = geometry.extent.height;
    framebuffer_info.layers = 1;
    VkFramebuffer framebuffer = VK_NULL_HANDLE;
    result =
        vkCreateFramebuffer(r.device, &framebuffer_info, nullptr, &framebuffer);
    if (result != VK_SUCCESS)
      return Fail(&r, "vkCreateFramebuffer", result, false, false,
                  unsafe_teardown);
    r.framebuffers.push_back(framebuffer);
  }
  VkCommandPoolCreateInfo pool_info{};
  pool_info.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO;
  pool_info.queueFamilyIndex = queue_family;
  result = vkCreateCommandPool(r.device, &pool_info, nullptr, &r.command_pool);
  if (result != VK_SUCCESS)
    return Fail(&r, "vkCreateCommandPool", result, false, false,
                unsafe_teardown);
  r.command_buffers.resize(r.images.size());
  VkCommandBufferAllocateInfo command_allocate{};
  command_allocate.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO;
  command_allocate.commandPool = r.command_pool;
  command_allocate.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
  command_allocate.commandBufferCount =
      static_cast<uint32_t>(r.command_buffers.size());
  result = vkAllocateCommandBuffers(r.device, &command_allocate,
                                    r.command_buffers.data());
  if (result != VK_SUCCESS)
    return Fail(&r, "vkAllocateCommandBuffers", result, false, false,
                unsafe_teardown);
  struct Push {
    uint32_t width;
    uint32_t height;
    uint32_t transform;
  } push{geometry.extent.width, geometry.extent.height, geometry.pre_transform};
  for (size_t i = 0; i < r.command_buffers.size(); ++i) {
    VkCommandBufferBeginInfo begin{};
    begin.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO;
    begin.flags = VK_COMMAND_BUFFER_USAGE_SIMULTANEOUS_USE_BIT;
    result = vkBeginCommandBuffer(r.command_buffers[i], &begin);
    if (result != VK_SUCCESS)
      return Fail(&r, "vkBeginCommandBuffer", result, false, false,
                  unsafe_teardown);
    VkClearValue clear{};
    clear.color = {{0.0f, 0.0f, 0.0f, 1.0f}};
    VkRenderPassBeginInfo render_begin{};
    render_begin.sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO;
    render_begin.renderPass = r.render_pass;
    render_begin.framebuffer = r.framebuffers[i];
    render_begin.renderArea.extent = {geometry.extent.width,
                                      geometry.extent.height};
    render_begin.clearValueCount = 1;
    render_begin.pClearValues = &clear;
    vkCmdBeginRenderPass(r.command_buffers[i], &render_begin,
                         VK_SUBPASS_CONTENTS_INLINE);
    vkCmdBindPipeline(r.command_buffers[i], VK_PIPELINE_BIND_POINT_GRAPHICS,
                      r.pipeline);
    vkCmdPushConstants(r.command_buffers[i], r.pipeline_layout,
                       VK_SHADER_STAGE_FRAGMENT_BIT, 0, sizeof(push), &push);
    vkCmdDraw(r.command_buffers[i], 3, 1, 0, 0);
    vkCmdEndRenderPass(r.command_buffers[i]);
    result = vkEndCommandBuffer(r.command_buffers[i]);
    if (result != VK_SUCCESS)
      return Fail(&r, "vkEndCommandBuffer", result, false, false,
                  unsafe_teardown);
  }
#else
  bengal_wsi::GaneshCreateInfo ganesh_info;
  ganesh_info.instance = r.instance;
  ganesh_info.physical_device = selected;
  ganesh_info.device = r.device;
  ganesh_info.queue = queue;
  ganesh_info.queue_family = queue_family;
  ganesh_info.enabled_features = &enabled_features;
  ganesh_info.instance_extensions = kGaneshInstanceExtensions;
  ganesh_info.instance_extension_count =
      static_cast<uint32_t>(std::size(kGaneshInstanceExtensions));
  ganesh_info.device_extensions = kGaneshDeviceExtensions;
  ganesh_info.device_extension_count =
      static_cast<uint32_t>(std::size(kGaneshDeviceExtensions));
  ganesh_info.images = r.images.data();
  ganesh_info.image_count = static_cast<uint32_t>(r.images.size());
  ganesh_info.format = surface_format.format;
  ganesh_info.usage = swapchain_info.imageUsage;
  ganesh_info.width = geometry.extent.width;
  ganesh_info.height = geometry.extent.height;
  ganesh_info.pre_transform = geometry.pre_transform;
  bengal_wsi::GaneshStage ganesh_failed_stage = bengal_wsi::GaneshStage::kNone;
  r.ganesh = bengal_wsi::PrivateGaneshRenderer::Create(
      ganesh_info, &ganesh_failed_stage);
  if (r.ganesh == nullptr)
    return Fail(&r, bengal_wsi::GaneshStageName(ganesh_failed_stage),
                VK_ERROR_INITIALIZATION_FAILED, false, false, unsafe_teardown);
  Tell("GANESH_INIT verdict=PASS renderer=private_single_context "
       "wrappedSwapchainImages=%u initialLayout=VK_IMAGE_LAYOUT_UNDEFINED "
       "reacquireLayoutTrackedByRetainedSurface=1 queueCount=1",
       returned_images);
#endif

  VkSemaphoreCreateInfo semaphore_info{};
  semaphore_info.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO;
  result =
      vkCreateSemaphore(r.device, &semaphore_info, nullptr, &r.image_available);
  r.render_finished.reserve(kFrameCount);
  for (uint32_t i = 0; result == VK_SUCCESS && i < kFrameCount; ++i) {
    VkSemaphore semaphore = VK_NULL_HANDLE;
    result = vkCreateSemaphore(r.device, &semaphore_info, nullptr, &semaphore);
    if (result == VK_SUCCESS)
      r.render_finished.push_back(semaphore);
  }
  VkFenceCreateInfo fence_info{};
  fence_info.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO;
  fence_info.flags = VK_FENCE_CREATE_SIGNALED_BIT;
  if (result == VK_SUCCESS)
    result = vkCreateFence(r.device, &fence_info, nullptr, &r.fence);
  if (result != VK_SUCCESS)
    return Fail(&r, "sync_object_create", result, false, false,
                unsafe_teardown);

  VulkanFrameContext frame_context{&r, r.device, r.swapchain, queue};
  const FrameLoopCallbacks frame_callbacks{
      &frame_context, AcquireFrame, ResetFrameFence, SubmitFrame,
      WaitFrameFence, PresentFrame, SafeFrameCleanup};
  const FrameLoopOutcome frame_outcome = bengal_wsi::RunBoundedFrameLoop(
#ifdef BENGAL_WSI_PRIVATE_GANESH
      kFrameCount, static_cast<uint32_t>(r.images.size()),
#else
      kFrameCount, static_cast<uint32_t>(r.command_buffers.size()),
#endif
      frame_callbacks);
  if (!frame_outcome.completed) {
    result = static_cast<VkResult>(frame_outcome.raw_result);
    Tell("FRAME_LOOP verdict=FAIL frame=%u operation=%s vk=%d name=%s "
         "presentAttemptedEver=%u acquireSignalPending=%u "
         "workIndeterminate=%u safeCleanupCalled=%u",
         frame_outcome.failed_frame,
         bengal_wsi::FrameOperationName(frame_outcome.failed_operation), result,
         VkResultName(result), frame_outcome.present_attempted_ever,
         frame_outcome.acquire_signal_pending,
         frame_outcome.submitted_work_indeterminate,
         frame_outcome.safe_cleanup_called);
    *unsafe_teardown = bengal_wsi::FrameOutcomeUnsafe(frame_outcome);
    if (*unsafe_teardown) {
      const char *reason = result == VK_ERROR_DEVICE_LOST
                               ? "device_lost"
                               : (frame_outcome.present_attempted_ever
                                      ? "presentation_release_unproven"
                                      : (frame_outcome.acquire_signal_pending
                                             ? "acquire_signal_unconsumed"
                                             : "submitted_work_indeterminate"));
      Tell("CLEANUP verdict=SKIPPED_UNSAFE reason=%s presentAttemptedEver=%u "
           "acquireSignalPending=%u workIndeterminate=%u noQueueWaitIdle=1 "
           "allVulkanDestroyCalls=0",
           reason, frame_outcome.present_attempted_ever,
           frame_outcome.acquire_signal_pending,
           frame_outcome.submitted_work_indeterminate);
    }
    return FrameFailureResult(frame_outcome.failed_operation, result);
  }
  if (frame_outcome.stopped_suboptimal) {
    Tell("LIFECYCLE verdict=STOP reason=surface_transform_changed "
         "relaunchRequired=1 noSwapchainRecreate=1");
  }

  *unsafe_teardown = true;
#ifdef BENGAL_WSI_MESA26_PROFILE
  Tell("OBSERVATION verdict=UNPROVEN manualRequired=1 renderer=raw_mesa26 "
       "expected=TL_RED_TR_GREEN_BL_BLUE_BR_YELLOW_gray_grid_magenta_vertical_"
       "cyan_horizontal expectedUprightAfterPreTransform=1 "
       "pixelPerfectOrUnscaled=UNPROVEN");
  Tell("RESULT HARDWARE_A610_MESA26_WSI_SUBMITTED_PRESENT_API_ACCEPTED "
       "frames=%u visualPixelsObserved=0 hardwareVisualPass=UNPROVEN",
       frame_outcome.presented_count);
#elif defined(BENGAL_WSI_PRIVATE_GANESH)
  Tell("OBSERVATION verdict=UNPROVEN manualRequired=1 renderer=private_ganesh "
       "expected=TL_RED_TR_GREEN_BL_BLUE_BR_YELLOW_gray_grid_red_path_green_"
       "clip_yellow_raster_white_procedural_glyph_magenta_vertical_cyan_"
       "horizontal expectedUprightAfterPreTransform=1 "
       "pixelPerfectOrUnscaled=UNPROVEN");
  Tell("RESULT HARDWARE_A610_PRIVATE_GANESH_WSI_SUBMITTED_PRESENT_API_ACCEPTED "
       "frames=%u visualPixelsObserved=0 hardwareVisualPass=UNPROVEN "
       "frameworkHWUI=0",
       frame_outcome.presented_count);
#else
  Tell("OBSERVATION verdict=UNPROVEN manualRequired=1 "
       "expected=TL_RED_TR_GREEN_BL_BLUE_BR_YELLOW_gray_grid_magenta_vertical_"
       "cyan_horizontal expectedUprightAfterPreTransform=1 "
       "pixelPerfectOrUnscaled=UNPROVEN");
  Tell("RESULT HARDWARE_A610_WSI_SUBMITTED_PRESENT_API_ACCEPTED frames=%u "
       "visualPixelsObserved=0 hardwareVisualPass=UNPROVEN",
       frame_outcome.presented_count);
#endif
  Tell("CLEANUP verdict=DEFERRED_TO_DEDICATED_PROCESS_EXIT "
       "reason=presentation_release_unproven presentAttempted=1 "
       "noQueueWaitIdle=1 allVulkanDestroyCalls=0");
  return RunResult::kPresentedUnobserved;
}

const char *AppCmdName(int32_t command) {
  switch (command) {
  case APP_CMD_INIT_WINDOW:
    return "INIT_WINDOW";
  case APP_CMD_TERM_WINDOW:
    return "TERM_WINDOW";
  case APP_CMD_WINDOW_RESIZED:
    return "WINDOW_RESIZED";
  case APP_CMD_CONTENT_RECT_CHANGED:
    return "CONTENT_RECT_CHANGED";
  case APP_CMD_CONFIG_CHANGED:
    return "CONFIG_CHANGED";
  case APP_CMD_DESTROY:
    return "DESTROY";
  default:
    return "OTHER";
  }
}

void HandleAppCommand(android_app *app, int32_t command) {
  auto *state = static_cast<AppState *>(app->userData);
  Tell("LIFECYCLE event=%s raw=%d window=%u runStarted=%u runFinished=%u",
       AppCmdName(command), command, app->window != nullptr, state->run_started,
       state->run_finished);
  switch (command) {
  case APP_CMD_INIT_WINDOW:
    state->window_ready = app->window != nullptr;
    if (bengal_wsi::DecideLifecycle(state->run_started,
                                    LifecycleEvent::kInitWindow) ==
        LifecycleAction::kIgnoreNewWindow) {
      state->lifecycle_changed_after_start = true;
      Tell("LIFECYCLE verdict=IGNORED_AFTER_RUN "
           "reason=no_bounded_present_release_proof relaunchRequired=1");
    }
    break;
  case APP_CMD_TERM_WINDOW:
    state->window_ready = false;
    if (bengal_wsi::DecideLifecycle(state->run_started,
                                    LifecycleEvent::kTermWindow) ==
        LifecycleAction::kStopNoDestroy) {
      state->lifecycle_changed_after_start = true;
      Tell("LIFECYCLE verdict=STOP_NO_DESTROY reason=window_terminated "
           "noUnsafeVulkanDestroy=1 relaunchRequired=1");
    }
    break;
  case APP_CMD_WINDOW_RESIZED:
  case APP_CMD_CONTENT_RECT_CHANGED:
  case APP_CMD_CONFIG_CHANGED:
    if (bengal_wsi::DecideLifecycle(state->run_started,
                                    LifecycleEvent::kGeometryChanged) ==
        LifecycleAction::kStopNoRecreate) {
      state->lifecycle_changed_after_start = true;
      Tell("LIFECYCLE verdict=STOP_NO_RECREATE "
           "reason=window_geometry_or_transform_changed "
           "noUnsafeVulkanDestroy=1 relaunchRequired=1");
    }
    break;
  case APP_CMD_DESTROY:
    state->window_ready = false;
    break;
  default:
    break;
  }
}

} // namespace

void android_main(android_app *app) {
  std::setvbuf(stdout, nullptr, _IOLBF, 0);
  Tell("OUTPUT stdoutBuffering=line logcatTag=%s", kLogTag);
  Tell("ENTRY verdict=PASS kind=NativeActivity android_main=1 "
       "dedicatedProcessRequired=1");
  AppState state;
  state.app = app;
  app->userData = &state;
  app->onAppCmd = HandleAppCommand;

  while (app->destroyRequested == 0) {
    android_poll_source *source = nullptr;
    const int timeout_ms = state.window_ready && !state.run_started ? 0 : -1;
    int ident = ALooper_pollOnce(timeout_ms, nullptr, nullptr,
                                 reinterpret_cast<void **>(&source));
    while (ident >= 0) {
      if (source != nullptr)
        source->process(app, source);
      if (app->destroyRequested != 0)
        break;
      source = nullptr;
      ident = ALooper_pollOnce(0, nullptr, nullptr,
                               reinterpret_cast<void **>(&source));
    }
    if (app->destroyRequested != 0)
      break;
    if (state.window_ready && !state.run_started && app->window != nullptr) {
      state.run_started = true;
      state.result = RunSmoke(app->window, &state.unsafe_teardown);
      state.run_finished = true;
      Tell("RUN verdict=FINISHED resultRaw=%u unsafeTeardown=%u "
           "waitingForManualClose=1",
           static_cast<uint32_t>(state.result), state.unsafe_teardown);
    }
  }

  Tell("EXIT destroyRequested=%d runFinished=%u resultRaw=%u unsafeTeardown=%u",
       app->destroyRequested, state.run_finished,
       static_cast<uint32_t>(state.result), state.unsafe_teardown);
  if (state.unsafe_teardown) {
    Tell("PROCESS_EXIT verdict=IMMEDIATE_DEDICATED_PROCESS "
         "noCppOrVulkanTeardown=1");
    std::_Exit(state.result == RunResult::kPresentedUnobserved ? 0 : 1);
  }
}
