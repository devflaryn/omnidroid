// A5 fixture: signal delivery as the arm64 Linux kernel does it. Built with NDK r28c (build.txt).
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static volatile char* page;
static volatile void* fault_addr;
static volatile int fault_code, faults, on_altstack;
static char altstack[64 * 1024];

static void on_segv(int sig, siginfo_t* info, void* ucontext) {
    (void) sig; (void) ucontext;
    char probe;
    on_altstack = (&probe >= altstack && &probe < altstack + sizeof altstack);
    fault_addr = info->si_addr;
    fault_code = info->si_code;
    faults++;
    mprotect((void*) page, 4096, PROT_READ | PROT_WRITE);  // the write retries and completes
}

static volatile int usr1, usr2_tid;
static void on_usr1(int sig) { (void) sig; usr1++; }
static void on_usr2(int sig) { (void) sig; usr2_tid = gettid(); }

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
static volatile int worker_tid, stop;

static void* worker(void* arg) {
    (void) arg;
    worker_tid = gettid();
    pthread_mutex_lock(&lock);
    while (!stop) pthread_cond_wait(&cond, &lock);
    pthread_mutex_unlock(&lock);
    return NULL;
}

static void clobber(int sig) {
    (void) sig;
    // Scribble on the vector registers the interrupted code is using.
    volatile double junk = 12345.0;
    for (int i = 0; i < 8; i++) junk = junk * 3.0 + i;
    __asm__ volatile("fmov d8, #1.0\n fmov d9, #2.0\n fmov d10, #3.0\n cmp xzr, xzr" ::: "d8", "d9", "d10", "cc");
}

int main(void) {
    // (a) SIGSEGV on the alternate stack, then the faulting write completes.
    stack_t ss = { .ss_sp = altstack, .ss_size = sizeof altstack, .ss_flags = 0 };
    if (sigaltstack(&ss, NULL) != 0) { puts("sigaltstack failed"); return 1; }
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_segv;
    sa.sa_flags = SA_SIGINFO | SA_ONSTACK;
    sigaction(SIGSEGV, &sa, NULL);
    page = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    page[100] = 42;
    if (faults != 1 || page[100] != 42 || fault_addr != page + 100 || fault_code != SEGV_ACCERR || !on_altstack) {
        printf("segv: faults %d value %d addr %p want %p code %d altstack %d\n", faults, page[100], fault_addr, page + 100, fault_code, on_altstack);
        return 2;
    }

    // (b) raise runs the handler before it returns.
    signal(SIGUSR1, on_usr1);
    raise(SIGUSR1);
    if (usr1 != 1) { printf("raise: usr1 %d\n", usr1); return 3; }

    // (c) blocked: not run; unblocked: run once.
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    sigprocmask(SIG_BLOCK, &set, NULL);
    raise(SIGUSR1);
    if (usr1 != 1) { printf("blocked: usr1 %d\n", usr1); return 4; }
    sigprocmask(SIG_UNBLOCK, &set, NULL);
    if (usr1 != 2) { printf("unblocked: usr1 %d\n", usr1); return 5; }

    // (d) pthread_kill reaches the thread, while it waits on a condition variable.
    signal(SIGUSR2, on_usr2);
    pthread_t t;
    pthread_create(&t, NULL, worker, NULL);
    while (!worker_tid) usleep(1000);
    usleep(20000);
    pthread_kill(t, SIGUSR2);
    for (int i = 0; i < 500 && !usr2_tid; i++) usleep(1000);
    pthread_mutex_lock(&lock);
    stop = 1;
    pthread_cond_broadcast(&cond);
    pthread_mutex_unlock(&lock);
    pthread_join(t, NULL);
    if (usr2_tid != worker_tid) { printf("pthread_kill: ran on %d, want %d\n", usr2_tid, worker_tid); return 6; }

    // (e) a handler's vector and flag registers do not leak into the interrupted code.
    signal(SIGUSR1, clobber);
    volatile double acc = 0.0;
    for (int i = 1; i <= 1000; i++) {
        acc += i * 0.5;
        if (i == 500) raise(SIGUSR1);
    }
    if (acc != 250250.0) { printf("registers: acc %f\n", acc); return 7; }

    puts("signals ok");
    return 0;
}
