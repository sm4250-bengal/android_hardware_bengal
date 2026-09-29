#include "logic.h"

#include <algorithm>
#include <cctype>
#include <limits>

namespace bengal_wsi {
namespace {

constexpr uint32_t kMaxExtentDimension = 4096;
constexpr uint32_t kPhysicalDeviceTypeIntegratedGpu = 1;
constexpr uint32_t kPhysicalDeviceTypeCpu = 4;
constexpr uint32_t kQualcommVendorId = 0x5143;
constexpr uint32_t kA610ChipId = 0x06010000;
constexpr uint32_t kMesaTurnipDriverId = 18;

std::string Lower(std::string value) {
  std::transform(
      value.begin(), value.end(), value.begin(),
      [](unsigned char c) { return static_cast<char>(std::tolower(c)); });
  return value;
}

bool Contains(const std::string &value, const char *needle) {
  return Lower(value).find(needle) != std::string::npos;
}

} // namespace

const char *RejectIdentity(const IdentityInput &i) {
  if (i.device_type == kPhysicalDeviceTypeCpu ||
      Contains(i.device_name, "swiftshader")) {
    return "software_device";
  }
  if (i.device_type != kPhysicalDeviceTypeIntegratedGpu) {
    return "not_integrated_gpu";
  }
  if (i.vendor_id != kQualcommVendorId) {
    return "vendor_not_qualcomm_0x5143";
  }
  if (i.device_id != kA610ChipId || !Contains(i.device_name, "adreno") ||
      !Contains(i.device_name, "610")) {
    return "device_not_adreno_a610_0x06010000";
  }
#ifdef BENGAL_WSI_MESA26_PROFILE
  if (i.api_major < 1 || (i.api_major == 1 && i.api_minor < 3)) {
    return "physical_api_below_1_3";
  }
#else
  if (i.api_major != 1 || i.api_minor != 0) {
    return "physical_api_not_truthful_a610_vulkan_1_0";
  }
#endif
  if (!i.driver_properties_available) {
    return "driver_properties_unavailable";
  }
  if (i.driver_id != kMesaTurnipDriverId ||
      !Contains(i.driver_name, "turnip")) {
    return "driver_not_mesa_turnip";
  }
#ifdef BENGAL_WSI_MESA26_PROFILE
  if (!i.has_one_graphics_present_queue) {
    return "queue_topology_not_measured_mesa26_graphics_plus_sparse";
  }
#else
  if (i.queue_family_count != 1 || i.total_queue_count != 1 ||
      !i.has_one_graphics_present_queue) {
    return "queue_topology_not_exactly_one_graphics_present_queue";
  }
#endif
  return nullptr;
}

#ifdef BENGAL_WSI_MESA26_PROFILE
bool ValidateMesa26QueueTopology(const QueueFamilyInput *families,
                                 uint32_t family_count,
                                 uint32_t *selected_family) {
  constexpr uint32_t kGraphics = 0x00000001;
  constexpr uint32_t kSparseBinding = 0x00000008;
  if (families == nullptr || selected_family == nullptr || family_count != 2)
    return false;
  uint32_t graphics_present = UINT32_MAX;
  uint32_t sparse_only = UINT32_MAX;
  for (uint32_t index = 0; index < family_count; ++index) {
    const QueueFamilyInput &family = families[index];
    if (family.queue_count != 1 || !family.present)
      return false;
    if (family.graphics && (family.flags & kGraphics) != 0) {
      if (graphics_present != UINT32_MAX)
        return false;
      graphics_present = index;
      continue;
    }
    if (!family.graphics && family.flags == kSparseBinding) {
      if (sparse_only != UINT32_MAX)
        return false;
      sparse_only = index;
      continue;
    }
    return false;
  }
  if (graphics_present == UINT32_MAX || sparse_only == UINT32_MAX)
    return false;
  *selected_family = graphics_present;
  return true;
}
#endif

bool IsSupportedTransform(uint32_t transform) {
  switch (transform) {
  case kTransformIdentity:
  case kTransformRotate90:
  case kTransformRotate180:
  case kTransformRotate270:
  case kTransformHorizontalMirror:
  case kTransformHorizontalMirrorRotate90:
  case kTransformHorizontalMirrorRotate180:
  case kTransformHorizontalMirrorRotate270:
    return true;
  default:
    return false;
  }
}

const char *TransformName(uint32_t transform) {
  switch (transform) {
  case kTransformIdentity:
    return "IDENTITY";
  case kTransformRotate90:
    return "ROTATE_90";
  case kTransformRotate180:
    return "ROTATE_180";
  case kTransformRotate270:
    return "ROTATE_270";
  case kTransformHorizontalMirror:
    return "HORIZONTAL_MIRROR";
  case kTransformHorizontalMirrorRotate90:
    return "HORIZONTAL_MIRROR_ROTATE_90";
  case kTransformHorizontalMirrorRotate180:
    return "HORIZONTAL_MIRROR_ROTATE_180";
  case kTransformHorizontalMirrorRotate270:
    return "HORIZONTAL_MIRROR_ROTATE_270";
  default:
    return "UNSUPPORTED";
  }
}

GeometryPlan PlanAndroidGeometry(Extent current, Extent minimum, Extent maximum,
                                 uint32_t transform, uint64_t max_pixels) {
  GeometryPlan plan;
  if (!IsSupportedTransform(transform) || current.width == 0 ||
      current.height == 0 ||
      current.width == std::numeric_limits<uint32_t>::max() ||
      current.height == std::numeric_limits<uint32_t>::max() ||
      current.width > kMaxExtentDimension ||
      current.height > kMaxExtentDimension || current.width < minimum.width ||
      current.height < minimum.height || current.width > maximum.width ||
      current.height > maximum.height ||
      static_cast<uint64_t>(current.width) * current.height > max_pixels) {
    return plan;
  }
  plan.valid = true;
  plan.extent = current;
  plan.pre_transform = transform;
  return plan;
}

Point FramebufferToLogical(Point p, uint32_t transform) {
  switch (transform) {
  case kTransformRotate90:
    return {p.y, 1.0f - p.x};
  case kTransformRotate180:
    return {1.0f - p.x, 1.0f - p.y};
  case kTransformRotate270:
    return {1.0f - p.y, p.x};
  case kTransformHorizontalMirror:
    return {1.0f - p.x, p.y};
  case kTransformHorizontalMirrorRotate90:
    return {1.0f - p.y, 1.0f - p.x};
  case kTransformHorizontalMirrorRotate180:
    return {p.x, 1.0f - p.y};
  case kTransformHorizontalMirrorRotate270:
    return {p.y, p.x};
  case kTransformIdentity:
  default:
    return p;
  }
}

#ifdef BENGAL_WSI_PRIVATE_GANESH
Point LogicalToFramebuffer(Point p, uint32_t transform) {
  switch (transform) {
  case kTransformRotate90:
    return {1.0f - p.y, p.x};
  case kTransformRotate180:
    return {1.0f - p.x, 1.0f - p.y};
  case kTransformRotate270:
    return {p.y, 1.0f - p.x};
  case kTransformHorizontalMirror:
    return {1.0f - p.x, p.y};
  case kTransformHorizontalMirrorRotate90:
    return {1.0f - p.y, 1.0f - p.x};
  case kTransformHorizontalMirrorRotate180:
    return {p.x, 1.0f - p.y};
  case kTransformHorizontalMirrorRotate270:
    return {p.y, p.x};
  case kTransformIdentity:
  default:
    return p;
  }
}
#endif

TeardownDecision DecideTeardown(bool present_attempted,
                                bool submitted_work_indeterminate) {
  return present_attempted || submitted_work_indeterminate
             ? TeardownDecision::kSkipAllVulkan
             : TeardownDecision::kSafe;
}

LifecycleAction DecideLifecycle(bool run_started, LifecycleEvent event) {
  switch (event) {
  case LifecycleEvent::kInitWindow:
    return run_started ? LifecycleAction::kIgnoreNewWindow
                       : LifecycleAction::kAllowFirstWindow;
  case LifecycleEvent::kTermWindow:
    return run_started ? LifecycleAction::kStopNoDestroy
                       : LifecycleAction::kContinueWaiting;
  case LifecycleEvent::kGeometryChanged:
    return run_started ? LifecycleAction::kStopNoRecreate
                       : LifecycleAction::kContinueWaiting;
  case LifecycleEvent::kDestroy:
  default:
    return LifecycleAction::kDestroyRequested;
  }
}

} // namespace bengal_wsi
