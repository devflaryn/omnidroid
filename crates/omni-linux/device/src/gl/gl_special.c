/* The transport, and the GL commands the host cannot take as they are (gl.h says which and why). */
#include "gl.h"

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include <android/log.h>

#define LOGE(...) __android_log_print(ANDROID_LOG_ERROR, "omni-gl", __VA_ARGS__)

/* --- the transport ------------------------------------------------------------------------- */

static int g_fd = -1;
static pthread_once_t g_open_once = PTHREAD_ONCE_INIT;

static void open_device(void) {
    g_fd = open("/dev/omni-gpu", O_RDWR | O_CLOEXEC);
    if (g_fd < 0) LOGE("/dev/omni-gpu: %s", strerror(errno));
}

uint64_t omni_gl_call(uint32_t id, const uint64_t* args, uint32_t argc) {
    pthread_once(&g_open_once, open_device);
    struct omni_gl_call_args c = {.command = id, .argc = argc, .args = (uint64_t)(uintptr_t)args, .result = 0, .reserved = 0};
    if (g_fd < 0 || ioctl(g_fd, OMNI_GL_CALL, &c) != 0) {
        static int logged;
        if (logged++ < 16) LOGE("command %u: %s", id, g_fd < 0 ? "no /dev/omni-gpu" : strerror(errno));
        return 0;
    }
    return c.result;
}

static int by_name(const void* key, const void* entry) {
    return strcmp((const char*)key, ((const struct omni_gl_proc*)entry)->name);
}

uint32_t omni_gl_id(const char* name) {
    const struct omni_gl_proc* p = bsearch(name, omni_gl_procs, omni_gl_proc_count, sizeof *p, by_name);
    return p == NULL ? UINT32_MAX : (uint32_t)(p - omni_gl_procs);
}

#define CALL(...) omni_gl_call(__VA_ARGS__)

/* --- strings: the host's, copied once into guest memory ------------------------------------ */

#define GL_EXTENSIONS 0x1F03

struct cached_string {
    uint32_t name;
    uint32_t index; /* UINT32_MAX: glGetString's */
    char* text;
};

static pthread_mutex_t g_strings_lock = PTHREAD_MUTEX_INITIALIZER;
static struct cached_string* g_strings;
static size_t g_string_count, g_string_capacity;

/* The host's string for (name, index), kept for the process's life: GL's strings are constant, and a
 * pointer glGetString returned stays valid. NULL while the host has none (no current context). */
static const char* host_string(uint32_t name, uint32_t index) {
    pthread_mutex_lock(&g_strings_lock);
    for (size_t i = 0; i < g_string_count; i++) {
        if (g_strings[i].name == name && g_strings[i].index == index) {
            const char* found = g_strings[i].text;
            pthread_mutex_unlock(&g_strings_lock);
            return found;
        }
    }
    pthread_mutex_unlock(&g_strings_lock);
    uint64_t need = CALL(OMNI_GL_GET_STRING, (const uint64_t[]){name, index, 0, 0}, 4);
    if (need == 0) return NULL;
    char* text = malloc(need);
    if (text == NULL) return NULL;
    uint64_t got = CALL(OMNI_GL_GET_STRING, (const uint64_t[]){name, index, (uint64_t)(uintptr_t)text, need}, 4);
    if (got == 0 || got > need) {
        free(text);
        return NULL;
    }
    pthread_mutex_lock(&g_strings_lock);
    if (g_string_count == g_string_capacity) {
        size_t cap = g_string_capacity ? g_string_capacity * 2 : 32;
        struct cached_string* grown = realloc(g_strings, cap * sizeof *grown);
        if (grown == NULL) {
            pthread_mutex_unlock(&g_strings_lock);
            return text; /* not kept: leaks one string rather than failing the call */
        }
        g_strings = grown;
        g_string_capacity = cap;
    }
    g_strings[g_string_count++] = (struct cached_string){name, index, text};
    pthread_mutex_unlock(&g_strings_lock);
    return text;
}

OMNI_GL_EXPORT const unsigned char* glGetString(uint32_t name) {
    return (const unsigned char*)host_string(name, UINT32_MAX);
}

OMNI_GL_EXPORT const unsigned char* glGetStringi(uint32_t name, uint32_t index) {
    return (const unsigned char*)host_string(name, index);
}

/* --- buffer maps: a guest shadow the host copies in and out -------------------------------- */

#define GL_BUFFER_SIZE 0x8764
#define GL_MAP_READ_BIT 0x0001
#define GL_MAP_WRITE_BIT 0x0002

