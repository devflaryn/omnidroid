/* Will WHPX map a FILE-BACKED view as guest RAM, and does it stay off the
 * commit charge?  This is the single question the memory design rests on. */
#include <windows.h>
#include <psapi.h>
#include <winhvplatform.h>
#include <stdio.h>

static double sys_commit_mb(void) {
    PERFORMANCE_INFORMATION pi = { sizeof(pi) };
    GetPerformanceInfo(&pi, sizeof(pi));
    return (double)(pi.CommitTotal * pi.PageSize) / (1024.0*1024.0);
}
static double proc_priv_mb(void) {
    PROCESS_MEMORY_COUNTERS_EX p = { sizeof(p) };
    GetProcessMemoryInfo(GetCurrentProcess(), (PROCESS_MEMORY_COUNTERS*)&p, sizeof(p));
    return (double)p.PrivateUsage / (1024.0*1024.0);
}
static void show(const char *tag) {
    printf("%-30s sys_commit=%7.0f MB  proc_private=%7.0f MB\n",
           tag, sys_commit_mb(), proc_priv_mb());
    fflush(stdout);
}

int main(void) {
    const SIZE_T SZ = (SIZE_T)3072 * 1024 * 1024;
    const char *path = "C:/Users/berat/AppData/Local/Temp/claude/whpx_ram.bin";
    HRESULT hr;
    WHV_CAPABILITY cap = {0};
    UINT32 written = 0;

    hr = WHvGetCapability(WHvCapabilityCodeHypervisorPresent, &cap, sizeof(cap), &written);
    printf("hypervisor present: hr=0x%08lx value=%d\n", (unsigned long)hr, cap.HypervisorPresent);
    if (FAILED(hr) || !cap.HypervisorPresent) { printf("WHPX unavailable\n"); return 2; }

    WHV_PARTITION_HANDLE part = NULL;
    hr = WHvCreatePartition(&part);
    if (FAILED(hr)) { printf("WHvCreatePartition 0x%08lx\n", (unsigned long)hr); return 1; }
    WHV_PARTITION_PROPERTY prop = {0};
    prop.ProcessorCount = 1;
    hr = WHvSetPartitionProperty(part, WHvPartitionPropertyCodeProcessorCount, &prop, sizeof(prop));
    if (FAILED(hr)) { printf("SetProcessorCount 0x%08lx\n", (unsigned long)hr); return 1; }
    hr = WHvSetupPartition(part);
    if (FAILED(hr)) { printf("WHvSetupPartition 0x%08lx\n", (unsigned long)hr); return 1; }
    show("partition ready");

    HANDLE f = CreateFileA(path, GENERIC_READ|GENERIC_WRITE, 0, NULL,
                           CREATE_ALWAYS, FILE_ATTRIBUTE_TEMPORARY, NULL);
    if (f == INVALID_HANDLE_VALUE) { printf("CreateFile %lu\n", GetLastError()); return 1; }
    DWORD junk = 0;
    DeviceIoControl(f, FSCTL_SET_SPARSE, NULL, 0, NULL, 0, &junk, NULL);
    HANDLE map = CreateFileMappingA(f, NULL, PAGE_READWRITE,
                                    (DWORD)(SZ>>32), (DWORD)(SZ & 0xFFFFFFFF), NULL);
    if (!map) { printf("CreateFileMapping %lu\n", GetLastError()); return 1; }
    void *view = MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, SZ);
    if (!view) { printf("MapViewOfFile %lu\n", GetLastError()); return 1; }
    show("file view mapped 3G");

    hr = WHvMapGpaRange(part, view, 0, SZ,
                        WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite |
                        WHvMapGpaRangeFlagExecute);
    printf("WHvMapGpaRange(file-backed) -> 0x%08lx  %s\n",
           (unsigned long)hr, SUCCEEDED(hr) ? "OK" : "FAILED");
    if (FAILED(hr)) { printf("VERDICT: WHPX refuses file-backed guest RAM\n"); return 3; }
    show("after WHvMapGpaRange");

    memset(view, 0xAB, 256u*1024*1024);
    show("after host touch 256M");

    /* Does the hypervisor pin it?  Punch the range back out and see. */
    FILE_ZERO_DATA_INFORMATION z;
    z.FileOffset.QuadPart = 0; z.BeyondFinalZero.QuadPart = 256ll*1024*1024;
    BOOL ok = DeviceIoControl(f, FSCTL_SET_ZERO_DATA, &z, sizeof(z), NULL, 0, &junk, NULL);
    printf("FSCTL_SET_ZERO_DATA while mapped -> %d (err %lu)\n", ok, ok?0:GetLastError());
    show("after punch hole");
    printf("readback[0]=0x%02X (0x00 means the hole is visible to the guest)\n",
           ((unsigned char*)view)[0]);

    FILE_STANDARD_INFO si; LARGE_INTEGER comp;
    GetFileInformationByHandleEx(f, FileStandardInfo, &si, sizeof(si));
    printf("file EOF=%.0f MB  allocated=%.0f MB\n",
           (double)si.EndOfFile.QuadPart/1048576.0,
           (double)si.AllocationSize.QuadPart/1048576.0);

    WHvUnmapGpaRange(part, 0, SZ);
    UnmapViewOfFile(view); CloseHandle(map); CloseHandle(f);
    WHvDeletePartition(part);
    DeleteFileA(path);
    show("cleaned up");
    printf("VERDICT: WHPX ACCEPTS file-backed guest RAM\n");
    return 0;
}
