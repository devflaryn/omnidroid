/* The platform's native window and buffer, as an EGL driver sees them: AOSP's
 * `nativebase/nativebase.h` and `system/window.h` (android15-release), cut to what egl.c uses. The
 * NDK ships only the opaque `ANativeWindow`; a driver is handed the struct and calls through it, as
 * every Android GLES driver does. */
#pragma once

#include <stdint.h>

#include <cutils/native_handle.h>

#define OMNI_MAGIC(a, b, c, d) (((unsigned)(a) << 24) | ((unsigned)(b) << 16) | ((unsigned)(c) << 8) | (unsigned)(d))
#define ANDROID_NATIVE_WINDOW_MAGIC OMNI_MAGIC('_', 'w', 'n', 'd')
#define ANDROID_NATIVE_BUFFER_MAGIC OMNI_MAGIC('_', 'b', 'f', 'r')

typedef struct android_native_base_t {
    int magic;
    int version;
    void* reserved[4];
    void (*incRef)(struct android_native_base_t* base);
    void (*decRef)(struct android_native_base_t* base);
} android_native_base_t;

typedef struct ANativeWindowBuffer {
    struct android_native_base_t common;
    int width;
    int height;
    int stride;
    int format;
    int usage_deprecated;
    uintptr_t layerCount;
    void* reserved[1];
    const native_handle_t* handle;
    uint64_t usage;
    void* reserved_proc[8 - (sizeof(uint64_t) / sizeof(void*))];
} ANativeWindowBuffer;

struct omni_native_window {
    struct android_native_base_t common;
    const uint32_t flags;
    const int minSwapInterval;
    const int maxSwapInterval;
    const float xdpi;
    const float ydpi;
    intptr_t oem[4];
    int (*setSwapInterval)(struct omni_native_window* window, int interval);
    int (*dequeueBuffer_DEPRECATED)(struct omni_native_window* window, ANativeWindowBuffer** buffer);
    int (*lockBuffer_DEPRECATED)(struct omni_native_window* window, ANativeWindowBuffer* buffer);
    int (*queueBuffer_DEPRECATED)(struct omni_native_window* window, ANativeWindowBuffer* buffer);
    int (*query)(const struct omni_native_window* window, int what, int* value);
    int (*perform)(struct omni_native_window* window, int operation, ...);
    int (*cancelBuffer_DEPRECATED)(struct omni_native_window* window, ANativeWindowBuffer* buffer);
    int (*dequeueBuffer)(struct omni_native_window* window, ANativeWindowBuffer** buffer, int* fenceFd);
    int (*queueBuffer)(struct omni_native_window* window, ANativeWindowBuffer* buffer, int fenceFd);
    int (*cancelBuffer)(struct omni_native_window* window, ANativeWindowBuffer* buffer, int fenceFd);
};

/* `query`'s questions and `perform`'s operations (system/window.h). */
#define NATIVE_WINDOW_WIDTH 0
#define NATIVE_WINDOW_HEIGHT 1
#define NATIVE_WINDOW_FORMAT 2
#define NATIVE_WINDOW_SET_USAGE64 30

/* gralloc usage a GLES driver's window buffers carry (hardware/gralloc.h). */
#define GRALLOC_USAGE_HW_TEXTURE 0x00000100u
#define GRALLOC_USAGE_HW_RENDER 0x00000200u

/* HAL pixel formats (system/graphics-base.h). */
#define HAL_PIXEL_FORMAT_RGBA_8888 1
#define HAL_PIXEL_FORMAT_RGBX_8888 2
#define HAL_PIXEL_FORMAT_RGB_565 4
#define HAL_PIXEL_FORMAT_BGRA_8888 5
