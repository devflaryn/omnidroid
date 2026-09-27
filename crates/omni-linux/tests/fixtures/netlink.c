// A kernel uevent socket as ueventd and vold open it: socket, buffer and credential options, bind
// to every multicast group, getsockname. With no device added or removed, nothing is there to read.
#include <linux/netlink.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

int main(void) {
    int fd = socket(PF_NETLINK, SOCK_DGRAM | SOCK_CLOEXEC, NETLINK_KOBJECT_UEVENT);
    if (fd < 0) { perror("socket"); return 1; }
    int size = 64 * 1024, on = 1;
    if (setsockopt(fd, SOL_SOCKET, SO_RCVBUFFORCE, &size, sizeof(size)) < 0) { perror("SO_RCVBUFFORCE"); return 1; }
    if (setsockopt(fd, SOL_SOCKET, SO_PASSCRED, &on, sizeof(on)) < 0) { perror("SO_PASSCRED"); return 1; }
    struct sockaddr_nl addr;
    memset(&addr, 0, sizeof(addr));
    addr.nl_family = AF_NETLINK;
    addr.nl_pid = 0; // the kernel assigns one
    addr.nl_groups = 0xffffffff;
    if (bind(fd, (struct sockaddr*)&addr, sizeof(addr)) < 0) { perror("bind"); return 1; }
    struct sockaddr_nl bound;
    socklen_t len = sizeof(bound);
    memset(&bound, 0, sizeof(bound));
    if (getsockname(fd, (struct sockaddr*)&bound, &len) < 0) { perror("getsockname"); return 1; }
    if (bound.nl_family != AF_NETLINK || bound.nl_pid == 0 || len != sizeof(bound)) {
        fprintf(stderr, "getsockname: family %d pid %u len %u\n", bound.nl_family, bound.nl_pid, len);
        return 1;
    }
    struct pollfd p = {.fd = fd, .events = POLLIN};
    int ready = poll(&p, 1, 100);
    if (ready != 0) { fprintf(stderr, "poll: %d revents %#x\n", ready, p.revents); return 1; }
    char buf[256];
    if (recv(fd, buf, sizeof(buf), MSG_DONTWAIT) >= 0) { fprintf(stderr, "recv: an event\n"); return 1; }
    printf("netlink ok pid %s\n", bound.nl_pid == (unsigned)getpid() ? "=getpid" : "other");
    return 0;
}
