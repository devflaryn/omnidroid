/* Reproduce the live 87s: punch while another thread writes the mapping.
 *
 * Live QEMU fails in runs of exactly 64 (a reporting batch), then succeeds in
 * runs of 64, on the same handle with identical 4 MiB aligned arguments. Same
 * arguments cannot be both valid and invalid, so the variable is concurrent
 * state. This writes the view from a second thread while punching, counts the
 * outcome, and then tests whether a flush-and-retry converts a failure.
 */
#include <windows.h>
#include <winioctl.h>
#include <stdio.h>
#include <stdint.h>

static char *g_view; static SIZE_T g_sz; static volatile LONG g_stop;

static DWORD WINAPI writer(LPVOID p) {
    (void)p;
    while (!g_stop) {
        for (SIZE_T o = 0; o < g_sz && !g_stop; o += 65536) g_view[o] ^= 1;
    }
    return 0;
}

static BOOL punch(HANDLE h, long long off, long long len, DWORD *gle) {
    FILE_ZERO_DATA_INFORMATION z; DWORD junk = 0;
    z.FileOffset.QuadPart = off; z.BeyondFinalZero.QuadPart = off + len;
    BOOL ok = DeviceIoControl(h, FSCTL_SET_ZERO_DATA, &z, sizeof(z), NULL, 0, &junk, NULL);
    *gle = ok ? 0 : GetLastError();
    return ok;
}

int main(void) {
    const char *path = "C:/Users/berat/AppData/Local/Temp/claude/punch_conc.bin";
    const SIZE_T SZ = (SIZE_T)768 * 1024 * 1024;
    const long long LEN = 4LL << 20;
    DWORD junk = 0, gle = 0;
    HANDLE fh = CreateFileA(path, GENERIC_READ | GENERIC_WRITE,
                            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                            NULL, CREATE_ALWAYS,
                            FILE_ATTRIBUTE_TEMPORARY | FILE_FLAG_DELETE_ON_CLOSE, NULL);
    DeviceIoControl(fh, FSCTL_SET_SPARSE, NULL, 0, NULL, 0, &junk, NULL);
    HANDLE mh = CreateFileMappingA(fh, NULL, PAGE_READWRITE,
                                   (DWORD)((uint64_t)SZ >> 32),
                                   (DWORD)((uint64_t)SZ & 0xFFFFFFFFu), NULL);
    g_view = MapViewOfFile(mh, FILE_MAP_ALL_ACCESS, 0, 0, SZ);
    CloseHandle(mh);
    if (!g_view) { printf("map %lu\n", GetLastError()); return 1; }
    g_sz = SZ;
    memset(g_view, 0xAB, SZ);            /* make it all allocated */
    FlushFileBuffers(fh);

    HANDLE th = CreateThread(NULL, 0, writer, NULL, 0, NULL);
    Sleep(200);

    int okc = 0, failc = 0, retry_ok = 0, retry_fail = 0;
    DWORD firstgle = 0;
    for (long long off = 0; off + LEN <= (long long)SZ; off += LEN) {
        if (punch(fh, off, LEN, &gle)) { okc++; continue; }
        failc++; if (!firstgle) firstgle = gle;
        /* flush just that range, then retry once */
        FlushViewOfFile(g_view + off, (SIZE_T)LEN);
        if (punch(fh, off, LEN, &gle)) retry_ok++; else retry_fail++;
    }
    InterlockedExchange(&g_stop, 1);
    WaitForSingleObject(th, 5000);

    printf("punches attempted : %lld\n", (long long)(SZ / LEN));
    printf("  first-try ok    : %d\n", okc);
    printf("  first-try FAIL  : %d   (first gle=%lu)\n", failc, firstgle);
    printf("  retry after flush ok  : %d\n", retry_ok);
    printf("  retry after flush FAIL: %d\n", retry_fail);
    FILE_STANDARD_INFO si;
    GetFileInformationByHandleEx(fh, FileStandardInfo, &si, sizeof(si));
    printf("allocated at end  : %.1f MB of %.1f MB\n",
           (double)si.AllocationSize.QuadPart/1048576.0, (double)SZ/1048576.0);
    UnmapViewOfFile(g_view); CloseHandle(fh);
    return 0;
}
