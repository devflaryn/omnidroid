// Loopback private per namespace (crate::loopns): run as `loopiso server <port>` it listens on
// 127.0.0.1:<port> (TCP) and 127.0.0.1:<port> (UDP), prints "ready", answers one TCP client with
// "hi" and echoes UDP datagrams for 20 s. Run as `loopiso client <port> <expect: reach|refused>`
// it checks what reaching that port does; `loopiso self` runs the single-process checks.
// Prints "ok ..." or "FAIL ...".
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static int failed;
static void check(int ok, const char* what) { printf("%s %s\n", ok ? "ok" : "FAIL", what); fflush(stdout); if (!ok) failed = 1; }

static struct sockaddr_in lo(int port) {
    struct sockaddr_in a;
    memset(&a, 0, sizeof(a));
    a.sin_family = AF_INET;
    a.sin_port = htons(port);
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    return a;
}

static int server(int port) {
    int t = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = lo(port);
    check(bind(t, (struct sockaddr*)&a, sizeof(a)) == 0 && listen(t, 4) == 0, "server tcp bind+listen");
    struct sockaddr_in got;
    socklen_t len = sizeof(got);
    check(getsockname(t, (struct sockaddr*)&got, &len) == 0 && ntohs(got.sin_port) == port, "getsockname shows the guest port");
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    check(bind(u, (struct sockaddr*)&a, sizeof(a)) == 0, "server udp bind");
    printf("ready\n");
    fflush(stdout);
    // Non-blocking: a connection or datagram the guest's namespace drops after poll() reported it
    // must not leave this server stuck in accept or recvfrom.
    fcntl(t, F_SETFL, O_NONBLOCK);
    fcntl(u, F_SETFL, O_NONBLOCK);
    struct pollfd fds[2] = {{t, POLLIN, 0}, {u, POLLIN, 0}};
    for (int i = 0; i < 200; i++) {
        if (poll(fds, 2, 100) <= 0) continue;
        if (fds[0].revents & POLLIN) {
            int c = accept(t, NULL, NULL);
            if (c >= 0) { write(c, "hi", 2); close(c); }
        }
        if (fds[1].revents & POLLIN) {
            char buf[64];
            struct sockaddr_in from;
            socklen_t flen = sizeof(from);
            ssize_t n = recvfrom(u, buf, sizeof(buf), 0, (struct sockaddr*)&from, &flen);
            if (n > 0) { printf("udp got %.*s\n", (int)n, buf); fflush(stdout); }
            if (n > 0) sendto(u, buf, n, 0, (struct sockaddr*)&from, flen);
        }
    }
    return 0;
}

// A non-blocking socket whose connect to `a` was refused: EINPROGRESS, then poll reports it.
static int refused_socket(const struct sockaddr_in* a) {
    int s = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    int r = connect(s, (const struct sockaddr*)a, sizeof(*a));
    struct pollfd p = {s, POLLOUT, 0};
    check(r == -1 && errno == EINPROGRESS && poll(&p, 1, 5000) == 1, "refused connect: EINPROGRESS, then ready");
    return s;
}

