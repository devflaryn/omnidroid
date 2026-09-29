/* EGL 1.4 over the host's EGL (gl.h): one display, a fixed set of configs (each a descriptor the host
 * picks its own config for), contexts and pbuffers that are the host's, and window surfaces that
 * render into a host pbuffer whose frame eglSwapBuffers puts into the window's gralloc buffer.
 *
 * What Android's EGL loader does itself is not repeated here: it connects a window
 * (NATIVE_WINDOW_API_EGL) and sets its buffer format from EGL_NATIVE_VISUAL_ID before
 * eglCreateWindowSurface, disconnects it after eglDestroySurface, and answers
 * eglGetNativeClientBufferANDROID. */
#define EGL_EGLEXT_PROTOTYPES
#include <EGL/egl.h>
#include <EGL/eglext.h>

#include <poll.h>
#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <android/log.h>

#include "gl.h"
#include "window.h"

/* The EGL entry points are this library's exports (the NDK's EGLAPI carries no visibility, and the
 * library is built with -fvisibility=hidden). */
#pragma GCC visibility push(default)

#define LOGE(...) __android_log_print(ANDROID_LOG_ERROR, "omni-egl", __VA_ARGS__)
#define LOGI(...) __android_log_print(ANDROID_LOG_INFO, "omni-egl", __VA_ARGS__)

#ifndef EGL_RECORDABLE_ANDROID
#define EGL_RECORDABLE_ANDROID 0x3142
#endif
#ifndef EGL_FRAMEBUFFER_TARGET_ANDROID
#define EGL_FRAMEBUFFER_TARGET_ANDROID 0x3147
#endif
#ifndef EGL_NATIVE_BUFFER_ANDROID
#define EGL_NATIVE_BUFFER_ANDROID 0x3140
#endif

/* --- errors and the thread's current state ------------------------------------------------- */

static __thread EGLint t_error = EGL_SUCCESS;

#define FAIL(err, value)      \
    do {                      \
        t_error = (err);      \
        return (value);       \
    } while (0)

static int g_display_object;
#define DISPLAY ((EGLDisplay)&g_display_object)
static int g_initialized;
/* The host's GLES version, major << 8 | minor (eglInitialize). */
static uint32_t g_host_version;

struct config {
    EGLint id, r, g, b, a, depth, stencil, visual;
};

/* The configs this device offers: 8888 with and without alpha, and 565, each without and with a
 * depth/stencil buffer. No multisampled configs: a multisampled window cannot be read back as it is
 * (ES 3 refuses glReadPixels there). */
static const struct config k_configs[] = {
    {1, 8, 8, 8, 8, 0, 0, HAL_PIXEL_FORMAT_RGBA_8888},  {2, 8, 8, 8, 8, 24, 8, HAL_PIXEL_FORMAT_RGBA_8888},
    {3, 8, 8, 8, 8, 24, 0, HAL_PIXEL_FORMAT_RGBA_8888}, {4, 8, 8, 8, 8, 16, 0, HAL_PIXEL_FORMAT_RGBA_8888},
    {5, 8, 8, 8, 0, 0, 0, HAL_PIXEL_FORMAT_RGBX_8888},  {6, 8, 8, 8, 0, 24, 8, HAL_PIXEL_FORMAT_RGBX_8888},
    {7, 5, 6, 5, 0, 0, 0, HAL_PIXEL_FORMAT_RGB_565},    {8, 5, 6, 5, 0, 24, 8, HAL_PIXEL_FORMAT_RGB_565},
};
#define CONFIG_COUNT ((EGLint)(sizeof k_configs / sizeof *k_configs))

static const struct config* config_of(EGLConfig c) {
    for (EGLint i = 0; i < CONFIG_COUNT; i++) {
        if ((EGLConfig)&k_configs[i] == c) return &k_configs[i];
    }
    return NULL;
}

/* What the host picks its config by. */
static uint64_t descriptor(const struct config* c) {
    return (uint64_t)c->r | (uint64_t)c->g << 8 | (uint64_t)c->b << 16 | (uint64_t)c->a << 24 | (uint64_t)c->depth << 32 |
           (uint64_t)c->stencil << 40;
}

struct context {
    uint64_t host;
    const struct config* config; /* NULL: EGL_NO_CONFIG_KHR */
    EGLint major;
};

enum { SURFACE_WINDOW = 1, SURFACE_PBUFFER = 2 };

struct surface {
    int kind;
    uint64_t host;
    const struct config* config;
    struct omni_native_window* window;
    EGLint width, height;
};

struct image {
    uint32_t magic;
    ANativeWindowBuffer* buffer;
};
#define IMAGE_MAGIC 0x696d6721u

struct sync {
    uint64_t gl; /* the host's GLsync */
};

static __thread struct context* t_context;
static __thread struct surface* t_draw;
static __thread struct surface* t_read;

static int is_display(EGLDisplay dpy) {
    return dpy == DISPLAY;
}

#define CHECK_DISPLAY(dpy, value)                                     \
    do {                                                              \
        if (!is_display(dpy)) FAIL(EGL_BAD_DISPLAY, value);           \
        if (!g_initialized) FAIL(EGL_NOT_INITIALIZED, value);         \
    } while (0)

/* A host answer: the value, or FAIL with the EGL error it carries. */
static int host_failed(uint64_t r) {
    if (r & OMNI_GL_ERROR_BIT) {
        t_error = (EGLint)(r & 0xffff);
        return 1;
    }
    return 0;
}

