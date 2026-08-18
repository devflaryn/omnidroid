/* Does a file-backed section cost Windows system commit the way private RAM does? */
#include <windows.h>
#include <psapi.h>
#include <stdio.h>

static void show(const char *tag) {
    PERFORMANCE_INFORMATION pi = { sizeof(pi) };
    GetPerformanceInfo(&pi, sizeof(pi));
    PROCESS_MEMORY_COUNTERS_EX pmc = { sizeof(pmc) };
    GetProcessMemoryInfo(GetCurrentProcess(), (PROCESS_MEMORY_COUNTERS*)&pmc, sizeof(pmc));
    printf("%-22s sys_commit=%6.0f MB  proc_private=%6.0f MB  proc_ws=%6.0f MB\n",
           tag,
           (double)(pi.CommitTotal * pi.PageSize) / (1024.0*1024.0),
           (double)pmc.PrivateUsage / (1024.0*1024.0),
           (double)pmc.WorkingSetSize / (1024.0*1024.0));
    fflush(stdout);
}

int main(int argc, char **argv) {
    const SIZE_T SZ = (SIZE_T)3072 * 1024 * 1024;   /* 3 GiB, one farming instance */
    const char *path = argc > 1 ? argv[1] : "C:/Users/berat/AppData/Local/Temp/claude/ramfile.bin";

    show("baseline");

    /* --- A: private commit, the way QEMU allocates guest RAM today --- */
    void *priv = VirtualAlloc(NULL, SZ, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE);
    if (!priv) { printf("VirtualAlloc failed %lu\n", GetLastError()); return 1; }
    show("after VirtualAlloc 3G");
    memset(priv, 1, 512u * 1024 * 1024);           /* touch 512 MB */
    show("after touch 512M");
    VirtualFree(priv, 0, MEM_RELEASE);
    show("after free");

    /* --- B: file-backed section, the way memory-backend-file would --- */
    HANDLE f = CreateFileA(path, GENERIC_READ | GENERIC_WRITE, 0, NULL,
                           CREATE_ALWAYS, FILE_ATTRIBUTE_TEMPORARY, NULL);
    if (f == INVALID_HANDLE_VALUE) { printf("CreateFile failed %lu\n", GetLastError()); return 1; }
    DWORD junk = 0;
    DeviceIoControl(f, FSCTL_SET_SPARSE, NULL, 0, NULL, 0, &junk, NULL);   /* sparse */

    HANDLE map = CreateFileMappingA(f, NULL, PAGE_READWRITE,
                                    (DWORD)(SZ >> 32), (DWORD)(SZ & 0xFFFFFFFF), NULL);
    if (!map) { printf("CreateFileMapping failed %lu\n", GetLastError()); return 1; }
    show("after CreateFileMapping");
    void *view = MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, SZ);
    if (!view) { printf("MapViewOfFile failed %lu\n", GetLastError()); return 1; }
    show("after MapViewOfFile 3G");
    memset(view, 1, 512u * 1024 * 1024);
    show("after touch 512M (file)");

    /* --- C: can we punch the touched range back out? (the discard path) --- */
    FILE_ZERO_DATA_INFORMATION z; z.FileOffset.QuadPart = 0; z.BeyondFinalZero.QuadPart = 512ll*1024*1024;
    BOOL ok = DeviceIoControl(f, FSCTL_SET_ZERO_DATA, &z, sizeof(z), NULL, 0, &junk, NULL);
    printf("FSCTL_SET_ZERO_DATA -> %d (err %lu)\n", ok, ok ? 0 : GetLastError());
    show("after punch hole");

    UnmapViewOfFile(view); CloseHandle(map); CloseHandle(f);
    show("after unmap/close");
    DeleteFileA(path);
    return 0;
}