static int client(int port, int reach) {
    int c = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = lo(port);
    int r = connect(c, (struct sockaddr*)&a, sizeof(a));
    if (reach) {
        char buf[4] = {0};
        check(r == 0 && read(c, buf, 2) == 2 && memcmp(buf, "hi", 2) == 0, "tcp: reaches the server");
        int u = socket(AF_INET, SOCK_DGRAM, 0); // unbound: its reply address must still resolve
        check(sendto(u, "ping", 4, 0, (struct sockaddr*)&a, sizeof(a)) == 4, "udp send");
        struct pollfd p = {u, POLLIN, 0};
        char got[8] = {0};
        check(poll(&p, 1, 5000) == 1 && recv(u, got, sizeof(got), 0) == 4 && memcmp(got, "ping", 4) == 0, "udp reply to an unbound client");
    } else {
        check(r == -1 && errno == ECONNREFUSED, "tcp: refused");
        // Non-blocking, as Linux: EINPROGRESS, then writable and in error, SO_ERROR once.
        int cn = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
        int rn = connect(cn, (struct sockaddr*)&a, sizeof(a));
        int en = errno;
        struct pollfd pn = {cn, POLLOUT, 0};
        int polled = poll(&pn, 1, 5000);
        int soerr = -1, soerr2 = -1;
        socklen_t sl = sizeof(soerr);
        getsockopt(cn, SOL_SOCKET, SO_ERROR, &soerr, &sl);
        sl = sizeof(soerr2);
        getsockopt(cn, SOL_SOCKET, SO_ERROR, &soerr2, &sl);
        check(rn == -1 && en == EINPROGRESS && polled == 1 && (pn.revents & (POLLOUT | POLLERR)) && soerr == ECONNREFUSED, "non-blocking tcp: EINPROGRESS, then SO_ERROR ECONNREFUSED");
        check(soerr2 == 0, "SO_ERROR is cleared once read");
        // The refusal is the socket's pending error: whichever call comes first reports it, once.
        int s1 = refused_socket(&a);
        ssize_t x1 = send(s1, "x", 1, MSG_NOSIGNAL);
        int e1 = errno;
        ssize_t x2 = send(s1, "x", 1, MSG_NOSIGNAL);
        check(x1 == -1 && e1 == ECONNREFUSED && x2 == -1 && errno == EPIPE, "send reports the refusal once, then EPIPE");
        int s2 = refused_socket(&a);
        char rb[4];
        check(recv(s2, rb, sizeof(rb), 0) == -1 && errno == ECONNREFUSED, "recv reports the refusal");
        int s3 = refused_socket(&a);
        check(connect(s3, (struct sockaddr*)&a, sizeof(a)) == -1 && errno == ECONNREFUSED, "a second connect reports the refusal");
        int s4 = refused_socket(&a);
        check(write(s4, "x", 1) == -1 && errno == ECONNREFUSED, "write reports the refusal");
        // The wildcard as a destination is the host's loopback on Linux and macOS: it must name
        // the namespace's ports as 127.0.0.1 does, never the host's.
        struct sockaddr_in w = a;
        w.sin_addr.s_addr = htonl(INADDR_ANY);
        int cw = socket(AF_INET, SOCK_STREAM, 0);
        check(connect(cw, (struct sockaddr*)&w, sizeof(w)) == -1 && errno == ECONNREFUSED, "tcp to 0.0.0.0: refused");
        int uw = socket(AF_INET, SOCK_DGRAM, 0);
        sendto(uw, "ping", 4, 0, (struct sockaddr*)&w, sizeof(w));
        struct pollfd pw = {uw, POLLIN, 0};
        check(poll(&pw, 1, 1500) == 0, "udp to 0.0.0.0: nothing comes back");
        int u = socket(AF_INET, SOCK_DGRAM, 0);
        sendto(u, "ping", 4, 0, (struct sockaddr*)&a, sizeof(a));
        struct pollfd p = {u, POLLIN, 0};
        check(poll(&p, 1, 1500) == 0, "udp: nothing comes back");
        // This namespace's own server may take the same port.
        int t = socket(AF_INET, SOCK_STREAM, 0);
        check(bind(t, (struct sockaddr*)&a, sizeof(a)) == 0 && listen(t, 1) == 0, "the same port is free here");
    }
    return 0;
}

static int self_checks(void) {
    // A v6 dual-stack wildcard server, reached by a v4 client over 127.0.0.1.
    int s6 = socket(AF_INET6, SOCK_STREAM, 0);
    struct sockaddr_in6 w6;
    memset(&w6, 0, sizeof(w6));
    w6.sin6_family = AF_INET6;
    w6.sin6_port = htons(47111);
    w6.sin6_addr = in6addr_any;
    check(bind(s6, (struct sockaddr*)&w6, sizeof(w6)) == 0 && listen(s6, 1) == 0, "v6 wildcard server");
    int c4 = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = lo(47111);
    check(connect(c4, (struct sockaddr*)&a, sizeof(a)) == 0, "v4 client reaches a v6 wildcard server");
    struct sockaddr_in peer;
    socklen_t plen = sizeof(peer);
    check(getpeername(c4, (struct sockaddr*)&peer, &plen) == 0 && ntohs(peer.sin_port) == 47111, "getpeername shows the guest port");
    int acc = accept(s6, NULL, NULL);
    struct sockaddr_in6 an;
    socklen_t alen = sizeof(an);
    check(acc >= 0 && getsockname(acc, (struct sockaddr*)&an, &alen) == 0 && ntohs(an.sin6_port) == 47111, "an accepted socket shows the guest port");
    int two = socket(AF_INET6, SOCK_STREAM, 0);
    check(bind(two, (struct sockaddr*)&w6, sizeof(w6)) == -1 && errno == EADDRINUSE, "a held port: EADDRINUSE");
    // UDP connect names a peer, as Linux: it succeeds before the peer binds, the early send is
    // dropped, and a peer that binds later is reached.
    int uc = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in p2 = lo(47112);
    check(connect(uc, (struct sockaddr*)&p2, sizeof(p2)) == 0 && send(uc, "x", 1, 0) == 1, "udp connect to an unheld port succeeds; the send is dropped");
    int us = socket(AF_INET, SOCK_DGRAM, 0);
    bind(us, (struct sockaddr*)&p2, sizeof(p2));
    send(uc, "y", 1, 0);
    struct pollfd pf = {us, POLLIN, 0};
    char g1[2] = {0};
    check(poll(&pf, 1, 3000) == 1 && recv(us, g1, 1, 0) == 1 && g1[0] == 'y', "a peer that binds later is reached");
    return 0;
}

int main(int argc, char** argv) {
    if (argc >= 3 && strcmp(argv[1], "server") == 0) server(atoi(argv[2]));
    else if (argc >= 4 && strcmp(argv[1], "client") == 0) client(atoi(argv[2]), strcmp(argv[3], "reach") == 0);
    else if (argc >= 2 && strcmp(argv[1], "self") == 0) self_checks();
    else { printf("FAIL usage\n"); return 2; }
    return failed;
}
