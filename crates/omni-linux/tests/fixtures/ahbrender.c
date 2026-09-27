/* D3a gate fixture: GLES renders into a gralloc buffer, as SurfaceFlinger's RenderEngine renders
 * into every output buffer. An AHardwareBuffer (D2's allocator), an EGLImage of it
 * (EGL_ANDROID_get_native_client_buffer, EGL_KHR_image_base), a framebuffer on a texture of that
 * image, glClear to (0.25, 0.5, 0.75, 1.0), glFinish; then the buffer locked for CPU reading must
 * show the colour. Prints "ahb render ok stride=<s>" and waits for /data/local/tmp/ahbrender.go
 * (the host reads the same buffer meanwhile), then exits 0; or names the failed step, exits 1. */
#define EGL_EGLEXT_PROTOTYPES
#define GL_GLEXT_PROTOTYPES
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <GLES2/gl2ext.h>
#include <android/hardware_buffer.h>
#include <stdint.h>
#include <stdio.h>
#include <time.h>
#include <unistd.h>

#define FAIL(...)            \
    do {                     \
        printf(__VA_ARGS__); \
        printf("\n");        \
        fflush(stdout);      \
        return 1;            \
    } while (0)

enum { W = 64, H = 32 };

int main(void) {
    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, NULL, NULL)) FAIL("eglInitialize failed 0x%x", eglGetError());
    const EGLint config_attribs[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT, EGL_NONE};
    EGLConfig config;
    EGLint n = 0;
    if (!eglChooseConfig(dpy, config_attribs, &config, 1, &n) || n == 0) FAIL("eglChooseConfig failed 0x%x", eglGetError());
    const EGLint pbuffer_attribs[] = {EGL_WIDTH, 1, EGL_HEIGHT, 1, EGL_NONE};
    EGLSurface surface = eglCreatePbufferSurface(dpy, config, pbuffer_attribs);
    const EGLint context_attribs[] = {EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, config, EGL_NO_CONTEXT, context_attribs);
    if (surface == EGL_NO_SURFACE || ctx == EGL_NO_CONTEXT || !eglMakeCurrent(dpy, surface, surface, ctx)) {
        FAIL("context failed 0x%x", eglGetError());
    }

    AHardwareBuffer_Desc desc = {.width = W, .height = H, .layers = 1, .format = AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM,
                                 .usage = AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE | AHARDWAREBUFFER_USAGE_GPU_COLOR_OUTPUT |
                                          AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN};
    AHardwareBuffer* buf = NULL;
    int status = AHardwareBuffer_allocate(&desc, &buf);
    if (status != 0) FAIL("allocate failed %d", status);
    AHardwareBuffer_describe(buf, &desc);

    EGLClientBuffer client = eglGetNativeClientBufferANDROID(buf);
    const EGLint image_attribs[] = {EGL_IMAGE_PRESERVED_KHR, EGL_TRUE, EGL_NONE};
    EGLImageKHR image = eglCreateImageKHR(dpy, EGL_NO_CONTEXT, EGL_NATIVE_BUFFER_ANDROID, client, image_attribs);
    if (image == EGL_NO_IMAGE_KHR) FAIL("eglCreateImageKHR failed 0x%x", eglGetError());
    GLuint tex, fbo;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glEGLImageTargetTexture2DOES(GL_TEXTURE_2D, (GLeglImageOES)image);
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
    GLenum fb = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    if (fb != GL_FRAMEBUFFER_COMPLETE) FAIL("framebuffer incomplete 0x%x", fb);
    glViewport(0, 0, W, H);
    glClearColor(0.25f, 0.5f, 0.75f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glFinish();
    GLenum err = glGetError();
    if (err != GL_NO_ERROR) FAIL("GL error 0x%x", err);

    void* addr = NULL;
    status = AHardwareBuffer_lock(buf, AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN, -1, NULL, &addr);
    if (status != 0) FAIL("lock failed %d", status);
    for (uint32_t y = 0; y < H; y++) {
        for (uint32_t x = 0; x < W; x++) {
            const uint8_t* q = (const uint8_t*)addr + (y * desc.stride + x) * 4;
            if (!(q[0] >= 63 && q[0] <= 65 && q[1] >= 127 && q[1] <= 129 && q[2] >= 190 && q[2] <= 192 && q[3] == 255)) {
                FAIL("pixel %u,%u is %u %u %u %u", x, y, q[0], q[1], q[2], q[3]);
            }
        }
    }
    AHardwareBuffer_unlock(buf, NULL);
    printf("ahb render ok stride=%u\n", desc.stride);
    fflush(stdout);

    for (int i = 0; i < 6000 && access("/data/local/tmp/ahbrender.go", F_OK) != 0; i++) {
        struct timespec ts = {0, 10 * 1000 * 1000};
        nanosleep(&ts, NULL);
    }
    glDeleteFramebuffers(1, &fbo);
    glDeleteTextures(1, &tex);
    eglDestroyImageKHR(dpy, image);
    AHardwareBuffer_release(buf);
    eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    eglDestroyContext(dpy, ctx);
    eglDestroySurface(dpy, surface);
    eglTerminate(dpy);
    return 0;
}