/* GL commands the driver calls itself (forwarded like any other). */
extern void glFinish(void);
extern uint64_t glFenceSync(uint64_t condition, uint64_t flags);
extern uint64_t glClientWaitSync(uint64_t sync, uint64_t flags, uint64_t timeout);
extern void glWaitSync(uint64_t sync, uint64_t flags, uint64_t timeout);
extern void glDeleteSync(uint64_t sync);
extern void glGetSynciv(uint64_t sync, uint64_t pname, uint64_t count, uint64_t length, uint64_t values);

static void flush_images(void) {
    omni_gl_call(OMNI_GL_FLUSH_IMAGES, NULL, 0);
}

/* --- display ------------------------------------------------------------------------------- */

EGLDisplay eglGetDisplay(EGLNativeDisplayType display_id) {
    if (display_id != EGL_DEFAULT_DISPLAY) FAIL(EGL_BAD_PARAMETER, EGL_NO_DISPLAY);
    return DISPLAY;
}

EGLBoolean eglInitialize(EGLDisplay dpy, EGLint* major, EGLint* minor) {
    if (!is_display(dpy)) FAIL(EGL_BAD_DISPLAY, EGL_FALSE);
    if (!g_initialized) {
        uint64_t v = omni_gl_call(OMNI_GL_HELLO, (const uint64_t[]){omni_gl_fingerprint}, 1);
        if (v == 0 || host_failed(v)) {
            LOGE("the host has no GLES for this driver (answer %#llx)", (unsigned long long)v);
            FAIL(EGL_NOT_INITIALIZED, EGL_FALSE);
        }
        g_host_version = (uint32_t)v;
        g_initialized = 1;
        LOGI("host GLES %u.%u", g_host_version >> 8, g_host_version & 0xff);
    }
    if (major) *major = 1;
    if (minor) *minor = 4;
    return EGL_TRUE;
}

EGLBoolean eglTerminate(EGLDisplay dpy) {
    if (!is_display(dpy)) FAIL(EGL_BAD_DISPLAY, EGL_FALSE);
    /* Objects stay valid: one display lives as long as the process, as Android's does. */
    return EGL_TRUE;
}

static const char k_extensions[] =
    "EGL_KHR_image_base EGL_KHR_image EGL_ANDROID_image_native_buffer EGL_KHR_fence_sync EGL_KHR_wait_sync "
    "EGL_KHR_surfaceless_context EGL_KHR_no_config_context EGL_KHR_create_context EGL_ANDROID_recordable "
    "EGL_ANDROID_framebuffer_target";

const char* eglQueryString(EGLDisplay dpy, EGLint name) {
    CHECK_DISPLAY(dpy, NULL);
    switch (name) {
        case EGL_VENDOR:
            return "omnidroid";
        case EGL_VERSION:
            return "1.4 omnidroid (the host's GLES)";
        case EGL_CLIENT_APIS:
            return "OpenGL_ES";
        case EGL_EXTENSIONS:
            return k_extensions;
        default:
            FAIL(EGL_BAD_PARAMETER, NULL);
    }
}

EGLint eglGetError(void) {
    EGLint e = t_error;
    t_error = EGL_SUCCESS;
    return e;
}

/* --- configs ------------------------------------------------------------------------------- */

static EGLint renderable(void) {
    return EGL_OPENGL_ES2_BIT | ((g_host_version >> 8) >= 3 ? EGL_OPENGL_ES3_BIT_KHR : 0);
}

/* A config's attribute, or 0 with *known 0. */
static EGLint attribute(const struct config* c, EGLint attr, int* known) {
    *known = 1;
    switch (attr) {
        case EGL_BUFFER_SIZE: return c->r + c->g + c->b + c->a;
        case EGL_RED_SIZE: return c->r;
        case EGL_GREEN_SIZE: return c->g;
        case EGL_BLUE_SIZE: return c->b;
        case EGL_ALPHA_SIZE: return c->a;
        case EGL_DEPTH_SIZE: return c->depth;
        case EGL_STENCIL_SIZE: return c->stencil;
        case EGL_CONFIG_ID: return c->id;
        case EGL_CONFIG_CAVEAT: return EGL_NONE;
        case EGL_LEVEL: return 0;
        case EGL_MAX_PBUFFER_WIDTH: return 8192;
        case EGL_MAX_PBUFFER_HEIGHT: return 8192;
        case EGL_MAX_PBUFFER_PIXELS: return 8192 * 8192;
        case EGL_NATIVE_RENDERABLE: return EGL_TRUE;
        case EGL_NATIVE_VISUAL_ID: return c->visual;
        case EGL_NATIVE_VISUAL_TYPE: return 0;
        case EGL_SAMPLES: return 0;
        case EGL_SAMPLE_BUFFERS: return 0;
        case EGL_SURFACE_TYPE: return EGL_WINDOW_BIT | EGL_PBUFFER_BIT;
        case EGL_TRANSPARENT_TYPE: return EGL_NONE;
        case EGL_TRANSPARENT_RED_VALUE:
        case EGL_TRANSPARENT_GREEN_VALUE:
        case EGL_TRANSPARENT_BLUE_VALUE: return 0;
        case EGL_BIND_TO_TEXTURE_RGB:
        case EGL_BIND_TO_TEXTURE_RGBA: return EGL_FALSE;
        case EGL_MIN_SWAP_INTERVAL: return 0;
        case EGL_MAX_SWAP_INTERVAL: return 1;
        case EGL_LUMINANCE_SIZE: return 0;
        case EGL_ALPHA_MASK_SIZE: return 0;
        case EGL_COLOR_BUFFER_TYPE: return EGL_RGB_BUFFER;
        case EGL_RENDERABLE_TYPE: return renderable();
        case EGL_CONFORMANT: return renderable();
        case EGL_RECORDABLE_ANDROID: return EGL_TRUE;
        case EGL_FRAMEBUFFER_TARGET_ANDROID: return c->visual == HAL_PIXEL_FORMAT_RGBA_8888 ? EGL_TRUE : EGL_FALSE;
        default: *known = 0; return 0;
    }
}

