#pragma once

#include <cstdint>
#include <string>

namespace bengal_wsi {

constexpr uint32_t kTransformIdentity = 0x00000001;
constexpr uint32_t kTransformRotate90 = 0x00000002;
constexpr uint32_t kTransformRotate180 = 0x00000004;
constexpr uint32_t kTransformRotate270 = 0x00000008;
constexpr uint32_t kTransformHorizontalMirror = 0x00000010;
constexpr uint32_t kTransformHorizontalMirrorRotate90 = 0x00000020;
constexpr uint32_t kTransformHorizontalMirrorRotate180 = 0x00000040;
constexpr uint32_t kTransformHorizontalMirrorRotate270 = 0x00000080;

struct IdentityInput {
  uint32_t device_type = 0;
  uint32_t vendor_id = 0;
  uint32_t device_id = 0;
  uint32_t api_major = 0;
  uint32_t api_minor = 0;
  std::string device_name;
  bool driver_properties_available = false;
  uint32_t driver_id = 0;
  std::string driver_name;
  uint32_t queue_family_count = 0;
  uint32_t total_queue_count = 0;
  bool has_one_graphics_present_queue = false;
};

const char *RejectIdentity(const IdentityInput &identity);

#ifdef BENGAL_WSI_MESA26_PROFILE
struct QueueFamilyInput {
  uint32_t queue_count = 0;
  uint32_t flags = 0;
  bool graphics = false;
  bool present = false;
};

// Mesa26's measured A610 topology is one graphics+present queue and one
// present-capable sparse-only queue. Only the former may be selected.
bool ValidateMesa26QueueTopology(const QueueFamilyInput *families,
                                 uint32_t family_count,
                                 uint32_t *selected_family);
#endif

struct Extent {
  uint32_t width = 0;
  uint32_t height = 0;
};

struct GeometryPlan {
  bool valid = false;
  Extent extent{};
  uint32_t pre_transform = 0;
};

GeometryPlan PlanAndroidGeometry(Extent current_extent, Extent min_extent,
                                 Extent max_extent, uint32_t current_transform,
                                 uint64_t max_pixels);
bool IsSupportedTransform(uint32_t transform);
const char *TransformName(uint32_t transform);

struct Point {
  float x = 0.0f;
  float y = 0.0f;
};

// Converts a normalized swapchain framebuffer coordinate into the logical
// upright coordinate used to choose the diagnostic color.
Point FramebufferToLogical(Point framebuffer, uint32_t pre_transform);
#ifdef BENGAL_WSI_PRIVATE_GANESH
Point LogicalToFramebuffer(Point logical, uint32_t pre_transform);
#endif

enum class TeardownDecision {
  kSafe,
  kSkipAllVulkan,
};

TeardownDecision DecideTeardown(bool present_attempted,
                                bool submitted_work_indeterminate);

enum class LifecycleEvent {
  kInitWindow,
  kTermWindow,
  kGeometryChanged,
  kDestroy,
};

enum class LifecycleAction {
  kAllowFirstWindow,
  kContinueWaiting,
  kIgnoreNewWindow,
  kStopNoDestroy,
  kStopNoRecreate,
  kDestroyRequested,
};

LifecycleAction DecideLifecycle(bool run_started, LifecycleEvent event);

} // namespace bengal_wsi
