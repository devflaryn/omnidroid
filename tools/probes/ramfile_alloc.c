/* Allocated vs logical size of a live QEMU RAM file.
 *
 * `fsutil file queryAllocRanges` cannot answer this while the guest runs --
 * it opens without sharing and gets ERROR_SHARING_VIOLATION (32). QEMU holds
 * the file with FILE_SHARE_READ|WRITE|DELETE, so a reader that asks for the
 * same sharing gets in fine.
 *
 * EndOfFile stays at -m for the life of the guest and tells you nothing. The
 * number that answers "is the punch-hole discard working" is AllocationSize.
 */
#include <windows.h>
#include <stdio.h>

int main(int argc, char **argv) {
    if (argc < 2) { fprintf(stderr, "usage: %s <ram-file> [...]\n", argv[0]); return 2; }
    for (int i = 1; i < argc; i++) {
        HANDLE h = CreateFileA(argv[i], FILE_READ_ATTRIBUTES,
                               FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                               NULL, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, NULL);
        if (h == INVALID_HANDLE_VALUE) {
            printf("%-46s  OPEN FAILED (%lu)\n", argv[i], GetLastError());
            continue;
        }
        FILE_STANDARD_INFO si;
        if (GetFileInformationByHandleEx(h, FileStandardInfo, &si, sizeof(si))) {
            double eof = (double)si.EndOfFile.QuadPart / 1048576.0;
            double alloc = (double)si.AllocationSize.QuadPart / 1048576.0;
            printf("%-46s  eof=%8.1f MB  allocated=%8.1f MB  (%.1f%%)\n",
                   argv[i], eof, alloc, eof > 0 ? alloc * 100.0 / eof : 0.0);
        } else {
            printf("%-46s  QUERY FAILED (%lu)\n", argv[i], GetLastError());
        }
        CloseHandle(h);
    }
    return 0;
}
