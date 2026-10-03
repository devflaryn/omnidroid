// The omnidroid root tool: one multi-call binary for `su`, `resetprop` and `magisk`.
// Our own source, not Magisk's. It talks to the engine through the omni_root syscall (nr 510).
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define OP_ELEVATE 1
#define OP_STATUS 2
#define OP_SETPROP 3
#define OP_DELPROP 4

static long omni_root(long op, long a1, long a2, long a3) {
    register long x8 asm("x8") = 510;
    register long x0 asm("x0") = op;
    register long x1 asm("x1") = a1;
    register long x2 asm("x2") = a2;
    register long x3 asm("x3") = a3;
    asm volatile ("svc #0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2), "r"(x3) : "memory");
    return x0;
}

static void err(const char *s) {
    ssize_t r = write(2, s, strlen(s));
    (void)r;
}

static int is_number(const char *s) {
    if (!*s) return 0;
    for (; *s; s++)
        if (*s < '0' || *s > '9') return 0;
    return 1;
}

static int applet_su(int argc, char **argv) {
    long uid = 0;
    const char *cmd = NULL;
    for (int i = 1; i < argc; i++) {
        const char *a = argv[i];
        if (!strcmp(a, "-c") || !strcmp(a, "--command")) {
            // The rest is one command string.
            static char buf[8192];
            buf[0] = 0;
            for (int j = i + 1; j < argc; j++) {
                if (j > i + 1) strncat(buf, " ", sizeof buf - strlen(buf) - 1);
                strncat(buf, argv[j], sizeof buf - strlen(buf) - 1);
            }
            cmd = buf;
            break;
        }
        if (is_number(a)) uid = atol(a);
        // -, -l, -p, -mm, --mount-master and anything else: accepted and ignored.
    }
    if (omni_root(OP_ELEVATE, uid, 0, 0) < 0) {
        err("Permission denied\n");
        exit(1);
    }
    if (cmd) {
        char *args[] = {"sh", "-c", (char *)cmd, NULL};
        execv("/system/bin/sh", args);
    } else {
        char *args[] = {"sh", NULL};
        execv("/system/bin/sh", args);
    }
    err("su: exec failed\n");
    return 127;
}

static int set_prop(const char *name, const char *value) {
    long r = omni_root(OP_SETPROP, (long)name, (long)value, 0);
    if (r < 0) {
        err("resetprop: failed\n");
        return 1;
    }
    return 0;
}

static int apply_file(const char *path) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        err("resetprop: cannot open file\n");
        return 1;
    }
    static char data[65536];
    size_t n = 0;
    ssize_t r;
    while (n < sizeof data - 1 && (r = read(fd, data + n, sizeof data - 1 - n)) > 0) n += (size_t)r;
    close(fd);
    data[n] = 0;
    int rc = 0;
    char *save = data;
    while (*save) {
        char *line = save;
        char *nl = strchr(save, '\n');
        if (nl) { *nl = 0; save = nl + 1; } else { save += strlen(save); }
        size_t len = strlen(line);
        if (len && line[len - 1] == '\r') line[--len] = 0;
        while (*line == ' ' || *line == '\t') line++;
        if (!*line || *line == '#') continue;
        char *sep = line + strcspn(line, "= \t");
        if (!*sep) continue;
        char sc = *sep;
        *sep = 0;
        char *value = sep + 1;
        if (sc != '=') {
            // `name value`: skip blanks, and a single `=` between them is tolerated.
            while (*value == ' ' || *value == '\t') value++;
            if (*value == '=') value++;
        }
        if (set_prop(line, value)) rc = 1;
    }
    return rc;
}

static int applet_resetprop(int argc, char **argv) {
    int del = 0;
    const char *file = NULL;
    const char *pos[2] = {NULL, NULL};
    int np = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "-n")) continue;
        if (!strcmp(argv[i], "--delete") || !strcmp(argv[i], "-d")) { del = 1; continue; }
        if (!strcmp(argv[i], "--file") || !strcmp(argv[i], "-f")) {
            if (i + 1 < argc) file = argv[++i];
            continue;
        }
        if (np < 2) pos[np++] = argv[i];
    }
    if (file) return apply_file(file);
    if (del) {
        if (!pos[0]) return 1;
        return omni_root(OP_DELPROP, (long)pos[0], 0, 0) < 0 ? 1 : 0;
    }
    if (np == 2) return set_prop(pos[0], pos[1]);
    // R1: get (NAME only) and list (no args) print nothing; use getprop.
    return 0;
}

static int applet_magisk(int argc, char **argv) {
    if (argc < 2) {
        err("usage: magisk [-v|-V|-c|--path]\n");
        return 1;
    }
    const char *a = argv[1];
    if (!strcmp(a, "-v") || !strcmp(a, "-c")) {
        printf("%ld:MAGISK:R\n", omni_root(OP_STATUS, 0, 0, 0));
        return 0;
    }
    if (!strcmp(a, "-V")) {
        printf("%ld\n", omni_root(OP_STATUS, 0, 0, 0));
        return 0;
    }
    if (!strcmp(a, "--path")) {
        printf("/debug_ramdisk\n");
        return 0;
    }
    if (!strcmp(a, "--denylist") || !strcmp(a, "--sqlite")) {
        err("unsupported in omnidroid (R1)\n");
        return 1;
    }
    err("magisk: unknown applet\n");
    return 1;
}

int main(int argc, char **argv) {
    const char *base = strrchr(argv[0], '/');
    base = base ? base + 1 : argv[0];
    if (!strcmp(base, "su")) return applet_su(argc, argv);
    if (!strcmp(base, "resetprop")) return applet_resetprop(argc, argv);
    if (!strcmp(base, "magisk")) {
        if (argc >= 2 && (!strcmp(argv[1], "su"))) return applet_su(argc - 1, argv + 1);
        if (argc >= 2 && (!strcmp(argv[1], "resetprop"))) return applet_resetprop(argc - 1, argv + 1);
        return applet_magisk(argc, argv);
    }
    err("magisk: unknown applet name\n");
    return 1;
}
