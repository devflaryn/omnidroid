// seccomp filters, as minijail installs them for the media daemons: a filter (a classic BPF
// program on struct seccomp_data) answers each system call -- allow, an errno, a SIGSYS trap --
// and applies from then on, to the threads and children too. Prints "ok ..." or "FAIL ..." per
// check.
#define _GNU_SOURCE
#include <errno.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int failed;
static void check(int ok, const char* what) { printf("%s %s\n", ok ? "ok" : "FAIL", what); if (!ok) failed = 1; }

static volatile int trapped_nr = -1;
static volatile unsigned trapped_arch;
static volatile int trapped_code;
static void on_sigsys(int sig, siginfo_t* info, void* uc) {
    (void)sig; (void)uc;
    trapped_nr = info->si_syscall;
    trapped_arch = info->si_arch;
    trapped_code = info->si_code;
}

int main(void) {
    struct sock_filter bad[] = { BPF_STMT(BPF_LD | BPF_W | BPF_ABS, 3), BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW) };
    struct sock_fprog bad_prog = { 2, bad };
    check(prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0, "PR_SET_NO_NEW_PRIVS");
    check(prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) == 1, "PR_GET_NO_NEW_PRIVS");
    check(prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &bad_prog) == -1 && errno == EINVAL, "an unaligned load is refused: EINVAL");
    check(prctl(PR_GET_SECCOMP, 0, 0, 0, 0) == 0, "no filter yet");

    struct sock_filter f[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_AARCH64, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_getppid, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_sched_yield, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_TRAP | 7),
        // uname(buf) with a null buffer: ENOTTY, to tell args apart from the number.
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_uname, 0, 3),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[0])),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, 0, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | ENOTTY),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    struct sock_fprog prog = { sizeof(f) / sizeof(f[0]), f };
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = on_sigsys;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGSYS, &sa, NULL);
    check(syscall(__NR_seccomp, SECCOMP_SET_MODE_FILTER, 0, &prog) == 0, "seccomp(SECCOMP_SET_MODE_FILTER)");
    check(prctl(PR_GET_SECCOMP, 0, 0, 0, 0) == 2, "PR_GET_SECCOMP: filter mode");
    check(getppid() == -1 && errno == EPERM, "getppid: the filter's EPERM");
    check(syscall(__NR_uname, NULL) == -1 && errno == ENOTTY, "uname(NULL): decided by its argument");
    check(getpid() > 0, "getpid: allowed");
    syscall(__NR_sched_yield);
    check(trapped_nr == __NR_sched_yield && trapped_arch == AUDIT_ARCH_AARCH64 && trapped_code == 1, "sched_yield: SIGSYS, SYS_SECCOMP, with the call and arch");
    pid_t child = fork();
    if (child == 0) _exit(getppid() == -1 && errno == EPERM ? 0 : 1);
    int status = -1;
    waitpid(child, &status, 0);
    check(WIFEXITED(status) && WEXITSTATUS(status) == 0, "a child keeps the filter");
    printf(failed ? "FAILED\n" : "PASSED\n");
    return failed;
}
