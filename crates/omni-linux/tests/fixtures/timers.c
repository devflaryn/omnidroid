// POSIX timers through the real bionic: a SIGEV_THREAD timer's callback runs (bionic's own thread,
// woken by SI_TIMER), periodically; a SIGEV_SIGNAL timer's handler gets SI_TIMER with its value and
// timer id; a timer whose signal stays blocked counts overruns; SIGEV_THREAD_ID reaches the thread
// named; timer_gettime counts down; timer_delete stops it. One line per finding; non-zero on failure.
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static atomic_int calls;
static void callback(union sigval v) {
    if (v.sival_int == 42) atomic_fetch_add(&calls, 1);
}

static volatile sig_atomic_t got_code, got_value, got_tid;
static void handler(int sig, siginfo_t* si, void* uc) {
    (void)sig;
    (void)uc;
    got_code = si->si_code;
    got_value = si->si_value.sival_int;
    got_tid = gettid();
}

static void sleep_ms(int ms) {
    struct timespec t = {ms / 1000, (ms % 1000) * 1000000L};
    while (nanosleep(&t, &t) == -1 && errno == EINTR) {}
}

static int thread_tid;
static void* idle(void* arg) {
    (void)arg;
    thread_tid = gettid();
    for (int i = 0; i < 100 && !got_tid; i++) sleep_ms(10);
    return NULL;
}

int main(void) {
    int bad = 0;
    // SIGEV_THREAD: periodic, 20 ms.
    struct sigevent se;
    memset(&se, 0, sizeof se);
    se.sigev_notify = SIGEV_THREAD;
    se.sigev_notify_function = callback;
    se.sigev_value.sival_int = 42;
    timer_t t1;
    if (timer_create(CLOCK_MONOTONIC, &se, &t1) != 0) {
        printf("timer_create SIGEV_THREAD: %s\n", strerror(errno));
        return 1;
    }
    struct itimerspec its = {{0, 20000000}, {0, 20000000}};
    timer_settime(t1, 0, &its, NULL);
    sleep_ms(300);
    struct itimerspec left;
    timer_gettime(t1, &left);
    timer_delete(t1);
    int n = atomic_load(&calls);
    printf("SIGEV_THREAD: %d callbacks in 300 ms (20 ms period); interval left %ld ns\n", n, left.it_interval.tv_nsec);
    if (n < 5 || left.it_interval.tv_nsec != 20000000) bad = 1;
    sleep_ms(100);
    if (atomic_load(&calls) > n + 1) {
        printf("callbacks after timer_delete\n");
        bad = 1;
    }

    // SIGEV_SIGNAL with a handler: SI_TIMER and the value.
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = handler;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGUSR1, &sa, NULL);
    memset(&se, 0, sizeof se);
    se.sigev_notify = SIGEV_SIGNAL;
    se.sigev_signo = SIGUSR1;
    se.sigev_value.sival_int = 7;
    timer_t t2;
    timer_create(CLOCK_REALTIME, &se, &t2);
    struct itimerspec once = {{0, 0}, {0, 10000000}};
    timer_settime(t2, 0, &once, NULL);
    for (int i = 0; i < 100 && !got_code; i++) sleep_ms(5);
    printf("SIGEV_SIGNAL: si_code %d (SI_TIMER %d), value %d\n", got_code, SI_TIMER, got_value);
    if (got_code != SI_TIMER || got_value != 7) bad = 1;

    // Blocked: the expiries while the signal waits are overruns.
    sigset_t block;
    sigemptyset(&block);
    sigaddset(&block, SIGUSR1);
    sigprocmask(SIG_BLOCK, &block, NULL);
    struct itimerspec fast = {{0, 5000000}, {0, 5000000}};
    timer_settime(t2, 0, &fast, NULL);
    sleep_ms(100);
    siginfo_t si;
    struct timespec zero = {0, 0};
    int sig = sigtimedwait(&block, &si, &zero);
    struct itimerspec off = {{0, 0}, {0, 0}};
    timer_settime(t2, 0, &off, NULL);
    printf("blocked: sigtimedwait %d, si_code %d, si_overrun %d, timer_getoverrun %d\n", sig, si.si_code, si.si_overrun, timer_getoverrun(t2));
    if (sig != SIGUSR1 || si.si_code != SI_TIMER || si.si_overrun < 5 || timer_getoverrun(t2) != si.si_overrun) bad = 1;
    sigprocmask(SIG_UNBLOCK, &block, NULL);
    timer_delete(t2);

    // SIGEV_THREAD_ID: to the thread named.
    got_tid = 0;
    pthread_t th;
    pthread_create(&th, NULL, idle, NULL);
    while (!thread_tid) sleep_ms(1);
    memset(&se, 0, sizeof se);
    se.sigev_notify = SIGEV_THREAD_ID;
    se.sigev_signo = SIGUSR1;
    se._sigev_un._tid = thread_tid;
    timer_t t3;
    if (timer_create(CLOCK_MONOTONIC, &se, &t3) != 0) {
        printf("timer_create SIGEV_THREAD_ID: %s\n", strerror(errno));
        bad = 1;
    } else {
        timer_settime(t3, 0, &once, NULL);
        pthread_join(th, NULL);
        printf("SIGEV_THREAD_ID: handled on tid %d, the thread is %d\n", got_tid, thread_tid);
        if (got_tid != thread_tid) bad = 1;
        timer_delete(t3);
    }
    printf(bad ? "FAIL\n" : "ok\n");
    return bad;
}
