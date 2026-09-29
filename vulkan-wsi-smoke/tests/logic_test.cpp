#include "logic.h"

#include <cmath>
#include <cstdio>

namespace {

bool Near(float a, float b) { return std::fabs(a - b) < 0.0001f; }

bool Same(bengal_wsi::Point a, bengal_wsi::Point b) {
  return Near(a.x, b.x) && Near(a.y, b.y);
}

} // namespace

int main() {
  using namespace bengal_wsi;
  bool pass = true;

  IdentityInput accepted;
  accepted.device_type = 1;
  accepted.vendor_id = 0x5143;
  accepted.device_id = 0x06010000;
  accepted.api_major = 1;
  accepted.api_minor =
#ifdef BENGAL_WSI_MESA26_PROFILE
      3;
#else
      0;
#endif
  accepted.device_name = "Turnip Adreno (TM) 610";
  accepted.driver_properties_available = true;
  accepted.driver_id = 18;
  accepted.driver_name = "Mesa Turnip";
  accepted.queue_family_count = 1;
  accepted.total_queue_count = 1;
  accepted.has_one_graphics_present_queue = true;
  pass &= RejectIdentity(accepted) == nullptr;
  IdentityInput rejected = accepted;
  rejected.device_type = 4;
  rejected.device_name = "llvmpipe CPU";
  pass &= RejectIdentity(rejected) != nullptr;
  rejected = accepted;
  rejected.api_minor =
#ifdef BENGAL_WSI_MESA26_PROFILE
      0;
#else
      1;
#endif
  pass &= RejectIdentity(rejected) != nullptr;
  rejected = accepted;
  rejected.total_queue_count = 2;
#ifdef BENGAL_WSI_MESA26_PROFILE
  rejected.has_one_graphics_present_queue = false;
#endif
  pass &= RejectIdentity(rejected) != nullptr;

#ifdef BENGAL_WSI_MESA26_PROFILE
  const QueueFamilyInput measured[] = {
      {1, 0x0000000f, true, true}, {1, 0x00000008, false, true}};
  uint32_t selected_family = UINT32_MAX;
  pass &= ValidateMesa26QueueTopology(measured, 2, &selected_family) &&
          selected_family == 0;
  const QueueFamilyInput missing_graphics[] = {
      {1, 0x0000000e, false, true}, {1, 0x00000008, false, true}};
  pass &= !ValidateMesa26QueueTopology(missing_graphics, 2, &selected_family);
  const QueueFamilyInput wrong_sparse[] = {
      {1, 0x0000000f, true, true}, {1, 0x0000000c, false, true}};
  pass &= !ValidateMesa26QueueTopology(wrong_sparse, 2, &selected_family);
  const QueueFamilyInput wrong_count[] = {
      {2, 0x0000000f, true, true}, {1, 0x00000008, false, true}};
  pass &= !ValidateMesa26QueueTopology(wrong_count, 2, &selected_family);
  const QueueFamilyInput missing_present[] = {
      {1, 0x0000000f, true, true}, {1, 0x00000008, false, false}};
  pass &= !ValidateMesa26QueueTopology(missing_present, 2, &selected_family);
#endif

  const GeometryPlan portrait = PlanAndroidGeometry(
      {720, 1600}, {1, 1}, {4096, 4096}, kTransformRotate90, 4096ULL * 4096ULL);
  pass &= portrait.valid && portrait.extent.width == 720 &&
          portrait.extent.height == 1600 &&
          portrait.pre_transform == kTransformRotate90;
  pass &= !PlanAndroidGeometry({0, 1600}, {1, 1}, {4096, 4096},
                               kTransformIdentity, 4096ULL * 4096ULL)
               .valid;
  pass &= !PlanAndroidGeometry({5000, 1600}, {1, 1}, {5000, 4096},
                               kTransformIdentity, 4096ULL * 4096ULL)
               .valid;

  const Point p{0.2f, 0.3f};
  pass &= Same(FramebufferToLogical(p, kTransformIdentity), {0.2f, 0.3f});
  pass &= Same(FramebufferToLogical(p, kTransformRotate90), {0.3f, 0.8f});
  pass &= Same(FramebufferToLogical(p, kTransformRotate180), {0.8f, 0.7f});
  pass &= Same(FramebufferToLogical(p, kTransformRotate270), {0.7f, 0.2f});
  pass &=
      Same(FramebufferToLogical(p, kTransformHorizontalMirror), {0.8f, 0.3f});
  pass &= Same(FramebufferToLogical(p, kTransformHorizontalMirrorRotate90),
               {0.7f, 0.8f});
  pass &= Same(FramebufferToLogical(p, kTransformHorizontalMirrorRotate180),
               {0.2f, 0.7f});
  pass &= Same(FramebufferToLogical(p, kTransformHorizontalMirrorRotate270),
               {0.3f, 0.2f});
#ifdef BENGAL_WSI_PRIVATE_GANESH
  const uint32_t transforms[] = {
      kTransformIdentity,
      kTransformRotate90,
      kTransformRotate180,
      kTransformRotate270,
      kTransformHorizontalMirror,
      kTransformHorizontalMirrorRotate90,
      kTransformHorizontalMirrorRotate180,
      kTransformHorizontalMirrorRotate270,
  };
  for (uint32_t transform : transforms) {
    const Point framebuffer = LogicalToFramebuffer(p, transform);
    pass &= Same(FramebufferToLogical(framebuffer, transform), p);
  }
#endif

  pass &= DecideTeardown(false, false) == TeardownDecision::kSafe;
  pass &= DecideTeardown(true, false) == TeardownDecision::kSkipAllVulkan;
  pass &= DecideTeardown(false, true) == TeardownDecision::kSkipAllVulkan;
  pass &= DecideTeardown(true, true) == TeardownDecision::kSkipAllVulkan;

  pass &= DecideLifecycle(false, LifecycleEvent::kInitWindow) ==
          LifecycleAction::kAllowFirstWindow;
  pass &= DecideLifecycle(false, LifecycleEvent::kTermWindow) ==
          LifecycleAction::kContinueWaiting;
  pass &= DecideLifecycle(false, LifecycleEvent::kGeometryChanged) ==
          LifecycleAction::kContinueWaiting;
  pass &= DecideLifecycle(true, LifecycleEvent::kInitWindow) ==
          LifecycleAction::kIgnoreNewWindow;
  pass &= DecideLifecycle(true, LifecycleEvent::kTermWindow) ==
          LifecycleAction::kStopNoDestroy;
  pass &= DecideLifecycle(true, LifecycleEvent::kGeometryChanged) ==
          LifecycleAction::kStopNoRecreate;
  pass &= DecideLifecycle(true, LifecycleEvent::kDestroy) ==
          LifecycleAction::kDestroyRequested;

#ifdef BENGAL_WSI_PRIVATE_GANESH
  std::printf("LOGIC_TEST verdict=%s profile=legacy_vulkan_1_0 "
              "identity=accepted_1.0_refused_1.1 geometry=3 transforms=16 "
              "teardown=4 lifecycle=7\n",
              pass ? "PASS" : "FAIL");
#elif defined(BENGAL_WSI_MESA26_PROFILE)
  std::printf("LOGIC_TEST verdict=%s profile=mesa26_vulkan_1_3 "
              "identity=accepted_1.3_refused_1.0 queues=accepted_2family_"
              "refused_missing_gfx_sparse_count_present geometry=3 transforms=8 "
              "teardown=4 lifecycle=7\n",
              pass ? "PASS" : "FAIL");
#else
  std::printf("LOGIC_TEST verdict=%s profile=legacy_vulkan_1_0 "
              "identity=accepted_1.0_refused_1.1 geometry=3 transforms=8 "
              "teardown=4 lifecycle=7\n",
              pass ? "PASS" : "FAIL");
#endif
  return pass ? 0 : 1;
}
