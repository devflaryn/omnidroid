/* D3a gate fixture: GLES through the real EGL loader (libEGL.so), which takes the image's ANGLE as
 * the driver (ro.hardware.egl=angle), on the device's Vulkan. Clears a 32x32 pbuffer to
 * (0.25, 0.5, 0.75, 1.0) and reads it back. Prints "gles ok <GL_RENDERER> <r> <g> <b> <a>" and
 * exits 0, or names the step that failed and exits 1. */
#include <EGL/egl.h>
#include <GLES3/gl3.h>
#include <stdio.h>

#define FAIL(...)                  \
    do {                           \
        printf(__VA_ARGS__);       \
        printf("\n");              \
        fflush(stdout);            \
        return 1;                  \
    } while (0)

int main(void) {
    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (dpy == EGL_NO_DISPLAY) FAIL("eglGetDisplay failed 0x%x", eglGetError());
    EGLint major = 0, minor = 0;
    if (!eglInitialize(dpy, &major, &minor)) FAIL("eglInitialize failed 0x%x", eglGetError());
    const EGLint config_attribs[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT,
                                     EGL_RED_SIZE,     8,               EGL_GREEN_SIZE,      8,
                                     EGL_BLUE_SIZE,    8,               EGL_ALPHA_SIZE,      8,
                                     EGL_NONE};
    EGLConfig config;
    EGLint n = 0;
    if (!eglChooseConfig(dpy, config_attribs, &config, 1, &n) || n == 0) FAIL("eglChooseConfig failed 0x%x (%d)", eglGetError(), n);
    const EGLint pbuffer_attribs[] = {EGL_WIDTH, 32, EGL_HEIGHT, 32, EGL_NONE};
    EGLSurface surface = eglCreatePbufferSurface(dpy, config, pbuffer_attribs);
    if (surface == EGL_NO_SURFACE) FAIL("eglCreatePbufferSurface failed 0x%x", eglGetError());
    const EGLint context_attribs[] = {EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, config, EGL_NO_CONTEXT, context_attribs);
    if (ctx == EGL_NO_CONTEXT) FAIL("eglCreateContext failed 0x%x", eglGetError());
    if (!eglMakeCurrent(dpy, surface, surface, ctx)) FAIL("eglMakeCurrent failed 0x%x", eglGetError());

    glClearColor(0.25f, 0.5f, 0.75f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    unsigned char px[32 * 32 * 4];
    glReadPixels(0, 0, 32, 32, GL_RGBA, GL_UNSIGNED_BYTE, px);
    GLenum err = glGetError();
    if (err != GL_NO_ERROR) FAIL("glReadPixels failed 0x%x", err);
    for (int i = 0; i < 32 * 32; i++) {
        const unsigned char* q = px + i * 4;
        if (!(q[0] >= 63 && q[0] <= 65 && q[1] >= 127 && q[1] <= 129 && q[2] >= 190 && q[2] <= 192 && q[3] == 255)) {
            FAIL("pixel %d is %u %u %u %u", i, q[0], q[1], q[2], q[3]);
        }
    }
    printf("gles ok %s %u %u %u %u\n", (const char*)glGetString(GL_RENDERER), px[0], px[1], px[2], px[3]);
    eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    eglDestroyContext(dpy, ctx);
    eglDestroySurface(dpy, surface);
    eglTerminate(dpy);
    fflush(stdout);
    return 0;
}
