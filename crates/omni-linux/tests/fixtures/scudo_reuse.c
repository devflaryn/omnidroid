/* Scudo's large ("secondary") blocks, freed into its cache, purged back to the OS (madvise
 * MADV_DONTNEED), and handed out again -- by calloc, which zeroes the whole block. Roblox's engine
 * died in exactly that memset, 64 KiB into a reused secondary block (SEGV_ACCERR, 2026-09-29).
 * Prints "scudo ok <rounds>" and exits 0, or names what failed and exits 1. */
#include <malloc.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifndef M_PURGE
#define M_PURGE (-101)
#endif
#ifndef M_PURGE_ALL
#define M_PURGE_ALL (-104)
#endif

int main(void) {
    static const size_t sizes[] = {176 * 1024, 300 * 1024, 100 * 1024, 1024 * 1024, 150 * 1024, 700 * 1024, 90 * 1024};
    enum { N = sizeof sizes / sizeof *sizes, ROUNDS = 40 };
    for (int round = 0; round < ROUNDS; round++) {
        void* p[N];
        for (int i = 0; i < N; i++) {
            p[i] = malloc(sizes[i]);
            if (p[i] == NULL) {
                printf("malloc %zu failed in round %d\n", sizes[i], round);
                return 1;
            }
            memset(p[i], 0x5a, sizes[i]);
        }
        for (int i = 0; i < N; i++) free(p[i]);
        mallopt(round % 2 ? M_PURGE_ALL : M_PURGE, 0);
        for (int i = 0; i < N; i++) {
            size_t n = sizes[(i + round) % N] + (size_t)(round * 4096) % 65536;
            unsigned char* q = calloc(1, n);
            if (q == NULL) {
                printf("calloc %zu failed in round %d\n", n, round);
                return 1;
            }
            for (size_t k = 0; k < n; k += 4096) {
                if (q[k] != 0) {
                    printf("calloc %zu not zero at %zu in round %d\n", n, k, round);
                    return 1;
                }
            }
            memset(q, 0xa5, n);
            free(q);
        }
    }
    printf("scudo ok %d\n", ROUNDS);
    fflush(stdout);
    return 0;
}
