/* omnidroid's guest Vulkan driver: the hwvulkan HAL module, the transport to the host, and the
 * proc-address tables. The commands themselves are generated.c (forwarded as they are) and
 * special.c (the ones that need more). */
#include "driver.h"

#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include <android/log.h>

#define LOGE(...) __android_log_print(ANDROID_LOG_ERROR, "omni-vulkan", __VA_ARGS__)

/* --- the transport ------------------------------------------------------------------------- */

static int g_fd = -1;
static pthread_once_t g_open_once = PTHREAD_ONCE_INIT;
static __thread int t_failed;

static void open_device(void) {
    g_fd = open("/dev/omni-gpu", O_RDWR | O_CLOEXEC);
    if (g_fd < 0) LOGE("/dev/omni-gpu: %s", strerror(errno));
}

uint64_t omni_vk_call(uint32_t id, const uint64_t* args, uint32_t argc) {
    pthread_once(&g_open_once, open_device);
    t_failed = 0;
    struct omni_gpu_call c = {.command = id, .argc = argc, .args = OMNI_U64(args), .result = 0, .reserved = 0};
    if (g_fd < 0 || ioctl(g_fd, OMNI_GPU_CALL, &c) != 0) {
        t_failed = 1;
        LOGE("command %u: %s", id, g_fd < 0 ? "no /dev/omni-gpu" : strerror(errno));
        return (uint64_t)(uint32_t)VK_ERROR_DEVICE_LOST;
    }
    return c.result;
}

VkResult omni_vk_result(uint64_t r) { return (VkResult)(int32_t)(uint32_t)r; }

int omni_vk_failed(void) { return t_failed; }

/* --- dispatchable wrappers ----------------------------------------------------------------- */

struct omni_vk_object* omni_vk_wrap(uint64_t host) {
    struct omni_vk_object* o = calloc(1, sizeof *o);
    if (o == NULL) return NULL;
    o->dispatch.magic = HWVULKAN_DISPATCH_MAGIC;
    o->host = host;
    return o;
}

struct omni_vk_object* omni_vk_child(struct omni_vk_parent* parent, uint64_t host) {
    pthread_mutex_lock(&parent->lock);
    struct omni_vk_object* found = NULL;
    for (uint32_t i = 0; i < parent->count && found == NULL; i++) {
        if (parent->children[i]->host == host) found = parent->children[i];
    }
    if (found == NULL) {
        if (parent->count == parent->capacity) {
            uint32_t cap = parent->capacity ? parent->capacity * 2 : 8;
            struct omni_vk_object** grown = realloc(parent->children, cap * sizeof *grown);
            if (grown == NULL) {
                pthread_mutex_unlock(&parent->lock);
                return NULL;
            }
            parent->children = grown;
            parent->capacity = cap;
        }
        found = omni_vk_wrap(host);
        if (found != NULL) parent->children[parent->count++] = found;
    }
    pthread_mutex_unlock(&parent->lock);
    return found;
}

void omni_vk_free_children(struct omni_vk_parent* parent) {
    pthread_mutex_lock(&parent->lock);
    for (uint32_t i = 0; i < parent->count; i++) free(parent->children[i]);
    free(parent->children);
    parent->children = NULL;
    parent->count = parent->capacity = 0;
    pthread_mutex_unlock(&parent->lock);
}

/* --- proc addresses ------------------------------------------------------------------------ */

/* VK_ANDROID_native_buffer's commands, which vk.xml does not list (special.c). */
VKAPI_ATTR VkResult VKAPI_CALL omni_vkGetSwapchainGrallocUsageANDROID(VkDevice, VkFormat, VkImageUsageFlags, int*);
VKAPI_ATTR VkResult VKAPI_CALL omni_vkGetSwapchainGrallocUsage2ANDROID(VkDevice, VkFormat, VkImageUsageFlags,
                                                                       VkSwapchainImageUsageFlagsANDROID, uint64_t*, uint64_t*);
