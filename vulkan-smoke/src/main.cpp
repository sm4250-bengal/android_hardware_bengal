#include <vulkan/vulkan.h>

#include <algorithm>
#include <cctype>
#include <cinttypes>
#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

#include "triangle_spv.h"

namespace {

constexpr uint32_t kWidth = 64;
constexpr uint32_t kHeight = 64;
constexpr VkDeviceSize kReadbackBytes = kWidth * kHeight * 4;
constexpr VkDeviceSize kAllocationCap = 16 * 1024 * 1024;
constexpr uint32_t kMaxPhysicalDevices = 16;
constexpr uint32_t kMaxInstanceExtensions = 256;
constexpr uint32_t kMaxDeviceExtensions = 256;
constexpr uint32_t kMaxQueueFamilies = 32;
constexpr uint64_t kFenceTimeoutNs = 5'000'000'000ULL;
constexpr uint32_t kQualcommVendorId = 0x5143;
constexpr uint32_t kA610ChipId = 0x06010000;

enum ExitCode : int {
    kPass = 0,
    kUsage = 2,
    kNotHardwareControl = 3,
    kInstanceFailure = 10,
    kNoDevice = 20,
    kIdentityRejected = 21,
    kDeviceSetupFailure = 30,
    kSubmitFailure = 40,
    kFenceTimeout = 41,
    kDeviceLost = 42,
    kReadbackFailure = 50,
    kSelfTestFailure = 60,
};

struct Identity {
    VkPhysicalDeviceProperties properties{};
    bool driver_properties_available = false;
    VkDriverId driver_id = static_cast<VkDriverId>(0);
    char driver_name[VK_MAX_DRIVER_NAME_SIZE]{};
    char driver_info[VK_MAX_DRIVER_INFO_SIZE]{};
};

struct PixelVerdict {
    bool pass = false;
    uint8_t inside[4]{};
    uint8_t outside[4]{};
    uint64_t fnv1a = 0;
};

struct HostMapPlan {
    VkDeviceSize map_size = 0;
    VkDeviceSize invalidate_size = 0;
};

struct RenderResources {
    VkDevice device = VK_NULL_HANDLE;
    VkImage image = VK_NULL_HANDLE;
    VkDeviceMemory image_memory = VK_NULL_HANDLE;
    VkBuffer buffer = VK_NULL_HANDLE;
    VkDeviceMemory buffer_memory = VK_NULL_HANDLE;
    VkImageView image_view = VK_NULL_HANDLE;
    VkRenderPass render_pass = VK_NULL_HANDLE;
    VkFramebuffer framebuffer = VK_NULL_HANDLE;
    VkShaderModule vert = VK_NULL_HANDLE;
    VkShaderModule frag = VK_NULL_HANDLE;
    VkPipelineLayout pipeline_layout = VK_NULL_HANDLE;
    VkPipeline pipeline = VK_NULL_HANDLE;
    VkCommandPool command_pool = VK_NULL_HANDLE;
    VkFence fence = VK_NULL_HANDLE;
};

const char* VkResultName(VkResult result) {
    switch (result) {
        case VK_SUCCESS: return "VK_SUCCESS";
        case VK_NOT_READY: return "VK_NOT_READY";
        case VK_TIMEOUT: return "VK_TIMEOUT";
        case VK_EVENT_SET: return "VK_EVENT_SET";
        case VK_EVENT_RESET: return "VK_EVENT_RESET";
        case VK_INCOMPLETE: return "VK_INCOMPLETE";
        case VK_ERROR_OUT_OF_HOST_MEMORY: return "VK_ERROR_OUT_OF_HOST_MEMORY";
        case VK_ERROR_OUT_OF_DEVICE_MEMORY: return "VK_ERROR_OUT_OF_DEVICE_MEMORY";
        case VK_ERROR_INITIALIZATION_FAILED: return "VK_ERROR_INITIALIZATION_FAILED";
        case VK_ERROR_DEVICE_LOST: return "VK_ERROR_DEVICE_LOST";
        case VK_ERROR_MEMORY_MAP_FAILED: return "VK_ERROR_MEMORY_MAP_FAILED";
        case VK_ERROR_LAYER_NOT_PRESENT: return "VK_ERROR_LAYER_NOT_PRESENT";
        case VK_ERROR_EXTENSION_NOT_PRESENT: return "VK_ERROR_EXTENSION_NOT_PRESENT";
        case VK_ERROR_FEATURE_NOT_PRESENT: return "VK_ERROR_FEATURE_NOT_PRESENT";
        case VK_ERROR_INCOMPATIBLE_DRIVER: return "VK_ERROR_INCOMPATIBLE_DRIVER";
        case VK_ERROR_TOO_MANY_OBJECTS: return "VK_ERROR_TOO_MANY_OBJECTS";
        case VK_ERROR_FORMAT_NOT_SUPPORTED: return "VK_ERROR_FORMAT_NOT_SUPPORTED";
        default: return "VK_RESULT_OTHER";
    }
}

std::string Lower(const char* value) {
    std::string result(value == nullptr ? "" : value);
    std::transform(result.begin(), result.end(), result.begin(),
                   [](unsigned char c) { return static_cast<char>(std::tolower(c)); });
    return result;
}

bool Contains(const char* value, const char* needle) {
    return Lower(value).find(Lower(needle)) != std::string::npos;
}

const char* RejectReason(const Identity& identity, bool mesa26 = false) {
    const auto& p = identity.properties;
    if (p.deviceType == VK_PHYSICAL_DEVICE_TYPE_CPU ||
        Contains(p.deviceName, "swiftshader") ||
        (identity.driver_properties_available &&
         identity.driver_id == VK_DRIVER_ID_GOOGLE_SWIFTSHADER)) {
        return "software_device";
    }
    if (p.deviceType != VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU) {
        return "not_integrated_gpu";
    }
    if (p.vendorID != kQualcommVendorId) {
        return "vendor_not_qualcomm_0x5143";
    }
    if (p.deviceID != kA610ChipId || !Contains(p.deviceName, "adreno") ||
        !Contains(p.deviceName, "610")) {
        return "device_not_adreno_a610_0x06010000";
    }
    if (mesa26 && (VK_VERSION_MAJOR(p.apiVersion) != 1 || VK_VERSION_MINOR(p.apiVersion) < 3)) {
        return "mesa26_profile_requires_vulkan_1_3";
    }
    if (!mesa26 && (VK_VERSION_MAJOR(p.apiVersion) != 1 || VK_VERSION_MINOR(p.apiVersion) != 0)) {
        return "physical_api_not_truthful_a610_vulkan_1_0";
    }
    if (!identity.driver_properties_available) {
        return "driver_properties_unavailable";
    }
    if (identity.driver_id != VK_DRIVER_ID_MESA_TURNIP ||
        !Contains(identity.driver_name, "turnip")) {
        return "driver_not_mesa_turnip";
    }
    return nullptr;
}

PixelVerdict CheckPixels(const uint8_t* pixels, size_t size) {
    PixelVerdict result;
    if (pixels == nullptr || size != static_cast<size_t>(kReadbackBytes)) {
        return result;
    }
    uint64_t hash = 14695981039346656037ULL;
    for (size_t i = 0; i < size; ++i) {
        hash ^= pixels[i];
        hash *= 1099511628211ULL;
    }
    result.fnv1a = hash;
    const size_t inside_offset = (32 * kWidth + 32) * 4;
    const size_t outside_offset = (2 * kWidth + 2) * 4;
    std::memcpy(result.inside, pixels + inside_offset, 4);
    std::memcpy(result.outside, pixels + outside_offset, 4);
    const bool inside_green = result.inside[0] <= 8 && result.inside[1] >= 240 &&
                              result.inside[2] <= 8 && result.inside[3] >= 240;
    const bool outside_black = result.outside[0] <= 8 && result.outside[1] <= 8 &&
                               result.outside[2] <= 8 && result.outside[3] >= 240;
    result.pass = inside_green && outside_black;
    return result;
}

HostMapPlan MakeHostMapPlan(VkDeviceSize allocation_size, bool coherent) {
    if (allocation_size < kReadbackBytes) return {};
    return {VK_WHOLE_SIZE, coherent ? 0 : VK_WHOLE_SIZE};
}

bool HasExtension(const std::vector<VkExtensionProperties>& extensions, const char* name) {
    return std::any_of(extensions.begin(), extensions.end(), [name](const auto& extension) {
        return std::strcmp(extension.extensionName, name) == 0;
    });
}

bool EnumerateInstanceExtensions(std::vector<VkExtensionProperties>* extensions) {
    uint32_t count = 0;
    VkResult result = vkEnumerateInstanceExtensionProperties(nullptr, &count, nullptr);
    if (result != VK_SUCCESS) {
        std::printf("INSTANCE_EXTENSIONS verdict=FAIL vk=%d name=%s\n", result,
                    VkResultName(result));
        return false;
    }
    if (count > kMaxInstanceExtensions) {
        std::printf("INSTANCE_EXTENSIONS verdict=FAIL reason=count_cap count=%u cap=%u\n",
                    count, kMaxInstanceExtensions);
        return false;
    }
    extensions->resize(count);
    result = count == 0 ? VK_SUCCESS
                        : vkEnumerateInstanceExtensionProperties(nullptr, &count,
                                                                  extensions->data());
    if (result != VK_SUCCESS) {
        std::printf("INSTANCE_EXTENSIONS verdict=FAIL vk=%d name=%s\n", result,
                    VkResultName(result));
        return false;
    }
    extensions->resize(count);
    std::printf("INSTANCE_EXTENSIONS verdict=PASS count=%u cap=%u\n", count,
                kMaxInstanceExtensions);
    return true;
}

bool EnumerateDeviceExtensions(VkPhysicalDevice physical,
                               std::vector<VkExtensionProperties>* extensions,
                               uint32_t device_index) {
    uint32_t count = 0;
    VkResult result = vkEnumerateDeviceExtensionProperties(physical, nullptr, &count, nullptr);
    if (result != VK_SUCCESS) {
        std::printf("DEVICE_EXTENSIONS device=%u verdict=FAIL vk=%d name=%s\n",
                    device_index, result, VkResultName(result));
        return false;
    }
    if (count > kMaxDeviceExtensions) {
        std::printf("DEVICE_EXTENSIONS device=%u verdict=FAIL reason=count_cap count=%u cap=%u\n",
                    device_index, count, kMaxDeviceExtensions);
        return false;
    }
    extensions->resize(count);
    result = count == 0 ? VK_SUCCESS
                        : vkEnumerateDeviceExtensionProperties(physical, nullptr, &count,
                                                                extensions->data());
    if (result != VK_SUCCESS) {
        std::printf("DEVICE_EXTENSIONS device=%u verdict=FAIL vk=%d name=%s\n",
                    device_index, result, VkResultName(result));
        return false;
    }
    extensions->resize(count);
    std::printf("DEVICE_EXTENSIONS device=%u verdict=PASS count=%u cap=%u\n",
                device_index, count, kMaxDeviceExtensions);
    for (const auto& extension : *extensions) {
        std::printf("DEVICE_EXTENSION device=%u name=%s spec=%u\n", device_index,
                    extension.extensionName, extension.specVersion);
    }
    return true;
}

void PrintVersion(const char* key, uint32_t version) {
    std::printf("%s_raw=%u %s=%u.%u.%u", key, version, key,
                VK_VERSION_MAJOR(version), VK_VERSION_MINOR(version),
                VK_VERSION_PATCH(version));
}

bool PrintQueues(VkPhysicalDevice physical, uint32_t device_index,
                 uint32_t* first_graphics_family) {
    uint32_t count = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(physical, &count, nullptr);
    if (count == 0 || count > kMaxQueueFamilies) {
        std::printf("QUEUES device=%u verdict=FAIL count=%u cap=%u\n", device_index,
                    count, kMaxQueueFamilies);
        return false;
    }
    std::vector<VkQueueFamilyProperties> queues(count);
    vkGetPhysicalDeviceQueueFamilyProperties(physical, &count, queues.data());
    if (count > queues.size()) {
        std::printf("QUEUES device=%u verdict=FAIL reason=count_changed count=%u cap=%zu\n",
                    device_index, count, queues.size());
        return false;
    }
    queues.resize(count);
    *first_graphics_family = UINT32_MAX;
    uint64_t total_queues = 0;
    for (uint32_t i = 0; i < count; ++i) {
        total_queues += queues[i].queueCount;
        std::printf("QUEUE_FAMILY device=%u index=%u queueCount=%u flags_raw=0x%08x "
                    "graphics=%u compute=%u transfer=%u sparse=%u\n",
                    device_index, i, queues[i].queueCount, queues[i].queueFlags,
                    !!(queues[i].queueFlags & VK_QUEUE_GRAPHICS_BIT),
                    !!(queues[i].queueFlags & VK_QUEUE_COMPUTE_BIT),
                    !!(queues[i].queueFlags & VK_QUEUE_TRANSFER_BIT),
                    !!(queues[i].queueFlags & VK_QUEUE_SPARSE_BINDING_BIT));
        if (*first_graphics_family == UINT32_MAX && queues[i].queueCount > 0 &&
            (queues[i].queueFlags & VK_QUEUE_GRAPHICS_BIT)) {
            *first_graphics_family = i;
        }
    }
    std::printf("QUEUES device=%u verdict=%s familyCount=%u totalQueueCount=%" PRIu64 "\n",
                device_index, *first_graphics_family == UINT32_MAX ? "FAIL" : "PASS",
                count, total_queues);
    return *first_graphics_family != UINT32_MAX;
}

Identity ReadIdentity(VkInstance instance, VkPhysicalDevice physical,
                      const std::vector<VkExtensionProperties>& device_extensions,
                      bool properties2_enabled) {
    Identity identity;
    vkGetPhysicalDeviceProperties(physical, &identity.properties);
    if (!properties2_enabled ||
        !HasExtension(device_extensions, VK_KHR_DRIVER_PROPERTIES_EXTENSION_NAME)) {
        return identity;
    }
    auto get_properties2 = reinterpret_cast<PFN_vkGetPhysicalDeviceProperties2KHR>(
        vkGetInstanceProcAddr(instance, "vkGetPhysicalDeviceProperties2KHR"));
    if (get_properties2 == nullptr) {
        return identity;
    }
    VkPhysicalDeviceDriverPropertiesKHR driver{};
    driver.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES_KHR;
    VkPhysicalDeviceProperties2KHR properties2{};
    properties2.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2_KHR;
    properties2.pNext = &driver;
    get_properties2(physical, &properties2);
    identity.properties = properties2.properties;
    identity.driver_properties_available = true;
    identity.driver_id = driver.driverID;
    std::snprintf(identity.driver_name, sizeof(identity.driver_name), "%s", driver.driverName);
    std::snprintf(identity.driver_info, sizeof(identity.driver_info), "%s", driver.driverInfo);
    return identity;
}

void PrintIdentity(uint32_t index, const Identity& identity) {
    const auto& p = identity.properties;
    std::printf("DEVICE index=%u ", index);
    PrintVersion("api", p.apiVersion);
    std::printf(" driverVersion_raw=%u vendorID=0x%08x deviceID=0x%08x "
                "deviceType_raw=%u name=\"%s\"\n",
                p.driverVersion, p.vendorID, p.deviceID, p.deviceType, p.deviceName);
    if (identity.driver_properties_available) {
        std::printf("DRIVER_PROPERTIES device=%u available=1 driverID_raw=%u "
                    "driverName=\"%s\" driverInfo=\"%s\"\n",
                    index, static_cast<uint32_t>(identity.driver_id), identity.driver_name,
                    identity.driver_info);
    } else {
        std::printf("DRIVER_PROPERTIES device=%u available=0 driverID_raw=NA\n", index);
    }
}

void PrintFeatures(VkPhysicalDevice physical, uint32_t index) {
    VkPhysicalDeviceFeatures f{};
    vkGetPhysicalDeviceFeatures(physical, &f);
    std::printf("FEATURES device=%u robustBufferAccess=%u fullDrawIndexUint32=%u "
                "imageCubeArray=%u independentBlend=%u geometryShader=%u "
                "tessellationShader=%u multiDrawIndirect=%u multiViewport=%u "
                "samplerAnisotropy=%u shaderInt64=%u sparseBinding=%u\n",
                index, f.robustBufferAccess, f.fullDrawIndexUint32, f.imageCubeArray,
                f.independentBlend, f.geometryShader, f.tessellationShader,
                f.multiDrawIndirect, f.multiViewport, f.samplerAnisotropy,
                f.shaderInt64, f.sparseBinding);
}

bool FindMemoryType(const VkPhysicalDeviceMemoryProperties& memory_properties,
                    uint32_t compatible_bits, VkMemoryPropertyFlags required,
                    VkMemoryPropertyFlags preferred, uint32_t* index,
                    VkMemoryPropertyFlags* selected_flags) {
    for (int pass = 0; pass < 2; ++pass) {
        for (uint32_t i = 0; i < memory_properties.memoryTypeCount; ++i) {
            const auto flags = memory_properties.memoryTypes[i].propertyFlags;
            if (!(compatible_bits & (1u << i)) || (flags & required) != required) {
                continue;
            }
            if (pass == 0 && (flags & preferred) != preferred) {
                continue;
            }
            *index = i;
            *selected_flags = flags;
            return true;
        }
    }
    return false;
}

void Cleanup(RenderResources* r) {
    if (r->device == VK_NULL_HANDLE) return;
    if (r->fence != VK_NULL_HANDLE) vkDestroyFence(r->device, r->fence, nullptr);
    if (r->command_pool != VK_NULL_HANDLE) vkDestroyCommandPool(r->device, r->command_pool, nullptr);
    if (r->pipeline != VK_NULL_HANDLE) vkDestroyPipeline(r->device, r->pipeline, nullptr);
    if (r->pipeline_layout != VK_NULL_HANDLE) vkDestroyPipelineLayout(r->device, r->pipeline_layout, nullptr);
    if (r->vert != VK_NULL_HANDLE) vkDestroyShaderModule(r->device, r->vert, nullptr);
    if (r->frag != VK_NULL_HANDLE) vkDestroyShaderModule(r->device, r->frag, nullptr);
    if (r->framebuffer != VK_NULL_HANDLE) vkDestroyFramebuffer(r->device, r->framebuffer, nullptr);
    if (r->render_pass != VK_NULL_HANDLE) vkDestroyRenderPass(r->device, r->render_pass, nullptr);
    if (r->image_view != VK_NULL_HANDLE) vkDestroyImageView(r->device, r->image_view, nullptr);
    if (r->buffer != VK_NULL_HANDLE) vkDestroyBuffer(r->device, r->buffer, nullptr);
    if (r->buffer_memory != VK_NULL_HANDLE) vkFreeMemory(r->device, r->buffer_memory, nullptr);
    if (r->image != VK_NULL_HANDLE) vkDestroyImage(r->device, r->image, nullptr);
    if (r->image_memory != VK_NULL_HANDLE) vkFreeMemory(r->device, r->image_memory, nullptr);
    vkDestroyDevice(r->device, nullptr);
    *r = {};
}

using ResourceCleanupFn = void (*)(RenderResources*);
using InstanceDestroyFn = void (*)(VkInstance, const VkAllocationCallbacks*);

bool CleanupResourcesUnlessUnsafe(RenderResources* r, bool unsafe,
                                  const char* reason,
                                  ResourceCleanupFn cleanup = Cleanup) {
    if (unsafe) {
        std::printf("CLEANUP verdict=SKIPPED_UNSAFE reason=%s "
                    "device_cleanup_called=0 no_queue_wait_idle=1\n", reason);
        return false;
    }
    cleanup(r);
    return true;
}

bool DestroyInstanceUnlessUnsafe(VkInstance instance, bool unsafe,
                                 InstanceDestroyFn destroy = vkDestroyInstance) {
    if (unsafe) {
        std::printf("INSTANCE_CLEANUP verdict=SKIPPED_UNSAFE "
                    "instance_destroy_called=0 live_device_may_remain=1\n");
        return false;
    }
    destroy(instance, nullptr);
    return true;
}

int HandleIndeterminateWorkFailure(RenderResources* r, VkResult result,
                                   bool* unsafe_teardown,
                                   ResourceCleanupFn cleanup = Cleanup) {
    *unsafe_teardown = true;
    const char* reason = result == VK_TIMEOUT ? "fence_timeout"
                                              : (result == VK_ERROR_DEVICE_LOST
                                                     ? "device_lost"
                                                     : "fence_wait_failure");
    CleanupResourcesUnlessUnsafe(r, true, reason, cleanup);
    if (result == VK_TIMEOUT) return kFenceTimeout;
    if (result == VK_ERROR_DEVICE_LOST) return kDeviceLost;
    return kSubmitFailure;
}

int SetupFailure(RenderResources* r, const char* operation, VkResult result,
                 bool* unsafe_teardown) {
    std::printf("SETUP verdict=FAIL operation=%s vk=%d name=%s\n", operation, result,
                VkResultName(result));
    if (result == VK_ERROR_DEVICE_LOST) {
        if (r->device != VK_NULL_HANDLE) {
            return HandleIndeterminateWorkFailure(r, result, unsafe_teardown);
        }
        return kDeviceLost;
    }
    CleanupResourcesUnlessUnsafe(r, false, "safe");
    return kDeviceSetupFailure;
}

int RenderTriangle(VkPhysicalDevice physical, uint32_t queue_family,
                   bool hardware_accepted, bool* unsafe_teardown) {
    *unsafe_teardown = false;
    RenderResources r;
    const float priority = 1.0f;
    VkDeviceQueueCreateInfo queue_info{};
    queue_info.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO;
    queue_info.queueFamilyIndex = queue_family;
    queue_info.queueCount = 1;
    queue_info.pQueuePriorities = &priority;
    VkDeviceCreateInfo device_info{};
    device_info.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO;
    device_info.queueCreateInfoCount = 1;
    device_info.pQueueCreateInfos = &queue_info;
    device_info.enabledExtensionCount = 0;
    device_info.pEnabledFeatures = nullptr;
    VkResult result = vkCreateDevice(physical, &device_info, nullptr, &r.device);
    if (result != VK_SUCCESS) return SetupFailure(&r, "vkCreateDevice", result, unsafe_teardown);
    std::printf("DEVICE_CREATE verdict=PASS requestedQueueCount=1 enabledFeatureCount=0 "
                "enabledExtensionCount=0\n");

    VkQueue queue = VK_NULL_HANDLE;
    vkGetDeviceQueue(r.device, queue_family, 0, &queue);
    if (queue == VK_NULL_HANDLE) return SetupFailure(&r, "vkGetDeviceQueue", VK_ERROR_INITIALIZATION_FAILED, unsafe_teardown);

    VkFormatProperties format_properties{};
    vkGetPhysicalDeviceFormatProperties(physical, VK_FORMAT_R8G8B8A8_UNORM, &format_properties);
    constexpr VkFormatFeatureFlags required_format =
        VK_FORMAT_FEATURE_COLOR_ATTACHMENT_BIT | VK_FORMAT_FEATURE_TRANSFER_SRC_BIT;
    if ((format_properties.optimalTilingFeatures & required_format) != required_format) {
        return SetupFailure(&r, "RGBA8_format_features", VK_ERROR_FORMAT_NOT_SUPPORTED, unsafe_teardown);
    }

    VkPhysicalDeviceMemoryProperties memory_properties{};
    vkGetPhysicalDeviceMemoryProperties(physical, &memory_properties);

    VkImageCreateInfo image_info{};
    image_info.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO;
    image_info.imageType = VK_IMAGE_TYPE_2D;
    image_info.format = VK_FORMAT_R8G8B8A8_UNORM;
    image_info.extent = {kWidth, kHeight, 1};
    image_info.mipLevels = 1;
    image_info.arrayLayers = 1;
    image_info.samples = VK_SAMPLE_COUNT_1_BIT;
    image_info.tiling = VK_IMAGE_TILING_OPTIMAL;
    image_info.usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT;
    image_info.sharingMode = VK_SHARING_MODE_EXCLUSIVE;
    image_info.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED;
    result = vkCreateImage(r.device, &image_info, nullptr, &r.image);
    if (result != VK_SUCCESS) return SetupFailure(&r, "vkCreateImage", result, unsafe_teardown);
    VkMemoryRequirements image_requirements{};
    vkGetImageMemoryRequirements(r.device, r.image, &image_requirements);
    if (image_requirements.size > kAllocationCap) {
        std::printf("ALLOCATION verdict=FAIL object=image bytes=%" PRIu64 " cap=%" PRIu64 "\n",
                    static_cast<uint64_t>(image_requirements.size),
                    static_cast<uint64_t>(kAllocationCap));
        Cleanup(&r);
        return kDeviceSetupFailure;
    }
    uint32_t image_memory_type = 0;
    VkMemoryPropertyFlags image_memory_flags = 0;
    if (!FindMemoryType(memory_properties, image_requirements.memoryTypeBits, 0,
                        VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, &image_memory_type,
                        &image_memory_flags)) {
        return SetupFailure(&r, "find_image_memory_type", VK_ERROR_FEATURE_NOT_PRESENT, unsafe_teardown);
    }
    VkMemoryAllocateInfo image_allocate{};
    image_allocate.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO;
    image_allocate.allocationSize = image_requirements.size;
    image_allocate.memoryTypeIndex = image_memory_type;
    result = vkAllocateMemory(r.device, &image_allocate, nullptr, &r.image_memory);
    if (result != VK_SUCCESS) return SetupFailure(&r, "vkAllocateMemory(image)", result, unsafe_teardown);
    result = vkBindImageMemory(r.device, r.image, r.image_memory, 0);
    if (result != VK_SUCCESS) return SetupFailure(&r, "vkBindImageMemory", result, unsafe_teardown);

    VkBufferCreateInfo buffer_info{};
    buffer_info.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO;
    buffer_info.size = kReadbackBytes;
    buffer_info.usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT;
    buffer_info.sharingMode = VK_SHARING_MODE_EXCLUSIVE;
    result = vkCreateBuffer(r.device, &buffer_info, nullptr, &r.buffer);
    if (result != VK_SUCCESS) return SetupFailure(&r, "vkCreateBuffer", result, unsafe_teardown);
    VkMemoryRequirements buffer_requirements{};
    vkGetBufferMemoryRequirements(r.device, r.buffer, &buffer_requirements);
    if (buffer_requirements.size > kAllocationCap) {
        std::printf("ALLOCATION verdict=FAIL object=readback bytes=%" PRIu64 " cap=%" PRIu64 "\n",
                    static_cast<uint64_t>(buffer_requirements.size),
                    static_cast<uint64_t>(kAllocationCap));
        Cleanup(&r);
        return kDeviceSetupFailure;
    }
    uint32_t buffer_memory_type = 0;
    VkMemoryPropertyFlags buffer_memory_flags = 0;
    if (!FindMemoryType(memory_properties, buffer_requirements.memoryTypeBits,
                        VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT,
                        VK_MEMORY_PROPERTY_HOST_COHERENT_BIT, &buffer_memory_type,
                        &buffer_memory_flags)) {
        return SetupFailure(&r, "find_readback_memory_type", VK_ERROR_FEATURE_NOT_PRESENT, unsafe_teardown);
    }
    VkMemoryAllocateInfo buffer_allocate{};
    buffer_allocate.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO;
    buffer_allocate.allocationSize = buffer_requirements.size;
    buffer_allocate.memoryTypeIndex = buffer_memory_type;
    result = vkAllocateMemory(r.device, &buffer_allocate, nullptr, &r.buffer_memory);
    if (result != VK_SUCCESS) return SetupFailure(&r, "vkAllocateMemory(readback)", result, unsafe_teardown);
    result = vkBindBufferMemory(r.device, r.buffer, r.buffer_memory, 0);
    if (result != VK_SUCCESS) return SetupFailure(&r, "vkBindBufferMemory", result, unsafe_teardown);
    std::printf("ALLOCATIONS verdict=PASS count=2 imageBytes=%" PRIu64
                " readbackBytes=%" PRIu64 " perAllocationCap=%" PRIu64
                " imageMemoryFlags=0x%08x readbackMemoryFlags=0x%08x\n",
                static_cast<uint64_t>(image_requirements.size),
                static_cast<uint64_t>(buffer_requirements.size),
                static_cast<uint64_t>(kAllocationCap), image_memory_flags,
                buffer_memory_flags);

    VkImageViewCreateInfo view_info{};
    view_info.sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO;
    view_info.image = r.image;
    view_info.viewType = VK_IMAGE_VIEW_TYPE_2D;
    view_info.format = VK_FORMAT_R8G8B8A8_UNORM;
    view_info.subresourceRange.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT;
    view_info.subresourceRange.levelCount = 1;
    view_info.subresourceRange.layerCount = 1;
    result = vkCreateImageView(r.device, &view_info, nullptr, &r.image_view);
    if (result != VK_SUCCESS) return SetupFailure(&r, "vkCreateImageView", result, unsafe_teardown);

    VkAttachmentDescription attachment{};
    attachment.format = VK_FORMAT_R8G8B8A8_UNORM;
    attachment.samples = VK_SAMPLE_COUNT_1_BIT;
    attachment.loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR;
    attachment.storeOp = VK_ATTACHMENT_STORE_OP_STORE;
    attachment.stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE;
    attachment.stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE;
    attachment.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED;
    attachment.finalLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
    VkAttachmentReference color_reference{0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL};
    VkSubpassDescription subpass{};
    subpass.pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS;
    subpass.colorAttachmentCount = 1;
    subpass.pColorAttachments = &color_reference;
    VkSubpassDependency dependencies[2]{};
    dependencies[0].srcSubpass = VK_SUBPASS_EXTERNAL;
    dependencies[0].dstSubpass = 0;
    dependencies[0].srcStageMask = VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT;
    dependencies[0].dstStageMask = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
    dependencies[0].dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT;
    dependencies[1].srcSubpass = 0;
    dependencies[1].dstSubpass = VK_SUBPASS_EXTERNAL;
    dependencies[1].srcStageMask = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
    dependencies[1].dstStageMask = VK_PIPELINE_STAGE_TRANSFER_BIT;
    dependencies[1].srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT;
    dependencies[1].dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
    VkRenderPassCreateInfo render_pass_info{};
    render_pass_info.sType = VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO;
    render_pass_info.attachmentCount = 1;
    render_pass_info.pAttachments = &attachment;
    render_pass_info.subpassCount = 1;
    render_pass_info.pSubpasses = &subpass;
    render_pass_info.dependencyCount = 2;
    render_pass_info.pDependencies = dependencies;
    result = vkCreateRenderPass(r.device, &render_pass_info, nullptr, &r.render_pass);
    if (result != VK_SUCCESS) return SetupFailure(&r, "vkCreateRenderPass", result, unsafe_teardown);

    VkFramebufferCreateInfo framebuffer_info{};
    framebuffer_info.sType = VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO;
    framebuffer_info.renderPass = r.render_pass;
    framebuffer_info.attachmentCount = 1;
    framebuffer_info.pAttachments = &r.image_view;
    framebuffer_info.width = kWidth;
    framebuffer_info.height = kHeight;
    framebuffer_info.layers = 1;
    result = vkCreateFramebuffer(r.device, &framebuffer_info, nullptr, &r.framebuffer);
    if (result != VK_SUCCESS) return SetupFailure(&r, "vkCreateFramebuffer", result, unsafe_teardown);

    VkShaderModuleCreateInfo shader_info{};
    shader_info.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO;
    shader_info.codeSize = kTriangleVertSpvBytes;
    shader_info.pCode = kTriangleVertSpv;
    result = vkCreateShaderModule(r.device, &shader_info, nullptr, &r.vert);
    if (result != VK_SUCCESS) return SetupFailure(&r, "vkCreateShaderModule(vertex)", result, unsafe_teardown);
    shader_info.codeSize = kTriangleFragSpvBytes;
    shader_info.pCode = kTriangleFragSpv;
    result = vkCreateShaderModule(r.device, &shader_info, nullptr, &r.frag);
    if (result != VK_SUCCESS) return SetupFailure(&r, "vkCreateShaderModule(fragment)", result, unsafe_teardown);

    VkPipelineLayoutCreateInfo layout_info{};
    layout_info.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO;
    result = vkCreatePipelineLayout(r.device, &layout_info, nullptr, &r.pipeline_layout);
    if (result != VK_SUCCESS) return SetupFailure(&r, "vkCreatePipelineLayout", result, unsafe_teardown);

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
    vertex_input.sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO;
    VkPipelineInputAssemblyStateCreateInfo input_assembly{};
    input_assembly.sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO;
    input_assembly.topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST;
    VkViewport viewport{0.0f, 0.0f, static_cast<float>(kWidth),
                        static_cast<float>(kHeight), 0.0f, 1.0f};
    VkRect2D scissor{{0, 0}, {kWidth, kHeight}};
    VkPipelineViewportStateCreateInfo viewport_state{};
    viewport_state.sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO;
    viewport_state.viewportCount = 1;
    viewport_state.pViewports = &viewport;
    viewport_state.scissorCount = 1;
    viewport_state.pScissors = &scissor;
    VkPipelineRasterizationStateCreateInfo raster{};
    raster.sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO;
    raster.polygonMode = VK_POLYGON_MODE_FILL;
    raster.cullMode = VK_CULL_MODE_NONE;
    raster.frontFace = VK_FRONT_FACE_COUNTER_CLOCKWISE;
    raster.lineWidth = 1.0f;
    VkPipelineMultisampleStateCreateInfo multisample{};
    multisample.sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO;
    multisample.rasterizationSamples = VK_SAMPLE_COUNT_1_BIT;
    VkPipelineColorBlendAttachmentState blend_attachment{};
    blend_attachment.colorWriteMask = VK_COLOR_COMPONENT_R_BIT | VK_COLOR_COMPONENT_G_BIT |
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
    pipeline_info.pInputAssemblyState = &input_assembly;
    pipeline_info.pViewportState = &viewport_state;
    pipeline_info.pRasterizationState = &raster;
    pipeline_info.pMultisampleState = &multisample;
    pipeline_info.pColorBlendState = &blend;
    pipeline_info.layout = r.pipeline_layout;
    pipeline_info.renderPass = r.render_pass;
    pipeline_info.subpass = 0;
    result = vkCreateGraphicsPipelines(r.device, VK_NULL_HANDLE, 1, &pipeline_info,
                                       nullptr, &r.pipeline);
    if (result != VK_SUCCESS) return SetupFailure(&r, "vkCreateGraphicsPipelines", result, unsafe_teardown);

    VkCommandPoolCreateInfo pool_info{};
    pool_info.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO;
    pool_info.queueFamilyIndex = queue_family;
    result = vkCreateCommandPool(r.device, &pool_info, nullptr, &r.command_pool);
    if (result != VK_SUCCESS) return SetupFailure(&r, "vkCreateCommandPool", result, unsafe_teardown);
    VkCommandBuffer command_buffer = VK_NULL_HANDLE;
    VkCommandBufferAllocateInfo command_allocate{};
    command_allocate.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO;
    command_allocate.commandPool = r.command_pool;
    command_allocate.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
    command_allocate.commandBufferCount = 1;
    result = vkAllocateCommandBuffers(r.device, &command_allocate, &command_buffer);
    if (result != VK_SUCCESS) return SetupFailure(&r, "vkAllocateCommandBuffers", result, unsafe_teardown);
    VkCommandBufferBeginInfo begin_info{};
    begin_info.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO;
    begin_info.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT;
    result = vkBeginCommandBuffer(command_buffer, &begin_info);
    if (result != VK_SUCCESS) return SetupFailure(&r, "vkBeginCommandBuffer", result, unsafe_teardown);
    VkClearValue clear{};
    clear.color.float32[0] = 0.0f;
    clear.color.float32[1] = 0.0f;
    clear.color.float32[2] = 0.0f;
    clear.color.float32[3] = 1.0f;
    VkRenderPassBeginInfo render_begin{};
    render_begin.sType = VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO;
    render_begin.renderPass = r.render_pass;
    render_begin.framebuffer = r.framebuffer;
    render_begin.renderArea = {{0, 0}, {kWidth, kHeight}};
    render_begin.clearValueCount = 1;
    render_begin.pClearValues = &clear;
    vkCmdBeginRenderPass(command_buffer, &render_begin, VK_SUBPASS_CONTENTS_INLINE);
    vkCmdBindPipeline(command_buffer, VK_PIPELINE_BIND_POINT_GRAPHICS, r.pipeline);
    vkCmdDraw(command_buffer, 3, 1, 0, 0);
    vkCmdEndRenderPass(command_buffer);
    VkBufferImageCopy copy{};
    copy.imageSubresource.aspectMask = VK_IMAGE_ASPECT_COLOR_BIT;
    copy.imageSubresource.layerCount = 1;
    copy.imageExtent = {kWidth, kHeight, 1};
    vkCmdCopyImageToBuffer(command_buffer, r.image, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                           r.buffer, 1, &copy);
    VkBufferMemoryBarrier host_read_barrier{};
    host_read_barrier.sType = VK_STRUCTURE_TYPE_BUFFER_MEMORY_BARRIER;
    host_read_barrier.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
    host_read_barrier.dstAccessMask = VK_ACCESS_HOST_READ_BIT;
    host_read_barrier.srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
    host_read_barrier.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED;
    host_read_barrier.buffer = r.buffer;
    host_read_barrier.offset = 0;
    host_read_barrier.size = VK_WHOLE_SIZE;
    vkCmdPipelineBarrier(command_buffer, VK_PIPELINE_STAGE_TRANSFER_BIT,
                         VK_PIPELINE_STAGE_HOST_BIT, 0, 0, nullptr, 1,
                         &host_read_barrier, 0, nullptr);
    result = vkEndCommandBuffer(command_buffer);
    if (result != VK_SUCCESS) return SetupFailure(&r, "vkEndCommandBuffer", result, unsafe_teardown);

    VkFenceCreateInfo fence_info{};
    fence_info.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO;
    result = vkCreateFence(r.device, &fence_info, nullptr, &r.fence);
    if (result != VK_SUCCESS) return SetupFailure(&r, "vkCreateFence", result, unsafe_teardown);
    VkSubmitInfo submit{};
    submit.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO;
    submit.commandBufferCount = 1;
    submit.pCommandBuffers = &command_buffer;
    result = vkQueueSubmit(queue, 1, &submit, r.fence);
    if (result != VK_SUCCESS) {
        std::printf("SUBMIT verdict=FAIL vk=%d name=%s\n", result, VkResultName(result));
        if (result == VK_ERROR_DEVICE_LOST) {
            return HandleIndeterminateWorkFailure(&r, result, unsafe_teardown);
        }
        CleanupResourcesUnlessUnsafe(&r, false, "safe");
        return kSubmitFailure;
    }
    std::printf("SUBMIT verdict=PASS vk=0 name=VK_SUCCESS queueCount=1\n");
    result = vkWaitForFences(r.device, 1, &r.fence, VK_TRUE, kFenceTimeoutNs);
    if (result != VK_SUCCESS) {
        std::printf("FENCE verdict=FAIL vk=%d name=%s timeoutNs=%" PRIu64 "\n", result,
                    VkResultName(result), kFenceTimeoutNs);
        return HandleIndeterminateWorkFailure(&r, result, unsafe_teardown);
    }
    std::printf("FENCE verdict=PASS timeoutNs=%" PRIu64 "\n", kFenceTimeoutNs);

    const HostMapPlan map_plan = MakeHostMapPlan(
        buffer_requirements.size,
        !!(buffer_memory_flags & VK_MEMORY_PROPERTY_HOST_COHERENT_BIT));
    if (map_plan.map_size == 0) {
        return SetupFailure(&r, "host_map_plan", VK_ERROR_MEMORY_MAP_FAILED,
                            unsafe_teardown);
    }
    void* mapped = nullptr;
    result = vkMapMemory(r.device, r.buffer_memory, 0, map_plan.map_size, 0, &mapped);
    if (result != VK_SUCCESS) return SetupFailure(&r, "vkMapMemory", result, unsafe_teardown);
    if (map_plan.invalidate_size != 0) {
        VkMappedMemoryRange range{};
        range.sType = VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE;
        range.memory = r.buffer_memory;
        range.offset = 0;
        range.size = map_plan.invalidate_size;
        result = vkInvalidateMappedMemoryRanges(r.device, 1, &range);
        if (result != VK_SUCCESS) {
            vkUnmapMemory(r.device, r.buffer_memory);
            return SetupFailure(&r, "vkInvalidateMappedMemoryRanges", result, unsafe_teardown);
        }
    }
    const PixelVerdict pixels = CheckPixels(static_cast<const uint8_t*>(mapped),
                                             static_cast<size_t>(kReadbackBytes));
    vkUnmapMemory(r.device, r.buffer_memory);
    std::printf("READBACK verdict=%s bytes=%" PRIu64 " fnv1a64=0x%016" PRIx64
                " inside_xy=32,32 inside_rgba=%u,%u,%u,%u "
                "outside_xy=2,2 outside_rgba=%u,%u,%u,%u\n",
                pixels.pass ? "PASS" : "FAIL", static_cast<uint64_t>(kReadbackBytes),
                pixels.fnv1a, pixels.inside[0], pixels.inside[1], pixels.inside[2],
                pixels.inside[3], pixels.outside[0], pixels.outside[1],
                pixels.outside[2], pixels.outside[3]);
    Cleanup(&r);
    std::printf("CLEANUP verdict=PASS no_queue_wait_idle=1\n");
    if (!pixels.pass) return kReadbackFailure;
    std::printf("RESULT %s shader_render=PASS submit=PASS readback=PASS\n",
                hardware_accepted ? "HARDWARE_A610_PASS" : "NOT_HARDWARE");
    return hardware_accepted ? kPass : kNotHardwareControl;
}

int g_cleanup_stub_calls = 0;
int g_destroy_instance_stub_calls = 0;

void CountCleanupStub(RenderResources*) {
    ++g_cleanup_stub_calls;
}

void CountDestroyInstanceStub(VkInstance, const VkAllocationCallbacks*) {
    ++g_destroy_instance_stub_calls;
}

int SelfTest() {
    Identity a610;
    a610.properties.deviceType = VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU;
    a610.properties.apiVersion = VK_MAKE_VERSION(1, 0, 0);
    a610.properties.vendorID = kQualcommVendorId;
    a610.properties.deviceID = kA610ChipId;
    std::snprintf(a610.properties.deviceName, sizeof(a610.properties.deviceName),
                  "Turnip Adreno (TM) 610");
    a610.driver_properties_available = true;
    a610.driver_id = VK_DRIVER_ID_MESA_TURNIP;
    std::snprintf(a610.driver_name, sizeof(a610.driver_name), "turnip Mesa driver");
    bool pass = RejectReason(a610) == nullptr;

    Identity swiftshader = a610;
    swiftshader.properties.deviceType = VK_PHYSICAL_DEVICE_TYPE_CPU;
    std::snprintf(swiftshader.properties.deviceName,
                  sizeof(swiftshader.properties.deviceName), "SwiftShader Device");
    swiftshader.driver_id = VK_DRIVER_ID_GOOGLE_SWIFTSHADER;
    pass &= std::strcmp(RejectReason(swiftshader), "software_device") == 0;

    Identity wrong_chip = a610;
    wrong_chip.properties.deviceID = 0x06030000;
    pass &= std::strcmp(RejectReason(wrong_chip),
                        "device_not_adreno_a610_0x06010000") == 0;

    Identity inflated_api = a610;
    inflated_api.properties.apiVersion = VK_MAKE_VERSION(1, 3, 0);
    pass &= std::strcmp(RejectReason(inflated_api),
                        "physical_api_not_truthful_a610_vulkan_1_0") == 0;
    // The legacy Mesa25 contract stays exact 1.0; the accepted Mesa26
    // software-multiview carrier has its own explicit diagnostic profile.
    pass &= RejectReason(inflated_api, true) == nullptr;
    pass &= std::strcmp(RejectReason(a610, true),
                        "mesa26_profile_requires_vulkan_1_3") == 0;
    pass &= std::strcmp(RejectReason(swiftshader, true), "software_device") == 0;

    Identity wrong_driver = a610;
    wrong_driver.driver_id = VK_DRIVER_ID_MESA_LLVMPIPE;
    std::snprintf(wrong_driver.driver_name, sizeof(wrong_driver.driver_name), "llvmpipe");
    pass &= std::strcmp(RejectReason(wrong_driver), "driver_not_mesa_turnip") == 0;

    std::vector<uint8_t> pixels(static_cast<size_t>(kReadbackBytes), 0);
    for (size_t i = 3; i < pixels.size(); i += 4) pixels[i] = 255;
    const size_t center = (32 * kWidth + 32) * 4;
    pixels[center + 1] = 255;
    pass &= CheckPixels(pixels.data(), pixels.size()).pass;
    pixels[center + 1] = 0;
    pass &= !CheckPixels(pixels.data(), pixels.size()).pass;

    RenderResources fake_resources;
    g_cleanup_stub_calls = 0;
    bool timeout_unsafe = false;
    pass &= HandleIndeterminateWorkFailure(&fake_resources, VK_TIMEOUT,
                                           &timeout_unsafe,
                                           CountCleanupStub) == kFenceTimeout;
    pass &= timeout_unsafe;
    pass &= g_cleanup_stub_calls == 0;
    bool lost_unsafe = false;
    pass &= HandleIndeterminateWorkFailure(&fake_resources, VK_ERROR_DEVICE_LOST,
                                           &lost_unsafe,
                                           CountCleanupStub) == kDeviceLost;
    pass &= lost_unsafe;
    pass &= g_cleanup_stub_calls == 0;
    pass &= CleanupResourcesUnlessUnsafe(&fake_resources, false, "safe",
                                         CountCleanupStub);
    pass &= g_cleanup_stub_calls == 1;

    g_destroy_instance_stub_calls = 0;
    pass &= !DestroyInstanceUnlessUnsafe(VK_NULL_HANDLE, timeout_unsafe,
                                         CountDestroyInstanceStub);
    pass &= g_destroy_instance_stub_calls == 0;
    pass &= !DestroyInstanceUnlessUnsafe(VK_NULL_HANDLE, lost_unsafe,
                                         CountDestroyInstanceStub);
    pass &= g_destroy_instance_stub_calls == 0;
    pass &= DestroyInstanceUnlessUnsafe(VK_NULL_HANDLE, false,
                                        CountDestroyInstanceStub);
    pass &= g_destroy_instance_stub_calls == 1;

    const HostMapPlan padded_noncoherent =
        MakeHostMapPlan(kReadbackBytes + 4096, false);
    pass &= padded_noncoherent.map_size == VK_WHOLE_SIZE;
    pass &= padded_noncoherent.invalidate_size == VK_WHOLE_SIZE;

    std::printf("SELF_TEST verdict=%s cases=17 classification=8 pixel_checks=2 "
                "teardown=6 padded_noncoherent_map=1\n",
                pass ? "PASS" : "FAIL");
    return pass ? kPass : kSelfTestFailure;
}

void Usage(const char* argv0) {
    std::printf("usage: %s [--allow-software-control | --mesa26 | --self-test]\n", argv0);
}

}  // namespace

