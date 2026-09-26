# macOS: dynarmic's arm64 backend

The M1 builds dynarmic's arm64 backend, which the x64 hosts never compile. Each patch is carried
in `crates/dynarmic-sys/patches/` with its evidence in `patches/README.md`; `dynarmic-sys/tools/verify_patches.py`
checks the tree is the pin plus the patches, byte for byte. This file keeps the host facts the
tests and the code cite.

## The arm64 patches

| patch | what was wrong on arm64 |
|---|---|
| 0002 | the `Interpret` terminal was `ASSERT_FALSE`: the first undecodable word (every LSE atomic) ended the process |
| 0003-0006 | unimplemented IR opcodes (scalar saturation, FP16, SM4, 64-bit unsigned max/min) aborted; 0004 also fixed a fallback that dropped its result (`FRINT* V.8H` silently wrong) |
| 0007 | `fastmem_exclusive_access` ignored: every `LDXR`/`STXR` took two callbacks |
| 0008 | the memory-abort check read the u32 halt word with a 64-bit `LDAR`: every guest access to unmapped memory aborted the process |
| 0009-0013, 0015, 0016 | memory: per-block bookkeeping (`macos-memory.md`) |
| 0014 | the inline store-exclusive's store was not a fastmem patch location (`macos-hvf.md` 4.7) |
| 0020 | a return-stack-buffer hit checks the budget and the halt word (D35); `INTERRUPTIBLE` is `ALL_SAFE` less `FAST_DISPATCH`, which arm64 does not implement |
| 0021 | the inline exclusives honour value-compare (D31 amendment 1): before, every guest atomic took the global monitor's lock and scanned its slots (1,306 ns against 9.4) |

Nothing else reachable is left unimplemented: `tests/decoder_sweep.rs` executes every one of the
874 decoder entries on both memory paths, 0 deaths. `tests/hostile.rs::the_stoppability_matrix`
holds the arm64 stoppability table.

## Host facts

* **FPCR**: the arm64 prelude installs the guest's `FPCR` and restores the host's only on return
  from `run`, so the dispatcher's guard switches it (`omni-cpu` `dynarmic::fpcr`, re-exported as
  `mxcsr`). `MSR FPCR` costs 26 ns when the words differ.
* **x18** is never allocated (`tests/x18.rs`); the guest's `TPIDR_EL0`/`TPIDRRO_EL0` are memory
  slots and the host's TLS register is untouched (`tests/thread_pointer_host.rs`).
* No fast-dispatch table on arm64: `OD_FIXED_PER_JIT_BYTES` is 0 there.

## W^X (the D12 exception on this host)

* The code cache is `mmap(RWX, MAP_JIT)`; every write is bracketed by
  `pthread_jit_write_protect_np`, which switches **the calling thread's** view.
* MEASURED (`tests/wx.rs`, child processes): a write to the cache from the jit's thread between runs
  faults, and so does one from a callback in the middle of guest execution.
* MEASURED once (`dynarmic-sys/tools/map_jit_probe.c`): another thread with its own write window
  open **can** write a `MAP_JIT` page while this thread executes it. W^X holds per thread, not per
  page: a guest thread never writes the cache, but while dynarmic emits for another jit every
  `MAP_JIT` page is writable to that thread. `od_jit_effective_config` says `W_XOR_X_PER_THREAD`.
* Emit + execute on `MAP_JIT`: median 192.4 ns, 0 mismatches in 6,200,000 (Windows dual-mapped
  162 ns).

## Open

* The pin's arm64 memory trampolines pass `u8`/`u16` values to callbacks unextended, which Apple's
  ABI requires of the caller; 0007's take `u64`. Unmeasured.
* Patch 0023 (a mid-run clear forgets the return-stack buffer) is unmerged (`macos.md`).
