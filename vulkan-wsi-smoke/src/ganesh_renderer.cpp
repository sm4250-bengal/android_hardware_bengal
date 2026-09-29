#include "ganesh_renderer.h"

#include "logic.h"

#include "experimental/bengal-vulkan10/SkUserConfig.h"
#include "include/core/SkBitmap.h"
#include "include/core/SkCanvas.h"
#include "include/core/SkColorSpace.h"
#include "include/core/SkFont.h"
#include "include/core/SkImage.h"
#include "include/core/SkMatrix.h"
#include "include/core/SkPaint.h"
#include "include/core/SkPathBuilder.h"
#include "include/core/SkSurface.h"
#include "include/core/SkTextBlob.h"
#include "include/gpu/ganesh/GrBackendSurface.h"
#include "include/gpu/ganesh/GrDirectContext.h"
#include "include/gpu/ganesh/SkSurfaceGanesh.h"
#include "include/gpu/ganesh/vk/GrVkBackendSemaphore.h"
#include "include/gpu/ganesh/vk/GrVkBackendSurface.h"
#include "include/gpu/ganesh/vk/GrVkDirectContext.h"
#include "include/gpu/ganesh/vk/GrVkTypes.h"
#include "include/gpu/vk/VulkanBackendContext.h"
#include "include/gpu/vk/VulkanExtensions.h"
#include "include/utils/SkCustomTypeface.h"

#include <atomic>
#include <iterator>
#include <new>
#include <utility>
#include <vector>

#ifndef SK_BENGAL_EXPERIMENTAL_VULKAN_10
#error The private Ganesh WSI adapter requires the isolated Bengal Skia build.
#endif

namespace bengal_wsi {
namespace {

bool MakeLogicalCanvasMatrix(uint32_t width, uint32_t height,
                             uint32_t transform, SkMatrix *matrix) {
  constexpr SkScalar kLogicalSize = 128.0f;
  const SkPoint source[] = {{0, 0}, {kLogicalSize, 0}, {0, kLogicalSize}};
  const Point logical[] = {{0, 0}, {1, 0}, {0, 1}};
  SkPoint destination[3];
  for (size_t i = 0; i < std::size(logical); ++i) {
    const Point framebuffer =
        bengal_wsi::LogicalToFramebuffer(logical[i], transform);
    destination[i] = {
        framebuffer.x * static_cast<SkScalar>(width),
        framebuffer.y * static_cast<SkScalar>(height),
    };
  }
  return matrix->setPolyToPoly(source, destination);
}

SkColorType ColorTypeForFormat(VkFormat format) {
  if (format == VK_FORMAT_R8G8B8A8_UNORM)
    return kRGBA_8888_SkColorType;
  if (format == VK_FORMAT_B8G8R8A8_UNORM)
    return kBGRA_8888_SkColorType;
  return kUnknown_SkColorType;
}

} // namespace

struct PrivateGaneshRenderer::Impl {
  VkQueue queue = VK_NULL_HANDLE;
  uint32_t width = 0;
  uint32_t height = 0;
  uint32_t pre_transform = 0;
  std::atomic<bool> device_lost{false};
  skgpu::VulkanExtensions extensions;
  sk_sp<GrDirectContext> context;
  std::vector<sk_sp<SkSurface>> surfaces;
  sk_sp<SkImage> raster_image;
  sk_sp<SkTextBlob> glyph;