int main(int argc, char** argv) {
    std::setvbuf(stdout, nullptr, _IOLBF, 0);
    std::printf("OUTPUT buffering=line newline_flush=1\n");
    bool allow_software_control = false;
    bool mesa26 = false;
    if (argc == 2 && std::strcmp(argv[1], "--self-test") == 0) return SelfTest();
    if (argc == 2 && std::strcmp(argv[1], "--allow-software-control") == 0) {
        allow_software_control = true;
    } else if (argc == 2 && std::strcmp(argv[1], "--mesa26") == 0) {
        mesa26 = true;
    } else if (argc != 1) {
        Usage(argv[0]);
        return kUsage;
    }

    std::printf("PROFILE mesa26=%u (basic shader/readback only, not conformance)\n", mesa26);
    std::printf("MODE hardware_required=%u software_control=%u framebuffer=%ux%u "
                "format=VK_FORMAT_R8G8B8A8_UNORM fenceTimeoutNs=%" PRIu64 "\n",
                !allow_software_control, allow_software_control, kWidth, kHeight,
                kFenceTimeoutNs);
    std::printf("INSTANCE requested_api_raw=%u requested_api=1.0.0 layers=0\n",
                VK_API_VERSION_1_0);
    std::vector<VkExtensionProperties> instance_extensions;
    if (!EnumerateInstanceExtensions(&instance_extensions)) return kInstanceFailure;
    const bool has_properties2 =
        HasExtension(instance_extensions, VK_KHR_GET_PHYSICAL_DEVICE_PROPERTIES_2_EXTENSION_NAME);
    const char* enabled_instance_extension =
        has_properties2 ? VK_KHR_GET_PHYSICAL_DEVICE_PROPERTIES_2_EXTENSION_NAME : nullptr;
    VkApplicationInfo application{};
    application.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO;
    application.pApplicationName = "bengal-vulkan-offscreen-smoke";
    application.applicationVersion = 1;
    application.pEngineName = "none";
    application.engineVersion = 0;
    application.apiVersion = VK_API_VERSION_1_0;
    VkInstanceCreateInfo instance_info{};
    instance_info.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO;
    instance_info.pApplicationInfo = &application;
    instance_info.enabledExtensionCount = has_properties2 ? 1u : 0u;
    instance_info.ppEnabledExtensionNames = has_properties2 ? &enabled_instance_extension : nullptr;
    VkInstance instance = VK_NULL_HANDLE;
    VkResult result = vkCreateInstance(&instance_info, nullptr, &instance);
    if (result != VK_SUCCESS) {
        std::printf("INSTANCE_CREATE verdict=FAIL vk=%d name=%s\n", result,
                    VkResultName(result));
        return kInstanceFailure;
    }
    std::printf("INSTANCE_CREATE verdict=PASS properties2_extension=%u\n", has_properties2);

    uint32_t physical_count = 0;
    result = vkEnumeratePhysicalDevices(instance, &physical_count, nullptr);
    if (result != VK_SUCCESS || physical_count == 0) {
        std::printf("PHYSICAL_DEVICES verdict=FAIL vk=%d name=%s count=%u\n", result,
                    VkResultName(result), physical_count);
        vkDestroyInstance(instance, nullptr);
        return kNoDevice;
    }
    if (physical_count > kMaxPhysicalDevices) {
        std::printf("PHYSICAL_DEVICES verdict=FAIL reason=count_cap count=%u cap=%u\n",
                    physical_count, kMaxPhysicalDevices);
        vkDestroyInstance(instance, nullptr);
        return kNoDevice;
    }
    std::vector<VkPhysicalDevice> physical_devices(physical_count);
    result = vkEnumeratePhysicalDevices(instance, &physical_count, physical_devices.data());
    if (result != VK_SUCCESS) {
        std::printf("PHYSICAL_DEVICES verdict=FAIL vk=%d name=%s\n", result,
                    VkResultName(result));
        vkDestroyInstance(instance, nullptr);
        return kNoDevice;
    }
    physical_devices.resize(physical_count);
    std::printf("PHYSICAL_DEVICES verdict=PASS count=%u cap=%u\n", physical_count,
                kMaxPhysicalDevices);

    VkPhysicalDevice selected = VK_NULL_HANDLE;
    uint32_t selected_queue_family = UINT32_MAX;
    bool selected_is_hardware = false;
    for (uint32_t i = 0; i < physical_count; ++i) {
        std::vector<VkExtensionProperties> device_extensions;
        if (!EnumerateDeviceExtensions(physical_devices[i], &device_extensions, i)) continue;
        const Identity identity = ReadIdentity(instance, physical_devices[i], device_extensions,
                                               has_properties2);
        PrintIdentity(i, identity);
        PrintFeatures(physical_devices[i], i);
        uint32_t graphics_family = UINT32_MAX;
        const bool queues_ok = PrintQueues(physical_devices[i], i, &graphics_family);
        const char* rejection = RejectReason(identity, mesa26);
        std::printf("IDENTITY device=%u verdict=%s reason=%s\n", i,
                    rejection == nullptr ? "ACCEPTED_A610" : "REJECTED",
                    rejection == nullptr ? "exact_a610_turnip" : rejection);
        if (selected == VK_NULL_HANDLE && !allow_software_control &&
            rejection == nullptr && queues_ok) {
            selected = physical_devices[i];
            selected_queue_family = graphics_family;
            selected_is_hardware = true;
        }
        if (selected == VK_NULL_HANDLE && allow_software_control && queues_ok) {
            selected = physical_devices[i];
            selected_queue_family = graphics_family;
            selected_is_hardware = false;
        }
    }

    if (selected == VK_NULL_HANDLE) {
        std::printf("RESULT HARDWARE_A610_FAIL reason=no_accepted_a610_graphics_device\n");
        vkDestroyInstance(instance, nullptr);
        return kIdentityRejected;
    }
    if (!selected_is_hardware) {
        std::printf("CONTROL verdict=NOT_HARDWARE reason=explicit_software_control "
                    "hardware_pass_forbidden=1\n");
    }
    bool unsafe_teardown = false;
    const int render_result = RenderTriangle(selected, selected_queue_family,
                                             selected_is_hardware,
                                             &unsafe_teardown);
    DestroyInstanceUnlessUnsafe(instance, unsafe_teardown);
    return render_result;
}
