// fork, execve and waitpid as a daemon uses them (vold's ForkExecvp, installd's dexopt): the
// child points stdout at a pipe and executes a program; the parent reads what it wrote and waits
// for it. Then a child that exits before executing anything, WNOHANG, and ECHILD. Prints one line
// per check, "ok ..." or "FAIL ...".
#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static int failed;
static void check(int ok, const char* what) {
    printf("%s %s\n", ok ? "ok" : "FAIL", what);
    if (!ok) failed = 1;
}

// A fork from a thread that is not the main one, deep in its calls, as vold's binder thread
// forks: the thread's function must still return what it returns.
static __attribute__((noinline)) int fork_run_true(int depth) {
    if (depth > 0) return fork_run_true(depth - 1) + 1;
    pid_t child = fork();
    if (child == 0) {
        char* argv[] = {"/system/bin/true", NULL};
        execve(argv[0], argv, NULL);
        _exit(127);
    }
    int status = -1;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) return -1000;
    return 0;
}

// What an atfork child handler and the child itself write -- a global, the heap -- is the child's
// own (libbinder's handler marks its ProcessState forked; the parent must not see it).
static volatile int forked_flag;
static void mark_forked(void) { forked_flag = 1; }

static void* forking_thread(void* arg) {
    (void)arg;
    return (void*)(intptr_t)(fork_run_true(8) + 34);
}

int main(void) {
    volatile int local = 1234;
    pid_t self = getpid();
    int fds[2];
    if (pipe(fds) < 0) { perror("pipe"); return 1; }
    pid_t child = fork();
    if (child == 0) {
        local = 99; // a fork child's writes to its stack are its own
        dup2(fds[1], 1);
        close(fds[0]);
        close(fds[1]);
        char* argv[] = {"/system/bin/echo", "hello", NULL};
        char* envp[] = {"PATH=/system/bin", NULL};
        execve(argv[0], argv, envp);
        _exit(127);
    }
    check(child > 0, "fork");
    close(fds[1]);
    char buf[64] = {0};
    ssize_t n = 0, r;
    while ((r = read(fds[0], buf + n, sizeof(buf) - 1 - n)) > 0) n += r;
    check(n == 6 && memcmp(buf, "hello\n", 6) == 0, "the child's program wrote to the pipe");
    int status = -1;
    check(waitpid(child, &status, 0) == child, "waitpid");
    check(WIFEXITED(status) && WEXITSTATUS(status) == 0, "it exited 0");
    check(local == 1234, "the parent's locals are its own");
    check(getpid() == self, "the parent's pid is its own");

    pid_t quick = fork();
    if (quick == 0) _exit(3);
    status = -1;
    check(waitpid(quick, &status, 0) == quick && WIFEXITED(status) && WEXITSTATUS(status) == 3, "a child's _exit(3)");
    check(waitpid(-1, &status, WNOHANG) == -1 && errno == ECHILD, "no children: ECHILD");

    pid_t slow = fork();
    if (slow == 0) {
        char* argv[] = {"/system/bin/sleep", "1", NULL};
        execve(argv[0], argv, NULL);
        _exit(127);
    }
    check(waitpid(slow, &status, WNOHANG) == 0, "WNOHANG before it ends");
    check(wait(&status) == slow && WIFEXITED(status) && WEXITSTATUS(status) == 0, "wait for any child");
    pthread_atfork(NULL, NULL, mark_forked);
    int* heap = malloc(sizeof(int));
    *heap = 7;
    pid_t writer = fork();
    if (writer == 0) {
        *heap = 8;
        char* argv[] = {"/system/bin/true", NULL};
        execve(argv[0], argv, NULL);
        _exit(127);
    }
    waitpid(writer, &status, 0);
    check(*heap == 7 && forked_flag == 0, "what the child and its atfork handler wrote is its own");
    free(heap);

    pthread_t thread;
    void* result = NULL;
    check(pthread_create(&thread, NULL, forking_thread, NULL) == 0 && pthread_join(thread, &result) == 0 && (intptr_t)result == 42,
          "a thread that forked returns what it returns");
    return failed;
}