  bool DrawScene(SkSurface *surface) {
    SkMatrix transform;
    if (!MakeLogicalCanvasMatrix(width, height, pre_transform, &transform))
      return false;
    SkCanvas *canvas = surface->getCanvas();
    canvas->clear(SkColorSetRGB(16, 20, 24));
    canvas->save();
    canvas->concat(transform);

    SkPaint paint;
    paint.setAntiAlias(false);
    paint.setStrokeWidth(0.35f);
    paint.setColor(SkColorSetRGB(88, 88, 88));
    for (int i = 1; i < 10; ++i) {
      const SkScalar p = 12.8f * i;
      canvas->drawLine(p, 0, p, 128, paint);
      canvas->drawLine(0, p, 128, p, paint);
    }

    paint.setStyle(SkPaint::kFill_Style);
    paint.setColor(SK_ColorRED);
    canvas->drawRect(SkRect::MakeXYWH(0, 0, 5, 5), paint);
    paint.setColor(SK_ColorGREEN);
    canvas->drawRect(SkRect::MakeXYWH(123, 0, 5, 5), paint);
    paint.setColor(SK_ColorBLUE);
    canvas->drawRect(SkRect::MakeXYWH(0, 123, 5, 5), paint);
    paint.setColor(SK_ColorYELLOW);
    canvas->drawRect(SkRect::MakeXYWH(123, 123, 5, 5), paint);

    paint.setColor(SK_ColorRED);
    canvas->drawPath(SkPathBuilder()
                         .moveTo(8, 8)
                         .lineTo(56, 8)
                         .lineTo(8, 56)
                         .close()
                         .detach(),
                     paint);
    canvas->save();
    canvas->clipRect(SkRect::MakeXYWH(72, 8, 32, 32));
    paint.setColor(SK_ColorGREEN);
    canvas->drawRect(SkRect::MakeXYWH(64, 0, 56, 56), paint);
    canvas->restore();
    canvas->drawImage(raster_image, 8, 72);
    paint.setColor(SK_ColorWHITE);
    canvas->drawTextBlob(glyph, 72, 96, paint);

    paint.setStrokeWidth(0.75f);
    paint.setColor(SK_ColorMAGENTA);
    canvas->drawLine(64, 48, 64, 80, paint);
    paint.setColor(SK_ColorCYAN);
    canvas->drawLine(48, 64, 80, 64, paint);
    canvas->restore();
    return true;
  }
};

const char *GaneshStageName(GaneshStage stage) {
  switch (stage) {
  case GaneshStage::kContext:
    return "GrDirectContexts::MakeVulkan";
  case GaneshStage::kSceneResources:
    return "scene_resources";
  case GaneshStage::kWrapSwapchainImage:
    return "SkSurfaces::WrapBackendRenderTarget";
  case GaneshStage::kAcquireWait:
    return "SkSurface::wait";
  case GaneshStage::kDraw:
    return "SkCanvas_draw_scene";
  case GaneshStage::kFlushSignal:
    return "GrDirectContext::flush_present_signal";
  case GaneshStage::kSubmit:
    return "GrDirectContext::submit";
  case GaneshStage::kFenceSubmit:
    return "vkQueueSubmit_empty_fence";
  case GaneshStage::kNone:
  default:
    return "none";
  }
}

PrivateGaneshRenderer::PrivateGaneshRenderer(Impl *impl) : impl_(impl) {}

PrivateGaneshRenderer::~PrivateGaneshRenderer() { delete impl_; }

PrivateGaneshRenderer *
PrivateGaneshRenderer::Create(const GaneshCreateInfo &info,
                              GaneshStage *failed_stage) {
  auto fail = [failed_stage](GaneshStage stage) {
    if (failed_stage != nullptr)
      *failed_stage = stage;
    return static_cast<PrivateGaneshRenderer *>(nullptr);
  };
  if (failed_stage != nullptr)
    *failed_stage = GaneshStage::kNone;
  if (info.instance == VK_NULL_HANDLE ||
      info.physical_device == VK_NULL_HANDLE || info.device == VK_NULL_HANDLE ||
      info.queue == VK_NULL_HANDLE || info.queue_family == UINT32_MAX ||
      info.enabled_features == nullptr || info.instance_extensions == nullptr ||
      info.device_extensions == nullptr || info.images == nullptr ||
      info.image_count == 0 || info.width == 0 || info.height == 0 ||
      ColorTypeForFormat(info.format) == kUnknown_SkColorType) {
    return fail(GaneshStage::kContext);
  }

  Impl *impl = new (std::nothrow) Impl;
  if (impl == nullptr)
    return fail(GaneshStage::kContext);
  impl->queue = info.queue;
  impl->width = info.width;
  impl->height = info.height;
  impl->pre_transform = info.pre_transform;

  skgpu::VulkanGetProc get_proc = [](const char *name, VkInstance instance,
                                      VkDevice device) {
    return device != VK_NULL_HANDLE ? vkGetDeviceProcAddr(device, name)
                                    : vkGetInstanceProcAddr(instance, name);
  };
  impl->extensions.init(get_proc, info.instance, info.physical_device,
                        info.instance_extension_count, info.instance_extensions,
                        info.device_extension_count, info.device_extensions);
  skgpu::VulkanBackendContext backend;
  backend.fInstance = info.instance;
  backend.fPhysicalDevice = info.physical_device;
  backend.fDevice = info.device;
  backend.fQueue = info.queue;
  backend.fGraphicsQueueIndex = info.queue_family;
  backend.fMaxAPIVersion = VK_API_VERSION_1_0;
  backend.fVkExtensions = &impl->extensions;
  backend.fDeviceFeatures2 = info.enabled_features;
  backend.fGetProc = std::move(get_proc);
  backend.fDeviceLostContext = impl;
  backend.fDeviceLostProc = [](void *context, const auto &, const auto &,
                               const auto &, const auto &) {
    static_cast<Impl *>(context)->device_lost.store(true);
  };
  impl->context = GrDirectContexts::MakeVulkan(backend);
  if (impl->context == nullptr) {
    delete impl;
    return fail(GaneshStage::kContext);
  }

  SkBitmap raster;
  if (!raster.tryAllocPixels(SkImageInfo::MakeN32Premul(24, 24))) {
    delete impl;
    return fail(GaneshStage::kSceneResources);
  }
  raster.eraseColor(SK_ColorYELLOW);
  impl->raster_image = SkImages::RasterFromBitmap(raster);
  SkCustomTypefaceBuilder typeface;
  SkFontMetrics metrics{};
  metrics.fAscent = -1;
  metrics.fDescent = 0;
  typeface.setMetrics(metrics);
  typeface.setGlyph(1, 1,
                    SkPath::Rect(SkRect::MakeLTRB(0, -1, 0.75f, 0)));
  SkFont font(typeface.detach(), 24);
  font.setEdging(SkFont::Edging::kAlias);
  const SkGlyphID glyph_id = 1;
  impl->glyph = SkTextBlob::MakeFromText(&glyph_id, sizeof(glyph_id), font,
                                         SkTextEncoding::kGlyphID);
  if (impl->raster_image == nullptr || impl->glyph == nullptr) {
    delete impl;
    return fail(GaneshStage::kSceneResources);
  }

  impl->surfaces.reserve(info.image_count);
  for (uint32_t i = 0; i < info.image_count; ++i) {
    GrVkImageInfo image_info;
    image_info.fImage = info.images[i];
    image_info.fImageTiling = VK_IMAGE_TILING_OPTIMAL;
    image_info.fImageLayout = VK_IMAGE_LAYOUT_UNDEFINED;
    image_info.fFormat = info.format;
    image_info.fImageUsageFlags = info.usage;
    image_info.fSampleCount = 1;
    image_info.fLevelCount = 1;
    image_info.fCurrentQueueFamily = info.queue_family;
    image_info.fSharingMode = VK_SHARING_MODE_EXCLUSIVE;
    const GrBackendRenderTarget target = GrBackendRenderTargets::MakeVk(
        static_cast<int>(info.width), static_cast<int>(info.height), image_info);
    sk_sp<SkSurface> surface = SkSurfaces::WrapBackendRenderTarget(
        impl->context.get(), target, kTopLeft_GrSurfaceOrigin,
        ColorTypeForFormat(info.format), nullptr, nullptr);
    if (surface == nullptr) {
      delete impl;
      return fail(GaneshStage::kWrapSwapchainImage);
    }
    impl->surfaces.push_back(std::move(surface));
  }

  PrivateGaneshRenderer *renderer =
      new (std::nothrow) PrivateGaneshRenderer(impl);
  if (renderer == nullptr) {
    delete impl;
    return fail(GaneshStage::kContext);
  }
  return renderer;
}

GaneshFrameResult PrivateGaneshRenderer::RenderAndSubmit(
    uint32_t, uint32_t image_index, VkSemaphore acquire_semaphore,
    VkSemaphore render_finished, VkFence completion_fence) {
  if (image_index >= impl_->surfaces.size())
    return {VK_ERROR_INITIALIZATION_FAILED, GaneshStage::kWrapSwapchainImage};
  SkSurface *surface = impl_->surfaces[image_index].get();
  const GrBackendSemaphore acquire =
      GrBackendSemaphores::MakeVk(acquire_semaphore);
  if (!surface->wait(1, &acquire, false))
    return {VK_ERROR_INITIALIZATION_FAILED, GaneshStage::kAcquireWait};

  if (!impl_->DrawScene(surface))
    return {VK_ERROR_INITIALIZATION_FAILED, GaneshStage::kDraw};
  if (impl_->device_lost.load() || impl_->context->abandoned())
    return {VK_ERROR_DEVICE_LOST, GaneshStage::kDraw};

  GrBackendSemaphore signal =
      GrBackendSemaphores::MakeVk(render_finished);
  GrFlushInfo flush_info;
  flush_info.fNumSemaphores = 1;
  flush_info.fSignalSemaphores = &signal;
  if (impl_->context->flush(surface, SkSurfaces::BackendSurfaceAccess::kPresent,
                            flush_info) != GrSemaphoresSubmitted::kYes) {
    return {VK_ERROR_INITIALIZATION_FAILED, GaneshStage::kFlushSignal};
  }
  if (!impl_->context->submit()) {
    return {impl_->device_lost.load() || impl_->context->abandoned()
                ? VK_ERROR_DEVICE_LOST
                : VK_ERROR_INITIALIZATION_FAILED,
            GaneshStage::kSubmit};
  }
  if (impl_->device_lost.load() || impl_->context->abandoned())
    return {VK_ERROR_DEVICE_LOST, GaneshStage::kSubmit};

  VkSubmitInfo empty_submit{};
  empty_submit.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO;
  const VkResult result =
      vkQueueSubmit(impl_->queue, 1, &empty_submit, completion_fence);
  return {result, GaneshStage::kFenceSubmit};
}

} // namespace bengal_wsi
