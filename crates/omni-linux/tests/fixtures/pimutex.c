// Priority-inheritance mutexes (FUTEX_LOCK_PI / UNLOCK_PI / TRYLOCK_PI), as audioserver's
// audio_utils mutexes are: four threads contend for one, a trylock fails while it is held, and a
// timed lock times out. Prints "ok ..." or "FAIL ..." per check.
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <time.h>

static int failed;
static void check(int ok, const char* what) { printf("%s %s\n", ok ? "ok" : "FAIL", what); if (!ok) failed = 1; }

static pthread_mutex_t m;
static long counter;

static void* work(void* arg) {
    (void)arg;
    for (int i = 0; i < 20000; i++) {
        if (pthread_mutex_lock(&m) != 0) return (void*)1;
        counter++;
        if (pthread_mutex_unlock(&m) != 0) return (void*)1;
    }
    return NULL;
}

static void* try_it(void* arg) {
    (void)arg;
    return (void*)(long)pthread_mutex_trylock(&m);
}

static void* time_it(void* arg) {
    (void)arg;
    struct timespec at;
    clock_gettime(CLOCK_REALTIME, &at);
    at.tv_nsec += 50 * 1000 * 1000;
    if (at.tv_nsec >= 1000000000) { at.tv_sec++; at.tv_nsec -= 1000000000; }
    return (void*)(long)pthread_mutex_timedlock(&m, &at);
}

int main(void) {
    pthread_mutexattr_t attr;
    pthread_mutexattr_init(&attr);
    check(pthread_mutexattr_setprotocol(&attr, PTHREAD_PRIO_INHERIT) == 0, "PTHREAD_PRIO_INHERIT");
    check(pthread_mutex_init(&m, &attr) == 0, "init");
    pthread_t t[4];
    for (int i = 0; i < 4; i++) pthread_create(&t[i], NULL, work, NULL);
    int errors = 0;
    for (int i = 0; i < 4; i++) {
        void* r;
        pthread_join(t[i], &r);
        errors += r != NULL;
    }
    check(errors == 0, "four threads lock and unlock");
    check(counter == 80000, "every increment under the lock");

    check(pthread_mutex_lock(&m) == 0, "held by main");
    pthread_t other;
    void* r;
    pthread_create(&other, NULL, try_it, NULL);
    pthread_join(other, &r);
    check((long)r == EBUSY, "trylock by another thread: EBUSY");
    pthread_create(&other, NULL, time_it, NULL);
    pthread_join(other, &r);
    check((long)r == ETIMEDOUT, "timedlock by another thread: ETIMEDOUT");
    check(pthread_mutex_unlock(&m) == 0, "unlocked");
    pthread_create(&other, NULL, try_it, NULL);
    pthread_join(other, &r);
    check((long)r == 0, "trylock once free");
    printf(failed ? "FAILED\n" : "PASSED\n");
    return failed;
}
