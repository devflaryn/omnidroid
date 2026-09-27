// A unix socket's peer credentials, as lmkd and netd's fwmarkd read them: a child (uid 1234 when
// run as root) connects and sends; the server's SO_PEERCRED names the child, and with SO_PASSCRED each
// message carries the sender's SCM_CREDENTIALS. A socketpair's peer is its own process.
// Prints "ok ..." or "FAIL ..." per check.
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

static int failed;
static void check(int ok, const char* what) { printf("%s %s\n", ok ? "ok" : "FAIL", what); if (!ok) failed = 1; }

int main(void) {
    int sv[2];
    check(socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sv) == 0, "socketpair");
    struct ucred c;
    socklen_t len = sizeof(c);
    check(getsockopt(sv[0], SOL_SOCKET, SO_PEERCRED, &c, &len) == 0 && c.pid == getpid() && c.uid == getuid(), "a socketpair's peer is this process");

    struct sockaddr_un a;
    memset(&a, 0, sizeof(a));
    a.sun_family = AF_UNIX;
    strcpy(a.sun_path, "/data/local/tmp/peercred.sock");
    unlink(a.sun_path);
    int server = socket(AF_UNIX, SOCK_SEQPACKET, 0);
    check(bind(server, (struct sockaddr*)&a, sizeof(a)) == 0 && listen(server, 4) == 0, "bind and listen");
    // On the listening socket, as init sets it for lmkd's (`seqpacket+passcred`): its connections
    // inherit it.
    int on = 1;
    check(setsockopt(server, SOL_SOCKET, SO_PASSCRED, &on, sizeof(on)) == 0, "SO_PASSCRED on the listening socket");

    uid_t want = getuid() == 0 ? 1234 : getuid();
    pid_t child = fork();
    if (child == 0) {
        if (setuid(want) != 0) _exit(2);
        int s = socket(AF_UNIX, SOCK_SEQPACKET, 0);
        if (connect(s, (struct sockaddr*)&a, sizeof(a)) != 0) _exit(3);
        if (write(s, "hello", 5) != 5) _exit(4);
        _exit(0);
    }
    int status = -1;
    int sent = waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0;
    check(sent, "the child connected and sent");
    if (!sent) return 1;

    int conn = accept(server, NULL, NULL);
    check(conn >= 0, "accept");
    len = sizeof(c);
    check(getsockopt(conn, SOL_SOCKET, SO_PEERCRED, &c, &len) == 0 && c.pid == child && c.uid == want, "SO_PEERCRED names the child");

    char buf[16];
    union { struct cmsghdr h; char space[CMSG_SPACE(sizeof(struct ucred))]; } control;
    struct iovec iov = { buf, sizeof(buf) };
    struct msghdr msg;
    memset(&msg, 0, sizeof(msg));
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = &control;
    msg.msg_controllen = sizeof(control);
    ssize_t n = recvmsg(conn, &msg, 0);
    check(n == 5 && memcmp(buf, "hello", 5) == 0, "recvmsg: the message");
    struct cmsghdr* h = CMSG_FIRSTHDR(&msg);
    check(h && h->cmsg_level == SOL_SOCKET && h->cmsg_type == SCM_CREDENTIALS, "recvmsg: SCM_CREDENTIALS");
    if (h) {
        struct ucred* sent = (struct ucred*)CMSG_DATA(h);
        check(sent->pid == child && sent->uid == want, "the credentials are the sender's");
    }
    unlink(a.sun_path);
    printf(failed ? "FAILED\n" : "PASSED\n");
    return failed;
}
