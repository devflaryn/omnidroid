/* omnidroid's guest Vulkan driver: the hwvulkan HAL module, the transport to the host, and the
 * proc-address tables. The commands themselves are generated.c (forwarded as they are) and
 * special.c (the ones that need more). */
#include "driver.h"

#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

#include <android/log.h>

#define LOGE(...) __android_log_print(ANDROID_LOG_ERROR, "omni-vulkan", __VA_ARGS__)

/* --- the transport ------------------------------------------------------------------------- */

static int g_fd = -1;
static pthread_once_t g_open_once = PTHREAD_ONCE_INIT;
static __thread int t_failed;

/* The request with its arguments right after it, its size in the number: the host reads both in
 * one checked copy instead of two (~55 ns on an E-core). Sent while the host says so (bit 1 of its
 * configuration: its vk_inline lever), asked when the device opens and at a vkBeginCommandBuffer;
 * a host without it answers ENOTTY, and the plain request is used from then on. */
#define OMNI_GPU_CALL_INLINE(argc) (0xc0004702u | ((32u + 8u * (uint32_t)(argc)) << 16))
static int g_inline;

static uint64_t host_config(void);

static void open_device(void) {
    g_fd = open("/dev/omni-gpu", O_RDWR | O_CLOEXEC);
    if (g_fd < 0) {
        LOGE("/dev/omni-gpu: %s", strerror(errno));
        return;
    }
    __atomic_store_n(&g_inline, (int)((host_config() >> 1) & 1u), __ATOMIC_RELAXED);
}

/* The request in the 32 bytes right before `args` (OMNI_VK_FRAME's room), so the inline form
 * needs no copy of the arguments: one request serves both forms. */
uint64_t omni_vk_call_framed(uint32_t id, uint64_t* args, uint32_t argc) {
    pthread_once(&g_open_once, open_device);
    t_failed = 0;
    struct omni_gpu_call* c = (struct omni_gpu_call*)(void*)((char*)args - sizeof(struct omni_gpu_call));
    *c = (struct omni_gpu_call){.command = id, .argc = argc, .args = OMNI_U64(args), .result = 0, .reserved = 0};
    if (g_fd >= 0 && argc <= 32u && __atomic_load_n(&g_inline, __ATOMIC_RELAXED)) {
        if (ioctl(g_fd, (int)OMNI_GPU_CALL_INLINE(argc), c) == 0) return c->result;
        if (errno != ENOTTY) {
            t_failed = 1;
            LOGE("command %u: %s", id, strerror(errno));
            return (uint64_t)(uint32_t)VK_ERROR_DEVICE_LOST;
        }
        __atomic_store_n(&g_inline, 0, __ATOMIC_RELAXED);
        c->result = 0;
    }
    if (g_fd < 0 || ioctl(g_fd, OMNI_GPU_CALL, c) != 0) {
        t_failed = 1;
        LOGE("command %u: %s", id, g_fd < 0 ? "no /dev/omni-gpu" : strerror(errno));
        return (uint64_t)(uint32_t)VK_ERROR_DEVICE_LOST;
    }
    return c->result;
}

/* For callers whose arguments have no room before them (special.c, the batches): copied into a
 * frame here -- by a loop the compiler may not turn into a call to memcpy (a libc call from here
 * is the game's libc, whatever it hooks). */
__attribute__((no_builtin("memcpy"))) uint64_t omni_vk_call(uint32_t id, const uint64_t* args, uint32_t argc) {
    if (argc > 32u) {
        pthread_once(&g_open_once, open_device);
        t_failed = 0;
        struct omni_gpu_call c = {.command = id, .argc = argc, .args = OMNI_U64(args), .result = 0, .reserved = 0};
        if (g_fd < 0 || ioctl(g_fd, OMNI_GPU_CALL, &c) != 0) {
            t_failed = 1;
            return (uint64_t)(uint32_t)VK_ERROR_DEVICE_LOST;
        }
        return c.result;
    }
    struct {
        struct omni_gpu_call c;
        uint64_t a[32];
    } f;
    for (uint32_t i = 0; i < argc; i++) f.a[i] = args[i];
    return omni_vk_call_framed(id, f.a, argc);
}

VkResult omni_vk_result(uint64_t r) { return (VkResult)(int32_t)(uint32_t)r; }

int omni_vk_failed(void) { return t_failed; }

/* --- batching (OMNI_VK_BATCH) ------------------------------------------------------------- */

/* Each forwarded command is one system call into the host -- ~0.35 us of crossing besides the
 * driver's own time, ~3,100 of them a frame in a Roblox world (docs/HANDOFF.md). A command that
 * returns nothing and only records into a command buffer (vkCmdDraw, vkCmdBindPipeline, ...,
 * gen_vk_forward.py's batch_plan) is instead appended to its command buffer's batch, and the batch
 * goes to the host in one call (OMNI_VK_ID_BATCH) when the buffer ends, before any command on the
 * buffer that is not batched (so the buffer sees every command in the order it was made), or when
 * it is full. What a command points at is copied into the batch with it, and the copy's address
 * passed instead (one address space: the host's driver reads the copy where it lies), since the
 * caller may reuse its memory once the command has returned.
 *
 * A record: u32 id, u32 argc, u32 size (all of it, 8-aligned), u32 0, u64 args[argc], the copies. */

