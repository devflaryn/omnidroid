/* Does a DIRTY mapped page make FSCTL_SET_ZERO_DATA fail with 87?
 *
 * Live QEMU shows 277 failures (gle=87) and 171 successes on the SAME handle,
 * same 4 MiB size, same 64 KiB alignment, offsets well inside the file. Same
 * arguments cannot be both valid and invalid, so the difference has to be
 * file/section STATE. The one state that differs between a live guest and the
 * earlier passing sweep is whether the range has unflushed dirty pages in the
 * mapped section -- the sweep always flushed before punching.
 */
#include <windows.h>
#include <winioctl.h>
#include <stdio.h>
#include <stdint.h>

static double alloc_mb(HANDLE h) {
    FILE_STANDARD_INFO si;
    GetFileInformationByHandleEx(h, FileStandardInfo, &si, sizeof(si));
    return (double)si.AllocationSize.QuadPart / 1048576.0;
}
static void punch(HANDLE h, const char *tag, long long off, long long len) {
    FILE_ZERO_DATA_INFORMATION z; DWORD junk = 0;
    double before = alloc_mb(h);
    z.FileOffset.QuadPart = off; z.BeyondFinalZero.QuadPart = off + len;
    BOOL ok = DeviceIoControl(h, FSCTL_SET_ZERO_DATA, &z, sizeof(z), NULL, 0, &junk, NULL);
    DWORD e = ok ? 0 : GetLastError();
    printf("  %-38s ok=%d gle=%-4lu  %8.1f -> %8.1f MB\n",
           tag, ok, e, before, alloc_mb(h));
}

int main(void) {
    const char *path = "C:/Users/berat/AppData/Local/Temp/claude/punch_dirty.bin";
    const SIZE_T SZ = (SIZE_T)512 * 1024 * 1024;
    DWORD junk = 0;
    HANDLE fh = CreateFileA(path, GENERIC_READ | GENERIC_WRITE,
                            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                            NULL, CREATE_ALWAYS,
                            FILE_ATTRIBUTE_TEMPORARY | FILE_FLAG_DELETE_ON_CLOSE, NULL);
    DeviceIoControl(fh, FSCTL_SET_SPARSE, NULL, 0, NULL, 0, &junk, NULL);
    HANDLE mh = CreateFileMappingA(fh, NULL, PAGE_READWRITE,
                                   (DWORD)((uint64_t)SZ >> 32),
                                   (DWORD)((uint64_t)SZ & 0xFFFFFFFFu), NULL);
    char *view = MapViewOfFile(mh, FILE_MAP_ALL_ACCESS, 0, 0, SZ);
    CloseHandle(mh);
    if (!view) { printf("map %lu\n", GetLastError()); return 1; }

    const long long OFF1 = 64LL << 20, OFF2 = 128LL << 20, LEN = 4LL << 20;

    printf("1. touch a range, punch it WITHOUT flushing\n");
    memset(view + OFF1, 0xAB, (size_t)LEN);
    punch(fh, "dirty, no flush", OFF1, LEN);

    printf("2. same range, FlushViewOfFile first, punch again\n");
    FlushViewOfFile(view + OFF1, (SIZE_T)LEN);
    punch(fh, "after FlushViewOfFile", OFF1, LEN);

    printf("3. a different range, flush BEFORE punching\n");
    memset(view + OFF2, 0xCD, (size_t)LEN);
    FlushViewOfFile(view + OFF2, (SIZE_T)LEN);
    punch(fh, "touched then flushed", OFF2, LEN);

    UnmapViewOfFile(view); CloseHandle(fh);
    return 0;
}
