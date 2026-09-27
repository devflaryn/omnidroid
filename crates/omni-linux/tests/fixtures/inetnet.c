// Internet sockets on the host's network, as a guest uses them: a TCP client of a host echo server
// (blocking, over IPv6 v4-mapped, non-blocking with poll and SO_ERROR), a refused connect both
// ways, a UDP round trip, a loopback server of the guest's own (listen/accept4 in a thread,
// select on the listener), epoll woken by data the host sends later, SO_RCVTIMEO, options,
// FIONBIO/FIONREAD and shutdown. Prints "ok ..." or "FAIL ..." per check.
//
// argv: <echo tcp port> <late tcp port> <closed tcp port> <echo udp port>
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

static int failed;
static void check(int ok, const char* what) {
    printf("%s %s\n", ok ? "ok" : "FAIL", what);
    if (!ok) { failed = 1; printf("  (errno %d)\n", errno); }
    fflush(stdout);
}

static struct sockaddr_in v4(int port) {
    struct sockaddr_in a;
    memset(&a, 0, sizeof(a));
    a.sin_family = AF_INET;
    a.sin_port = htons(port);
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    return a;
}

static long ms_since(struct timespec* start) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (now.tv_sec - start->tv_sec) * 1000 + (now.tv_nsec - start->tv_nsec) / 1000000;
}

static int echoes(int fd, const char* msg) {
    size_t n = strlen(msg);
    if (write(fd, msg, n) != (ssize_t)n) return 0;
    char buf[64] = {0};
    size_t got = 0;
    while (got < n) {
        ssize_t r = read(fd, buf + got, sizeof(buf) - 1 - got);
        if (r <= 0) return 0;
        got += r;
    }
    return memcmp(buf, msg, n) == 0;
}

static void* serve_one(void* arg) {
    int s = *(int*)arg;
    struct sockaddr_in peer;
    socklen_t len = sizeof(peer);
    int c = accept4(s, (struct sockaddr*)&peer, &len, SOCK_CLOEXEC);
    if (c < 0) return (void*)1;
    char buf[64];
    ssize_t n = read(c, buf, sizeof(buf));
    if (n > 0) write(c, buf, n);
    close(c);
    return peer.sin_addr.s_addr == htonl(INADDR_LOOPBACK) && len == sizeof(peer) ? NULL : (void*)1;
}

