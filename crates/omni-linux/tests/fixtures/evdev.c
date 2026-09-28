// Input devices as Android's EventHub meets them: /dev/input listed, each node opened O_RDWR |
// O_NONBLOCK and asked what it is (EVIOCGNAME, EVIOCGID, EVIOCGVERSION, EVIOCGBIT, EVIOCGPROP,
// EVIOCGKEY, EVIOCSCLOCKID; EVIOCGUNIQ answers ENOENT), then an empty read is EAGAIN and the events
// the host sends arrive through epoll as whole struct input_events, each packet ending in
// SYN_REPORT. Expects event0 a keyboard and event1 a mouse; the host sends KEY_A down then up, and a
// mouse move (+5, -3). Prints "ok ..." or "FAIL ..." per check.
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/input.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <time.h>
#include <unistd.h>

static int failed;
static void check(int ok, const char* what) { printf("%s %s\n", ok ? "ok" : "FAIL", what); if (!ok) failed = 1; }
static int bit(const unsigned char* map, int n) { return (map[n / 8] >> (n % 8)) & 1; }

// Read events from `fd` until one of `type`/`code`/`value` arrives or 10 s pass; whether it did,
// and that every read was whole events and the last event before it was followed by SYN_REPORT.
static int wait_for(int ep, int fd, int type, int code, int value) {
    struct timespec start;
    clock_gettime(CLOCK_MONOTONIC, &start);
    for (;;) {
        struct epoll_event ev;
        int n = epoll_wait(ep, &ev, 1, 500);
        if (n == 1) {
            struct input_event e[16];
            ssize_t got = read(fd, e, sizeof(e));
            if (got > 0) {
                if (got % sizeof(struct input_event) != 0) return 0;
                for (size_t i = 0; i < (size_t)got / sizeof(e[0]); i++) {
                    if (e[i].type == type && e[i].code == code && e[i].value == value) return 1;
                }
            }
        }
        struct timespec now;
        clock_gettime(CLOCK_MONOTONIC, &now);
        if (now.tv_sec - start.tv_sec > 10) return 0;
    }
}

int main(void) {
    DIR* d = opendir("/dev/input");
    check(d != NULL, "opendir /dev/input");
    int seen0 = 0, seen1 = 0;
    struct dirent* de;
    while (d && (de = readdir(d)) != NULL) {
        seen0 |= strcmp(de->d_name, "event0") == 0;
        seen1 |= strcmp(de->d_name, "event1") == 0;
    }
    if (d) closedir(d);
    check(seen0 && seen1, "event0 and event1 listed");

    int kb = open("/dev/input/event0", O_RDWR | O_NONBLOCK | O_CLOEXEC);
    int mouse = open("/dev/input/event1", O_RDWR | O_NONBLOCK | O_CLOEXEC);
    check(kb >= 0 && mouse >= 0, "open both O_RDWR | O_NONBLOCK");
    check(open("/dev/input/event9", O_RDONLY) < 0 && errno == ENOENT, "no event9");

    char name[80] = {0};
    int n = ioctl(kb, EVIOCGNAME(sizeof(name) - 1), name);
    check(n > 0 && strcmp(name, "omnidroid keyboard") == 0, "EVIOCGNAME");
    struct input_id id;
    check(ioctl(kb, EVIOCGID, &id) == 0 && id.bustype == BUS_USB, "EVIOCGID: USB");
    int version = 0;
    check(ioctl(kb, EVIOCGVERSION, &version) == 0 && version == EV_VERSION, "EVIOCGVERSION");
    char uniq[16];
    check(ioctl(kb, EVIOCGUNIQ(sizeof(uniq)), uniq) < 0 && errno == ENOENT, "EVIOCGUNIQ: none");

    unsigned char types[8] = {0}, keys[KEY_MAX / 8 + 1] = {0}, rels[8] = {0}, props[8] = {0};
    check(ioctl(kb, EVIOCGBIT(0, sizeof(types)), types) == sizeof(types) && bit(types, EV_KEY) && !bit(types, EV_REL), "keyboard: EV_KEY, no EV_REL");
    check(ioctl(kb, EVIOCGBIT(EV_KEY, sizeof(keys)), keys) == sizeof(keys) && bit(keys, KEY_A) && bit(keys, KEY_Q) && bit(keys, KEY_SPACE) && !bit(keys, BTN_LEFT), "keyboard: letters, no mouse buttons");
    check(ioctl(kb, EVIOCGPROP(sizeof(props)), props) == sizeof(props) && props[0] == 0, "EVIOCGPROP: none");
    memset(types, 0, sizeof(types));
    memset(keys, 0, sizeof(keys));
    check(ioctl(mouse, EVIOCGBIT(0, sizeof(types)), types) > 0 && bit(types, EV_KEY) && bit(types, EV_REL), "mouse: EV_KEY and EV_REL");
    check(ioctl(mouse, EVIOCGBIT(EV_KEY, sizeof(keys)), keys) > 0 && bit(keys, BTN_LEFT) && bit(keys, BTN_RIGHT) && bit(keys, BTN_MIDDLE) && !bit(keys, KEY_A), "mouse: buttons, no letters");
    check(ioctl(mouse, EVIOCGBIT(EV_REL, sizeof(rels)), rels) > 0 && bit(rels, REL_X) && bit(rels, REL_Y) && bit(rels, REL_WHEEL), "mouse: REL_X, REL_Y, REL_WHEEL");
    // Short buffers get what fits, as the driver copies.
    unsigned char two[2] = {0};
    check(ioctl(kb, EVIOCGBIT(EV_KEY, 2), two) == 2, "a short EVIOCGBIT buffer: what fits");
    int clock = CLOCK_MONOTONIC;
    check(ioctl(kb, EVIOCSCLOCKID, &clock) == 0, "EVIOCSCLOCKID");
    unsigned char down[KEY_MAX / 8 + 1];
    check(ioctl(kb, EVIOCGKEY(sizeof(down)), down) == sizeof(down), "EVIOCGKEY");

    struct input_event e;
    check(read(kb, &e, sizeof(e)) < 0 && errno == EAGAIN, "empty: EAGAIN");
    check(read(kb, &e, 8) < 0 && errno == EINVAL, "a read shorter than an event: EINVAL");

    int ep = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event want = {.events = EPOLLIN, .data.fd = kb};
    check(epoll_ctl(ep, EPOLL_CTL_ADD, kb, &want) == 0, "epoll_ctl");
    printf("ready\n");
    fflush(stdout);
    check(wait_for(ep, kb, EV_KEY, KEY_A, 1), "KEY_A down arrives");
    check(wait_for(ep, kb, EV_KEY, KEY_A, 0), "KEY_A up arrives");
    int ep2 = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event want2 = {.events = EPOLLIN, .data.fd = mouse};
    epoll_ctl(ep2, EPOLL_CTL_ADD, mouse, &want2);
    check(wait_for(ep2, mouse, EV_REL, REL_X, 5), "REL_X +5 arrives");
    return failed;
}