OMNI_GL_EXPORT void* glMapBufferRange(uint32_t target, intptr_t offset, intptr_t length, uint32_t access) {
    if (length <= 0) {
        /* The host's own error for an empty or negative range. */
        CALL(OMNI_GL_MAP, (const uint64_t[]){target, (uint64_t)offset, (uint64_t)length, access, 0}, 5);
        return NULL;
    }
    void* shadow = NULL;
    if (posix_memalign(&shadow, 64, (size_t)length) != 0) return NULL;
    if (CALL(OMNI_GL_MAP, (const uint64_t[]){target, (uint64_t)offset, (uint64_t)length, access, (uint64_t)(uintptr_t)shadow}, 5) != 1) {
        free(shadow);
        return NULL;
    }
    return shadow;
}

OMNI_GL_EXPORT void* glMapBufferRangeEXT(uint32_t target, intptr_t offset, intptr_t length, uint32_t access) {
    return glMapBufferRange(target, offset, length, access);
}

OMNI_GL_EXPORT void glGetBufferParameteriv(uint64_t target, uint64_t pname, uint64_t params);

/* OES_mapbuffer: the whole buffer, write-only. */
OMNI_GL_EXPORT void* glMapBufferOES(uint32_t target, uint32_t access) {
    (void)access;
    int32_t size = 0;
    glGetBufferParameteriv(target, GL_BUFFER_SIZE, (uint64_t)(uintptr_t)&size);
    return glMapBufferRange(target, 0, size, GL_MAP_WRITE_BIT);
}

OMNI_GL_EXPORT uint8_t glUnmapBuffer(uint32_t target) {
    uint64_t r = CALL(OMNI_GL_UNMAP, (const uint64_t[]){target}, 1);
    void* shadow = (void*)(uintptr_t)(r & ~(1ull << 63));
    free(shadow);
    return (r >> 63) ? 1 : 0;
}

OMNI_GL_EXPORT uint8_t glUnmapBufferOES(uint32_t target) {
    return glUnmapBuffer(target);
}

OMNI_GL_EXPORT void glFlushMappedBufferRange(uint32_t target, intptr_t offset, intptr_t length) {
    CALL(OMNI_GL_FLUSH_MAPPED, (const uint64_t[]){target, (uint64_t)offset, (uint64_t)length}, 3);
}

OMNI_GL_EXPORT void glFlushMappedBufferRangeEXT(uint32_t target, intptr_t offset, intptr_t length) {
    glFlushMappedBufferRange(target, offset, length);
}

#define GL_BUFFER_MAP_POINTER 0x88BD

OMNI_GL_EXPORT void glGetBufferPointerv(uint32_t target, uint32_t pname, void** params) {
    if (params == NULL) return;
    *params = pname == GL_BUFFER_MAP_POINTER ? (void*)(uintptr_t)CALL(OMNI_GL_BUFFER_POINTER, (const uint64_t[]){target}, 1) : NULL;
}

OMNI_GL_EXPORT void glGetBufferPointervOES(uint32_t target, uint32_t pname, void** params) {
    glGetBufferPointerv(target, pname, params);
}

/* --- debug callbacks: a guest function the host cannot call -------------------------------- */

/* The host never calls guest code; the messages are not delivered (GL_KHR_debug stays usable for
 * labels, groups and glGetDebugMessageLog, which forward). */
OMNI_GL_EXPORT void glDebugMessageCallback(void* callback, const void* user) {
    (void)callback;
    (void)user;
}

OMNI_GL_EXPORT void glDebugMessageCallbackKHR(void* callback, const void* user) {
    (void)callback;
    (void)user;
}

/* --- EGLImages: a gralloc buffer, which the host makes a texture of ------------------------ */

static void image_target(uint64_t kind, uint32_t target, void* image) {
    uint64_t handle = 0;
    uint32_t w = 0, h = 0, format = 0;
    if (!omni_egl_image_info(image, &handle, &w, &h, &format)) {
        /* Not one of this driver's images: the host sees a null image and sets GL's error. */
        handle = 0;
    }
    CALL(OMNI_GL_IMAGE_TARGET, (const uint64_t[]){kind, target, (uint64_t)(uintptr_t)image, handle, w, h, format}, 7);
}

OMNI_GL_EXPORT void glEGLImageTargetTexture2DOES(uint32_t target, void* image) {
    image_target(0, target, image);
}

OMNI_GL_EXPORT void glEGLImageTargetRenderbufferStorageOES(uint32_t target, void* image) {
    image_target(1, target, image);
}

/* GL_EXT_EGL_image_storage is not offered (the host withholds it from GL_EXTENSIONS). */
OMNI_GL_EXPORT void glEGLImageTargetTexStorageEXT(uint32_t target, void* image, const int32_t* attribs) {
    (void)attribs;
    image_target(0, target, image);
}

OMNI_GL_EXPORT void glEGLImageTargetTextureStorageEXT(uint32_t texture, void* image, const int32_t* attribs) {
    (void)texture;
    (void)image;
    (void)attribs;
}