EGLBoolean eglGetConfigs(EGLDisplay dpy, EGLConfig* configs, EGLint size, EGLint* num) {
    CHECK_DISPLAY(dpy, EGL_FALSE);
    if (num == NULL) FAIL(EGL_BAD_PARAMETER, EGL_FALSE);
    if (configs == NULL) {
        *num = CONFIG_COUNT;
        return EGL_TRUE;
    }
    EGLint n = 0;
    for (; n < size && n < CONFIG_COUNT; n++) configs[n] = (EGLConfig)&k_configs[n];
    *num = n;
    return EGL_TRUE;
}

EGLBoolean eglGetConfigAttrib(EGLDisplay dpy, EGLConfig config, EGLint attr, EGLint* value) {
    CHECK_DISPLAY(dpy, EGL_FALSE);
    const struct config* c = config_of(config);
    if (c == NULL) FAIL(EGL_BAD_CONFIG, EGL_FALSE);
    int known;
    EGLint v = attribute(c, attr, &known);
    if (!known) FAIL(EGL_BAD_ATTRIBUTE, EGL_FALSE);
    if (value) *value = v;
    return EGL_TRUE;
}

enum { MATCH_AT_LEAST, MATCH_EXACT, MATCH_MASK, MATCH_IGNORE };

static int match_kind(EGLint attr) {
    switch (attr) {
        case EGL_BUFFER_SIZE: case EGL_RED_SIZE: case EGL_GREEN_SIZE: case EGL_BLUE_SIZE: case EGL_ALPHA_SIZE:
        case EGL_DEPTH_SIZE: case EGL_STENCIL_SIZE: case EGL_SAMPLES: case EGL_SAMPLE_BUFFERS:
        case EGL_LUMINANCE_SIZE: case EGL_ALPHA_MASK_SIZE:
            return MATCH_AT_LEAST;
        case EGL_SURFACE_TYPE: case EGL_RENDERABLE_TYPE: case EGL_CONFORMANT:
            return MATCH_MASK;
        case EGL_MAX_PBUFFER_WIDTH: case EGL_MAX_PBUFFER_HEIGHT: case EGL_MAX_PBUFFER_PIXELS:
        case EGL_NATIVE_VISUAL_TYPE:
            return MATCH_IGNORE;
        default:
            return MATCH_EXACT;
    }
}

struct choice {
    const struct config* config;
    EGLint color_bits; /* of the components asked for */
};

static int by_preference(const void* x, const void* y) {
    const struct choice* a = x;
    const struct choice* b = y;
    if (a->color_bits != b->color_bits) return b->color_bits - a->color_bits;
    EGLint sa = a->config->r + a->config->g + a->config->b + a->config->a;
    EGLint sb = b->config->r + b->config->g + b->config->b + b->config->a;
    if (sa != sb) return sa - sb;
    if (a->config->depth != b->config->depth) return a->config->depth - b->config->depth;
    if (a->config->stencil != b->config->stencil) return a->config->stencil - b->config->stencil;
    return a->config->id - b->config->id;
}

EGLBoolean eglChooseConfig(EGLDisplay dpy, const EGLint* attribs, EGLConfig* configs, EGLint size, EGLint* num) {
    CHECK_DISPLAY(dpy, EGL_FALSE);
    if (num == NULL) FAIL(EGL_BAD_PARAMETER, EGL_FALSE);
    struct choice found[CONFIG_COUNT];
    EGLint n = 0;
    EGLint wanted_id = EGL_DONT_CARE;
    for (const EGLint* a = attribs; a && a[0] != EGL_NONE; a += 2) {
        if (a[0] == EGL_CONFIG_ID) wanted_id = a[1];
    }
    for (EGLint i = 0; i < CONFIG_COUNT; i++) {
        const struct config* c = &k_configs[i];
        int ok = 1;
        EGLint color_bits = 0;
        if (wanted_id != EGL_DONT_CARE) {
            /* EGL_CONFIG_ID alone decides (EGL 1.4 section 3.4.1.2). */
            ok = c->id == wanted_id;
        } else {
            for (const EGLint* a = attribs; ok && a && a[0] != EGL_NONE; a += 2) {
                if (a[1] == EGL_DONT_CARE) continue;
                int known;
                EGLint v = attribute(c, a[0], &known);
                if (!known) FAIL(EGL_BAD_ATTRIBUTE, EGL_FALSE);
                switch (match_kind(a[0])) {
                    case MATCH_AT_LEAST: ok = v >= a[1]; break;
                    case MATCH_MASK: ok = (v & a[1]) == a[1]; break;
                    case MATCH_EXACT: ok = v == a[1]; break;
                    default: break;
                }
                if (a[1] > 0 && (a[0] == EGL_RED_SIZE || a[0] == EGL_GREEN_SIZE || a[0] == EGL_BLUE_SIZE || a[0] == EGL_ALPHA_SIZE)) {
                    color_bits += v;
                }
            }
        }
        if (ok) found[n++] = (struct choice){c, color_bits};
    }
    qsort(found, (size_t)n, sizeof *found, by_preference);
    if (configs == NULL) {
        *num = n;
        return EGL_TRUE;
    }
    EGLint k = 0;
    for (; k < n && k < size; k++) configs[k] = (EGLConfig)found[k].config;
    *num = k;
    return EGL_TRUE;
}

