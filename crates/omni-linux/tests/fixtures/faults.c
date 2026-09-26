// Synchronous faults as the arm64 kernel raises them (A2-A5 review, Important 4 and 5):
//   faults ill       -- an undefined instruction reaches a SIGILL handler (ILL_ILLOPC, si_addr = pc)
//   faults trap      -- brk #0 reaches a SIGTRAP handler
//   faults blocked   -- a fault while SIGSEGV is blocked kills the process with SIGSEGV, handler or not
//   faults ignored   -- a fault while SIGSEGV is ignored kills it too
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>

static sigjmp_buf back;
static volatile int seen_code;
static volatile void *seen_addr;

static void on_signal(int sig, siginfo_t *info, void *uc) {
    (void)uc;
    seen_code = info->si_code;
    seen_addr = info->si_addr;
    siglongjmp(back, sig);
}

static void install(int sig) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_signal;
    sa.sa_flags = SA_SIGINFO;
    sigaction(sig, &sa, NULL);
}

extern char undefined_at[];
extern char brk_at[];

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "";
    if (strcmp(mode, "ill") == 0) {
        install(SIGILL);
        int sig = sigsetjmp(back, 1);
        if (sig == 0) {
            __asm__ volatile(".globl undefined_at\nundefined_at: .inst 0x00000000" ::: "memory");
            puts("no SIGILL");
            return 1;
        }
        if (sig != SIGILL || seen_code != ILL_ILLOPC || seen_addr != (void *)undefined_at) {
            printf("SIGILL wrong: sig %d code %d addr %p want %p\n", sig, seen_code, seen_addr, (void *)undefined_at);
            return 1;
        }
        puts("ill ok");
        return 0;
    }
    if (strcmp(mode, "trap") == 0) {
        install(SIGTRAP);
        int sig = sigsetjmp(back, 1);
        if (sig == 0) {
            __asm__ volatile(".globl brk_at\nbrk_at: brk #0" ::: "memory");
            puts("no SIGTRAP");
            return 1;
        }
        if (sig != SIGTRAP) {
            printf("SIGTRAP wrong: sig %d\n", sig);
            return 1;
        }
        puts("trap ok");
        return 0;
    }
    if (strcmp(mode, "blocked") == 0 || strcmp(mode, "ignored") == 0) {
        if (mode[0] == 'b') {
            install(SIGSEGV);
            sigset_t set;
            sigemptyset(&set);
            sigaddset(&set, SIGSEGV);
            sigprocmask(SIG_BLOCK, &set, NULL);
        } else {
            signal(SIGSEGV, SIG_IGN);
        }
        if (sigsetjmp(back, 1) != 0) {
            puts("handler ran while blocked");
            return 1;
        }
        puts("faulting");
        fflush(stdout);
        *(volatile int *)8 = 1;
        puts("survived");
        return 1;
    }
    puts("usage: faults ill|trap|blocked|ignored");
    return 2;
}
