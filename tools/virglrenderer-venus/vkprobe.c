/* Host Vulkan extension probe. Build under MSYS2 MINGW64:
 *   cc vkprobe.c -lvulkan-1 -o vkprobe
 * On this box (RTX 4060, driver Vulkan 1.4.325 via System32\vulkan-1.dll) it printed:
 *   VK_KHR_external_memory_fd     : NO
 *   VK_EXT_external_memory_dma_buf: NO
 *   VK_KHR_external_memory_win32  : YES
 * -- see docs/bench-2026-09-03.md third pass, Blocker B.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vulkan/vulkan.h>

int main(void)
{
   VkInstance inst;
   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                             .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                                .pApplicationInfo = &app };
   if (vkCreateInstance(&ici, NULL, &inst) != VK_SUCCESS) {
      printf("vkCreateInstance FAILED\n");
      return 1;
   }
   uint32_t n = 0;
   vkEnumeratePhysicalDevices(inst, &n, NULL);
   VkPhysicalDevice *pd = malloc(n * sizeof(*pd));
   vkEnumeratePhysicalDevices(inst, &n, pd);
   printf("physical devices: %u\n", n);
   for (uint32_t i = 0; i < n; i++) {
      VkPhysicalDeviceProperties p;
      vkGetPhysicalDeviceProperties(pd[i], &p);
      printf("[%u] %s (api %u.%u.%u)\n", i, p.deviceName,
             VK_VERSION_MAJOR(p.apiVersion), VK_VERSION_MINOR(p.apiVersion),
             VK_VERSION_PATCH(p.apiVersion));
      uint32_t e = 0;
      vkEnumerateDeviceExtensionProperties(pd[i], NULL, &e, NULL);
      VkExtensionProperties *ep = malloc(e * sizeof(*ep));
      vkEnumerateDeviceExtensionProperties(pd[i], NULL, &e, ep);
      int fd_ext = 0, dmabuf = 0, win32 = 0;
      for (uint32_t j = 0; j < e; j++) {
         if (!strcmp(ep[j].extensionName, "VK_KHR_external_memory_fd")) fd_ext = 1;
         if (!strcmp(ep[j].extensionName, "VK_EXT_external_memory_dma_buf")) dmabuf = 1;
         if (!strcmp(ep[j].extensionName, "VK_KHR_external_memory_win32")) win32 = 1;
      }
      printf("    total device extensions: %u\n", e);
      printf("    VK_KHR_external_memory_fd     : %s\n", fd_ext ? "YES" : "NO");
      printf("    VK_EXT_external_memory_dma_buf: %s\n", dmabuf ? "YES" : "NO");
      printf("    VK_KHR_external_memory_win32  : %s\n", win32 ? "YES" : "NO");
      free(ep);
   }
   return 0;
}