/* --- contexts ------------------------------------------------------------------------------ */

EGLBoolean eglBindAPI(EGLenum api) {
    if (api != EGL_OPENGL_ES_API) FAIL(EGL_BAD_PARAMETER, EGL_FALSE);
    return EGL_TRUE;
}

EGLenum eglQueryAPI(void) {
    return EGL_OPENGL_ES_API;
}

EGLContext eglCreateContext(EGLDisplay dpy, EGLConfig config, EGLContext share, const EGLint* attribs) {
    CHECK_DISPLAY(dpy, EGL_NO_CONTEXT);
    const struct config* c = NULL;
    if (config != EGL_NO_CONFIG_KHR) {
        c = config_of(config);
        if (c == NULL) FAIL(EGL_BAD_CONFIG, EGL_NO_CONTEXT);
    }
    EGLint major = 1, minor = 0, flags = 0;
    for (const EGLint* a = attribs; a && a[0] != EGL_NONE; a += 2) {
        switch (a[0]) {
            case EGL_CONTEXT_CLIENT_VERSION: major = a[1]; break;
            case EGL_CONTEXT_MINOR_VERSION_KHR: minor = a[1]; break;
            case EGL_CONTEXT_FLAGS_KHR: flags = a[1]; break;
            default: break; /* priority, robustness, ...: a hint the host is not asked for */
        }
    }
    if (major < 2) {
        /* ES 1.x is not offered: no config has EGL_OPENGL_ES_BIT. */
        FAIL(EGL_BAD_CONFIG, EGL_NO_CONTEXT);
    }
    uint64_t share_host = 0;
    if (share != EGL_NO_CONTEXT) share_host = ((struct context*)share)->host;
    uint64_t r = omni_gl_call(OMNI_GL_CONTEXT_CREATE, (const uint64_t[]){share_host, (uint64_t)major, (uint64_t)minor, (uint64_t)flags}, 4);
    if (r == 0 || host_failed(r)) {
        if (r == 0) t_error = EGL_BAD_ALLOC;
        return EGL_NO_CONTEXT;
    }
    struct context* ctx = calloc(1, sizeof *ctx);
    if (ctx == NULL) FAIL(EGL_BAD_ALLOC, EGL_NO_CONTEXT);
    ctx->host = r;
    ctx->config = c;
    ctx->major = major;
    return (EGLContext)ctx;
}

EGLBoolean eglDestroyContext(EGLDisplay dpy, EGLContext context) {
    CHECK_DISPLAY(dpy, EGL_FALSE);
    if (context == EGL_NO_CONTEXT) FAIL(EGL_BAD_CONTEXT, EGL_FALSE);
    struct context* ctx = context;
    omni_gl_call(OMNI_GL_CONTEXT_DESTROY, (const uint64_t[]){ctx->host}, 1);
    /* The guest's record stays: a context current on another thread is still named by it. */
    return EGL_TRUE;
}

EGLBoolean eglQueryContext(EGLDisplay dpy, EGLContext context, EGLint attr, EGLint* value) {
    CHECK_DISPLAY(dpy, EGL_FALSE);
    if (context == EGL_NO_CONTEXT) FAIL(EGL_BAD_CONTEXT, EGL_FALSE);
    struct context* ctx = context;
    EGLint v;
    switch (attr) {
        case EGL_CONFIG_ID: v = ctx->config ? ctx->config->id : 0; break;
        case EGL_CONTEXT_CLIENT_TYPE: v = EGL_OPENGL_ES_API; break;
        case EGL_CONTEXT_CLIENT_VERSION: v = ctx->major; break;
        case EGL_RENDER_BUFFER: v = t_context == ctx && t_draw ? EGL_BACK_BUFFER : EGL_NONE; break;
        default: FAIL(EGL_BAD_ATTRIBUTE, EGL_FALSE);
    }
    if (value) *value = v;
    return EGL_TRUE;
}

/* --- surfaces ------------------------------------------------------------------------------ */

static struct surface* make_surface(int kind, const struct config* c, EGLint w, EGLint h) {
    if (w < 1) w = 1;
    if (h < 1) h = 1;
    uint64_t r = omni_gl_call(OMNI_GL_SURFACE_CREATE, (const uint64_t[]){descriptor(c), (uint64_t)w, (uint64_t)h}, 3);
    if (r == 0 || host_failed(r)) {
        if (r == 0) t_error = EGL_BAD_ALLOC;
        return NULL;
    }
    struct surface* s = calloc(1, sizeof *s);
    if (s == NULL) {
        omni_gl_call(OMNI_GL_SURFACE_DESTROY, (const uint64_t[]){r}, 1);
        t_error = EGL_BAD_ALLOC;
        return NULL;
    }
    s->kind = kind;
    s->host = r;
    s->config = c;
    s->width = w;
    s->height = h;
    return s;
}

static void window_size(struct omni_native_window* w, EGLint* width, EGLint* height) {
    int v = 0;
    *width = w->query(w, NATIVE_WINDOW_WIDTH, &v) == 0 ? v : 1;
    v = 0;
    *height = w->query(w, NATIVE_WINDOW_HEIGHT, &v) == 0 ? v : 1;
}

