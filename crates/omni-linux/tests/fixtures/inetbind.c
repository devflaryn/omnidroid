// Internet sockets bound on a machine whose only interface is lo, as system_server's
// MulticastSocket and daemons bind them: the wildcard and loopback addresses bind, port 0 is
// given an ephemeral port getsockname reports, and an address no interface has is refused.
// Prints "ok ..." or "FAIL ..." per check.
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static int failed;
static void check(int ok, const char* what) { printf("%s %s\n", ok ? "ok" : "FAIL", what); if (!ok) failed = 1; }

int main(void) {
    int u6 = socket(AF_INET6, SOCK_DGRAM, 0);
    struct sockaddr_in6 a6;
    memset(&a6, 0, sizeof(a6));
    a6.sin6_family = AF_INET6;
    a6.sin6_addr = in6addr_any;
    check(bind(u6, (struct sockaddr*)&a6, sizeof(a6)) == 0, "udp6 wildcard port 0 binds");
    struct sockaddr_in6 got6;
    socklen_t len = sizeof(got6);
    check(getsockname(u6, (struct sockaddr*)&got6, &len) == 0 && len == sizeof(got6) && got6.sin6_family == AF_INET6, "udp6 getsockname");
    check(ntohs(got6.sin6_port) >= 32768, "udp6 given an ephemeral port");
    check(bind(u6, (struct sockaddr*)&a6, sizeof(a6)) == -1 && errno == EINVAL, "a bound socket binds again: EINVAL");

    int t4 = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a4;
    memset(&a4, 0, sizeof(a4));
    a4.sin_family = AF_INET;
    a4.sin_port = htons(5037);
    a4.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    check(bind(t4, (struct sockaddr*)&a4, sizeof(a4)) == 0, "tcp4 loopback port 5037 binds");
    struct sockaddr_in got4;
    len = sizeof(got4);
    check(getsockname(t4, (struct sockaddr*)&got4, &len) == 0 && ntohs(got4.sin_port) == 5037 && got4.sin_addr.s_addr == htonl(INADDR_LOOPBACK), "tcp4 getsockname: 127.0.0.1:5037");

    int t4b = socket(AF_INET, SOCK_STREAM, 0);
    check(bind(t4b, (struct sockaddr*)&a4, sizeof(a4)) == -1 && errno == EADDRINUSE, "the same port again: EADDRINUSE");
    close(t4);
    check(bind(t4b, (struct sockaddr*)&a4, sizeof(a4)) == 0, "free once closed");

    int u4 = socket(AF_INET, SOCK_DGRAM, 0);
    a4.sin_port = 0;
    a4.sin_addr.s_addr = inet_addr("192.0.2.1");
    check(bind(u4, (struct sockaddr*)&a4, sizeof(a4)) == -1 && errno == EADDRNOTAVAIL, "an address no interface has: EADDRNOTAVAIL");
    check(bind(u4, (struct sockaddr*)&a4, 4) == -1 && errno == EINVAL, "a short address: EINVAL");
    printf(failed ? "FAILED\n" : "PASSED\n");
    return failed;
}
