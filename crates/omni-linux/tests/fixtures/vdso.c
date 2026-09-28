// The vDSO, from a guest: is one named (AT_SYSINFO_EHDR), is bionic's clock_gettime faster than the
// system call it replaces, and do the two agree -- read one way then the other, the clock never goes
// back? Prints one line per finding; exits non-zero on a disagreement.
#include <stdint.h>
#include <stdio.h>
#include <sys/auxv.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

static int64_t ns(struct timespec t) { return (int64_t)t.tv_sec * 1000000000 + t.tv_nsec; }

static int64_t raw(clockid_t c) {
    struct timespec t;
    syscall(SYS_clock_gettime, c, &t);
    return ns(t);
}

static int64_t libc(clockid_t c) {
    struct timespec t;
    clock_gettime(c, &t);
    return ns(t);
}

int main(void) {
    printf("vdso at %#lx\n", getauxval(AT_SYSINFO_EHDR));
    const int n = 200000;
    int bad = 0;
    clockid_t clocks[] = {CLOCK_MONOTONIC, CLOCK_REALTIME, CLOCK_BOOTTIME, CLOCK_MONOTONIC_COARSE};
    for (int c = 0; c < 4; c++) {
        // Interleaved: libc, raw, libc -- each no earlier than the one before.
        for (int i = 0; i < 1000; i++) {
            int64_t a = libc(clocks[c]), b = raw(clocks[c]), d = libc(clocks[c]);
            if (b < a || d < b) {
                printf("clock %d went back: %lld %lld %lld\n", clocks[c], (long long)a, (long long)b, (long long)d);
                bad = 1;
                break;
            }
        }
    }
    int64_t t0 = raw(CLOCK_MONOTONIC);
    for (int i = 0; i < n; i++) libc(CLOCK_MONOTONIC);
    int64_t t1 = raw(CLOCK_MONOTONIC);
    for (int i = 0; i < n / 10; i++) raw(CLOCK_MONOTONIC);
    int64_t t2 = raw(CLOCK_MONOTONIC);
    printf("clock_gettime: libc %lld ns a call, system call %lld ns a call\n", (long long)((t1 - t0) / n), (long long)((t2 - t1) / (n / 10)));
    struct timeval tv;
    gettimeofday(&tv, NULL);
    int64_t real = raw(CLOCK_REALTIME);
    int64_t diff = real / 1000 - ((int64_t)tv.tv_sec * 1000000 + tv.tv_usec);
    printf("gettimeofday agrees with CLOCK_REALTIME to %lld us\n", (long long)diff);
    if (diff < 0 || diff > 100000) bad = 1;
    struct timespec res;
    clock_getres(CLOCK_MONOTONIC, &res);
    printf("resolution %ld ns\n", res.tv_nsec);
    printf(bad ? "FAIL\n" : "ok\n");
    return bad;
}