EGLSurface eglCreateWindowSurface(EGLDisplay dpy, EGLConfig config, EGLNativeWindowType win, const EGLint* attribs) {
    (void)attribs;
    CHECK_DISPLAY(dpy, EGL_NO_SURFACE);
    const struct config* c = config_of(config);
    if (c == NULL) FAIL(EGL_BAD_CONFIG, EGL_NO_SURFACE);
    struct omni_native_window* w = (struct omni_native_window*)win;
    if (w == NULL || w->common.magic != (int)ANDROID_NATIVE_WINDOW_MAGIC) FAIL(EGL_BAD_NATIVE_WINDOW, EGL_NO_SURFACE);
    /* The loader connected it and set its format; this driver renders into its buffers on the GPU. */
    w->perform(w, NATIVE_WINDOW_SET_USAGE64, (uint64_t)(GRALLOC_USAGE_HW_RENDER | GRALLOC_USAGE_HW_TEXTURE));
    EGLint width, height;
    window_size(w, &width, &height);
    struct surface* s = make_surface(SURFACE_WINDOW, c, width, height);
    if (s == NULL) return EGL_NO_SURFACE;
    s->window = w;
    w->common.incRef(&w->common);
    return (EGLSurface)s;
}

EGLSurface eglCreatePbufferSurface(EGLDisplay dpy, EGLConfig config, const EGLint* attribs) {
    CHECK_DISPLAY(dpy, EGL_NO_SURFACE);
    const struct config* c = config_of(config);
    if (c == NULL) FAIL(EGL_BAD_CONFIG, EGL_NO_SURFACE);
    EGLint w = 0, h = 0;
    for (const EGLint* a = attribs; a && a[0] != EGL_NONE; a += 2) {
        if (a[0] == EGL_WIDTH) w = a[1];
        else if (a[0] == EGL_HEIGHT) h = a[1];
        else if (a[0] == EGL_TEXTURE_FORMAT && a[1] != EGL_NO_TEXTURE) FAIL(EGL_BAD_MATCH, EGL_NO_SURFACE);
    }
    if (w < 0 || h < 0) FAIL(EGL_BAD_PARAMETER, EGL_NO_SURFACE);
    struct surface* s = make_surface(SURFACE_PBUFFER, c, w, h);
    return s ? (EGLSurface)s : EGL_NO_SURFACE;
}

EGLSurface eglCreatePixmapSurface(EGLDisplay dpy, EGLConfig config, EGLNativePixmapType pixmap, const EGLint* attribs) {
    (void)config;
    (void)pixmap;
    (void)attribs;
    CHECK_DISPLAY(dpy, EGL_NO_SURFACE);
    FAIL(EGL_BAD_MATCH, EGL_NO_SURFACE);
}

EGLSurface eglCreatePbufferFromClientBuffer(EGLDisplay dpy, EGLenum type, EGLClientBuffer buffer, EGLConfig config, const EGLint* attribs) {
    (void)type;
    (void)buffer;
    (void)config;
    (void)attribs;
    CHECK_DISPLAY(dpy, EGL_NO_SURFACE);
    FAIL(EGL_BAD_PARAMETER, EGL_NO_SURFACE);
}

EGLBoolean eglDestroySurface(EGLDisplay dpy, EGLSurface surface) {
    CHECK_DISPLAY(dpy, EGL_FALSE);
    if (surface == EGL_NO_SURFACE) FAIL(EGL_BAD_SURFACE, EGL_FALSE);
    struct surface* s = surface;
    omni_gl_call(OMNI_GL_SURFACE_DESTROY, (const uint64_t[]){s->host}, 1);
    if (s->window) {
        s->window->common.decRef(&s->window->common);
        s->window = NULL;
    }
    return EGL_TRUE;
}

EGLBoolean eglQuerySurface(EGLDisplay dpy, EGLSurface surface, EGLint attr, EGLint* value) {
    CHECK_DISPLAY(dpy, EGL_FALSE);
    if (surface == EGL_NO_SURFACE) FAIL(EGL_BAD_SURFACE, EGL_FALSE);
    struct surface* s = surface;
    EGLint v;
    switch (attr) {
        case EGL_WIDTH: v = s->width; break;
        case EGL_HEIGHT: v = s->height; break;
        case EGL_CONFIG_ID: v = s->config->id; break;
        case EGL_RENDER_BUFFER: v = EGL_BACK_BUFFER; break;
        case EGL_SWAP_BEHAVIOR: v = EGL_BUFFER_DESTROYED; break;
        case EGL_LARGEST_PBUFFER: v = EGL_FALSE; break;
        case EGL_TEXTURE_FORMAT: case EGL_TEXTURE_TARGET: v = EGL_NO_TEXTURE; break;
        case EGL_MIPMAP_TEXTURE: case EGL_MIPMAP_LEVEL: v = 0; break;
        case EGL_HORIZONTAL_RESOLUTION: case EGL_VERTICAL_RESOLUTION: case EGL_PIXEL_ASPECT_RATIO: v = EGL_UNKNOWN; break;
        case EGL_MULTISAMPLE_RESOLVE: v = EGL_MULTISAMPLE_RESOLVE_DEFAULT; break;
        case EGL_VG_COLORSPACE: v = EGL_VG_COLORSPACE_sRGB; break;
        case EGL_VG_ALPHA_FORMAT: v = EGL_VG_ALPHA_FORMAT_NONPRE; break;
        default: FAIL(EGL_BAD_ATTRIBUTE, EGL_FALSE);
    }
    if (value) *value = v;
    return EGL_TRUE;
}

