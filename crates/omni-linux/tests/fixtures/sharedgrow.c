// A file mapped MAP_SHARED larger than it is, then grown, as AppSearch's icing maps its arrays at
// their largest and grows them: a page wholly past the file's end is SIGBUS (BUS_ADRERR, si_addr the
// access); once ftruncate through another descriptor grows the file over it, it reads zeros, shows
// what pwrite writes there, and a store through the mapping reaches the file. Then an empty file,
// as a new icing array file is when it is mapped. Prints "ok ..." or "FAIL ..." per check.
#include <fcntl.h>
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static int failed;
static void check(int ok, const char* what) { printf("%s %s\n", ok ? "ok" : "FAIL", what); if (!ok) failed = 1; }

static sigjmp_buf back;
static volatile int seen_code;
static volatile void* seen_addr;
// Where a probing read lands: a load into the zero register is no access to dynarmic.
static volatile char sink;

static void on_bus(int sig, siginfo_t* info, void* uc) {
    (void)uc;
    seen_code = info->si_code;
    seen_addr = info->si_addr;
    siglongjmp(back, sig);
}

// Whether reading `at` raises SIGBUS with BUS_ADRERR at that address.
static int bus_at(volatile char* at) {
    seen_code = 0;
    seen_addr = 0;
    if (sigsetjmp(back, 1) == 0) {
        sink = *at;
        return 0;
    }
    if (seen_code != BUS_ADRERR || seen_addr != (void*)at) printf("  (code %d at %p, not %p)\n", seen_code, seen_addr, (void*)at);
    return seen_code == BUS_ADRERR && seen_addr == (void*)at;
}

int main(void) {
    const size_t kib = 1024, len = 1024 * kib;
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_bus;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGBUS, &sa, NULL);

    const char* path = "/data/local/tmp/sharedgrow.bin";
    unlink(path);
    int fd = open(path, O_RDWR | O_CREAT, 0644);
    check(fd >= 0, "open");
    char head[100];
    memset(head, 'h', sizeof head);
    check(write(fd, head, sizeof head) == (ssize_t)sizeof head, "a 100-byte file");
    char* p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    check(p != MAP_FAILED, "mmap MAP_SHARED 1 MiB of it");
    if (p == MAP_FAILED) return 1;
    check(p[0] == 'h' && p[99] == 'h', "its bytes");
    check(bus_at(p + 512 * kib), "a page wholly past the end is SIGBUS");

    int other = open(path, O_RDWR);
    check(other >= 0 && ftruncate(other, 768 * kib) == 0, "ftruncate through another descriptor to 768 KiB");
    check(!bus_at(p + 512 * kib) && p[512 * kib] == 0, "grown over: zeros");
    check(pwrite(other, "pwritten", 8, 600 * kib) == 8, "pwrite past the old end");
    check(memcmp(p + 600 * kib, "pwritten", 8) == 0, "the mapping shows it");
    memcpy(p + 700 * kib, "stored", 6);
    char buf[8] = {0};
    check(pread(other, buf, 6, 700 * kib) == 6 && memcmp(buf, "stored", 6) == 0, "a store through the mapping reaches the file");
    check(bus_at(p + 900 * kib), "past the new end: SIGBUS still");
    check(munmap(p, len) == 0, "munmap");
    close(other);
    close(fd);

    // An empty file, mapped at its largest and then grown: a new icing array file.
    const char* epath = "/data/local/tmp/sharedgrow.empty";
    unlink(epath);
    int efd = open(epath, O_RDWR | O_CREAT, 0644);
    char* e = efd >= 0 ? mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, efd, 0) : MAP_FAILED;
    check(e != MAP_FAILED, "mmap MAP_SHARED 1 MiB of an empty file");
    if (e == MAP_FAILED) return 1;
    check(bus_at(e), "its first page is SIGBUS");
    int eother = open(epath, O_RDWR);
    check(eother >= 0 && ftruncate(eother, 64 * kib) == 0, "ftruncate it to 64 KiB through another descriptor");
    memcpy(e + 100, "trie", 4);
    check(pread(eother, buf, 4, 100) == 4 && memcmp(buf, "trie", 4) == 0, "a store through the mapping reaches the file");
    munmap(e, len);
    close(eother);
    close(efd);
    printf(failed ? "FAILED\n" : "PASSED\n");
    return failed;
}