/* Every live command buffer wrapper (its definition, below, with the wrappers). */
static pthread_mutex_t g_cmdbufs_lock = PTHREAD_MUTEX_INITIALIZER;
static struct omni_vk_cmdbuf* g_cmdbufs;
static void unlink_locked(struct omni_vk_cmdbuf* cb);

#define BATCH_BYTES (64u * 1024u)
/* A command bigger than this (vkCmdUpdateBuffer's data, up to 64 KiB) is sent as it is. */
#define RECORD_MAX (BATCH_BYTES / 4u)

struct omni_vk_batch {
    struct omni_vk_batch* next; /* on the free list */
    uint32_t used;
    uint32_t count;
    _Alignas(8) uint8_t data[BATCH_BYTES];
};

/* Batches not in use: a recording takes one at its first batched command and gives it back at its
 * end, so there are about as many as command buffers recording at once. */
static pthread_mutex_t g_free_lock = PTHREAD_MUTEX_INITIALIZER;
static struct omni_vk_batch* g_free;
static unsigned g_free_count;

static struct omni_vk_batch* take_batch(void) {
    pthread_mutex_lock(&g_free_lock);
    struct omni_vk_batch* b = g_free;
    if (b != NULL) {
        g_free = b->next;
        g_free_count--;
    }
    pthread_mutex_unlock(&g_free_lock);
    if (b == NULL) b = malloc(sizeof *b);
    if (b != NULL) b->used = b->count = 0;
    return b;
}

static void give_batch(struct omni_vk_batch* b) {
    pthread_mutex_lock(&g_free_lock);
    if (g_free_count < 32) {
        b->next = g_free;
        g_free = b;
        g_free_count++;
        b = NULL;
    }
    pthread_mutex_unlock(&g_free_lock);
    free(b);
}

/* Whether the host wants batching now (asked at each vkBeginCommandBuffer, so the host's lever
 * switches it for the next recording). A host without the query answers no. */
static uint64_t host_config(void) {
    if (g_fd < 0) return 0;
    struct omni_gpu_call c = {.command = OMNI_VK_ID_CONFIG, .argc = 0, .args = 0, .result = 0, .reserved = 0};
    if (ioctl(g_fd, OMNI_GPU_CALL, &c) != 0) return 0;
    return c.result;
}

/* The host's configuration, asked again at a vkBeginCommandBuffer at most every 250 ms (the host's
 * levers are read four times a second): one system call per begin was a cost of its own. */
static uint64_t g_config;
static int64_t g_config_at = -((int64_t)1 << 62);

