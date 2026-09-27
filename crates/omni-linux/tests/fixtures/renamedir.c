// A directory renamed while a file in it is open (PackageManager moves an install's staging
// directory while the APK in it is still open): the rename succeeds, the open descriptor still
// reads the file, and the file is found under the new name. Prints "ok ..." or "FAIL ...".
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static int failed;
static void check(int ok, const char* what) { printf("%s %s\n", ok ? "ok" : "FAIL", what); if (!ok) failed = 1; }

int main(void) {
    const char* from = "/data/local/tmp/stage.tmp";
    const char* to = "/data/local/tmp/final";
    unlink("/data/local/tmp/final/base.apk");
    unlink("/data/local/tmp/final/sub/x");
    rmdir("/data/local/tmp/final/sub");
    rmdir(to);
    check(mkdir(from, 0755) == 0 && mkdir("/data/local/tmp/stage.tmp/sub", 0755) == 0, "mkdir");
    int fd = open("/data/local/tmp/stage.tmp/base.apk", O_RDWR | O_CREAT, 0644);
    check(fd >= 0 && write(fd, "apk!", 4) == 4, "a file written and kept open");
    int sub = open("/data/local/tmp/stage.tmp/sub/x", O_RDWR | O_CREAT, 0644);
    check(sub >= 0, "a file in a subdirectory kept open");
    check(rename(from, to) == 0, "the directory renamed while they are open");
    char buf[8] = {0};
    check(pread(fd, buf, 4, 0) == 4 && memcmp(buf, "apk!", 4) == 0, "the open descriptor still reads the file");
    struct stat st;
    check(stat("/data/local/tmp/final/base.apk", &st) == 0 && st.st_size == 4, "the file under the new name");
    check(stat("/data/local/tmp/final/sub/x", &st) == 0, "the subdirectory's file under the new name");
    check(stat(from, &st) == -1, "the old name is gone");
    close(fd);
    close(sub);
    printf(failed ? "FAILED\n" : "PASSED\n");
    return failed;
}
