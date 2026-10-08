// thread-sys fixture: one thread, named "sys-mixed", alternates for argv[1] seconds (default 3)
// between 2 ms of work (spin_work) and a 2 ms condition-variable wait that nobody signals
// (wait_here -> pthread_cond_timedwait -> futex WAIT_BITSET). `[thread-sys]` must show about half
// its wall time in futex(wait), waited from wait_here. Built with NDK r28c (see build.txt).
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/prctl.h>
#include <time.h>

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t never = PTHREAD_COND_INITIALIZER;

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (double) t.tv_sec + (double) t.tv_nsec / 1e9;
}

__attribute__((noinline)) unsigned long spin_work(double seconds) {
    unsigned long x = 1;
    double end = now() + seconds;
    while (now() < end) {
        for (int i = 0; i < 10000; i++) {
            x = x * 6364136223846793005ul + 1442695040888963407ul;
            __asm__ volatile("" : "+r"(x));
        }
    }
    return x;
}

__attribute__((noinline)) int wait_here(long nanoseconds) {
    struct timespec until;
    clock_gettime(CLOCK_REALTIME, &until);
    until.tv_nsec += nanoseconds;
    if (until.tv_nsec >= 1000000000) {
        until.tv_sec += 1;
        until.tv_nsec -= 1000000000;
    }
    pthread_mutex_lock(&lock);
    int rc = pthread_cond_timedwait(&never, &lock, &until);
    pthread_mutex_unlock(&lock);
    return rc;
}

static void* worker(void* arg) {
    double seconds = *(double*) arg;
    prctl(PR_SET_NAME, "sys-mixed");
    unsigned long sum = 0;
    int waits = 0;
    double end = now() + seconds;
    while (now() < end) {
        sum += spin_work(0.002);
        waits += wait_here(2000000) != 0;
    }
    return (void*) (sum + (unsigned long) waits);
}

int main(int argc, char** argv) {
    double seconds = argc > 1 ? atof(argv[1]) : 3.0;
    pthread_t t;
    if (pthread_create(&t, NULL, worker, &seconds) != 0) { puts("pthread_create failed"); return 1; }
    void* r;
    pthread_join(t, &r);
    printf("syswait ok %d\n", r != NULL);
    return 0;
}
