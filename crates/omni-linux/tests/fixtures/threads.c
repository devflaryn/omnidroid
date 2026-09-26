// A4 fixture: real bionic threads under omni-linux. Built with NDK r28c (see build.txt).
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#define THREADS 8
#define ROUNDS 100000

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static long counter;
static __thread long mine;
static pid_t tids[THREADS];

static void* add(void* arg) {
    long index = (long) arg;
    tids[index] = gettid();
    for (int i = 0; i < ROUNDS; i++) {
        pthread_mutex_lock(&lock);
        counter++;
        pthread_mutex_unlock(&lock);
        mine++;
    }
    return (void*) mine;
}

static pthread_mutex_t qlock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t qcond = PTHREAD_COND_INITIALIZER;
static int queue[16];
static int head, tail;

static void* produce(void* arg) {
    (void) arg;
    for (int i = 1; i <= 1000; i++) {
        pthread_mutex_lock(&qlock);
        while (tail - head == 16) pthread_cond_wait(&qcond, &qlock);
        queue[tail++ % 16] = i;
        pthread_cond_broadcast(&qcond);
        pthread_mutex_unlock(&qlock);
    }
    return NULL;
}

int main(void) {
    pthread_t t[THREADS];
    for (long i = 0; i < THREADS; i++) {
        if (pthread_create(&t[i], NULL, add, (void*) i) != 0) { puts("pthread_create failed"); return 1; }
    }
    for (int i = 0; i < THREADS; i++) {
        void* result;
        pthread_join(t[i], &result);
        if ((long) result != ROUNDS) { printf("thread %d saw __thread %ld\n", i, (long) result); return 2; }
    }
    for (int i = 0; i < THREADS; i++)
        for (int j = i + 1; j < THREADS; j++)
            if (tids[i] == tids[j] || tids[i] == getpid()) { puts("tids not distinct"); return 3; }
    if (counter != (long) THREADS * ROUNDS) { printf("counter %ld\n", counter); return 4; }

    pthread_t producer;
    pthread_create(&producer, NULL, produce, NULL);
    long sum = 0;
    for (int received = 0; received < 1000; received++) {
        pthread_mutex_lock(&qlock);
        while (tail == head) pthread_cond_wait(&qcond, &qlock);
        sum += queue[head++ % 16];
        pthread_cond_broadcast(&qcond);
        pthread_mutex_unlock(&qlock);
    }
    pthread_join(producer, NULL);
    if (sum != 500500) { printf("sum %ld\n", sum); return 5; }
    printf("threads ok %ld\n", counter);
    return 0;
}
