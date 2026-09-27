// D2 fixture: a CPU-written AHardwareBuffer through the real libnativewindow -> libui -> gralloc 5
// path (the host allocator, mapper.omni.so). Writes a pattern, prints the buffer's id, waits for
// the host to read it and write the last pixel through its own mapping, then checks both.
// Built with NDK r28c (build.txt).
#include <android/hardware_buffer.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#define WIDTH 64
#define HEIGHT 32
#define GO_FILE "/data/local/tmp/gralloc.go"
#define HOST_VALUE 0x12345678u

static void check(int status, const char* step) {
    if (status != 0) {
        printf("%s failed %d\n", step, status);
        fflush(stdout);
        exit(1);
    }
}

static uint32_t pattern(uint32_t x, uint32_t y) { return 0xFF000000u | (y << 8) | x; }

int main(void) {
    AHardwareBuffer_Desc d = {
        .width = WIDTH,
        .height = HEIGHT,
        .layers = 1,
        .format = AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM,
        .usage = AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN | AHARDWAREBUFFER_USAGE_CPU_WRITE_OFTEN,
    };
    AHardwareBuffer* buf = NULL;
    check(AHardwareBuffer_allocate(&d, &buf), "allocate");

    AHardwareBuffer_Desc got;
    AHardwareBuffer_describe(buf, &got);
    uint32_t stride = got.stride;

    void* addr = NULL;
    check(AHardwareBuffer_lock(buf, AHARDWAREBUFFER_USAGE_CPU_WRITE_OFTEN, -1, NULL, &addr),
          "lock");
    for (uint32_t y = 0; y < HEIGHT; y++) {
        for (uint32_t x = 0; x < WIDTH; x++) {
            *(uint32_t*)((uint8_t*)addr + (y * stride + x) * 4) = pattern(x, y);
        }
    }
    check(AHardwareBuffer_unlock(buf, NULL), "unlock");

    uint64_t id = 0;
    check(AHardwareBuffer_getId(buf, &id), "getId");
    printf("id=%" PRIu64 " stride=%u\n", id, stride);
    fflush(stdout);

    // The host reads the pattern out of its own mapping, writes HOST_VALUE into the last pixel,
    // then creates GO_FILE.
    int waited_ms = 0;
    while (access(GO_FILE, F_OK) != 0) {
        if (waited_ms >= 60000) {
            printf("no go\n");
            fflush(stdout);
            exit(1);
        }
        usleep(10000);
        waited_ms += 10;
    }

    check(AHardwareBuffer_lock(buf, AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN, -1, NULL, &addr),
          "lock_read");
    for (uint32_t y = 0; y < HEIGHT; y++) {
        for (uint32_t x = 0; x < WIDTH; x++) {
            uint32_t want = (x == WIDTH - 1 && y == HEIGHT - 1) ? HOST_VALUE : pattern(x, y);
            uint32_t got = *(const uint32_t*)((const uint8_t*)addr + (y * stride + x) * 4);
            if (got != want) {
                printf("mismatch at %u,%u: got 0x%08x\n", x, y, got);
                fflush(stdout);
                exit(1);
            }
        }
    }
    check(AHardwareBuffer_unlock(buf, NULL), "unlock_read");
    printf("host write seen\n");
    printf("verified\n");
    fflush(stdout);

    AHardwareBuffer_release(buf);
    return 0;
}
