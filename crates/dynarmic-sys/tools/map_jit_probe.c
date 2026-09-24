/* macOS arm64 only. A one-off measurement of MAP_JIT, recorded in docs/ports/macos-cpu.md:
 *   1. another thread with its own write window open can write a MAP_JIT page while this thread
 *      executes it (W^X is per thread, not per page);
 *   2. the D12-style emit+execute cycle -- open the write window, write, close it, flush the
 *      I-cache, execute, check -- in ns, median of 31 samples of 200,000 cycles.
 * Build and run:  clang -O2 crates/dynarmic-sys/tools/map_jit_probe.c -o /tmp/map_jit_probe && /tmp/map_jit_probe
 */
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <pthread.h>
#include <sys/mman.h>
#include <libkern/OSCacheControl.h>
#include <mach/mach_time.h>
#include <stdatomic.h>
static uint32_t *mem;
static atomic_int stop, wrote;
static void *writer(void *arg) {
  // Another thread opens its own write window and writes the page the main thread is executing.
  pthread_jit_write_protect_np(0);
  volatile uint32_t w = mem[0]; mem[0] = w; // same word back
  pthread_jit_write_protect_np(1);
  atomic_store(&wrote, 1);
  return 0;
}
int main() {
  mem = mmap(0, 16384, PROT_READ|PROT_WRITE|PROT_EXEC, MAP_ANON|MAP_PRIVATE|MAP_JIT, -1, 0);
  // 1. cross-thread: main executes (write-protected), another thread writes.
  pthread_jit_write_protect_np(0);
  mem[0] = 0xD2800540; /* movz x0,#42 */ mem[1] = 0xD65F03C0; /* ret */
  pthread_jit_write_protect_np(1);
  sys_icache_invalidate(mem, 8);
  int (*f)(void) = (int(*)(void))mem;
  pthread_t t; pthread_create(&t, 0, writer, 0);
  long calls = 0; while (!atomic_load(&wrote)) { calls += f(); }
  pthread_join(t, 0);
  printf("cross-thread write while main executes: succeeded (main executed %ld times meanwhile)\n", calls/42);
  // 2. D12-style emit+execute cycle: open write window, write 2 words (value varies), close, flush, execute, check.
  enum { N = 200000, S = 31 };
  mach_timebase_info_data_t tb; mach_timebase_info(&tb);
  double samples[S]; long mismatches = 0;
  for (int s = 0; s < S; s++) {
    uint64_t t0 = mach_absolute_time();
    for (int i = 0; i < N; i++) {
      uint32_t imm = (i & 0xFFFF);
      pthread_jit_write_protect_np(0);
      mem[0] = 0xD2800000 | (imm << 5);
      pthread_jit_write_protect_np(1);
      sys_icache_invalidate(mem, 4);
      if (f() != (int)imm) mismatches++;
    }
    uint64_t t1 = mach_absolute_time();
    samples[s] = (double)(t1 - t0) * tb.numer / tb.denom / N;
  }
  // median
  for (int i = 0; i < S; i++) for (int j = i+1; j < S; j++) if (samples[j] < samples[i]) { double x = samples[i]; samples[i] = samples[j]; samples[j] = x; }
  printf("emit+execute cycle (MAP_JIT, pthread_jit_write_protect_np + sys_icache_invalidate): median %.1f ns, min %.1f, max %.1f; n = %d samples x %d cycles; mismatches %ld of %ld\n", samples[S/2], samples[0], samples[S-1], S, N, mismatches, (long)S*N);
}
