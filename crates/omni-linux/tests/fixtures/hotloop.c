// guestprof fixture: one thread, named "hot-worker", spends argv[1] seconds (default 3) in
// spin_hot(); `OMNI_GUEST_PROF` must attribute its translated-code samples there. Built with NDK
// r28c (see build.txt); not stripped, so `spin_hot` is in `.symtab`.
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/prctl.h>
#include <time.h>

__attribute__((noinline)) unsigned long spin_hot(unsigned long n) {
    unsigned long x = 1;
    for (unsigned long i = 0; i < n; i++) {
        x = x * 6364136223846793005ul + 1442695040888963407ul;
        __asm__ volatile("" : "+r"(x));
    }
    return x;
}

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (double) t.tv_sec + (double) t.tv_nsec / 1e9;
}

static void* worker(void* arg) {
    double seconds = *(double*) arg;
    prctl(PR_SET_NAME, "hot-worker");
    unsigned long sum = 0;
    double end = now() + seconds;
    while (now() < end) sum += spin_hot(1000000);
    return (void*) sum;
}

int main(int argc, char** argv) {
    double seconds = argc > 1 ? atof(argv[1]) : 3.0;
    pthread_t t;
    if (pthread_create(&t, NULL, worker, &seconds) != 0) { puts("pthread_create failed"); return 1; }
    void* r;
    pthread_join(t, &r);
    printf("hotloop ok %d\n", r != NULL);
    return 0;
}
