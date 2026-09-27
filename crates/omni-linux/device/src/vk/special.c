/* The commands tools/vk/special.txt lists, and VK_ANDROID_native_buffer's: each is forwarded like a
 * generated one, and what the host answers is finished here -- the dispatchable handles it returns
 * are wrapped for the loader, and freed with what owns them. The host's half is
 * crates/omni-linux/src/gpu/special.rs. */
#include "driver.h"

#include <dlfcn.h>
#include <stdlib.h>
#include <string.h>

#define CALL(id, ...)                                                        \
    ({                                                                       \
        const uint64_t a_[] = {__VA_ARGS__};                                 \
        omni_vk_call((id), a_, (uint32_t)(sizeof a_ / sizeof a_[0]));        \
    })
#define RESULT(id, ...) omni_vk_result(CALL(id, __VA_ARGS__))

static struct omni_vk_parent* parent_of(const void* handle) { return (struct omni_vk_parent*)handle; }

static struct omni_vk_parent* new_parent(uint64_t host) {
    struct omni_vk_parent* p = calloc(1, sizeof *p);
    if (p == NULL) return NULL;
    p->obj.dispatch.magic = HWVULKAN_DISPATCH_MAGIC;
    p->obj.host = host;
    pthread_mutex_init(&p->lock, NULL);
    return p;
}

/* --- instances ----------------------------------------------------------------------------- */

