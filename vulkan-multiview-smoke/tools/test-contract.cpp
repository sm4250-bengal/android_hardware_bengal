// Exercise the real client entrypoint up to instance creation, without a GPU.
#define vkCreateInstance ContractCreateInstance
#define main SmokeProgramMain
#include "../src/main.cpp"
#undef main
#undef vkCreateInstance

static uint32_t requested_api = 0;
VKAPI_ATTR VkResult VKAPI_CALL ContractCreateInstance(
    const VkInstanceCreateInfo* info, const VkAllocationCallbacks*, VkInstance*) {
  requested_api = info->pApplicationInfo->apiVersion;
  return VK_ERROR_INITIALIZATION_FAILED;
}

int main() {
  char name[] = "contract";
  char* args[] = {name};
  const int result = Main(1, args);
  const bool api_ok = result == kSetup && requested_api >= VK_API_VERSION_1_3;
  std::vector<uint8_t> pixels(kReadbackBytes, 0);
  for (size_t i = 3; i < pixels.size(); i += 4) pixels[i] = 255;
  for (uint32_t layer = 0; layer < kLayers; ++layer) {
    pixels[layer * kLayerBytes + (32 * kWidth + 14) * 4] = 255;
    pixels[layer * kLayerBytes + (32 * kWidth + 50) * 4 + 1] = 255;
  }
  const bool crossed_layers_rejected = !CheckPixels(pixels.data(), pixels.size()).pass;
  std::printf("CONTRACT requested_api=%u.%u api_ok=%u crossed_layers_rejected=%u\n",
              VK_API_VERSION_MAJOR(requested_api), VK_API_VERSION_MINOR(requested_api),
              api_ok, crossed_layers_rejected);
  return api_ok && crossed_layers_rejected ? 0 : 1;
}
