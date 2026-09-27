// Threads opening one new file with O_CREAT at once (keystore2's threads each open a database
// that does not exist yet): every open succeeds -- the one that loses the race to create it opens
// what the winner made. O_EXCL still refuses a file that exists. Prints "ok ..." or "FAIL ...".
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static int failed;
static void check(int ok, const char* what) { printf("%s %s\n", ok ? "ok" : "FAIL", what); if (!ok) failed = 1; }

static pthread_barrier_t start;
static int round_no;
static int failures;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;

static void* opener(void* arg) {
    (void)arg;
    for (int r = 0; r < 50; r++) {
        pthread_barrier_wait(&start);
        char path[64];
        snprintf(path, sizeof(path), "/data/local/tmp/race-%d", r);
        int fd = open(path, O_RDWR | O_CREAT | O_CLOEXEC, 0600);
        if (fd < 0) {
            pthread_mutex_lock(&lock);
            failures++;
            pthread_mutex_unlock(&lock);
        } else {
            close(fd);
        }
        pthread_barrier_wait(&start);
    }
    return NULL;
}

int main(void) {
    for (int r = 0; r < 50; r++) {
        char path[64];
        snprintf(path, sizeof(path), "/data/local/tmp/race-%d", r);
        unlink(path);
    }
    pthread_barrier_init(&start, NULL, 8);
    pthread_t t[8];
    for (int i = 0; i < 8; i++) pthread_create(&t[i], NULL, opener, NULL);
    for (int i = 0; i < 8; i++) pthread_join(t[i], NULL);
    (void)round_no;
    check(failures == 0, "eight threads open each new file with O_CREAT: every open succeeds");
    int fd = open("/data/local/tmp/race-0", O_RDWR | O_CREAT | O_EXCL, 0600);
    check(fd == -1 && errno == EEXIST, "O_EXCL on a file that exists: EEXIST");
    printf(failed ? "FAILED\n" : "PASSED\n");
    return failed;
}