VKAPI_ATTR VkResult VKAPI_CALL omni_vkEnumerateInstanceVersion(uint32_t* pApiVersion) {
    return RESULT(OMNI_VK_ID_VK_ENUMERATE_INSTANCE_VERSION, OMNI_U64(pApiVersion));
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkEnumerateInstanceExtensionProperties(const char* pLayerName, uint32_t* pPropertyCount,
                                                                           VkExtensionProperties* pProperties) {
    return RESULT(OMNI_VK_ID_VK_ENUMERATE_INSTANCE_EXTENSION_PROPERTIES, OMNI_U64(pLayerName), OMNI_U64(pPropertyCount),
                  OMNI_U64(pProperties));
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkEnumerateInstanceLayerProperties(uint32_t* pPropertyCount, VkLayerProperties* pProperties) {
    (void)pProperties;
    *pPropertyCount = 0;
    return VK_SUCCESS;
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkCreateInstance(const VkInstanceCreateInfo* pCreateInfo, const VkAllocationCallbacks* pAllocator,
                                                     VkInstance* pInstance) {
    (void)pAllocator; /* host memory is the host's; a guest allocator cannot serve it */
    uint64_t host = 0;
    VkResult r = RESULT(OMNI_VK_ID_VK_CREATE_INSTANCE, OMNI_U64(pCreateInfo), 0, OMNI_U64(&host));
    if (r != VK_SUCCESS) return r;
    struct omni_vk_parent* p = new_parent(host);
    if (p == NULL) {
        CALL(OMNI_VK_ID_VK_DESTROY_INSTANCE, OMNI_U64(&(struct omni_vk_object){.host = host}), 0);
        return VK_ERROR_OUT_OF_HOST_MEMORY;
    }
    *pInstance = (VkInstance)p;
    return VK_SUCCESS;
}

VKAPI_ATTR void VKAPI_CALL omni_vkDestroyInstance(VkInstance instance, const VkAllocationCallbacks* pAllocator) {
    (void)pAllocator;
    if (instance == VK_NULL_HANDLE) return;
    CALL(OMNI_VK_ID_VK_DESTROY_INSTANCE, OMNI_U64(instance), 0);
    omni_vk_free_children(parent_of(instance));
    free(instance);
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkEnumeratePhysicalDevices(VkInstance instance, uint32_t* pPhysicalDeviceCount,
                                                               VkPhysicalDevice* pPhysicalDevices) {
    if (pPhysicalDevices == NULL) {
        return RESULT(OMNI_VK_ID_VK_ENUMERATE_PHYSICAL_DEVICES, OMNI_U64(instance), OMNI_U64(pPhysicalDeviceCount), 0);
    }
    uint64_t* hosts = calloc(*pPhysicalDeviceCount ? *pPhysicalDeviceCount : 1, sizeof *hosts);
    if (hosts == NULL) return VK_ERROR_OUT_OF_HOST_MEMORY;
    VkResult r = RESULT(OMNI_VK_ID_VK_ENUMERATE_PHYSICAL_DEVICES, OMNI_U64(instance), OMNI_U64(pPhysicalDeviceCount), OMNI_U64(hosts));
    if (r == VK_SUCCESS || r == VK_INCOMPLETE) {
        for (uint32_t i = 0; i < *pPhysicalDeviceCount; i++) {
            struct omni_vk_object* o = omni_vk_child(parent_of(instance), hosts[i]);
            if (o == NULL) {
                r = VK_ERROR_OUT_OF_HOST_MEMORY;
                break;
            }
            pPhysicalDevices[i] = (VkPhysicalDevice)o;
        }
    }
    free(hosts);
    return r;
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkEnumeratePhysicalDeviceGroups(VkInstance instance, uint32_t* pCount,
                                                                    VkPhysicalDeviceGroupProperties* pProps) {
    /* The host writes its own handles into physicalDevices[]; each becomes its wrapper here. */
    VkResult r = RESULT(OMNI_VK_ID_VK_ENUMERATE_PHYSICAL_DEVICE_GROUPS, OMNI_U64(instance), OMNI_U64(pCount), OMNI_U64(pProps));
    if (pProps != NULL && (r == VK_SUCCESS || r == VK_INCOMPLETE)) {
        for (uint32_t g = 0; g < *pCount; g++) {
            for (uint32_t i = 0; i < pProps[g].physicalDeviceCount && i < VK_MAX_DEVICE_GROUP_SIZE; i++) {
                struct omni_vk_object* o = omni_vk_child(parent_of(instance), OMNI_U64(pProps[g].physicalDevices[i]));
                if (o == NULL) return VK_ERROR_OUT_OF_HOST_MEMORY;
                pProps[g].physicalDevices[i] = (VkPhysicalDevice)o;
            }
        }
    }
    return r;
}

/* --- devices, queues, command buffers ------------------------------------------------------ */

VKAPI_ATTR VkResult VKAPI_CALL omni_vkEnumerateDeviceExtensionProperties(VkPhysicalDevice physicalDevice, const char* pLayerName,
                                                                         uint32_t* pPropertyCount, VkExtensionProperties* pProperties) {
    return RESULT(OMNI_VK_ID_VK_ENUMERATE_DEVICE_EXTENSION_PROPERTIES, OMNI_U64(physicalDevice), OMNI_U64(pLayerName),
                  OMNI_U64(pPropertyCount), OMNI_U64(pProperties));
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkEnumerateDeviceLayerProperties(VkPhysicalDevice physicalDevice, uint32_t* pPropertyCount,
                                                                     VkLayerProperties* pProperties) {
    (void)physicalDevice;
    (void)pProperties;
    *pPropertyCount = 0;
    return VK_SUCCESS;
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkCreateDevice(VkPhysicalDevice physicalDevice, const VkDeviceCreateInfo* pCreateInfo,
                                                   const VkAllocationCallbacks* pAllocator, VkDevice* pDevice) {
    (void)pAllocator;
    uint64_t host = 0;
    VkResult r = RESULT(OMNI_VK_ID_VK_CREATE_DEVICE, OMNI_U64(physicalDevice), OMNI_U64(pCreateInfo), 0, OMNI_U64(&host));
    if (r != VK_SUCCESS) return r;
    struct omni_vk_parent* p = new_parent(host);
    if (p == NULL) {
        CALL(OMNI_VK_ID_VK_DESTROY_DEVICE, OMNI_U64(&(struct omni_vk_object){.host = host}), 0);
        return VK_ERROR_OUT_OF_HOST_MEMORY;
    }
    *pDevice = (VkDevice)p;
    return VK_SUCCESS;
}

VKAPI_ATTR void VKAPI_CALL omni_vkDestroyDevice(VkDevice device, const VkAllocationCallbacks* pAllocator) {
    (void)pAllocator;
    if (device == VK_NULL_HANDLE) return;
    CALL(OMNI_VK_ID_VK_DESTROY_DEVICE, OMNI_U64(device), 0);
    omni_vk_free_children(parent_of(device));
    free(device);
}

VKAPI_ATTR void VKAPI_CALL omni_vkGetDeviceQueue(VkDevice device, uint32_t queueFamilyIndex, uint32_t queueIndex, VkQueue* pQueue) {
    uint64_t host = 0;
    CALL(OMNI_VK_ID_VK_GET_DEVICE_QUEUE, OMNI_U64(device), queueFamilyIndex, queueIndex, OMNI_U64(&host));
    *pQueue = host ? (VkQueue)omni_vk_child(parent_of(device), host) : VK_NULL_HANDLE;
}

VKAPI_ATTR void VKAPI_CALL omni_vkGetDeviceQueue2(VkDevice device, const VkDeviceQueueInfo2* pQueueInfo, VkQueue* pQueue) {
    uint64_t host = 0;
    CALL(OMNI_VK_ID_VK_GET_DEVICE_QUEUE2, OMNI_U64(device), OMNI_U64(pQueueInfo), OMNI_U64(&host));
    *pQueue = host ? (VkQueue)omni_vk_child(parent_of(device), host) : VK_NULL_HANDLE;
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkAllocateCommandBuffers(VkDevice device, const VkCommandBufferAllocateInfo* pAllocateInfo,
                                                             VkCommandBuffer* pCommandBuffers) {
    /* The wrappers first, so nothing can fail after the host has made its command buffers. */
    uint32_t n = pAllocateInfo->commandBufferCount;
    struct omni_vk_object** wrappers = calloc(n ? n : 1, sizeof *wrappers);
    if (wrappers == NULL) return VK_ERROR_OUT_OF_HOST_MEMORY;
    for (uint32_t i = 0; i < n; i++) {
        wrappers[i] = omni_vk_wrap(0);
        if (wrappers[i] == NULL) {
            for (uint32_t j = 0; j < i; j++) free(wrappers[j]);
            free(wrappers);
            return VK_ERROR_OUT_OF_HOST_MEMORY;
        }
    }
    /* The host writes its handles into the array; each becomes a wrapper here. */
    VkResult r = RESULT(OMNI_VK_ID_VK_ALLOCATE_COMMAND_BUFFERS, OMNI_U64(device), OMNI_U64(pAllocateInfo), OMNI_U64(pCommandBuffers));
    for (uint32_t i = 0; i < n; i++) {
        if (r == VK_SUCCESS) {
            wrappers[i]->host = OMNI_U64(pCommandBuffers[i]);
            pCommandBuffers[i] = (VkCommandBuffer)wrappers[i];
        } else {
            free(wrappers[i]);
        }
    }
    free(wrappers);
    return r;
}

VKAPI_ATTR void VKAPI_CALL omni_vkFreeCommandBuffers(VkDevice device, VkCommandPool commandPool, uint32_t commandBufferCount,
                                                     const VkCommandBuffer* pCommandBuffers) {
    CALL(OMNI_VK_ID_VK_FREE_COMMAND_BUFFERS, OMNI_U64(device), OMNI_U64(commandPool), commandBufferCount, OMNI_U64(pCommandBuffers));
    for (uint32_t i = 0; i < commandBufferCount; i++) free(pCommandBuffers[i]);
}

/* --- forwarded as they are; the host does the work ----------------------------------------- */

VKAPI_ATTR VkResult VKAPI_CALL omni_vkQueueSubmit(VkQueue queue, uint32_t submitCount, const VkSubmitInfo* pSubmits, VkFence fence) {
    return RESULT(OMNI_VK_ID_VK_QUEUE_SUBMIT, OMNI_U64(queue), submitCount, OMNI_U64(pSubmits), OMNI_U64(fence));
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkQueueSubmit2(VkQueue queue, uint32_t submitCount, const VkSubmitInfo2* pSubmits, VkFence fence) {
    return RESULT(OMNI_VK_ID_VK_QUEUE_SUBMIT2, OMNI_U64(queue), submitCount, OMNI_U64(pSubmits), OMNI_U64(fence));
}

VKAPI_ATTR void VKAPI_CALL omni_vkCmdExecuteCommands(VkCommandBuffer commandBuffer, uint32_t commandBufferCount,
                                                     const VkCommandBuffer* pCommandBuffers) {
    CALL(OMNI_VK_ID_VK_CMD_EXECUTE_COMMANDS, OMNI_U64(commandBuffer), commandBufferCount, OMNI_U64(pCommandBuffers));
}

VKAPI_ATTR void VKAPI_CALL omni_vkGetPhysicalDeviceProperties2(VkPhysicalDevice physicalDevice, VkPhysicalDeviceProperties2* pProperties) {
    CALL(OMNI_VK_ID_VK_GET_PHYSICAL_DEVICE_PROPERTIES2, OMNI_U64(physicalDevice), OMNI_U64(pProperties));
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkGetPhysicalDeviceImageFormatProperties2(VkPhysicalDevice physicalDevice,
                                                                              const VkPhysicalDeviceImageFormatInfo2* pImageFormatInfo,
                                                                              VkImageFormatProperties2* pImageFormatProperties) {
    return RESULT(OMNI_VK_ID_VK_GET_PHYSICAL_DEVICE_IMAGE_FORMAT_PROPERTIES2, OMNI_U64(physicalDevice), OMNI_U64(pImageFormatInfo),
                  OMNI_U64(pImageFormatProperties));
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkCreateImage(VkDevice device, const VkImageCreateInfo* pCreateInfo, const VkAllocationCallbacks* pAllocator,
                                                  VkImage* pImage) {
    (void)pAllocator;
    return RESULT(OMNI_VK_ID_VK_CREATE_IMAGE, OMNI_U64(device), OMNI_U64(pCreateInfo), 0, OMNI_U64(pImage));
}

VKAPI_ATTR void VKAPI_CALL omni_vkDestroyImage(VkDevice device, VkImage image, const VkAllocationCallbacks* pAllocator) {
    (void)pAllocator;
    CALL(OMNI_VK_ID_VK_DESTROY_IMAGE, OMNI_U64(device), OMNI_U64(image), 0);
}

/* The gralloc handle of an AHardwareBuffer: libnativewindow's AHardwareBuffer_getNativeHandle (an
 * LLNDK function, not in the NDK's stub, so found at run time). */
static const native_handle_t* (*g_get_native_handle)(const struct AHardwareBuffer*);
static pthread_once_t g_get_native_handle_once = PTHREAD_ONCE_INIT;

static void resolve_get_native_handle(void) {
    void* lib = dlopen("libnativewindow.so", RTLD_NOW | RTLD_NOLOAD);
    if (lib == NULL) lib = dlopen("libnativewindow.so", RTLD_NOW);
    if (lib != NULL) g_get_native_handle = (const native_handle_t* (*)(const struct AHardwareBuffer*))dlsym(lib, "AHardwareBuffer_getNativeHandle");
}

static const native_handle_t* native_handle_of(const struct AHardwareBuffer* buffer) {
    pthread_once(&g_get_native_handle_once, resolve_get_native_handle);
    return g_get_native_handle != NULL && buffer != NULL ? g_get_native_handle(buffer) : NULL;
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkAllocateMemory(VkDevice device, const VkMemoryAllocateInfo* pAllocateInfo,
                                                     const VkAllocationCallbacks* pAllocator, VkDeviceMemory* pMemory) {
    (void)pAllocator;
    /* An import of a gralloc buffer passes the buffer's handle in the allocator's place (the host
     * cannot read an AHardwareBuffer, which is libnativewindow's object). */
    uint64_t handle = 0;
    for (const VkBaseInStructure* s = pAllocateInfo->pNext; s != NULL; s = s->pNext) {
        if (s->sType == VK_STRUCTURE_TYPE_IMPORT_ANDROID_HARDWARE_BUFFER_INFO_ANDROID) {
            handle = OMNI_U64(native_handle_of(((const VkImportAndroidHardwareBufferInfoANDROID*)s)->buffer));
            if (handle == 0) return VK_ERROR_INVALID_EXTERNAL_HANDLE;
        }
    }
    return RESULT(OMNI_VK_ID_VK_ALLOCATE_MEMORY, OMNI_U64(device), OMNI_U64(pAllocateInfo), handle, OMNI_U64(pMemory));
}

VKAPI_ATTR void VKAPI_CALL omni_vkFreeMemory(VkDevice device, VkDeviceMemory memory, const VkAllocationCallbacks* pAllocator) {
    (void)pAllocator;
    CALL(OMNI_VK_ID_VK_FREE_MEMORY, OMNI_U64(device), OMNI_U64(memory), 0);
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkCreateSemaphore(VkDevice device, const VkSemaphoreCreateInfo* pCreateInfo,
                                                      const VkAllocationCallbacks* pAllocator, VkSemaphore* pSemaphore) {
    (void)pAllocator;
    return RESULT(OMNI_VK_ID_VK_CREATE_SEMAPHORE, OMNI_U64(device), OMNI_U64(pCreateInfo), 0, OMNI_U64(pSemaphore));
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkCreateFence(VkDevice device, const VkFenceCreateInfo* pCreateInfo, const VkAllocationCallbacks* pAllocator,
                                                  VkFence* pFence) {
    (void)pAllocator;
    return RESULT(OMNI_VK_ID_VK_CREATE_FENCE, OMNI_U64(device), OMNI_U64(pCreateInfo), 0, OMNI_U64(pFence));
}

VKAPI_ATTR void VKAPI_CALL omni_vkGetPhysicalDeviceExternalSemaphoreProperties(VkPhysicalDevice physicalDevice,
                                                                               const VkPhysicalDeviceExternalSemaphoreInfo* pInfo,
                                                                               VkExternalSemaphoreProperties* pProperties) {
    CALL(OMNI_VK_ID_VK_GET_PHYSICAL_DEVICE_EXTERNAL_SEMAPHORE_PROPERTIES, OMNI_U64(physicalDevice), OMNI_U64(pInfo), OMNI_U64(pProperties));
}

VKAPI_ATTR void VKAPI_CALL omni_vkGetPhysicalDeviceExternalFenceProperties(VkPhysicalDevice physicalDevice,
                                                                           const VkPhysicalDeviceExternalFenceInfo* pInfo,
                                                                           VkExternalFenceProperties* pProperties) {
    CALL(OMNI_VK_ID_VK_GET_PHYSICAL_DEVICE_EXTERNAL_FENCE_PROPERTIES, OMNI_U64(physicalDevice), OMNI_U64(pInfo), OMNI_U64(pProperties));
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkBindImageMemory(VkDevice device, VkImage image, VkDeviceMemory memory, VkDeviceSize memoryOffset) {
    return RESULT(OMNI_VK_ID_VK_BIND_IMAGE_MEMORY, OMNI_U64(device), OMNI_U64(image), OMNI_U64(memory), memoryOffset);
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkBindImageMemory2(VkDevice device, uint32_t bindInfoCount, const VkBindImageMemoryInfo* pBindInfos) {
    return RESULT(OMNI_VK_ID_VK_BIND_IMAGE_MEMORY2, OMNI_U64(device), bindInfoCount, OMNI_U64(pBindInfos));
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkGetAndroidHardwareBufferPropertiesANDROID(VkDevice device, const struct AHardwareBuffer* buffer,
                                                                                VkAndroidHardwareBufferPropertiesANDROID* pProperties) {
    /* The host is given the buffer's gralloc handle, not the AHardwareBuffer. */
    const native_handle_t* handle = native_handle_of(buffer);
    if (handle == NULL) return VK_ERROR_INVALID_EXTERNAL_HANDLE;
    return RESULT(OMNI_VK_ID_VK_GET_ANDROID_HARDWARE_BUFFER_PROPERTIES_ANDROID, OMNI_U64(device), OMNI_U64(handle), OMNI_U64(pProperties));
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkGetMemoryAndroidHardwareBufferANDROID(VkDevice device,
                                                                            const VkMemoryGetAndroidHardwareBufferInfoANDROID* pInfo,
                                                                            struct AHardwareBuffer** pBuffer) {
    return RESULT(OMNI_VK_ID_VK_GET_MEMORY_ANDROID_HARDWARE_BUFFER_ANDROID, OMNI_U64(device), OMNI_U64(pInfo), OMNI_U64(pBuffer));
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkGetSemaphoreFdKHR(VkDevice device, const VkSemaphoreGetFdInfoKHR* pGetFdInfo, int* pFd) {
    return RESULT(OMNI_VK_ID_VK_GET_SEMAPHORE_FD_KHR, OMNI_U64(device), OMNI_U64(pGetFdInfo), OMNI_U64(pFd));
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkImportSemaphoreFdKHR(VkDevice device, const VkImportSemaphoreFdInfoKHR* pImportSemaphoreFdInfo) {
    return RESULT(OMNI_VK_ID_VK_IMPORT_SEMAPHORE_FD_KHR, OMNI_U64(device), OMNI_U64(pImportSemaphoreFdInfo));
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkGetFenceFdKHR(VkDevice device, const VkFenceGetFdInfoKHR* pGetFdInfo, int* pFd) {
    return RESULT(OMNI_VK_ID_VK_GET_FENCE_FD_KHR, OMNI_U64(device), OMNI_U64(pGetFdInfo), OMNI_U64(pFd));
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkImportFenceFdKHR(VkDevice device, const VkImportFenceFdInfoKHR* pImportFenceFdInfo) {
    return RESULT(OMNI_VK_ID_VK_IMPORT_FENCE_FD_KHR, OMNI_U64(device), OMNI_U64(pImportFenceFdInfo));
}

/* --- VK_ANDROID_native_buffer -------------------------------------------------------------- */

VKAPI_ATTR VkResult VKAPI_CALL omni_vkGetSwapchainGrallocUsageANDROID(VkDevice device, VkFormat format, VkImageUsageFlags imageUsage,
                                                                      int* grallocUsage) {
    return RESULT(OMNI_VK_ID_GRALLOC_USAGE, OMNI_U64(device), (uint64_t)format, imageUsage, OMNI_U64(grallocUsage));
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkGetSwapchainGrallocUsage2ANDROID(VkDevice device, VkFormat format, VkImageUsageFlags imageUsage,
                                                                       VkSwapchainImageUsageFlagsANDROID swapchainImageUsage,
                                                                       uint64_t* grallocConsumerUsage, uint64_t* grallocProducerUsage) {
    return RESULT(OMNI_VK_ID_GRALLOC_USAGE2, OMNI_U64(device), (uint64_t)format, imageUsage, swapchainImageUsage,
                  OMNI_U64(grallocConsumerUsage), OMNI_U64(grallocProducerUsage));
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkGetSwapchainGrallocUsage3ANDROID(VkDevice device, const VkGrallocUsageInfoANDROID* grallocUsageInfo,
                                                                       uint64_t* grallocUsage) {
    return RESULT(OMNI_VK_ID_GRALLOC_USAGE3, OMNI_U64(device), OMNI_U64(grallocUsageInfo), OMNI_U64(grallocUsage));
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkGetSwapchainGrallocUsage4ANDROID(VkDevice device, const VkGrallocUsageInfo2ANDROID* grallocUsageInfo,
                                                                       uint64_t* grallocUsage) {
    return RESULT(OMNI_VK_ID_GRALLOC_USAGE4, OMNI_U64(device), OMNI_U64(grallocUsageInfo), OMNI_U64(grallocUsage));
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkAcquireImageANDROID(VkDevice device, VkImage image, int nativeFenceFd, VkSemaphore semaphore,
                                                          VkFence fence) {
    return RESULT(OMNI_VK_ID_ACQUIRE_IMAGE, OMNI_U64(device), OMNI_U64(image), (uint64_t)(int64_t)nativeFenceFd, OMNI_U64(semaphore),
                  OMNI_U64(fence));
}

VKAPI_ATTR VkResult VKAPI_CALL omni_vkQueueSignalReleaseImageANDROID(VkQueue queue, uint32_t waitSemaphoreCount,
                                                                     const VkSemaphore* pWaitSemaphores, VkImage image, int* pNativeFenceFd) {
    return RESULT(OMNI_VK_ID_QUEUE_SIGNAL_RELEASE_IMAGE, OMNI_U64(queue), waitSemaphoreCount, OMNI_U64(pWaitSemaphores), OMNI_U64(image),
                  OMNI_U64(pNativeFenceFd));
}
