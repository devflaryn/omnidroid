// Unix-domain sockets bound to names, as services use them: a stream server accepts a client and
// echoes, a seqpacket server keeps messages whole, a datagram server receives what is sent to its
// name, and an abstract name works too. Prints "ok ..." or "FAIL ..." per check.
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

static int failed;
static void check(int ok, const char* what) { printf("%s %s\n", ok ? "ok" : "FAIL", what); if (!ok) failed = 1; }

static socklen_t address(struct sockaddr_un* a, const char* name, int abstract) {
    memset(a, 0, sizeof(*a));
    a->sun_family = AF_UNIX;
    if (abstract) { strcpy(a->sun_path + 1, name); return offsetof(struct sockaddr_un, sun_path) + 1 + strlen(name); }
    strcpy(a->sun_path, name);
    return sizeof(*a);
}

static void* echo_server(void* arg) {
    int s = *(int*)arg;
    int c = accept4(s, NULL, NULL, SOCK_CLOEXEC);
    char buf[64];
    ssize_t n = read(c, buf, sizeof(buf));
    if (n > 0) write(c, buf, n);
    close(c);
    return NULL;
}

int main(void) {
    struct sockaddr_un a;
    socklen_t len = address(&a, "/data/local/tmp/echo", 0);
    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    check(bind(s, (struct sockaddr*)&a, len) == 0 && listen(s, 4) == 0, "stream bind+listen");
    pthread_t th;
    pthread_create(&th, NULL, echo_server, &s);
    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    check(connect(c, (struct sockaddr*)&a, len) == 0, "connect by name");
    write(c, "ping", 4);
    char buf[16] = {0};
    check(read(c, buf, sizeof(buf)) == 4 && memcmp(buf, "ping", 4) == 0, "the server echoed");
    pthread_join(th, NULL);
    check(read(c, buf, sizeof(buf)) == 0, "end of file once the server closed");

    int bad = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un none;
    socklen_t nlen = address(&none, "/data/local/tmp/nobody", 0);
    check(connect(bad, (struct sockaddr*)&none, nlen) == -1, "no socket at a name: refused");

    len = address(&a, "omni_dgram", 1);
    int d = socket(AF_UNIX, SOCK_DGRAM, 0);
    check(bind(d, (struct sockaddr*)&a, len) == 0, "datagram bind (abstract)");
    int dc = socket(AF_UNIX, SOCK_DGRAM, 0);
    check(connect(dc, (struct sockaddr*)&a, len) == 0, "datagram connect");
    send(dc, "one", 3, 0);
    send(dc, "two!", 4, 0);
    check(recv(d, buf, sizeof(buf), 0) == 3 && recv(d, buf, sizeof(buf), 0) == 4 && memcmp(buf, "two!", 4) == 0, "datagrams whole, in order");

    len = address(&a, "/data/local/tmp/seq", 0);
    int q = socket(AF_UNIX, SOCK_SEQPACKET, 0);
    check(bind(q, (struct sockaddr*)&a, len) == 0 && listen(q, 1) == 0, "seqpacket bind+listen");
    int qc = socket(AF_UNIX, SOCK_SEQPACKET, 0);
    check(connect(qc, (struct sockaddr*)&a, len) == 0, "seqpacket connect before accept");
    int qs = accept(q, NULL, NULL);
    send(qc, "ab", 2, 0);
    send(qc, "cde", 3, 0);
    check(recv(qs, buf, sizeof(buf), 0) == 2 && recv(qs, buf, sizeof(buf), 0) == 3, "packets whole");
    return failed;
}