int main(int argc, char** argv) {
    if (argc < 5) { printf("FAIL usage\n"); return 2; }
    int echo_port = atoi(argv[1]), late_port = atoi(argv[2]), closed_port = atoi(argv[3]), udp_port = atoi(argv[4]);

    // A blocking TCP client.
    int c = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
    struct sockaddr_in a = v4(echo_port);
    check(connect(c, (struct sockaddr*)&a, sizeof(a)) == 0, "tcp connect to a host server");
    check(echoes(c, "hello, host"), "tcp bytes echoed");
    struct sockaddr_in got;
    socklen_t len = sizeof(got);
    check(getpeername(c, (struct sockaddr*)&got, &len) == 0 && len == sizeof(got) && got.sin_port == htons(echo_port) && got.sin_addr.s_addr == htonl(INADDR_LOOPBACK), "getpeername: the server");
    len = sizeof(got);
    check(getsockname(c, (struct sockaddr*)&got, &len) == 0 && got.sin_family == AF_INET && got.sin_port != 0, "getsockname: an ephemeral port");
    int one = 1, val = 0;
    socklen_t vlen = sizeof(val);
    check(setsockopt(c, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one)) == 0 && getsockopt(c, IPPROTO_TCP, TCP_NODELAY, &val, &vlen) == 0 && val != 0, "TCP_NODELAY set and read back");
    vlen = sizeof(val);
    check(setsockopt(c, SOL_SOCKET, SO_KEEPALIVE, &one, sizeof(one)) == 0 && getsockopt(c, SOL_SOCKET, SO_KEEPALIVE, &val, &vlen) == 0 && val != 0, "SO_KEEPALIVE set and read back");
    vlen = sizeof(val);
    check(getsockopt(c, SOL_SOCKET, SO_TYPE, &val, &vlen) == 0 && val == SOCK_STREAM, "SO_TYPE");
    vlen = sizeof(val);
    check(getsockopt(c, SOL_SOCKET, SO_RCVBUF, &val, &vlen) == 0 && val > 0, "SO_RCVBUF");
    unsigned mark = 0x10064;
    check(setsockopt(c, SOL_SOCKET, SO_MARK, &mark, sizeof(mark)) == 0, "SO_MARK accepted");
    check(shutdown(c, SHUT_WR) == 0, "shutdown(SHUT_WR)");
    char buf[64];
    check(read(c, buf, sizeof(buf)) == 0, "the server saw end of file and closed: read 0");
    close(c);

    // IPv6 dual-stack, as Android's Java stack connects: a v4-mapped address on an AF_INET6 socket.
    int c6 = socket(AF_INET6, SOCK_STREAM, 0);
    struct sockaddr_in6 a6;
    memset(&a6, 0, sizeof(a6));
    a6.sin6_family = AF_INET6;
    a6.sin6_port = htons(echo_port);
    inet_pton(AF_INET6, "::ffff:127.0.0.1", &a6.sin6_addr);
    check(connect(c6, (struct sockaddr*)&a6, sizeof(a6)) == 0 && echoes(c6, "mapped"), "tcp6 to a v4-mapped address");
    close(c6);

    // Non-blocking: EINPROGRESS, poll for POLLOUT, SO_ERROR 0; then epoll for the echo.
    int n = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    int r = connect(n, (struct sockaddr*)&a, sizeof(a));
    check(r == 0 || (r == -1 && errno == EINPROGRESS), "non-blocking connect: EINPROGRESS");
    struct pollfd pfd = {n, POLLOUT, 0};
    check(poll(&pfd, 1, 5000) == 1 && (pfd.revents & POLLOUT), "poll: writable once connected");
    int err = -1;
    vlen = sizeof(err);
    check(getsockopt(n, SOL_SOCKET, SO_ERROR, &err, &vlen) == 0 && err == 0, "SO_ERROR 0");
    check(recv(n, buf, sizeof(buf), 0) == -1 && errno == EAGAIN, "recv with nothing there: EAGAIN");
    int ep = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event ev = {.events = EPOLLIN, .data.u32 = 7};
    epoll_ctl(ep, EPOLL_CTL_ADD, n, &ev);
    send(n, "ping", 4, MSG_NOSIGNAL);
    struct epoll_event out;
    check(epoll_wait(ep, &out, 1, 5000) == 1 && out.data.u32 == 7 && (out.events & EPOLLIN), "epoll: readable when the echo arrives");
    int avail = 0;
    check(ioctl(n, FIONREAD, &avail) == 0 && avail == 4, "FIONREAD: 4");
    check(recv(n, buf, sizeof(buf), 0) == 4 && memcmp(buf, "ping", 4) == 0, "the echo");
    // Woken as the bytes arrive, not at the next look: 40 round trips, each waited for in epoll.
    // (Found only by a poll every 50 ms, they would take 2 s.)
    struct timespec t0;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    int trips = 0;
    for (int i = 0; i < 40; i++) {
        if (send(n, "x", 1, 0) != 1 || epoll_wait(ep, &out, 1, 5000) != 1 || recv(n, buf, 1, 0) != 1) break;
        trips++;
    }
    long took = ms_since(&t0);
    printf("  40 round trips: %ld ms\n", took);
    check(trips == 40 && took < 1000, "40 epoll round trips in under a second");
    close(n);
    close(ep);

    // Refused: blocking, and non-blocking through SO_ERROR.
    struct sockaddr_in closed = v4(closed_port);
    int f = socket(AF_INET, SOCK_STREAM, 0);
    check(connect(f, (struct sockaddr*)&closed, sizeof(closed)) == -1 && errno == ECONNREFUSED, "blocking connect to a closed port: ECONNREFUSED");
    close(f);
    f = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    r = connect(f, (struct sockaddr*)&closed, sizeof(closed));
    pfd.fd = f;
    pfd.events = POLLOUT;
    pfd.revents = 0;
    int polled = poll(&pfd, 1, 10000);
    err = 0;
    vlen = sizeof(err);
    getsockopt(f, SOL_SOCKET, SO_ERROR, &err, &vlen);
    check(r == -1 && errno == EINPROGRESS && polled == 1 && (pfd.revents & (POLLOUT | POLLERR)) && err == ECONNREFUSED, "non-blocking: SO_ERROR ECONNREFUSED");
    close(f);

    // epoll woken by data the host sends later; SO_RCVTIMEO; FIONBIO.
    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in la = v4(late_port);
    check(connect(l, (struct sockaddr*)&la, sizeof(la)) == 0, "connect to the late server");
    struct timeval tv = {0, 200000};
    setsockopt(l, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
    struct timespec start;
    clock_gettime(CLOCK_MONOTONIC, &start);
    r = recv(l, buf, sizeof(buf), 0);
    long waited = ms_since(&start);
    check(r == -1 && errno == EAGAIN && waited >= 150 && waited < 2000, "SO_RCVTIMEO: EAGAIN after the timeout");
    ep = epoll_create1(0);
    ev.events = EPOLLIN;
    ev.data.u32 = 9;
    epoll_ctl(ep, EPOLL_CTL_ADD, l, &ev);
    clock_gettime(CLOCK_MONOTONIC, &start);
    int woke = epoll_wait(ep, &out, 1, 10000);
    waited = ms_since(&start);
    check(woke == 1 && out.data.u32 == 9 && waited < 5000, "epoll_wait woken by host data");
    check(recv(l, buf, sizeof(buf), 0) == 4 && memcmp(buf, "late", 4) == 0, "the late bytes");
    int on = 1;
    check(ioctl(l, FIONBIO, &on) == 0 && recv(l, buf, sizeof(buf), 0) == -1 && errno == EAGAIN, "FIONBIO: non-blocking");
    close(ep);
    close(l);

    // UDP to a host echo socket: sendto/recvfrom, then connected send/recv.
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in ua = v4(udp_port);
    check(sendto(u, "datagram", 8, 0, (struct sockaddr*)&ua, sizeof(ua)) == 8, "udp sendto");
    struct sockaddr_in from;
    socklen_t flen = sizeof(from);
    memset(buf, 0, sizeof(buf));
    check(recvfrom(u, buf, sizeof(buf), 0, (struct sockaddr*)&from, &flen) == 8 && memcmp(buf, "datagram", 8) == 0 && from.sin_port == htons(udp_port) && flen == sizeof(from), "udp recvfrom: the echo, from the server");
    check(connect(u, (struct sockaddr*)&ua, sizeof(ua)) == 0 && send(u, "again", 5, 0) == 5 && recv(u, buf, sizeof(buf), 0) == 5, "udp connected send/recv");
    close(u);

    // A loopback server of the guest's own: listen, accept4 in a thread, select on the listener.
    int s = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in sa = v4(0);
    check(bind(s, (struct sockaddr*)&sa, sizeof(sa)) == 0 && listen(s, 4) == 0, "guest server bind+listen");
    len = sizeof(sa);
    getsockname(s, (struct sockaddr*)&sa, &len);
    check(ntohs(sa.sin_port) != 0, "the listener's port");
    int cl = socket(AF_INET, SOCK_STREAM, 0);
    check(connect(cl, (struct sockaddr*)&sa, sizeof(sa)) == 0, "connect to the guest's server");
    fd_set rs;
    FD_ZERO(&rs);
    FD_SET(s, &rs);
    struct timeval five = {5, 0};
    check(select(s + 1, &rs, NULL, NULL, &five) == 1 && FD_ISSET(s, &rs), "select: the listener readable");
    pthread_t th;
    pthread_create(&th, NULL, serve_one, &s);
    check(echoes(cl, "loop"), "the guest's server echoed");
    void* res = (void*)1;
    pthread_join(th, &res);
    check(res == NULL, "accept4 reported the client's address");
    check(read(cl, buf, sizeof(buf)) == 0, "end of file once the server closed");
    close(cl);
    close(s);

    printf(failed ? "FAILED\n" : "PASSED\n");
    return failed;
}
