/* omnidroid's guest Vulkan driver: what driver.c and special.c share.
 *
 * Design: docs/superpowers/specs/2026-09-27-d3a-guest-vulkan-design.md. Each command is forwarded
 * to the host's Vulkan through ioctl(/dev/omni-gpu, OMNI_GPU_CALL); the host side is
 * crates/omni-linux/src/gpu/. */
#pragma once
#ifndef VK_USE_PLATFORM_ANDROID_KHR
#define VK_USE_PLATFORM_ANDROID_KHR
#endif
#include <stdint.h>
#include <pthread.h>
#include <hardware/hwvulkan.h>
#include <vulkan/vk_android_native_buffer.h>
#include "generated.h"

/* The ioctl and its argument (crates/omni-linux/src/gpu/mod.rs). */
#define OMNI_GPU_CALL 0xc0204701u
struct omni_gpu_call {
    uint32_t command;
    uint32_t argc;
    uint64_t args;
    uint64_t result;
    uint64_t reserved;
};
_Static_assert(sizeof(struct omni_gpu_call) == 32, "the transport's layout");

/* Commands with no id in vk.xml (VK_ANDROID_native_buffer is "disabled" there); the host's
 * gpu/special.rs numbers them the same. */
#define OMNI_VK_ID_GRALLOC_USAGE 0x1000u
#define OMNI_VK_ID_GRALLOC_USAGE2 0x1001u
#define OMNI_VK_ID_GRALLOC_USAGE3 0x1002u
#define OMNI_VK_ID_GRALLOC_USAGE4 0x1003u
#define OMNI_VK_ID_ACQUIRE_IMAGE 0x1004u
#define OMNI_VK_ID_QUEUE_SIGNAL_RELEASE_IMAGE 0x1005u
/* A command buffer's batch: (command buffer, records, bytes, count) -- driver.c's omni_vk_record. */
#define OMNI_VK_ID_BATCH 0x1006u
/* What the host wants of this driver: bit 0, batch (OMNI_VK_BATCH / the vk_batch lever). */
#define OMNI_VK_ID_CONFIG 0x1007u

/* A dispatchable handle as this driver hands it out: the loader owns `dispatch`, the host's handle
 * is at +8 (the host reads it there). */
struct omni_vk_object {
    hwvulkan_dispatch_t dispatch;
    uint64_t host;
};
_Static_assert(__builtin_offsetof(struct omni_vk_object, host) == 8, "the host reads +8");

/* An instance or device, with the physical devices or queues handed out under it (kept, so each
 * host handle has one wrapper, and freed with it). */
struct omni_vk_parent {
    struct omni_vk_object obj;
    pthread_mutex_t lock;
    struct omni_vk_object** children;
    uint32_t count;
    uint32_t capacity;
};

/* A command buffer as this driver hands it out: the object, and the commands recorded into it and
 * not yet sent (batching, driver.c). Vulkan has the application synchronize all use of a command
 * buffer, so neither field needs a lock. */
struct omni_vk_batch;
struct omni_vk_cmdbuf {
    struct omni_vk_object obj;
    struct omni_vk_batch* batch;
    /* Whether this recording batches: asked of the host at each vkBeginCommandBuffer. */
    int batching;
    /* The pool it came from, and its place in the list of live command buffers (driver.c's
     * g_cmdbufs): a pool reset or destroyed finds its command buffers there. */
    uint64_t pool;
    struct omni_vk_cmdbuf* prev;
    struct omni_vk_cmdbuf* next;
};

uint64_t omni_vk_call(uint32_t id, const uint64_t* args, uint32_t argc);
/* A new command buffer wrapper (calloc'd: no batch, not batching, in no list). */
struct omni_vk_cmdbuf* omni_vk_wrap_cmdbuf(void);
/* Into / out of the list of live command buffers (allocated from `pool`). */
void omni_vk_cmdbuf_live(struct omni_vk_cmdbuf* cb, uint64_t pool);
void omni_vk_cmdbuf_gone(struct omni_vk_cmdbuf* cb);
/* A forwarded VkResult: the host's value, or VK_ERROR_DEVICE_LOST when the call could not be made. */
VkResult omni_vk_result(uint64_t r);
/* Whether the last call on this thread failed at the transport (not a Vulkan result). */
int omni_vk_failed(void);

struct omni_vk_object* omni_vk_wrap(uint64_t host);
/* The wrapper of `host` under `parent`, made once. NULL when out of memory. */
struct omni_vk_object* omni_vk_child(struct omni_vk_parent* parent, uint64_t host);
void omni_vk_free_children(struct omni_vk_parent* parent);

#define OMNI_U64(x) ((uint64_t)(uintptr_t)(x))