VKAPI_ATTR VkResult VKAPI_CALL omni_vkGetSwapchainGrallocUsage3ANDROID(VkDevice, const VkGrallocUsageInfoANDROID*, uint64_t*);
VKAPI_ATTR VkResult VKAPI_CALL omni_vkGetSwapchainGrallocUsage4ANDROID(VkDevice, const VkGrallocUsageInfo2ANDROID*, uint64_t*);
VKAPI_ATTR VkResult VKAPI_CALL omni_vkAcquireImageANDROID(VkDevice, VkImage, int, VkSemaphore, VkFence);
VKAPI_ATTR VkResult VKAPI_CALL omni_vkQueueSignalReleaseImageANDROID(VkQueue, uint32_t, const VkSemaphore*, VkImage, int*);

static const struct omni_vk_entry k_native_buffer[] = {
    {"vkAcquireImageANDROID", (PFN_vkVoidFunction)omni_vkAcquireImageANDROID, 2},
    {"vkGetSwapchainGrallocUsage2ANDROID", (PFN_vkVoidFunction)omni_vkGetSwapchainGrallocUsage2ANDROID, 2},
    {"vkGetSwapchainGrallocUsage3ANDROID", (PFN_vkVoidFunction)omni_vkGetSwapchainGrallocUsage3ANDROID, 2},
    {"vkGetSwapchainGrallocUsage4ANDROID", (PFN_vkVoidFunction)omni_vkGetSwapchainGrallocUsage4ANDROID, 2},
    {"vkGetSwapchainGrallocUsageANDROID", (PFN_vkVoidFunction)omni_vkGetSwapchainGrallocUsageANDROID, 2},
    {"vkQueueSignalReleaseImageANDROID", (PFN_vkVoidFunction)omni_vkQueueSignalReleaseImageANDROID, 2},
};

static int by_name(const void* key, const void* entry) {
    return strcmp((const char*)key, ((const struct omni_vk_entry*)entry)->name);
}

static const struct omni_vk_entry* find(const char* name) {
    if (name == NULL) return NULL;
    const struct omni_vk_entry* e = bsearch(name, omni_vk_entries, omni_vk_entry_count, sizeof *e, by_name);
    if (e == NULL) {
        e = bsearch(name, k_native_buffer, sizeof k_native_buffer / sizeof *k_native_buffer, sizeof *e, by_name);
    }
    return e;
}

VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL omni_vkGetInstanceProcAddr(VkInstance instance, const char* pName) {
    const struct omni_vk_entry* e = find(pName);
    if (e == NULL) return NULL;
    /* With no instance, only the global commands (and this one) are answered. */
    if (instance == VK_NULL_HANDLE && e->level != 0 && strcmp(pName, "vkGetInstanceProcAddr") != 0) return NULL;
    return e->fn;
}

VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL omni_vkGetDeviceProcAddr(VkDevice device, const char* pName) {
    (void)device;
    const struct omni_vk_entry* e = find(pName);
    return e != NULL && e->level == 2 ? e->fn : NULL;
}

/* --- the HAL module ------------------------------------------------------------------------ */

__attribute__((visibility("default"))) extern hwvulkan_module_t HAL_MODULE_INFO_SYM;

static int close_device(struct hw_device_t* device) {
    (void)device;
    return 0;
}

static hwvulkan_device_t g_device = {
    .common =
        {
            .tag = HARDWARE_DEVICE_TAG,
            .version = HWVULKAN_DEVICE_API_VERSION_0_1,
            .module = &HAL_MODULE_INFO_SYM.common,
            .close = close_device,
        },
    .EnumerateInstanceExtensionProperties = omni_vkEnumerateInstanceExtensionProperties,
    .CreateInstance = omni_vkCreateInstance,
    .GetInstanceProcAddr = omni_vkGetInstanceProcAddr,
};

static int open_hal(const struct hw_module_t* module, const char* id, struct hw_device_t** device) {
    (void)module;
    if (id == NULL || strcmp(id, HWVULKAN_DEVICE_0) != 0) return -ENOENT;
    *device = &g_device.common;
    return 0;
}

static struct hw_module_methods_t g_methods = {.open = open_hal};

__attribute__((visibility("default"))) hwvulkan_module_t HAL_MODULE_INFO_SYM = {
    .common =
        {
            .tag = HARDWARE_MODULE_TAG,
            .module_api_version = HWVULKAN_MODULE_API_VERSION_0_1,
            .hal_api_version = HARDWARE_HAL_API_VERSION,
            .id = HWVULKAN_HARDWARE_MODULE_ID,
            .name = "omnidroid Vulkan (the host's GPU)",
            .author = "omnidroid",
            .methods = &g_methods,
        },
};
