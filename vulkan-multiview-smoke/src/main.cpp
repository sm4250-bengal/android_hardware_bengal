#include <vulkan/vulkan.h>

#include "multiview_spv.h"

#include <array>
#include <cinttypes>
#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

namespace {
constexpr uint32_t kWidth = 64;
constexpr uint32_t kHeight = 64;
constexpr uint32_t kLayers = 2;
constexpr VkDeviceSize kLayerBytes = kWidth * kHeight * 4;
constexpr VkDeviceSize kReadbackBytes = kLayerBytes * kLayers;
constexpr uint64_t kFenceTimeoutNs = 5'000'000'000ULL;
constexpr uint32_t kQualcommVendor = 0x5143;
constexpr uint32_t kAdreno610Device = 0x06010000;
constexpr uint32_t kMaxExtensions = 1024;
constexpr uint32_t kMaxPhysicalDevices = 32;
constexpr uint32_t kMaxQueueFamilies = 64;
constexpr VkDeviceSize kMaxAllocation = 16 * 1024 * 1024;

enum Exit : int { kPass = 0, kUsage = 2, kNotHardware = 3, kNoDevice = 20,
                  kIdentity = 21, kUnsupported = 22, kSetup = 30,
                  kSubmit = 40, kFenceTimeout = 41, kDeviceLost = 42, kUncertain = 43,
                  kReadback = 50, kSelfTest = 60 };

struct Identity {
  VkPhysicalDeviceProperties properties{};
  VkDriverId driver_id = VK_DRIVER_ID_MAX_ENUM;
  std::string driver_name;
  bool has_driver_properties = false;
  uint32_t max_multiview_views = 0;
};

struct Features { bool extension = false; bool multiview = false; };

struct Pixels {
  bool pass = false;
  std::string reason;
};

struct Resources {
  VkDevice device = VK_NULL_HANDLE;
  VkImage image = VK_NULL_HANDLE;
  VkDeviceMemory image_memory = VK_NULL_HANDLE;
  VkImageView view = VK_NULL_HANDLE;
  VkBuffer readback = VK_NULL_HANDLE;
  VkDeviceMemory readback_memory = VK_NULL_HANDLE;
  VkRenderPass render_pass = VK_NULL_HANDLE;
  VkFramebuffer framebuffer = VK_NULL_HANDLE;
  VkPipelineLayout pipeline_layout = VK_NULL_HANDLE;
  VkPipeline pipeline = VK_NULL_HANDLE;
  VkCommandPool command_pool = VK_NULL_HANDLE;
  VkCommandBuffer command = VK_NULL_HANDLE;
  VkFence fence = VK_NULL_HANDLE;
};

bool HasExtension(const std::vector<VkExtensionProperties>& extensions, const char* name) {
  for (const auto& extension : extensions) {
    if (std::strcmp(extension.extensionName, name) == 0) return true;
  }
  return false;
}

std::string Lower(std::string value) {
  for (char& c : value) if (c >= 'A' && c <= 'Z') c = static_cast<char>(c + ('a' - 'A'));
  return value;
}

std::string IdentityRejection(const Identity& identity) {
  const std::string name = Lower(identity.properties.deviceName);
  const std::string driver = Lower(identity.driver_name);
  if (identity.properties.deviceType == VK_PHYSICAL_DEVICE_TYPE_CPU) return "software_device";
  if (identity.properties.deviceType != VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU) return "not_integrated_gpu";
  if (identity.properties.vendorID != kQualcommVendor) return "vendor_not_qualcomm";
  if (identity.properties.deviceID != kAdreno610Device) return "device_not_adreno_a610";
  if (name.find("adreno") == std::string::npos || name.find("610") == std::string::npos) return "name_not_adreno_a610";
  if (VK_API_VERSION_MAJOR(identity.properties.apiVersion) < 1 ||
      (VK_API_VERSION_MAJOR(identity.properties.apiVersion) == 1 && VK_API_VERSION_MINOR(identity.properties.apiVersion) < 3)) return "physical_api_below_1_3";
  if (!identity.has_driver_properties || identity.driver_id != VK_DRIVER_ID_MESA_TURNIP || driver.find("turnip") == std::string::npos) return "driver_not_mesa_turnip";
  return {};
}

bool MultiviewUsable(uint32_t api_version, bool extension, bool feature) {
  return feature && (api_version >= VK_API_VERSION_1_1 || extension);
}

uint32_t FindMemoryType(VkPhysicalDevice physical, uint32_t bits, VkMemoryPropertyFlags flags) {
  VkPhysicalDeviceMemoryProperties memory{};
  vkGetPhysicalDeviceMemoryProperties(physical, &memory);
  for (uint32_t i = 0; i < memory.memoryTypeCount; ++i) {
    if ((bits & (1u << i)) && (memory.memoryTypes[i].propertyFlags & flags) == flags) return i;
  }
  return UINT32_MAX;
}

void Destroy(Resources* r) {
  if (r->device == VK_NULL_HANDLE) return;
  if (r->fence) vkDestroyFence(r->device, r->fence, nullptr);
  if (r->command_pool) vkDestroyCommandPool(r->device, r->command_pool, nullptr);
  if (r->pipeline) vkDestroyPipeline(r->device, r->pipeline, nullptr);
  if (r->pipeline_layout) vkDestroyPipelineLayout(r->device, r->pipeline_layout, nullptr);
  if (r->framebuffer) vkDestroyFramebuffer(r->device, r->framebuffer, nullptr);
  if (r->render_pass) vkDestroyRenderPass(r->device, r->render_pass, nullptr);
  if (r->view) vkDestroyImageView(r->device, r->view, nullptr);
  if (r->readback) vkDestroyBuffer(r->device, r->readback, nullptr);
  if (r->readback_memory) vkFreeMemory(r->device, r->readback_memory, nullptr);
  if (r->image) vkDestroyImage(r->device, r->image, nullptr);
  if (r->image_memory) vkFreeMemory(r->device, r->image_memory, nullptr);
  vkDestroyDevice(r->device, nullptr);
  *r = {};
}

Pixels CheckPixels(const uint8_t* data, size_t bytes) {
  if (data == nullptr || bytes != kReadbackBytes) return {false, "missing_layer"};
  const auto pixel = [data](uint32_t layer, uint32_t x, uint32_t y) {
    return data + layer * kLayerBytes + (y * kWidth + x) * 4;
  };
  const auto equals = [](const uint8_t* p, uint8_t r, uint8_t g, uint8_t b) {
    return p[0] == r && p[1] == g && p[2] == b && p[3] == 255;
  };
  if (!equals(pixel(0, 14, 32), 255, 0, 0)) return {false, "layer0_interior_not_red"};
  if (!equals(pixel(1, 50, 32), 0, 255, 0)) return {false, "layer1_interior_not_green"};
  if (!equals(pixel(0, 50, 32), 0, 0, 0) || !equals(pixel(1, 14, 32), 0, 0, 0)) return {false, "view_geometry_leaked_to_other_layer"};
  if (!equals(pixel(0, 2, 2), 0, 0, 0) || !equals(pixel(1, 2, 2), 0, 0, 0)) return {false, "clear_outside_failed"};
  return {true, "ok"};
}

int SelfTest() {
  std::vector<uint8_t> pixels(kReadbackBytes, 0);
  for (size_t i = 3; i < pixels.size(); i += 4) pixels[i] = 255;
  auto set = [&pixels](uint32_t layer, uint32_t x, uint32_t y, uint8_t r, uint8_t g, uint8_t b) {
    uint8_t* p = pixels.data() + layer * kLayerBytes + (y * kWidth + x) * 4;
    p[0] = r; p[1] = g; p[2] = b;
  };
  set(0, 14, 32, 255, 0, 0); set(1, 50, 32, 0, 255, 0);
  if (!CheckPixels(pixels.data(), pixels.size()).pass) return kSelfTest;
  auto swapped = pixels; set(0, 14, 32, 0, 255, 0); set(1, 50, 32, 255, 0, 0);
  const bool swapped_fails = !CheckPixels(pixels.data(), pixels.size()).pass;
  pixels = swapped;
  pixels.data()[1 * kLayerBytes + (32 * kWidth + 50) * 4] = 255;
  pixels.data()[1 * kLayerBytes + (32 * kWidth + 50) * 4 + 1] = 0;
  const bool duplicate_fails = !CheckPixels(pixels.data(), pixels.size()).pass;
  const bool missing_fails = !CheckPixels(swapped.data(), kLayerBytes).pass;
  Identity exact{};
  exact.properties.deviceType = VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU;
  exact.properties.vendorID = kQualcommVendor; exact.properties.deviceID = kAdreno610Device;
  exact.properties.apiVersion = VK_API_VERSION_1_3; std::strcpy(exact.properties.deviceName, "Adreno (TM) 610");
  exact.has_driver_properties = true; exact.driver_id = VK_DRIVER_ID_MESA_TURNIP; exact.driver_name = "Mesa Turnip";
  Identity software = exact; software.properties.deviceType = VK_PHYSICAL_DEVICE_TYPE_CPU;
  Identity old = exact; old.properties.apiVersion = VK_API_VERSION_1_0;
  const bool identities = IdentityRejection(exact).empty() && IdentityRejection(software) == "software_device" && IdentityRejection(old) == "physical_api_below_1_3";
  const bool features = MultiviewUsable(VK_API_VERSION_1_3, false, true) && !MultiviewUsable(VK_API_VERSION_1_0, false, true) && !MultiviewUsable(VK_API_VERSION_1_3, true, false);
  if (!swapped_fails || !duplicate_fails || !missing_fails || !identities || !features) {
    std::printf("SELF_TEST verdict=FAIL swapped=%u duplicate=%u missing=%u identities=%u features=%u\n",
                swapped_fails, duplicate_fails, missing_fails, identities, features);
    return kSelfTest;
  }
  std::puts("SELF_TEST verdict=PASS pixel_swapped=refused pixel_duplicated=refused pixel_missing=refused identity=refused feature=refused");
  return kPass;
}

Identity ReadIdentity(VkPhysicalDevice physical, PFN_vkGetPhysicalDeviceProperties2KHR get_properties2) {
  Identity identity{};
  vkGetPhysicalDeviceProperties(physical, &identity.properties);
  // Older baseline drivers need only an identity rejection, not an unsupported
  // driver-properties chain. This client intentionally targets core Vulkan1.3.
  if (identity.properties.apiVersion < VK_API_VERSION_1_3) return identity;
  VkPhysicalDeviceProperties2 properties{VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2};
  VkPhysicalDeviceDriverProperties driver{VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES};
  VkPhysicalDeviceMultiviewProperties multiview{VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MULTIVIEW_PROPERTIES};
  driver.pNext = &multiview;
  properties.pNext = &driver;
  get_properties2(physical, &properties);
  identity.properties = properties.properties;
  identity.driver_id = driver.driverID;
  identity.driver_name = driver.driverName;
  identity.has_driver_properties = true;
  identity.max_multiview_views = multiview.maxMultiviewViewCount;
  return identity;
}

Features ReadFeatures(VkPhysicalDevice physical, PFN_vkGetPhysicalDeviceFeatures2KHR get_features2) {
  Features result{};
  uint32_t count = 0;
  if (vkEnumerateDeviceExtensionProperties(physical, nullptr, &count, nullptr) != VK_SUCCESS || count > kMaxExtensions) return result;
  std::vector<VkExtensionProperties> extensions(count);
  if (count && vkEnumerateDeviceExtensionProperties(physical, nullptr, &count, extensions.data()) != VK_SUCCESS) return result;
  extensions.resize(count);
  result.extension = HasExtension(extensions, VK_KHR_MULTIVIEW_EXTENSION_NAME);
  VkPhysicalDeviceFeatures2 features{VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2};
  VkPhysicalDeviceMultiviewFeatures multiview{VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MULTIVIEW_FEATURES};
  features.pNext = &multiview;
  get_features2(physical, &features);
  result.multiview = multiview.multiview == VK_TRUE;
  return result;
}

bool CreateBuffer(VkPhysicalDevice physical, Resources* r) {
  VkBufferCreateInfo info{VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO};
  info.size = kReadbackBytes; info.usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT;
  if (vkCreateBuffer(r->device, &info, nullptr, &r->readback) != VK_SUCCESS) return false;
  VkMemoryRequirements requirements{}; vkGetBufferMemoryRequirements(r->device, r->readback, &requirements);
  if (requirements.size < kReadbackBytes || requirements.size > kMaxAllocation) return false;
  const uint32_t type = FindMemoryType(physical, requirements.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
  if (type == UINT32_MAX) return false;
  VkMemoryAllocateInfo allocate{VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO}; allocate.allocationSize = requirements.size; allocate.memoryTypeIndex = type;
  if (vkAllocateMemory(r->device, &allocate, nullptr, &r->readback_memory) != VK_SUCCESS) return false;
  return vkBindBufferMemory(r->device, r->readback, r->readback_memory, 0) == VK_SUCCESS;
}

bool CreateImage(VkPhysicalDevice physical, Resources* r) {
  VkImageCreateInfo image{VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO};
  image.imageType = VK_IMAGE_TYPE_2D; image.format = VK_FORMAT_R8G8B8A8_UNORM;
  image.extent = {kWidth, kHeight, 1}; image.mipLevels = 1; image.arrayLayers = kLayers;
  image.samples = VK_SAMPLE_COUNT_1_BIT; image.tiling = VK_IMAGE_TILING_OPTIMAL;
  image.usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT;
  image.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED;
  if (vkCreateImage(r->device, &image, nullptr, &r->image) != VK_SUCCESS) return false;
  VkMemoryRequirements requirements{}; vkGetImageMemoryRequirements(r->device, r->image, &requirements);
  if (!requirements.size || requirements.size > kMaxAllocation) return false;
  const uint32_t type = FindMemoryType(physical, requirements.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT);
  if (type == UINT32_MAX) return false;
  VkMemoryAllocateInfo allocate{VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO}; allocate.allocationSize = requirements.size; allocate.memoryTypeIndex = type;
  if (vkAllocateMemory(r->device, &allocate, nullptr, &r->image_memory) != VK_SUCCESS || vkBindImageMemory(r->device, r->image, r->image_memory, 0) != VK_SUCCESS) return false;
  VkImageViewCreateInfo view{VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO}; view.image = r->image; view.viewType = VK_IMAGE_VIEW_TYPE_2D_ARRAY; view.format = image.format;
  view.subresourceRange.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT; view.subresourceRange.levelCount = 1; view.subresourceRange.layerCount = kLayers;
  return vkCreateImageView(r->device, &view, nullptr, &r->view) == VK_SUCCESS;
}

bool CreateRenderObjects(Resources* r) {
  VkAttachmentDescription attachment{}; attachment.format = VK_FORMAT_R8G8B8A8_UNORM; attachment.samples = VK_SAMPLE_COUNT_1_BIT;
  attachment.loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR; attachment.storeOp = VK_ATTACHMENT_STORE_OP_STORE;
  attachment.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED; attachment.finalLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
  VkAttachmentReference color{0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL};
  VkSubpassDescription subpass{}; subpass.pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS; subpass.colorAttachmentCount = 1; subpass.pColorAttachments = &color;
  VkSubpassDependency dependency{}; dependency.srcSubpass = 0; dependency.dstSubpass = VK_SUBPASS_EXTERNAL;
  dependency.srcStageMask = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT; dependency.dstStageMask = VK_PIPELINE_STAGE_TRANSFER_BIT;
  dependency.srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT; dependency.dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
  uint32_t view_mask = 0x3;
  VkRenderPassMultiviewCreateInfo multiview{VK_STRUCTURE_TYPE_RENDER_PASS_MULTIVIEW_CREATE_INFO}; multiview.subpassCount = 1; multiview.pViewMasks = &view_mask;
  VkRenderPassCreateInfo pass{VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO}; pass.pNext = &multiview; pass.attachmentCount = 1; pass.pAttachments = &attachment;
  pass.subpassCount = 1; pass.pSubpasses = &subpass; pass.dependencyCount = 1; pass.pDependencies = &dependency;
  if (vkCreateRenderPass(r->device, &pass, nullptr, &r->render_pass) != VK_SUCCESS) return false;
  VkFramebufferCreateInfo framebuffer{VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO}; framebuffer.renderPass = r->render_pass; framebuffer.attachmentCount = 1; framebuffer.pAttachments = &r->view;
  framebuffer.width = kWidth; framebuffer.height = kHeight; framebuffer.layers = 1;
  if (vkCreateFramebuffer(r->device, &framebuffer, nullptr, &r->framebuffer) != VK_SUCCESS) return false;
  VkShaderModuleCreateInfo shader{VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO}; shader.codeSize = kMultiviewVertSpvBytes; shader.pCode = kMultiviewVertSpv;
  VkShaderModule vertex = VK_NULL_HANDLE, fragment = VK_NULL_HANDLE;
  if (vkCreateShaderModule(r->device, &shader, nullptr, &vertex) != VK_SUCCESS) return false;
  shader.codeSize = kMultiviewFragSpvBytes; shader.pCode = kMultiviewFragSpv;
  if (vkCreateShaderModule(r->device, &shader, nullptr, &fragment) != VK_SUCCESS) { vkDestroyShaderModule(r->device, vertex, nullptr); return false; }
  VkPipelineLayoutCreateInfo layout{VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO};
  if (vkCreatePipelineLayout(r->device, &layout, nullptr, &r->pipeline_layout) != VK_SUCCESS) { vkDestroyShaderModule(r->device, vertex, nullptr); vkDestroyShaderModule(r->device, fragment, nullptr); return false; }
  VkPipelineShaderStageCreateInfo stages[2] = {{VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO}, {VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO}};
  stages[0].stage = VK_SHADER_STAGE_VERTEX_BIT; stages[0].module = vertex; stages[0].pName = "main";
  stages[1].stage = VK_SHADER_STAGE_FRAGMENT_BIT; stages[1].module = fragment; stages[1].pName = "main";
  VkPipelineVertexInputStateCreateInfo vertex_input{VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO};
  VkPipelineInputAssemblyStateCreateInfo assembly{VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO}; assembly.topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST;
  VkViewport viewport{0, 0, static_cast<float>(kWidth), static_cast<float>(kHeight), 0, 1}; VkRect2D scissor{{0, 0}, {kWidth, kHeight}};
  VkPipelineViewportStateCreateInfo viewport_state{VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO}; viewport_state.viewportCount = 1; viewport_state.pViewports = &viewport; viewport_state.scissorCount = 1; viewport_state.pScissors = &scissor;
  VkPipelineRasterizationStateCreateInfo raster{VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO}; raster.polygonMode = VK_POLYGON_MODE_FILL; raster.cullMode = VK_CULL_MODE_NONE; raster.lineWidth = 1;
  VkPipelineMultisampleStateCreateInfo multisample{VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO}; multisample.rasterizationSamples = VK_SAMPLE_COUNT_1_BIT;
  VkPipelineColorBlendAttachmentState blend{}; blend.colorWriteMask = VK_COLOR_COMPONENT_R_BIT | VK_COLOR_COMPONENT_G_BIT | VK_COLOR_COMPONENT_B_BIT | VK_COLOR_COMPONENT_A_BIT;
  VkPipelineColorBlendStateCreateInfo blending{VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO}; blending.attachmentCount = 1; blending.pAttachments = &blend;
  VkGraphicsPipelineCreateInfo pipeline{VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO}; pipeline.stageCount = 2; pipeline.pStages = stages; pipeline.pVertexInputState = &vertex_input; pipeline.pInputAssemblyState = &assembly;
  pipeline.pViewportState = &viewport_state; pipeline.pRasterizationState = &raster; pipeline.pMultisampleState = &multisample; pipeline.pColorBlendState = &blending; pipeline.layout = r->pipeline_layout; pipeline.renderPass = r->render_pass;
  const VkResult result = vkCreateGraphicsPipelines(r->device, VK_NULL_HANDLE, 1, &pipeline, nullptr, &r->pipeline);
  vkDestroyShaderModule(r->device, vertex, nullptr); vkDestroyShaderModule(r->device, fragment, nullptr);
  return result == VK_SUCCESS;
}

int Render(VkPhysicalDevice physical, uint32_t queue_family, bool extension, bool hardware) {
  Resources r{}; float priority = 1.0f;
  VkDeviceQueueCreateInfo queue{VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO}; queue.queueFamilyIndex = queue_family; queue.queueCount = 1; queue.pQueuePriorities = &priority;
  VkPhysicalDeviceMultiviewFeatures multiview{VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_MULTIVIEW_FEATURES}; multiview.multiview = VK_TRUE;
  const char* extensions[] = {VK_KHR_MULTIVIEW_EXTENSION_NAME};
  VkDeviceCreateInfo device{VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO}; device.pNext = &multiview; device.queueCreateInfoCount = 1; device.pQueueCreateInfos = &queue;
  device.enabledExtensionCount = extension ? 1u : 0u; device.ppEnabledExtensionNames = extension ? extensions : nullptr;
  if (vkCreateDevice(physical, &device, nullptr, &r.device) != VK_SUCCESS) return kSetup;
  VkQueue queue_handle = VK_NULL_HANDLE; vkGetDeviceQueue(r.device, queue_family, 0, &queue_handle);
  if (!CreateImage(physical, &r) || !CreateBuffer(physical, &r) || !CreateRenderObjects(&r)) { Destroy(&r); return kSetup; }
  VkCommandPoolCreateInfo pool{VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO}; pool.queueFamilyIndex = queue_family;
  if (vkCreateCommandPool(r.device, &pool, nullptr, &r.command_pool) != VK_SUCCESS) { Destroy(&r); return kSetup; }
  VkCommandBufferAllocateInfo allocate{VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO}; allocate.commandPool = r.command_pool; allocate.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY; allocate.commandBufferCount = 1;
  if (vkAllocateCommandBuffers(r.device, &allocate, &r.command) != VK_SUCCESS) { Destroy(&r); return kSetup; }
  VkCommandBufferBeginInfo begin{VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO}; begin.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
  if (vkBeginCommandBuffer(r.command, &begin) != VK_SUCCESS) { Destroy(&r); return kSetup; }
  VkClearValue clear{}; clear.color = {{0, 0, 0, 1}};
  VkRenderPassBeginInfo pass{VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO}; pass.renderPass = r.render_pass; pass.framebuffer = r.framebuffer; pass.renderArea.extent = {kWidth, kHeight}; pass.clearValueCount = 1; pass.pClearValues = &clear;
  vkCmdBeginRenderPass(r.command, &pass, VK_SUBPASS_CONTENTS_INLINE); vkCmdBindPipeline(r.command, VK_PIPELINE_BIND_POINT_GRAPHICS, r.pipeline); vkCmdDraw(r.command, 3, 1, 0, 0); vkCmdEndRenderPass(r.command);
  std::array<VkBufferImageCopy, kLayers> copies{};
  for (uint32_t layer = 0; layer < kLayers; ++layer) { copies[layer].bufferOffset = layer * kLayerBytes; copies[layer].imageSubresource.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT; copies[layer].imageSubresource.layerCount = 1; copies[layer].imageSubresource.baseArrayLayer = layer; copies[layer].imageExtent = {kWidth, kHeight, 1}; }
  vkCmdCopyImageToBuffer(r.command, r.image, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, r.readback, kLayers, copies.data());
  VkBufferMemoryBarrier barrier{VK_STRUCTURE_TYPE_BUFFER_MEMORY_BARRIER}; barrier.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT; barrier.dstAccessMask = VK_ACCESS_HOST_READ_BIT; barrier.buffer = r.readback; barrier.size = kReadbackBytes;
  barrier.srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
  barrier.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
  vkCmdPipelineBarrier(r.command, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0, 0, nullptr, 1, &barrier, 0, nullptr);
  VkFenceCreateInfo fence_info{VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
  if (vkEndCommandBuffer(r.command) != VK_SUCCESS || vkCreateFence(r.device, &fence_info, nullptr, &r.fence) != VK_SUCCESS) { Destroy(&r); return kSetup; }
  VkSubmitInfo submit{VK_STRUCTURE_TYPE_SUBMIT_INFO}; submit.commandBufferCount = 1; submit.pCommandBuffers = &r.command;
  const VkResult submitted = vkQueueSubmit(queue_handle, 1, &submit, r.fence);
  if (submitted == VK_ERROR_DEVICE_LOST) return kDeviceLost;
  if (submitted != VK_SUCCESS) { std::fprintf(stderr, "SUBMIT error=%d completion=unproved\n", submitted); return kUncertain; }
  const VkResult waited = vkWaitForFences(r.device, 1, &r.fence, VK_TRUE, kFenceTimeoutNs);
  if (waited == VK_TIMEOUT) return kFenceTimeout;
  if (waited == VK_ERROR_DEVICE_LOST) return kDeviceLost;
  if (waited != VK_SUCCESS) { std::fprintf(stderr, "WAIT error=%d completion=unproved\n", waited); return kUncertain; }
  void* mapped = nullptr;
  if (vkMapMemory(r.device, r.readback_memory, 0, kReadbackBytes, 0, &mapped) != VK_SUCCESS) { Destroy(&r); return kReadback; }
  const Pixels pixels = CheckPixels(static_cast<const uint8_t*>(mapped), kReadbackBytes);
  vkUnmapMemory(r.device, r.readback_memory); Destroy(&r);
  if (!pixels.pass) { std::printf("READBACK verdict=FAIL reason=%s\n", pixels.reason.c_str()); return kReadback; }
  std::puts("READBACK verdict=PASS layer0=red layer1=green exterior=clear barrier=transfer_to_host");
  if (!hardware) { std::puts("CONTROL verdict=NOT_HARDWARE forced_software=1"); return kNotHardware; }
  std::puts("MULTIVIEW verdict=PASS hardware=1"); return kPass;
}

int Main(int argc, char** argv) {
  std::setvbuf(stdout, nullptr, _IOLBF, 0);
  bool allow_software = false;
  if (argc == 2 && std::strcmp(argv[1], "--self-test") == 0) return SelfTest();
  if (argc == 2 && std::strcmp(argv[1], "--allow-software-control") == 0) allow_software = true;
  else if (argc != 1) { std::fprintf(stderr, "usage: %s [--self-test|--allow-software-control]\n", argv[0]); return kUsage; }
  uint32_t instance_count = 0;
  if (vkEnumerateInstanceExtensionProperties(nullptr, &instance_count, nullptr) != VK_SUCCESS || instance_count > kMaxExtensions) return kSetup;
  std::vector<VkExtensionProperties> instance_extensions(instance_count);
  if (instance_count && vkEnumerateInstanceExtensionProperties(nullptr, &instance_count, instance_extensions.data()) != VK_SUCCESS) return kSetup;
  instance_extensions.resize(instance_count);
  if (!HasExtension(instance_extensions, VK_KHR_GET_PHYSICAL_DEVICE_PROPERTIES_2_EXTENSION_NAME)) return kUnsupported;
  const char* instance_extension = VK_KHR_GET_PHYSICAL_DEVICE_PROPERTIES_2_EXTENSION_NAME;
  VkApplicationInfo app{VK_STRUCTURE_TYPE_APPLICATION_INFO}; app.apiVersion = VK_API_VERSION_1_3;
  VkInstanceCreateInfo create{VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO}; create.pApplicationInfo = &app; create.enabledExtensionCount = 1; create.ppEnabledExtensionNames = &instance_extension;
  VkInstance instance = VK_NULL_HANDLE;
  if (vkCreateInstance(&create, nullptr, &instance) != VK_SUCCESS) return kSetup;
  const auto get_properties2 = reinterpret_cast<PFN_vkGetPhysicalDeviceProperties2KHR>(vkGetInstanceProcAddr(instance, "vkGetPhysicalDeviceProperties2KHR"));
  const auto get_features2 = reinterpret_cast<PFN_vkGetPhysicalDeviceFeatures2KHR>(vkGetInstanceProcAddr(instance, "vkGetPhysicalDeviceFeatures2KHR"));
  if (!get_properties2 || !get_features2) { vkDestroyInstance(instance, nullptr); return kUnsupported; }
  uint32_t count = 0;
  if (vkEnumeratePhysicalDevices(instance, &count, nullptr) != VK_SUCCESS || count == 0 || count > kMaxPhysicalDevices) { vkDestroyInstance(instance, nullptr); return kNoDevice; }
  std::vector<VkPhysicalDevice> physicals(count);
  if (vkEnumeratePhysicalDevices(instance, &count, physicals.data()) != VK_SUCCESS) { vkDestroyInstance(instance, nullptr); return kNoDevice; }
  for (uint32_t index = 0; index < count; ++index) {
    const Identity identity = ReadIdentity(physicals[index], get_properties2);
    const std::string reject = IdentityRejection(identity);
    std::printf("IDENTITY index=%u vendor=0x%x device=0x%x type=%d driver_id=%d driver_name=%s name=%s api=%u.%u.%u max_views=%u\n",
                index, identity.properties.vendorID, identity.properties.deviceID,
                identity.properties.deviceType, identity.driver_id, identity.driver_name.c_str(),
                identity.properties.deviceName, VK_API_VERSION_MAJOR(identity.properties.apiVersion),
                VK_API_VERSION_MINOR(identity.properties.apiVersion), VK_API_VERSION_PATCH(identity.properties.apiVersion),
                identity.max_multiview_views);
    if (!allow_software && !reject.empty()) { std::printf("IDENTITY REJECTED index=%u reason=%s\n", index, reject.c_str()); continue; }
    if (allow_software && (identity.properties.deviceType != VK_PHYSICAL_DEVICE_TYPE_CPU || identity.properties.apiVersion < VK_API_VERSION_1_3)) { std::printf("CONTROL REJECTED index=%u reason=not_vulkan13_cpu\n", index); continue; }
    const Features features = ReadFeatures(physicals[index], get_features2);
    std::printf("DEVICE index=%u name=%s api=%u.%u.%u multiview=%u\n", index, identity.properties.deviceName, VK_API_VERSION_MAJOR(identity.properties.apiVersion), VK_API_VERSION_MINOR(identity.properties.apiVersion), VK_API_VERSION_PATCH(identity.properties.apiVersion), features.multiview ? 1 : 0);
    if (identity.max_multiview_views < kLayers || !MultiviewUsable(identity.properties.apiVersion, features.extension, features.multiview)) { std::printf("FEATURE REJECTED index=%u reason=multiview_unavailable\n", index); continue; }
    uint32_t queues = 0; vkGetPhysicalDeviceQueueFamilyProperties(physicals[index], &queues, nullptr);
    if (!queues || queues > kMaxQueueFamilies) continue;
    std::vector<VkQueueFamilyProperties> properties(queues); vkGetPhysicalDeviceQueueFamilyProperties(physicals[index], &queues, properties.data());
    for (uint32_t family = 0; family < queues; ++family) if (properties[family].queueCount && (properties[family].queueFlags & VK_QUEUE_GRAPHICS_BIT)) {
      const int result = Render(physicals[index], family, false, !allow_software && reject.empty());
      if (result == kFenceTimeout || result == kDeviceLost || result == kUncertain) { std::fprintf(stderr, "UNCERTAIN completion=%d; intentionally retaining Vulkan objects\n", result); return result; }
      vkDestroyInstance(instance, nullptr); return result;
    }
  }
  vkDestroyInstance(instance, nullptr);
  return allow_software ? kUnsupported : kIdentity;
}
}  // namespace

int main(int argc, char** argv) { return Main(argc, argv); }
