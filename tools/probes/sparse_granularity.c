/* At what granularity does NTFS actually RECLAIM a punched hole?
 *
 * FSCTL_SET_ZERO_DATA always reports success. That is not the same as freeing
 * clusters: a range smaller than, or unaligned to, the filesystem's
 * deallocation unit is zero-FILLED and stays allocated. This decides whether a
 * guest's 4 KiB page-granular discards can ever return disk.
 */
#include <windows.h>
#include <winioctl.h>
#include <stdio.h>

static double alloc_mb(HANDLE h) {
    FILE_STANDARD_INFO si;
    if (!GetFileInformationByHandleEx(h, FileStandardInfo, &si, sizeof(si))) return -1;
    return (double)si.AllocationSize.QuadPart / 1048576.0;
}

static int punch(HANDLE h, long long off, long long len) {
    FILE_ZERO_DATA_INFORMATION z;
    DWORD junk = 0;
    z.FileOffset.QuadPart = off;
    z.BeyondFinalZero.QuadPart = off + len;
    return DeviceIoControl(h, FSCTL_SET_ZERO_DATA, &z, sizeof(z), NULL, 0, &junk, NULL);
}

int main(void) {
    const char *path = "C:/Users/berat/AppData/Local/Temp/claude/sparse_gran.bin";
    const SIZE_T SZ = 16u * 1024 * 1024;
    DWORD junk = 0, wrote = 0;
    HANDLE h = CreateFileA(path, GENERIC_READ | GENERIC_WRITE, 0, NULL,
                           CREATE_ALWAYS, FILE_ATTRIBUTE_TEMPORARY, NULL);
    if (h == INVALID_HANDLE_VALUE) { printf("open %lu\n", GetLastError()); return 1; }
    DeviceIoControl(h, FSCTL_SET_SPARSE, NULL, 0, NULL, 0, &junk, NULL);

    char *buf = malloc(SZ);
    memset(buf, 0xAB, SZ);
    WriteFile(h, buf, (DWORD)SZ, &wrote, NULL);
    FlushFileBuffers(h);
    printf("after writing 16 MB          allocated=%7.3f MB\n", alloc_mb(h));

    struct { const char *tag; long long off, len; } steps[] = {
        {"punch 4 KiB  @ 1 MiB (aligned)",   1048576LL,            4096LL},
        {"punch 32 KiB @ 2 MiB (aligned)",   2097152LL,           32768LL},
        {"punch 64 KiB @ 3 MiB (aligned)",   3145728LL,           65536LL},
        {"punch 64 KiB @ 4 MiB+4K (unalign)",4194304LL + 4096LL,  65536LL},
        {"punch 1 MiB  @ 8 MiB (aligned)",   8388608LL,         1048576LL},
    };
    for (int i = 0; i < 5; i++) {
        double before = alloc_mb(h);
        int ok = punch(h, steps[i].off, steps[i].len);
        FlushFileBuffers(h);
        double after = alloc_mb(h);
        printf("%-36s ok=%d  %7.3f -> %7.3f MB   reclaimed=%7.3f MB\n",
               steps[i].tag, ok, before, after, before - after);
    }
    CloseHandle(h);
    DeleteFileA(path);
    free(buf);
    return 0;
}