EGLBoolean eglSurfaceAttrib(EGLDisplay dpy, EGLSurface surface, EGLint attr, EGLint value) {
    (void)attr;
    (void)value;
    CHECK_DISPLAY(dpy, EGL_FALSE);
    if (surface == EGL_NO_SURFACE) FAIL(EGL_BAD_SURFACE, EGL_FALSE);
    /* EGL_SWAP_BEHAVIOR stays destroyed (no config has EGL_SWAP_BEHAVIOR_PRESERVED_BIT); the rest
     * are hints. */
    return EGL_TRUE;
}

EGLBoolean eglBindTexImage(EGLDisplay dpy, EGLSurface surface, EGLint buffer) {
    (void)surface;
    (void)buffer;
    CHECK_DISPLAY(dpy, EGL_FALSE);
    FAIL(EGL_BAD_MATCH, EGL_FALSE);
}

EGLBoolean eglReleaseTexImage(EGLDisplay dpy, EGLSurface surface, EGLint buffer) {
    (void)surface;
    (void)buffer;
    CHECK_DISPLAY(dpy, EGL_FALSE);
    FAIL(EGL_BAD_MATCH, EGL_FALSE);
}

EGLBoolean eglSwapInterval(EGLDisplay dpy, EGLint interval) {
    (void)interval;
    CHECK_DISPLAY(dpy, EGL_FALSE);
    /* The window's buffer queue paces presents, as it does for every driver. */
    return EGL_TRUE;
}

/* --- current ------------------------------------------------------------------------------- */

EGLBoolean eglMakeCurrent(EGLDisplay dpy, EGLSurface draw, EGLSurface read, EGLContext context) {
    CHECK_DISPLAY(dpy, EGL_FALSE);
    struct context* ctx = context;
    struct surface* d = draw;
    struct surface* r = read;
    if (ctx == NULL && (d != NULL || r != NULL)) FAIL(EGL_BAD_MATCH, EGL_FALSE);
    if ((d == NULL) != (r == NULL)) FAIL(EGL_BAD_MATCH, EGL_FALSE);
    if (t_context != NULL && t_context != ctx) flush_images();
    uint64_t res = omni_gl_call(OMNI_GL_MAKE_CURRENT, (const uint64_t[]){d ? d->host : 0, r ? r->host : 0, ctx ? ctx->host : 0}, 3);
    if (res == 0 || host_failed(res)) {
        if (res == 0) t_error = EGL_BAD_ACCESS;
        return EGL_FALSE;
    }
    t_context = ctx;
    t_draw = d;
    t_read = r;
    return EGL_TRUE;
}

EGLContext eglGetCurrentContext(void) {
    return t_context ? (EGLContext)t_context : EGL_NO_CONTEXT;
}

EGLSurface eglGetCurrentSurface(EGLint which) {
    if (which == EGL_DRAW) return t_draw ? (EGLSurface)t_draw : EGL_NO_SURFACE;
    if (which == EGL_READ) return t_read ? (EGLSurface)t_read : EGL_NO_SURFACE;
    FAIL(EGL_BAD_PARAMETER, EGL_NO_SURFACE);
}

EGLDisplay eglGetCurrentDisplay(void) {
    return t_context ? DISPLAY : EGL_NO_DISPLAY;
}

EGLBoolean eglWaitGL(void) {
    if (t_context) {
        glFinish();
    }
    return EGL_TRUE;
}

EGLBoolean eglWaitClient(void) {
    return eglWaitGL();
}

EGLBoolean eglWaitNative(EGLint engine) {
    (void)engine;
    return EGL_TRUE;
}

