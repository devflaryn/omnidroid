// mount(2) and umount2(2) as vold calls them: `bindmount bind <source> <target>` bind-mounts
// (MS_BIND | MS_REC) after vold's UnmountTree (umount2 MNT_DETACH, where EINVAL and ENOENT mean
// nothing was mounted); `bindmount umount <target>` unmounts. Prints "ok", or the call and errno.
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>

int main(int argc, char** argv) {
    if (argc == 4 && strcmp(argv[1], "bind") == 0) {
        if (umount2(argv[3], MNT_DETACH) < 0 && errno != EINVAL && errno != ENOENT) {
            printf("umount2 %s\n", strerror(errno));
            return 1;
        }
        if (mount(argv[2], argv[3], NULL, MS_BIND | MS_REC, NULL) < 0) {
            printf("mount %s\n", strerror(errno));
            return 1;
        }
    } else if (argc == 3 && strcmp(argv[1], "umount") == 0) {
        if (umount2(argv[2], MNT_DETACH) < 0) {
            printf("umount2 %s\n", strerror(errno));
            return 1;
        }
    } else {
        return 2;
    }
    printf("ok\n");
    return 0;
}
