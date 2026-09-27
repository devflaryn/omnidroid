// Name resolution as an app does it, through bionic's resolver client and libnetd_client to
// /dev/socket/dnsproxyd: getaddrinfo (the image's hosts file, a name the host resolves, a name no
// one has), gethostbyname, gethostbyaddr, and resNetworkQuery/resNetworkResult (the resnsend path
// of android_res_nquery and Java's DnsResolver). Prints "ok ..." or "FAIL ..." per check, and
// "addr <address>" for each address argv[1] resolved to.
//
// Names in the image's hosts file are answered by bionic itself before it asks the proxy (measured:
// with no proxy, the localhost, ip6-localhost, gethostbyname and gethostbyaddr checks still pass);
// the name the host resolves and the two resnsend checks are the proxy's.
//
// argv: <a name the host resolves, or "-">
#include <arpa/inet.h>
#include <dlfcn.h>
#include <errno.h>
#include <netdb.h>
#include <netinet/in.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static int failed;
static void check(int ok, const char* what) {
    printf("%s %s\n", ok ? "ok" : "FAIL", what);
    if (!ok) failed = 1;
    fflush(stdout);
}

int main(int argc, char** argv) {
    if (argc < 2) { printf("FAIL usage\n"); return 2; }
    struct addrinfo hints;
    memset(&hints, 0, sizeof(hints));
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    struct addrinfo* res = NULL;
    int rc = getaddrinfo("localhost", NULL, &hints, &res);
    check(rc == 0 && res != NULL && res->ai_family == AF_INET && res->ai_socktype == SOCK_STREAM &&
          ((struct sockaddr_in*)res->ai_addr)->sin_addr.s_addr == htonl(INADDR_LOOPBACK) && res->ai_next == NULL,
          "getaddrinfo localhost: 127.0.0.1 alone, as the image's hosts file says");
    if (rc != 0) printf("  (%d: %s)\n", rc, gai_strerror(rc));
    if (res) freeaddrinfo(res);

    hints.ai_family = AF_INET6;
    hints.ai_flags = AI_CANONNAME;
    res = NULL;
    rc = getaddrinfo("ip6-localhost", "8080", &hints, &res);
    struct in6_addr loop6 = IN6ADDR_LOOPBACK_INIT;
    check(rc == 0 && res && res->ai_family == AF_INET6 && res->ai_addrlen == sizeof(struct sockaddr_in6) &&
          memcmp(&((struct sockaddr_in6*)res->ai_addr)->sin6_addr, &loop6, 16) == 0 &&
          ((struct sockaddr_in6*)res->ai_addr)->sin6_port == htons(8080) && res->ai_canonname && strcmp(res->ai_canonname, "ip6-localhost") == 0,
          "getaddrinfo ip6-localhost 8080: [::1]:8080, its canonical name");
    if (res) freeaddrinfo(res);

    if (strcmp(argv[1], "-") != 0) {
        hints.ai_family = AF_UNSPEC;
        hints.ai_flags = 0;
        res = NULL;
        rc = getaddrinfo(argv[1], "443", &hints, &res);
        int n = 0;
        for (struct addrinfo* ai = res; ai; ai = ai->ai_next) {
            char text[INET6_ADDRSTRLEN] = "?";
            const void* a = ai->ai_family == AF_INET ? (const void*)&((struct sockaddr_in*)ai->ai_addr)->sin_addr
                                                     : (const void*)&((struct sockaddr_in6*)ai->ai_addr)->sin6_addr;
            inet_ntop(ai->ai_family, a, text, sizeof(text));
            int port = ntohs(ai->ai_family == AF_INET ? ((struct sockaddr_in*)ai->ai_addr)->sin_port : ((struct sockaddr_in6*)ai->ai_addr)->sin6_port);
            printf("addr %s\n", text);
            if (port == 443) n++;
        }
        check(rc == 0 && n > 0, "getaddrinfo of a name the host resolves, port 443");
        if (res) freeaddrinfo(res);
    }

    res = NULL;
    rc = getaddrinfo("no-such-name.invalid", NULL, NULL, &res);
    check(rc != 0 && res == NULL, "getaddrinfo of a name no one has: an EAI_ error");

    struct hostent* h = gethostbyname("localhost");
    check(h && h->h_addrtype == AF_INET && h->h_length == 4 && h->h_addr_list[0] &&
          memcmp(h->h_addr_list[0], "\x7f\0\0\x01", 4) == 0 && strcmp(h->h_name, "localhost") == 0,
          "gethostbyname localhost");

    struct in_addr lo = {htonl(INADDR_LOOPBACK)};
    h = gethostbyaddr(&lo, sizeof(lo), AF_INET);
    check(h && strcmp(h->h_name, "localhost") == 0, "gethostbyaddr 127.0.0.1: localhost");

    // resnsend: what android_res_nquery (libandroid) calls.
    void* netd = dlopen("libnetd_client.so", RTLD_NOW);
    int (*query)(unsigned, const char*, int, int, uint32_t) = netd ? (int (*)(unsigned, const char*, int, int, uint32_t))dlsym(netd, "resNetworkQuery") : NULL;
    int (*result)(int, int*, uint8_t*, size_t) = netd ? (int (*)(int, int*, uint8_t*, size_t))dlsym(netd, "resNetworkResult") : NULL;
    check(query && result, "libnetd_client's resNetworkQuery/resNetworkResult");
    if (query && result) {
        int fd = query(0, "localhost", 1 /* IN */, 1 /* A */, 0);
        uint8_t answer[512];
        int rcode = -1;
        int len = fd >= 0 ? result(fd, &rcode, answer, sizeof(answer)) : fd;
        // One answer: header, the question (localhost: 11 bytes + type + class), then the record.
        int ok = len >= 12 + 15 + 16 && rcode == 0 && (answer[2] & 0x80) && answer[7] >= 1 &&
                 memcmp(answer + len - 4, "\x7f\0\0\x01", 4) == 0;
        check(ok, "resnsend: an A record for localhost, 127.0.0.1");
        if (!ok) printf("  (fd %d len %d rcode %d)\n", fd, len, rcode);
        fd = query(0, "localhost", 1, 16 /* TXT */, 0);
        rcode = -1;
        len = fd >= 0 ? result(fd, &rcode, answer, sizeof(answer)) : fd;
        check(len >= 12 && rcode == 4, "resnsend: a TXT question is NOTIMP");
    }

    printf(failed ? "FAILED\n" : "PASSED\n");
    return failed;
}