EGLBoolean eglReleaseThread(void) {
    if (t_context) eglMakeCurrent(DISPLAY, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    t_error = EGL_SUCCESS;
    return EGL_TRUE;
}

/* --- presenting ---------------------------------------------------------------------------- */

EGLBoolean eglSwapBuffers(EGLDisplay dpy, EGLSurface surface) {
    CHECK_DISPLAY(dpy, EGL_FALSE);
    if (surface == EGL_NO_SURFACE) FAIL(EGL_BAD_SURFACE, EGL_FALSE);
    struct surface* s = surface;
    if (s->kind != SURFACE_WINDOW || s->window == NULL) {
        /* A pbuffer has nothing to present (EGL 1.4 section 3.9.1). */
        return EGL_TRUE;
    }
    if (t_context == NULL || t_draw != s) FAIL(EGL_BAD_SURFACE, EGL_FALSE);
    struct omni_native_window* w = s->window;
    ANativeWindowBuffer* buffer = NULL;
    int fence = -1;
    if (w->dequeueBuffer(w, &buffer, &fence) != 0 || buffer == NULL) FAIL(EGL_BAD_NATIVE_WINDOW, EGL_FALSE);
    if (fence >= 0) {
        /* The buffer's last reader is done with it once its fence signals. */
        struct pollfd p = {.fd = fence, .events = POLLIN};
        poll(&p, 1, 3000);
        close(fence);
    }
    uint64_t r = omni_gl_call(OMNI_GL_SWAP,
                              (const uint64_t[]){s->host, (uint64_t)(uintptr_t)buffer->handle, (uint64_t)buffer->width,
                                                 (uint64_t)buffer->height, (uint64_t)buffer->format},
                              5);
    if (r == 0 || host_failed(r)) {
        if (r == 0) t_error = EGL_BAD_SURFACE;
        w->cancelBuffer(w, buffer, -1);
        return EGL_FALSE;
    }
    if (w->queueBuffer(w, buffer, -1) != 0) FAIL(EGL_BAD_NATIVE_WINDOW, EGL_FALSE);
    /* The next frame is drawn at the window's size now (a resize shows in its next buffer). */
    EGLint width, height;
    window_size(w, &width, &height);
    if (width > 0 && height > 0 && (width != s->width || height != s->height)) {
        uint64_t rr = omni_gl_call(OMNI_GL_SURFACE_RESIZE, (const uint64_t[]){s->host, (uint64_t)width, (uint64_t)height}, 3);
        if (rr == 1) {
            s->width = width;
            s->height = height;
        }
    }
    return EGL_TRUE;
}

EGLBoolean eglCopyBuffers(EGLDisplay dpy, EGLSurface surface, EGLNativePixmapType target) {
    (void)surface;
    (void)target;
    CHECK_DISPLAY(dpy, EGL_FALSE);
    FAIL(EGL_BAD_NATIVE_PIXMAP, EGL_FALSE);
}

/* --- images -------------------------------------------------------------------------------- */

EGLImageKHR eglCreateImageKHR(EGLDisplay dpy, EGLContext ctx, EGLenum target, EGLClientBuffer buffer, const EGLint* attribs) {
    (void)attribs;
    CHECK_DISPLAY(dpy, EGL_NO_IMAGE_KHR);
    if (target != EGL_NATIVE_BUFFER_ANDROID) FAIL(EGL_BAD_PARAMETER, EGL_NO_IMAGE_KHR);
    if (ctx != EGL_NO_CONTEXT) FAIL(EGL_BAD_CONTEXT, EGL_NO_IMAGE_KHR);
    ANativeWindowBuffer* b = (ANativeWindowBuffer*)buffer;
    if (b == NULL || b->common.magic != (int)ANDROID_NATIVE_BUFFER_MAGIC || b->handle == NULL) {
        FAIL(EGL_BAD_PARAMETER, EGL_NO_IMAGE_KHR);
    }
    struct image* img = calloc(1, sizeof *img);
    if (img == NULL) FAIL(EGL_BAD_ALLOC, EGL_NO_IMAGE_KHR);
    img->magic = IMAGE_MAGIC;
    img->buffer = b;
    b->common.incRef(&b->common);
    return (EGLImageKHR)img;
}

EGLBoolean eglDestroyImageKHR(EGLDisplay dpy, EGLImageKHR image) {
    CHECK_DISPLAY(dpy, EGL_FALSE);
    struct image* img = image;
    if (img == NULL || img->magic != IMAGE_MAGIC) FAIL(EGL_BAD_PARAMETER, EGL_FALSE);
    omni_gl_call(OMNI_GL_IMAGE_DESTROY, (const uint64_t[]){(uint64_t)(uintptr_t)img}, 1);
    img->magic = 0;
    img->buffer->common.decRef(&img->buffer->common);
    free(img);
    return EGL_TRUE;
}

int omni_egl_image_info(const void* image, uint64_t* handle, uint32_t* width, uint32_t* height, uint32_t* format) {
    const struct image* img = image;
    if (img == NULL || img->magic != IMAGE_MAGIC) return 0;
    *handle = (uint64_t)(uintptr_t)img->buffer->handle;
    *width = (uint32_t)img->buffer->width;
    *height = (uint32_t)img->buffer->height;
    *format = (uint32_t)img->buffer->format;
    return 1;
}

/* --- fences: the host's GL syncs ----------------------------------------------------------- */

#define GL_SYNC_GPU_COMMANDS_COMPLETE 0x9117
#define GL_SYNC_FLUSH_COMMANDS_BIT 0x1
#define GL_TIMEOUT_IGNORED 0xFFFFFFFFFFFFFFFFull
#define GL_ALREADY_SIGNALED 0x911A
#define GL_TIMEOUT_EXPIRED 0x911B
#define GL_CONDITION_SATISFIED 0x911C
#define GL_SYNC_STATUS 0x9114
#define GL_SIGNALED 0x9119

EGLSyncKHR eglCreateSyncKHR(EGLDisplay dpy, EGLenum type, const EGLint* attribs) {
    CHECK_DISPLAY(dpy, EGL_NO_SYNC_KHR);
    if (type != EGL_SYNC_FENCE_KHR) FAIL(EGL_BAD_ATTRIBUTE, EGL_NO_SYNC_KHR);
    if (attribs != NULL && attribs[0] != EGL_NONE) FAIL(EGL_BAD_ATTRIBUTE, EGL_NO_SYNC_KHR);
    if (t_context == NULL) FAIL(EGL_BAD_MATCH, EGL_NO_SYNC_KHR);
    /* What the fence orders includes the images rendered so far reaching their buffers. */
    flush_images();
    struct sync* s = calloc(1, sizeof *s);
    if (s == NULL) FAIL(EGL_BAD_ALLOC, EGL_NO_SYNC_KHR);
    s->gl = glFenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0);
    return (EGLSyncKHR)s;
}

EGLBoolean eglDestroySyncKHR(EGLDisplay dpy, EGLSyncKHR sync) {
    CHECK_DISPLAY(dpy, EGL_FALSE);
    if (sync == EGL_NO_SYNC_KHR) FAIL(EGL_BAD_PARAMETER, EGL_FALSE);
    struct sync* s = sync;
    if (s->gl && t_context) glDeleteSync(s->gl);
    free(s);
    return EGL_TRUE;
}