static int host_batches(void) {
    pthread_once(&g_open_once, open_device);
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    int64_t now = (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
    uint64_t config = __atomic_load_n(&g_config, __ATOMIC_RELAXED);
    if (now - __atomic_load_n(&g_config_at, __ATOMIC_RELAXED) >= 250000000) {
        __atomic_store_n(&g_config_at, now, __ATOMIC_RELAXED);
        config = host_config();
        __atomic_store_n(&g_config, config, __ATOMIC_RELAXED);
        if (__atomic_load_n(&g_inline, __ATOMIC_RELAXED) != (int)((config >> 1) & 1u)) {
            __atomic_store_n(&g_inline, (int)((config >> 1) & 1u), __ATOMIC_RELAXED);
        }
    }
    return (int)(config & 1u);
}

static void send_batch(struct omni_vk_cmdbuf* cb) {
    struct omni_vk_batch* b = cb->batch;
    if (b == NULL || b->used == 0) return;
    const uint64_t a[4] = {OMNI_U64(cb), OMNI_U64(b->data), b->used, b->count};
    uint64_t r = omni_vk_call(OMNI_VK_ID_BATCH, a, 4);
    if (omni_vk_failed() || r != 0) {
        static int said;
        if (!said) {
            said = 1;
            LOGE("a batch of %u commands was refused (%llu)", b->count, (unsigned long long)r);
        }
    }
    b->used = b->count = 0;
}

void omni_vk_sync(VkCommandBuffer commandBuffer, int how) {
    struct omni_vk_cmdbuf* cb = (struct omni_vk_cmdbuf*)commandBuffer;
    if (cb == NULL) return;
    if (how == OMNI_VK_SYNC_FLUSH || how == OMNI_VK_SYNC_END) send_batch(cb);
    if (how != OMNI_VK_SYNC_FLUSH && cb->batch != NULL) {
        /* Ended, reset, begun again or freed: what was not sent belongs to a recording that is gone. */
        give_batch(cb->batch);
        cb->batch = NULL;
    }
    if (how == OMNI_VK_SYNC_BEGIN) cb->batching = host_batches();
}

void omni_vk_pool_sync(uint64_t commandPool, int how) {
    if (commandPool == 0) return;
    pthread_mutex_lock(&g_cmdbufs_lock);
    for (struct omni_vk_cmdbuf *cb = g_cmdbufs, *next; cb != NULL; cb = next) {
        next = cb->next;
        if (cb->pool != commandPool) continue;
        /* Reset: back to the initial state, so what was recorded and not sent is gone (a
         * vkBeginCommandBuffer would discard it too, but nothing need follow). Destroyed: the
         * command buffer with it -- its wrapper is freed here, as the pool frees the host's. */
        if (cb->batch != NULL) {
            give_batch(cb->batch);
            cb->batch = NULL;
        }
        if (how == OMNI_VK_POOL_DESTROY) {
            unlink_locked(cb);
            free(cb);
        }
    }
    pthread_mutex_unlock(&g_cmdbufs_lock);
}

/* `args` is a generated command's OMNI_VK_FRAME (the only callers): sent as it is when not batched. */
void omni_vk_record(uint32_t id, const uint64_t* args, uint32_t argc, const struct omni_vk_copy* copies, uint32_t ncopies) {
    struct omni_vk_cmdbuf* cb = (struct omni_vk_cmdbuf*)(uintptr_t)args[0];
    if (cb == NULL || !cb->batching) {
        (void)omni_vk_call_framed(id, (uint64_t*)(uintptr_t)args, argc);
        return;
    }
    size_t need = 16u + (size_t)argc * 8u;
    for (uint32_t i = 0; i < ncopies; i++) {
        if (args[copies[i].arg] != 0) need += (copies[i].bytes + 7u) & ~(size_t)7u;
    }
    if (need > RECORD_MAX || argc > 32u) {
        send_batch(cb);
        (void)omni_vk_call_framed(id, (uint64_t*)(uintptr_t)args, argc);
        return;
    }
    if (cb->batch == NULL && (cb->batch = take_batch()) == NULL) {
        (void)omni_vk_call_framed(id, (uint64_t*)(uintptr_t)args, argc);
        return;
    }
    struct omni_vk_batch* b = cb->batch;
    if (b->used + need > BATCH_BYTES) send_batch(cb);
    uint8_t* r = b->data + b->used;
    const uint32_t head[4] = {id, argc, (uint32_t)need, 0};
    memcpy(r, head, sizeof head);
    uint64_t* a = (uint64_t*)(void*)(r + 16);
    memcpy(a, args, (size_t)argc * 8u);
    uint8_t* d = r + 16 + (size_t)argc * 8u;
    for (uint32_t i = 0; i < ncopies; i++) {
        const void* src = (const void*)(uintptr_t)args[copies[i].arg];
        if (src == NULL) continue;
        memcpy(d, src, copies[i].bytes);
        a[copies[i].arg] = OMNI_U64(d);
        d += (copies[i].bytes + 7u) & ~(size_t)7u;
    }
    b->used += (uint32_t)need;
    b->count++;
}

/* --- dispatchable wrappers ----------------------------------------------------------------- */

struct omni_vk_object* omni_vk_wrap(uint64_t host) {
    struct omni_vk_object* o = calloc(1, sizeof *o);
    if (o == NULL) return NULL;
    o->dispatch.magic = HWVULKAN_DISPATCH_MAGIC;
    o->host = host;
    return o;
}

struct omni_vk_cmdbuf* omni_vk_wrap_cmdbuf(void) {
    struct omni_vk_cmdbuf* cb = calloc(1, sizeof *cb);
    if (cb == NULL) return NULL;
    cb->obj.dispatch.magic = HWVULKAN_DISPATCH_MAGIC;
    return cb;
}

/* Every live command buffer wrapper, so a pool reset or destroyed can find its own (Vulkan has the
 * application synchronize a pool with all its command buffers, so the batches themselves need no
 * lock; the list is shared by every pool). */
void omni_vk_cmdbuf_live(struct omni_vk_cmdbuf* cb, uint64_t pool) {
    cb->pool = pool;
    pthread_mutex_lock(&g_cmdbufs_lock);
    cb->prev = NULL;
    cb->next = g_cmdbufs;
    if (g_cmdbufs != NULL) g_cmdbufs->prev = cb;
    g_cmdbufs = cb;
    pthread_mutex_unlock(&g_cmdbufs_lock);
}

static void unlink_locked(struct omni_vk_cmdbuf* cb) {
    if (cb->prev != NULL) cb->prev->next = cb->next;
    else if (g_cmdbufs == cb) g_cmdbufs = cb->next;
    if (cb->next != NULL) cb->next->prev = cb->prev;
    cb->prev = cb->next = NULL;
}

void omni_vk_cmdbuf_gone(struct omni_vk_cmdbuf* cb) {
    pthread_mutex_lock(&g_cmdbufs_lock);
    unlink_locked(cb);
    pthread_mutex_unlock(&g_cmdbufs_lock);
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
