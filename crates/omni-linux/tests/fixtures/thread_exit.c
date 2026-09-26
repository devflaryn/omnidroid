// A4 fixture: exit() from a secondary thread ends the whole process, however main is blocked.
#include <pthread.h>
#include <stdlib.h>
#include <unistd.h>

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t never = PTHREAD_COND_INITIALIZER;

static void* leave(void* arg) {
    (void) arg;
    usleep(50000);
    exit(3);
}

int main(void) {
    pthread_t t;
    pthread_create(&t, NULL, leave, NULL);
    pthread_mutex_lock(&lock);
    for (;;) pthread_cond_wait(&never, &lock);
}