EGLint eglClientWaitSyncKHR(EGLDisplay dpy, EGLSyncKHR sync, EGLint flags, EGLTimeKHR timeout) {
    CHECK_DISPLAY(dpy, EGL_FALSE);
    if (sync == EGL_NO_SYNC_KHR) FAIL(EGL_BAD_PARAMETER, EGL_FALSE);
    struct sync* s = sync;
    /* With no context here the fence cannot be asked: the frame's pixels already reached their
     * buffers when it was made (flush_images), which is what a waiter without a context waits for. */
    if (s->gl == 0 || t_context == NULL) return EGL_CONDITION_SATISFIED_KHR;
    uint64_t r = glClientWaitSync(s->gl, (flags & EGL_SYNC_FLUSH_COMMANDS_BIT_KHR) ? GL_SYNC_FLUSH_COMMANDS_BIT : 0, timeout);
    switch ((uint32_t)r) {
        case GL_ALREADY_SIGNALED:
        case GL_CONDITION_SATISFIED: return EGL_CONDITION_SATISFIED_KHR;
        case GL_TIMEOUT_EXPIRED: return EGL_TIMEOUT_EXPIRED_KHR;
        default: FAIL(EGL_BAD_PARAMETER, EGL_FALSE);
    }
}

EGLint eglWaitSyncKHR(EGLDisplay dpy, EGLSyncKHR sync, EGLint flags) {
    CHECK_DISPLAY(dpy, EGL_FALSE);
    if (sync == EGL_NO_SYNC_KHR || flags != 0) FAIL(EGL_BAD_PARAMETER, EGL_FALSE);
    struct sync* s = sync;
    if (s->gl && t_context) glWaitSync(s->gl, 0, GL_TIMEOUT_IGNORED);
    return EGL_TRUE;
}

EGLBoolean eglGetSyncAttribKHR(EGLDisplay dpy, EGLSyncKHR sync, EGLint attr, EGLint* value) {
    CHECK_DISPLAY(dpy, EGL_FALSE);
    if (sync == EGL_NO_SYNC_KHR || value == NULL) FAIL(EGL_BAD_PARAMETER, EGL_FALSE);
    struct sync* s = sync;
    switch (attr) {
        case EGL_SYNC_TYPE_KHR: *value = EGL_SYNC_FENCE_KHR; return EGL_TRUE;
        case EGL_SYNC_CONDITION_KHR: *value = EGL_SYNC_PRIOR_COMMANDS_COMPLETE_KHR; return EGL_TRUE;
        case EGL_SYNC_STATUS_KHR: {
            int32_t status = GL_SIGNALED;
            if (s->gl && t_context) glGetSynciv(s->gl, GL_SYNC_STATUS, 1, 0, (uint64_t)(uintptr_t)&status);
            *value = status == GL_SIGNALED ? EGL_SIGNALED_KHR : EGL_UNSIGNALED_KHR;
            return EGL_TRUE;
        }
        default: FAIL(EGL_BAD_ATTRIBUTE, EGL_FALSE);
    }
}

/* --- proc addresses ------------------------------------------------------------------------ */

struct egl_proc {
    const char* name;
    __eglMustCastToProperFunctionPointerType fn;
};

#define P(f) {#f, (__eglMustCastToProperFunctionPointerType)f}
/* Sorted by name. */
static const struct egl_proc k_egl_procs[] = {
    P(eglBindAPI), P(eglBindTexImage), P(eglChooseConfig), P(eglClientWaitSyncKHR), P(eglCopyBuffers),
    P(eglCreateContext), P(eglCreateImageKHR), P(eglCreatePbufferFromClientBuffer), P(eglCreatePbufferSurface),
    P(eglCreatePixmapSurface), P(eglCreateSyncKHR), P(eglCreateWindowSurface), P(eglDestroyContext),
    P(eglDestroyImageKHR), P(eglDestroySurface), P(eglDestroySyncKHR), P(eglGetConfigAttrib), P(eglGetConfigs),
    P(eglGetCurrentContext), P(eglGetCurrentDisplay), P(eglGetCurrentSurface), P(eglGetDisplay), P(eglGetError),
    P(eglGetProcAddress), P(eglGetSyncAttribKHR), P(eglInitialize), P(eglMakeCurrent), P(eglQueryAPI),
    P(eglQueryContext), P(eglQueryString), P(eglQuerySurface), P(eglReleaseTexImage), P(eglReleaseThread),
    P(eglSurfaceAttrib), P(eglSwapBuffers), P(eglSwapInterval), P(eglTerminate), P(eglWaitClient), P(eglWaitGL),
    P(eglWaitNative), P(eglWaitSyncKHR),
};

static int egl_by_name(const void* key, const void* entry) {
    return strcmp((const char*)key, ((const struct egl_proc*)entry)->name);
}

__eglMustCastToProperFunctionPointerType eglGetProcAddress(const char* name) {
    if (name == NULL) return NULL;
    if (name[0] == 'e') {
        const struct egl_proc* p = bsearch(name, k_egl_procs, sizeof k_egl_procs / sizeof *k_egl_procs, sizeof *p, egl_by_name);
        return p ? p->fn : NULL;
    }
    uint32_t id = omni_gl_id(name);
    return id == UINT32_MAX ? NULL : (__eglMustCastToProperFunctionPointerType)omni_gl_procs[id].fn;
}

#pragma GCC visibility pop
