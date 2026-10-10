// A pool mapped MAP_SHARED over a file with nothing in it yet, then grown with ftruncate, as Roblox
// maps its asset pool (cache/wob/wob-<n>: 1 GiB over a file it unlinks and grows): what is written
// through the mapping is held, before and after the file grows, by a forked child as by the parent;
// the same at a file offset inside a host page, and at a fixed address. Prints "ok ..." or
// "FAIL ..." per check.
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

static int failed;
static void check(int ok, const char* what) { printf("%s %s\n", ok ? "ok" : "FAIL", what); if (!ok) failed = 1; }

static int fresh(const char* path) {
    unlink(path);
    int fd = open(path, O_RDWR | O_CREAT, 0600);
    if (fd >= 0) unlink(path);  // the pool's file has no name once open
    return fd;
}

// Every page of [p, p + len) marked with its index and `salt`; then checked.
static void fill(unsigned char* p, size_t len, unsigned salt) {
    for (size_t at = 0; at < len; at += 4096) {
        uint32_t v = (uint32_t)(at / 4096) * 2654435761u + salt;
        memcpy(p + at, &v, 4);
        memcpy(p + at + 4092, &v, 4);
    }
}
static int same(const unsigned char* p, size_t len, unsigned salt) {
    for (size_t at = 0; at < len; at += 4096) {
        uint32_t v = (uint32_t)(at / 4096) * 2654435761u + salt, a, b;
        memcpy(&a, p + at, 4);
        memcpy(&b, p + at + 4092, 4);
        if (a != v || b != v) return 0;
    }
    return 1;
}

int main(void) {
    const size_t len = 64u << 20;
    int fd = fresh("/data/local/tmp/sharedtail.pool");
    check(fd >= 0, "open and unlink");
    unsigned char* pool = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    check(pool != MAP_FAILED, "mmap 64 MiB MAP_SHARED over an empty file");
    check(ftruncate(fd, 16u << 20) == 0, "ftruncate to 16 MiB");
    fill(pool, 16u << 20, 1);
    check(same(pool, 16u << 20, 1), "the first 16 MiB hold what was written");
    check(ftruncate(fd, len) == 0, "ftruncate to 64 MiB");
    check(same(pool, 16u << 20, 1), "and still do once the file grew");
    fill(pool + (16u << 20), len - (16u << 20), 2);
    check(same(pool + (16u << 20), len - (16u << 20), 2), "the next 48 MiB hold what was written");

    pid_t child = fork();
    if (child == 0) {
        // MAP_SHARED: the child's writes are the parent's memory.
        int ok = same(pool, 16u << 20, 1);
        fill(pool + (32u << 20), 8u << 20, 3);
        _exit(ok ? 0 : 1);
    }
    int status = 0;
    check(child > 0 && waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0, "a forked child sees the pool");
    check(same(pool + (32u << 20), 8u << 20, 3), "and the parent sees the child's writes");
    check(munmap(pool, len) == 0, "munmap");
    close(fd);

    // A file offset inside a host page (8 KiB): the address agrees with it modulo the host page.
    int fd2 = fresh("/data/local/tmp/sharedtail.pool2");
    unsigned char* at_offset = mmap(NULL, 8u << 20, PROT_READ | PROT_WRITE, MAP_SHARED, fd2, 8192);
    check(at_offset != MAP_FAILED, "mmap 8 MiB at offset 8 KiB over an empty file");
    if (at_offset != MAP_FAILED) {
        fill(at_offset, 8u << 20, 4);
        check(same(at_offset, 8u << 20, 4), "it holds what was written");
        check(munmap(at_offset, 8u << 20) == 0, "munmap it");
    }

    // At a fixed address inside a reservation.
    unsigned char* room = mmap(NULL, 16u << 20, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    check(room != MAP_FAILED, "reserve 16 MiB");
    unsigned char* fixed = mmap(room + (1u << 20), 4u << 20, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_FIXED, fd2, 0);
    check(fixed == room + (1u << 20), "mmap 4 MiB MAP_FIXED over the empty file");
    if (fixed == room + (1u << 20)) {
        fill(fixed, 4u << 20, 5);
        check(same(fixed, 4u << 20, 5), "it holds what was written");
    }
    check(munmap(room, 16u << 20) == 0, "munmap the reservation");
    close(fd2);
    return failed;
}
