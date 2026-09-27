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

uint64_t omni_vk_call(uint32_t id, const uint64_t* args, uint32_t argc);
/* A forwarded VkResult: the host's value, or VK_ERROR_DEVICE_LOST when the call could not be made. */
VkResult omni_vk_result(uint64_t r);
/* Whether the last call on this thread failed at the transport (not a Vulkan result). */
int omni_vk_failed(void);

struct omni_vk_object* omni_vk_wrap(uint64_t host);
/* The wrapper of `host` under `parent`, made once. NULL when out of memory. */
struct omni_vk_object* omni_vk_child(struct omni_vk_parent* parent, uint64_t host);
void omni_vk_free_children(struct omni_vk_parent* parent);

#define OMNI_U64(x) ((uint64_t)(uintptr_t)(x))
