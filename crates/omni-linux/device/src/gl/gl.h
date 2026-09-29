/* omnidroid's guest GLES driver (the GL fallback of the real-AOSP path): `libGLES_omni.so`, an
 * all-in-one EGL + GLES driver that Android's EGL loader takes when `ro.hardware.egl=omni`.
 *
 * Every GLES command is forwarded to the host's GLES as one `ioctl(/dev/omni-gpu, OMNI_GL_CALL)`:
 * the command's id and its arguments as 64-bit values (generated.c, from the same table the host's
 * typed callers are generated from). Guest and host share one address space, so a pointer argument
 * is passed as it is and the host's driver reads and writes the guest's memory where it lies. What
 * the host cannot take as it is -- host memory handed back (strings, buffer maps), a guest function
 * (debug callbacks), a gralloc buffer (EGLImages, window surfaces) -- is gl_special.c's and egl.c's,
 * with host-side counterparts in `omni-linux`'s `gpu::gl`. */
#pragma once

#include <stdint.h>
#include <string.h>

#define OMNI_GL_EXPORT __attribute__((visibility("default")))

/* `_IOWR('G', 2, struct omni_gpu_call)`: the same 32-byte call as the Vulkan driver's
 * `OMNI_GPU_CALL`, on the same device node. */
#define OMNI_GL_CALL 0xc0204702u

struct omni_gl_call_args {
    uint32_t command;
    uint32_t argc;
    uint64_t args;
    uint64_t result;
    uint64_t reserved;
};

/* The driver's own requests of the host (`gpu::gl::special`), above every GL command's id. */
enum {
    OMNI_GL_HELLO = 0x10000,       /* (fingerprint) -> host GLES version (major << 8 | minor), or 0 */
    OMNI_GL_CONTEXT_CREATE,        /* (share, major, minor, flags) -> context id, or an error */
    OMNI_GL_CONTEXT_DESTROY,       /* (context) */
    OMNI_GL_SURFACE_CREATE,        /* (config descriptor, width, height) -> surface id, or an error */
    OMNI_GL_SURFACE_DESTROY,       /* (surface) */
    OMNI_GL_MAKE_CURRENT,          /* (draw, read, context) -> 1, or an error */
    OMNI_GL_SWAP,                  /* (surface, native_handle_t*, width, height, format) -> 1, or an error */
    OMNI_GL_SURFACE_RESIZE,        /* (surface, width, height) -> 1, or an error */
    OMNI_GL_GET_STRING,            /* (name, index, buffer, capacity) -> length (+1 for the NUL), or 0 */
    OMNI_GL_MAP,                   /* (target, offset, length, access, shadow) -> 1, or 0 */
    OMNI_GL_UNMAP,                 /* (target) -> the shadow to free, or 0; bit 63: glUnmapBuffer's answer */
    OMNI_GL_FLUSH_MAPPED,          /* (target, offset, length) */
    OMNI_GL_BUFFER_POINTER,        /* (target) -> the shadow, or 0 */
    OMNI_GL_IMAGE_TARGET,          /* (kind: 0 texture / 1 renderbuffer, target, image, native_handle_t*, width, height, format) */
    OMNI_GL_IMAGE_DESTROY,         /* (image) */
    OMNI_GL_FLUSH_IMAGES,          /* () the images the current context rendered into, back into their buffers */
};

/* A host answer with this bit set is an EGL error (its low 16 bits). */
#define OMNI_GL_ERROR_BIT (1ull << 63)

uint64_t omni_gl_call(uint32_t id, const uint64_t* args, uint32_t argc);

static inline uint64_t omni_gl_f32(float f) {
    uint32_t b;
    memcpy(&b, &f, 4);
    return b;
}

static inline uint64_t omni_gl_f64(double d) {
    uint64_t b;
    memcpy(&b, &d, 8);
    return b;
}

static inline float omni_gl_as_f32(uint64_t r) {
    uint32_t b = (uint32_t)r;
    float f;
    memcpy(&f, &b, 4);
    return f;
}

struct omni_gl_proc {
    const char* name;
    void (*fn)(void);
};

extern const uint64_t omni_gl_fingerprint;
extern const uint32_t omni_gl_command_count;
extern const struct omni_gl_proc omni_gl_procs[];
extern const uint32_t omni_gl_proc_count;

/* The id of a GL command forwarded by hand (gl_special.c): its index in the table. */
uint32_t omni_gl_id(const char* name);

/* egl.c: the host version eglInitialize learned, and the images the specials name. */
struct omni_egl_image;
int omni_egl_image_info(const void* image, uint64_t* handle, uint32_t* width, uint32_t* height, uint32_t* format);
