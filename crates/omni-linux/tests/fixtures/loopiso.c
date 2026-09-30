// Loopback private per namespace (crate::loopns): run as `loopiso server <port>` it listens on
// 127.0.0.1:<port> (TCP) and 127.0.0.1:<port> (UDP), prints "ready", answers one TCP client with
// "hi" and echoes UDP datagrams for 20 s. Run as `loopiso client <port> <expect: reach|refused>`
// it checks what reaching that port does; `loopiso self` runs the single-process checks.
// Prints "ok ..." or "FAIL ...".
#include <arpa/inet.h>
#include <errno.h>
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
            if (n > 0) sendto(u, buf, n, 0, (struct sockaddr*)&from, flen);
        }
    }
    return 0;
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
