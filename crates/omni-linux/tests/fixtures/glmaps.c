/* GL fallback gate fixture: two buffers mapped at once on one target, as a renderer streams
 * vertices and indices -- A mapped, B bound and mapped, A bound again and unmapped while B is still
 * mapped and written, then B unmapped. A mapping belongs to its buffer object, not to the target
 * it was made through: each buffer must read back its own bytes. (The GL backend once kept maps by
 * target: unmapping A copied B's shadow into A and freed B's shadow while it was still in use --
 * Roblox's engine then wrote into freed memory in PS99, SEGV_MAPERR, 2026-09-29.)
 * Prints "glmaps ok" and exits 0, or names what differed and exits 1. */
#include <EGL/egl.h>
#include <GLES3/gl3.h>
#include <stdio.h>
#include <string.h>

#define FAIL(...)                  \
    do {                           \
        printf(__VA_ARGS__);       \
        printf("\n");              \
        fflush(stdout);            \
        return 1;                  \
    } while (0)

enum { SIZE = 256 * 1024 };

static int check(GLuint buffer, unsigned char want, const char* name) {
    glBindBuffer(GL_ARRAY_BUFFER, buffer);
    const unsigned char* p = glMapBufferRange(GL_ARRAY_BUFFER, 0, SIZE, GL_MAP_READ_BIT);
    if (p == NULL) FAIL("map %s for reading failed 0x%x", name, glGetError());
    for (int i = 0; i < SIZE; i++) {
        if (p[i] != want) {
            printf("%s[%d] is 0x%02x, not 0x%02x\n", name, i, p[i], want);
            glUnmapBuffer(GL_ARRAY_BUFFER);
            return 1;
        }
    }
    glUnmapBuffer(GL_ARRAY_BUFFER);
    return 0;
}

int main(void) {
    EGLDisplay dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, NULL, NULL)) FAIL("eglInitialize failed 0x%x", eglGetError());
    const EGLint ca[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT, EGL_NONE};
    EGLConfig config;
    EGLint n = 0;
    if (!eglChooseConfig(dpy, ca, &config, 1, &n) || n == 0) FAIL("eglChooseConfig failed");
    const EGLint pa[] = {EGL_WIDTH, 1, EGL_HEIGHT, 1, EGL_NONE};
    EGLSurface surface = eglCreatePbufferSurface(dpy, config, pa);
    const EGLint xa[] = {EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE};
    EGLContext ctx = eglCreateContext(dpy, config, EGL_NO_CONTEXT, xa);
    if (!eglMakeCurrent(dpy, surface, surface, ctx)) FAIL("eglMakeCurrent failed 0x%x", eglGetError());

    GLuint b[2];
    glGenBuffers(2, b);
    for (int i = 0; i < 2; i++) {
        glBindBuffer(GL_ARRAY_BUFFER, b[i]);
        glBufferData(GL_ARRAY_BUFFER, SIZE, NULL, GL_DYNAMIC_DRAW);
    }
    const GLbitfield w = GL_MAP_WRITE_BIT | GL_MAP_INVALIDATE_BUFFER_BIT;
    glBindBuffer(GL_ARRAY_BUFFER, b[0]);
    unsigned char* a = glMapBufferRange(GL_ARRAY_BUFFER, 0, SIZE, w);
    glBindBuffer(GL_ARRAY_BUFFER, b[1]);
    unsigned char* bb = glMapBufferRange(GL_ARRAY_BUFFER, 0, SIZE, w);
    if (a == NULL || bb == NULL || a == bb) FAIL("maps %p %p (0x%x)", (void*)a, (void*)bb, glGetError());
    memset(a, 0x11, SIZE);
    memset(bb, 0x22, SIZE / 2);
    glBindBuffer(GL_ARRAY_BUFFER, b[0]);
    if (!glUnmapBuffer(GL_ARRAY_BUFFER)) FAIL("unmap A failed 0x%x", glGetError());
    /* B is still mapped: its pointer is still B's to write. */
    memset(bb + SIZE / 2, 0x22, SIZE / 2);
    glBindBuffer(GL_ARRAY_BUFFER, b[1]);
    if (!glUnmapBuffer(GL_ARRAY_BUFFER)) FAIL("unmap B failed 0x%x", glGetError());

    if (check(b[0], 0x11, "A") || check(b[1], 0x22, "B")) return 1;
    printf("glmaps ok\n");
    fflush(stdout);
    return 0;
}
