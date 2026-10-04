// A fork child that lives beside its parent and never executes a program, as an anti-tamper
// watchdog does (Clash of Clans' libsupercell: two pipes, fork, the child reads 4 bytes from its
// parent): the parent goes on after the fork while the child waits, they talk over the pipes, and
// each keeps memory of its own -- a global, the heap, the stack -- whatever the other writes. A
// parent thread works on throughout. Prints one line per check, "ok ..." or "FAIL ...".
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
    fflush(stdout);
    if (!ok) failed = 1;
}

static volatile int global = 1;
static volatile int stop;
static volatile long ticks;

// A parent thread that keeps running guest code and system calls while the child is about.
static void* ticker(void* arg) {
    (void)arg;
    while (!stop) {
        ticks++;
        usleep(1000);
    }
    return NULL;
}

int main(void) {
    int to_child[2], to_parent[2];
    if (pipe(to_child) < 0 || pipe(to_parent) < 0) { perror("pipe"); return 1; }
    volatile int* heap = malloc(64 << 10);
    heap[0] = 10;
    heap[4096] = 11;
    volatile int local = 5;
    pthread_t thread;
    pthread_create(&thread, NULL, ticker, NULL);

    pid_t child = fork();
    if (child == 0) {
        close(to_child[1]);
        close(to_parent[0]);
        uint32_t token = 0;
        // Blocks until the parent, which must have gone on, writes.
        if (read(to_child[0], &token, 4) != 4) _exit(10);
        // What the parent wrote after the fork is not the child's.
        uint32_t wrong = (global != 1) | (heap[0] != 10) << 1 | (heap[4096] != 11) << 2 | (local != 5) << 3;
        global = 3;
        heap[0] = 30;
        local = 7;
        uint32_t reply[2] = {token + 1, wrong};
        if (write(to_parent[1], reply, 8) != 8) _exit(11);
        // A second round, after the parent has run on again.
        if (read(to_child[0], &token, 4) != 4) _exit(12);
        reply[0] = token + 1;
        reply[1] = (global != 3) | (heap[0] != 30) << 1 | (local != 7) << 3;
        if (write(to_parent[1], reply, 8) != 8) _exit(13);
        // Then wait for the parent to go, as a watchdog waits: end of file.
        char c;
        if (read(to_child[0], &c, 1) != 0) _exit(14);
        _exit(global == 3 && heap[0] == 30 ? 0 : 15);
    }
    check(child > 0, "fork");
    close(to_child[0]);
    close(to_parent[1]);
    // The child is blocked in read: the parent runs on and writes its own memory.
    global = 2;
    heap[0] = 20;
    local = 6;
    long before = ticks;
    uint32_t token = 0x1234;
    check(write(to_child[1], &token, 4) == 4, "the parent writes to the waiting child");
    uint32_t reply[2] = {0, 0};
    check(read(to_parent[0], reply, 8) == 8 && reply[0] == 0x1235, "the child answers");
    check(reply[1] == 0, "the child sees its memory as at the fork");
    check(global == 2 && heap[0] == 20 && heap[4096] == 11 && local == 6, "the parent's memory is its own");
    global = 4;
    heap[0] = 40;
    token = 0x5678;
    check(write(to_child[1], &token, 4) == 4 && read(to_parent[0], reply, 8) == 8 && reply[0] == 0x5679, "a second round");
    check(reply[1] == 0, "the child's own writes are kept");
    check(global == 4 && heap[0] == 40, "the parent's own writes are kept");
    usleep(50000);
    check(ticks > before, "the parent's other thread ran");
    close(to_child[1]);
    int status = -1;
    check(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0, "the child saw end of file and exited 0");
    stop = 1;
    pthread_join(thread, NULL);
    check(global == 4 && heap[0] == 40 && local == 6, "the parent's memory after the child ended");
    printf("%s\n", failed ? "FAILED" : "DONE");
    return failed;
}
