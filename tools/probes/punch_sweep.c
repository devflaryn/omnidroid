/* Why does FSCTL_SET_ZERO_DATA return ERROR_INVALID_PARAMETER(87) against a
 * live guest's RAM file, when the same size and alignment succeed standalone?
 *
 * Sweeps the axes that differ between the two: whether the range is mapped,
 * whether it is mapped into a WHPX partition, whether the file is fully
 * written, and whether the offset is zero or deep into the file.
 */
#include <windows.h>
#include <winioctl.h>
#include <winhvplatform.h>
#include <stdio.h>
#include <stdint.h>

static double alloc_mb(HANDLE h) {
    FILE_STANDARD_INFO si;
    if (!GetFileInformationByHandleEx(h, FileStandardInfo, &si, sizeof(si))) return -1;
    return (double)si.AllocationSize.QuadPart / 1048576.0;
}

static void try_punch(HANDLE h, const char *tag, long long off, long long len) {
    FILE_ZERO_DATA_INFORMATION z;
    DWORD junk = 0;
    double before = alloc_mb(h);
    z.FileOffset.QuadPart = off;
    z.BeyondFinalZero.QuadPart = off + len;
    BOOL ok = DeviceIoControl(h, FSCTL_SET_ZERO_DATA, &z, sizeof(z), NULL, 0, &junk, NULL);
    DWORD err = ok ? 0 : GetLastError();
    double after = alloc_mb(h);
    printf("  %-34s off=%10lld len=%8lld  ok=%d err=%-4lu  %8.1f -> %8.1f MB\n",
           tag, off, len, ok, err, before, after);
}

int main(void) {
    const char *path = "C:/Users/berat/AppData/Local/Temp/claude/punch_sweep.bin";
    const SIZE_T SZ = (SIZE_T)3072 * 1024 * 1024;
    DWORD junk = 0;

    HANDLE fh = CreateFileA(path, GENERIC_READ | GENERIC_WRITE,
                            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                            NULL, CREATE_ALWAYS,
                            FILE_ATTRIBUTE_TEMPORARY | FILE_FLAG_DELETE_ON_CLOSE, NULL);
    if (fh == INVALID_HANDLE_VALUE) { printf("open %lu\n", GetLastError()); return 1; }
    if (!DeviceIoControl(fh, FSCTL_SET_SPARSE, NULL, 0, NULL, 0, &junk, NULL)) {
        printf("sparse %lu\n", GetLastError()); return 1;
    }
    HANDLE mh = CreateFileMappingA(fh, NULL, PAGE_READWRITE,
                                   (DWORD)((uint64_t)SZ >> 32),
                                   (DWORD)((uint64_t)SZ & 0xFFFFFFFFu), NULL);
    if (!mh) { printf("mapping %lu\n", GetLastError()); return 1; }
    void *view = MapViewOfFile(mh, FILE_MAP_ALL_ACCESS, 0, 0, SZ);
    CloseHandle(mh);
    if (!view) { printf("view %lu\n", GetLastError()); return 1; }

    printf("A. mapped, NOT whpx, file untouched\n");
    try_punch(fh, "4 MiB @ 512 MiB", 512LL<<20, 4LL<<20);

    printf("B. mapped, NOT whpx, range TOUCHED first\n");
    memset((char *)view + (512LL<<20), 0xAB, 8LL<<20);
    FlushFileBuffers(fh);
    try_punch(fh, "4 MiB @ 512 MiB (touched)", 512LL<<20, 4LL<<20);

    /* now add the hypervisor */
    WHV_CAPABILITY cap = {0}; UINT32 w = 0;
    if (FAILED(WHvGetCapability(WHvCapabilityCodeHypervisorPresent, &cap, sizeof(cap), &w))
        || !cap.HypervisorPresent) { printf("WHPX unavailable\n"); return 2; }
    WHV_PARTITION_HANDLE part = NULL;
    WHvCreatePartition(&part);
    WHV_PARTITION_PROPERTY prop = {0}; prop.ProcessorCount = 1;
    WHvSetPartitionProperty(part, WHvPartitionPropertyCodeProcessorCount, &prop, sizeof(prop));
    WHvSetupPartition(part);
    HRESULT hr = WHvMapGpaRange(part, view, 0, SZ,
                                WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite |
                                WHvMapGpaRangeFlagExecute);
    printf("C. WHvMapGpaRange -> 0x%08lx\n", (unsigned long)hr);

    try_punch(fh, "4 MiB @ 512 MiB (whpx)",  512LL<<20, 4LL<<20);
    try_punch(fh, "4 MiB @ 1 GiB   (whpx)",  1024LL<<20, 4LL<<20);
    try_punch(fh, "64 KiB @ 512 MiB (whpx)", 512LL<<20, 65536LL);
    try_punch(fh, "256 MiB @ 0     (whpx)",  0LL, 256LL<<20);

    printf("D. touch the whole 3 GiB, then punch under whpx\n");
    memset(view, 0xCD, SZ);
    FlushFileBuffers(fh);
    try_punch(fh, "4 MiB @ 512 MiB (full)",  512LL<<20, 4LL<<20);
    try_punch(fh, "4 MiB @ 2 GiB   (full)",  2048LL<<20, 4LL<<20);

    WHvUnmapGpaRange(part, 0, SZ);
    UnmapViewOfFile(view); CloseHandle(fh); WHvDeletePartition(part);
    return 0;
}
