// A file mapped MAP_SHARED, as SQLite maps a WAL database's -shm index: writes through the mapping
// reach the file (pread sees them), a second mapping sees the first's writes, and pwrite is seen
// through the mappings. Prints "ok ..." or "FAIL ..." per check.
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static int failed;
static void check(int ok, const char* what) { printf("%s %s\n", ok ? "ok" : "FAIL", what); if (!ok) failed = 1; }

int main(void) {
    const char* path = "/data/local/tmp/sharedmap.db-shm";
    unlink(path);
    int fd = open(path, O_RDWR | O_CREAT, 0644);
    check(fd >= 0, "open");
    check(ftruncate(fd, 32768) == 0, "ftruncate to 32 KiB");
    char* a = mmap(NULL, 32768, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    check(a != MAP_FAILED, "mmap MAP_SHARED read-write");
    char* b = mmap(NULL, 32768, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    check(b != MAP_FAILED && b != a, "a second mapping");
    if (a == MAP_FAILED || b == MAP_FAILED) return 1;
    memcpy(a + 100, "wal-index", 9);
    check(memcmp(b + 100, "wal-index", 9) == 0, "the second mapping sees the first's write");
    char buf[16] = {0};
    check(pread(fd, buf, 9, 100) == 9 && memcmp(buf, "wal-index", 9) == 0, "pread sees the mapping's write");
    check(pwrite(fd, "pwritten", 8, 20000) == 8, "pwrite");
    check(memcmp(a + 20000, "pwritten", 8) == 0, "the mapping sees pwrite");
    check(munmap(a, 32768) == 0 && munmap(b, 32768) == 0, "munmap");
    close(fd);
    fd = open(path, O_RDONLY);
    memset(buf, 0, sizeof(buf));
    check(pread(fd, buf, 9, 100) == 9 && memcmp(buf, "wal-index", 9) == 0, "the write outlives the mapping");
    // Read-only and shared, through a read-only descriptor (an idmap, an APK).
    char* r = mmap(NULL, 32768, PROT_READ, MAP_SHARED, fd, 0);
    check(r != MAP_FAILED && memcmp(r + 100, "wal-index", 9) == 0, "a read-only descriptor maps shared for reading");
    close(fd);
    printf(failed ? "FAILED\n" : "PASSED\n");
    return failed;
}
