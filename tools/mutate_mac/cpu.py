"""macOS rows: the cpu workstream (dynarmic's arm64 backend, dynarmic-sys, omni-cpu). Pure data; see
`__init__.py`.

**Rows that mutate `crates/dynarmic-sys/vendor/` need a rebuild the harness does not know about.**
The build script watches only `vendor/PIN.txt`, so every such command touches it first. The
consequence runs the other way too: when the harness restores the last vendored file, the dynarmic
library in `target/` still holds that row's mutation until something touches `PIN.txt` again. After
a run, `touch crates/dynarmic-sys/vendor/PIN.txt` and rebuild before trusting any test binary.
"""

ARM64 = "crates/dynarmic-sys/vendor/dynarmic/src/dynarmic/backend/arm64/"
A64 = ARM64 + "emit_arm64_a64.cpp"
SAT = ARM64 + "emit_arm64_saturation.cpp"
FP = ARM64 + "emit_arm64_floating_point.cpp"
VFP = ARM64 + "emit_arm64_vector_floating_point.cpp"
CRYPTO = ARM64 + "emit_arm64_cryptography.cpp"
VEC = ARM64 + "emit_arm64_vector.cpp"
MEM = ARM64 + "emit_arm64_memory.cpp"
ABI = ARM64 + "abi.h"
ASPACE = ARM64 + "address_space.h"
SHIM = "crates/dynarmic-sys/shim/od_dynarmic.cpp"
SYS_LIB = "crates/dynarmic-sys/src/lib.rs"
CPU_DYN = "crates/omni-cpu/src/dynarmic/mod.rs"
CPU_HARNESS = "crates/omni-cpu/tests/harness/mod.rs"


def dyn(*tests):
    """`cargo test -p dynarmic-sys` on the named test targets, after forcing the vendored rebuild."""
    targets = " ".join(f"--test {t}" for t in tests)
    return ["sh", "-c",
            "touch crates/dynarmic-sys/vendor/PIN.txt && CARGO_BUILD_JOBS=4 cargo test "
            f"-p dynarmic-sys --release {targets} --no-fail-fast -- --test-threads=1"]


def cpu(*tests):
    targets = " ".join(f"--test {t}" for t in tests)
    return ["sh", "-c",
            f"CARGO_BUILD_JOBS=4 cargo test -p omni-cpu --release {targets} --no-fail-fast "
            "-- --test-threads=1"]


ROWS = [
    # --- 0002: the Interpret terminal -------------------------------------------------------------
    ("mac-cpu-A1", "A", "0002 reverted: the Interpret terminal asserts instead of calling the fallback",
     A64,
     """    EmitRelocation(code, ctx, LinkTarget::InterpreterFallback);""",
     """    ASSERT_FALSE("Interpret should never be emitted.");""",
     dyn("interpret")),
    ("mac-cpu-A2", "A", "0002: the fallback runs under the guest's FPCR (no switch to the host's)",
     A64,
     """    code.LDR(Wscratch0, SP, offsetof(StackLayout, save_host_fpcr));
    code.MSR(oaknut::SystemReg::FPCR, Xscratch0);
    EmitRelocation(code, ctx, LinkTarget::InterpreterFallback);""",
     """    EmitRelocation(code, ctx, LinkTarget::InterpreterFallback);""",
     dyn("interpret")),
    ("mac-cpu-A3", "A", "0002: the guest's FPCR is not reloaded after the fallback",
     A64,
     """    EmitRelocation(code, ctx, LinkTarget::InterpreterFallback);
    code.LDR(Wscratch0, Xstate, offsetof(A64JitState, fpcr));
    code.MSR(oaknut::SystemReg::FPCR, Xscratch0);""",
     """    EmitRelocation(code, ctx, LinkTarget::InterpreterFallback);""",
     dyn("interpret")),
    # --- 0003: scalar saturation ------------------------------------------------------------------
    ("mac-cpu-A4", "A", "0003: the FPSR manager is not loaded, so the host QC never reaches the guest",
     SAT,
     """    RegAlloc::Realize(Vresult, Va, Vb);
    ctx.fpsr.Load();

    emit(*Vresult, *Va, *Vb);""",
     """    RegAlloc::Realize(Vresult, Va, Vb);

    emit(*Vresult, *Va, *Vb);""",
     dyn("a64_saturation")),
    ("mac-cpu-A5", "A", "0003: SQADD.B saturates unsigned",
     SAT,
     """    EmitSaturatedScalar<8>(code, ctx, inst, [&](auto Vresult, auto Va, auto Vb) { code.SQADD(Vresult, Va, Vb); });""",
     """    EmitSaturatedScalar<8>(code, ctx, inst, [&](auto Vresult, auto Va, auto Vb) { code.UQADD(Vresult, Va, Vb); });""",
     dyn("a64_saturation")),
    # --- 0004: half precision ---------------------------------------------------------------------
    ("mac-cpu-A6", "A", "0004 reverted: the fallback's save mask names the result's GPR twin again",
     VFP,
     """    ABI_PushRegisters(code, ABI_CALLER_SAVE & ~ToRegList(Qresult), stack_size);

    code.MOV(Xscratch0, mcl::bit_cast<u64>(fn));
    code.ADD(X0, SP, 0 * 16);
    code.ADD(X1, SP, 1 * 16);
    code.MOV(X2, fpcr);
    code.ADD(X3, Xstate, ctx.conf.state_fpsr_offset);
    code.STR(Qarg1, X1);
    code.BLR(Xscratch0);
    code.LDR(Qresult, SP);

    ABI_PopRegisters(code, ABI_CALLER_SAVE & ~ToRegList(Qresult), stack_size);""",
     """    ABI_PushRegisters(code, ABI_CALLER_SAVE & ~(1ull << Qresult.index()), stack_size);

    code.MOV(Xscratch0, mcl::bit_cast<u64>(fn));
    code.ADD(X0, SP, 0 * 16);
    code.ADD(X1, SP, 1 * 16);
    code.MOV(X2, fpcr);
    code.ADD(X3, Xstate, ctx.conf.state_fpsr_offset);
    code.STR(Qarg1, X1);
    code.BLR(Xscratch0);
    code.LDR(Qresult, SP);

    ABI_PopRegisters(code, ABI_CALLER_SAVE & ~(1ull << Qresult.index()), stack_size);""",
     dyn("a64_fp16")),
    ("mac-cpu-A7", "A", "0004: scalar FMADD half computes a fused multiply-subtract",
     FP,
     """        return FP::FPMulAdd<u16>(static_cast<u16>(a), static_cast<u16>(b), static_cast<u16>(c), FP::FPCR{fpcr}, fpsr);""",
     """        return FP::FPMulSub<u16>(static_cast<u16>(a), static_cast<u16>(b), static_cast<u16>(c), FP::FPCR{fpcr}, fpsr);""",
     dyn("a64_fp16")),
    ("mac-cpu-B1", "B", "0004: vector FNEG half sets the sign bit instead of flipping it",
     VFP,
     """    code.EOR(Qresult->B16(), Qresult->B16(), Qoperand->B16());""",
     """    code.ORR(Qresult->B16(), Qresult->B16(), Qoperand->B16());""",
     dyn("a64_fp16")),
    # --- 0005: SM4 --------------------------------------------------------------------------------
    ("mac-cpu-A8", "A", "0005: the SM4 S-box is the identity",
     CRYPTO,
     """        return Common::Crypto::SM4::AccessSubstitutionBox(static_cast<u8>(index));""",
     """        return static_cast<u8>(index);""",
     dyn("a64_sm4")),
    # --- 0006: 64-bit unsigned max/min ------------------------------------------------------------
    ("mac-cpu-A9", "A", "0006: VectorMaxU64 selects on a signed compare",
     VEC,
     """    code.CMHI(Qresult->D2(), Qa->D2(), Qb->D2());
    code.BSL(Qresult->B16(), Qa->B16(), Qb->B16());""",
     """    code.CMGT(Qresult->D2(), Qa->D2(), Qb->D2());
    code.BSL(Qresult->B16(), Qa->B16(), Qb->B16());""",
     dyn("a64_compare")),
    ("mac-cpu-A10", "A", "0006 reverted in VectorMinU64, detected by the decoder sweep (CMHI D/.2D)",
     VEC,
     """    code.CMHI(Qresult->D2(), Qa->D2(), Qb->D2());
    code.BSL(Qresult->B16(), Qb->B16(), Qa->B16());""",
     """    ASSERT_FALSE("Unimplemented");""",
     dyn("decoder_sweep")),
    # --- 0007: inline exclusives ------------------------------------------------------------------
    ("mac-cpu-A11", "A", "0007 reverted: exclusive reads ignore fastmem_exclusive_access again",
     MEM,
     """    if (ctx.conf.fastmem_exclusive_access && ctx.conf.global_monitor) {
        if (const auto marker = ShouldFastmem(ctx, inst)) {
            FastmemEmitExclusiveReadMemory<bitsize>(code, ctx, inst, *marker);""",
     """    if (false) {
        if (const auto marker = ShouldFastmem(ctx, inst)) {
            FastmemEmitExclusiveReadMemory<bitsize>(code, ctx, inst, *marker);""",
     dyn("exclusive", "a64_exec")),
    ("mac-cpu-A12", "A", "0007: the compare-and-swap does not compare (another observer's store is lost)",
     MEM,
     """            code.CMP(Xscratch0, Xscratch1);
            break;
        }
        code.B(NE, cas_failed);""",
     """            code.CMP(Xscratch0, Xscratch1);
            break;
        }""",
     dyn("exclusive")),
    ("mac-cpu-A13", "A", "0007: a successful store-exclusive clears no other processor's reservation",
     MEM,
     """        code.MOV(Xscratch2, 0xDEAD'DEAD'DEAD'DEADull);
        code.STR(Xscratch2, Xscratch0);""",
     """        code.MOV(Xscratch2, 0xDEAD'DEAD'DEAD'DEADull);""",
     dyn("exclusive")),
    ("mac-cpu-B2", "B", "0007: the inline store-exclusive does not take the monitor lock",
     MEM,
     """    EmitMonitorLock(code, ctx);
    code.MOV(*Wstatus, 1);""",
     """    code.MOV(*Wstatus, 1);""",
     dyn("exclusive")),
    ("mac-cpu-A14", "A", "0007: the 128-bit host-fault entry does not give the borrowed registers back",
     MEM,
     """        const u64 fault_entry = mcl::bit_cast<u64>(code.xptr<void*>());
        if constexpr (bitsize == 128) {
            code.LDP(borrowed[2], borrowed[3], SP, 16);
            code.LDP(borrowed[0], borrowed[1], SP, oaknut::PostIndexed{}, 32);
        }""",
     """        const u64 fault_entry = mcl::bit_cast<u64>(code.xptr<void*>());""",
     dyn("host_fault")),
    # --- 0008: the halt word ----------------------------------------------------------------------
    ("mac-cpu-A15", "A", "0008 reverted: the memory-abort check loads the u32 halt word as 64 bits",
     A64,
     """    code.LDAR(Wscratch0, Xhalt);
    code.TST(Wscratch0, static_cast<u32>(HaltReason::MemoryAbort));""",
     """    code.LDAR(Xscratch0, Xhalt);
    code.TST(Xscratch0, static_cast<u32>(HaltReason::MemoryAbort));""",
     dyn("host_fault")),
    # --- x18, W^X, stoppability -------------------------------------------------------------------
    ("mac-cpu-A16", "A", "the allocator hands out host x18 first",
     ABI,
     """constexpr std::initializer_list<int> GPR_ORDER{19, 20, 21,""",
     """constexpr std::initializer_list<int> GPR_ORDER{18, 19, 20, 21,""",
     dyn("x18")),
    ("mac-cpu-A17", "A", "the code cache is never write-protected again after a write",
     ASPACE,
     """    void ProtectCodeMemory() {
#if defined(DYNARMIC_ENABLE_NO_EXECUTE_SUPPORT) || defined(__APPLE__) || defined(__OpenBSD__)
        mem.protect();""",
     """    void ProtectCodeMemory() {
#if defined(DYNARMIC_ENABLE_NO_EXECUTE_SUPPORT) || defined(__APPLE__) || defined(__OpenBSD__)
        (void)0;""",
     dyn("wx")),
    ("mac-cpu-A18", "A", "the shim reports the arm64 cache as W+X, as the x64 one",
     SHIM,
     """    out->code_cache_w_xor_x = OD_CODE_CACHE_W_XOR_X_PER_THREAD;""",
     """    out->code_cache_w_xor_x = OD_CODE_CACHE_W_AND_X;""",
     dyn("wx")),
    ("mac-cpu-B3", "B", "the RSB push is a no-op, so the measured RET runaway would stop",
     ARM64 + "emit_arm64.cpp",
     """void EmitIR<IR::Opcode::PushRSB>(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst) {
    if (!ctx.conf.HasOptimization(OptimizationFlag::ReturnStackBuffer)) {""",
     """void EmitIR<IR::Opcode::PushRSB>(oaknut::CodeGenerator& code, EmitContext& ctx, IR::Inst* inst) {
    if (true) {""",
     dyn("hostile")),
    # --- dynarmic-sys / omni-cpu (Rust) -----------------------------------------------------------
    ("mac-cpu-A19", "A", "the aarch64 per-jit fixed cost claims the x64 fast-dispatch table",
     SYS_LIB,
     """#[cfg(target_arch = "aarch64")]
pub const OD_FIXED_PER_JIT_BYTES: usize = 0;""",
     """#[cfg(target_arch = "aarch64")]
pub const OD_FIXED_PER_JIT_BYTES: usize = 0x10 * 0x10_0000;""",
     cpu("roblox")),
    ("mac-cpu-A20", "A", "the FPCR guard never installs the host's word under an inline handler",
     CPU_DYN,
     """        pub(crate) fn enter(host: u32) -> Self {
            let guest = read();
            let switched = guest != host;
            if switched {
                write(host);
            }
            Self { guest, switched }
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            if self.switched {
                write(self.guest);
            }
        }
    }
}

/// On an `aarch64` host""",
     """        pub(crate) fn enter(host: u32) -> Self {
            let guest = read();
            let switched = false && guest != host;
            if switched {
                write(host);
            }
            Self { guest, switched }
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            if self.switched {
                write(self.guest);
            }
        }
    }
}

/// On an `aarch64` host""",
     cpu("thunk")),
    ("mac-cpu-A21", "A", "the test harness keeps a guest space the host placed below 64 GiB",
     CPU_HARNESS,
     """    if space.end() - 1 >= 1usize << 36 {
        return space;
    }""",
     """    if true {
        return space;
    }""",
     cpu("identity")),
    ("mac-cpu-C1", "A", "the prelude invalidates the whole code cache, charging every page of "
     "every jit's cache at creation (patch 0009 reverted)",
     ARM64 + "a64_address_space.cpp",
     """    mem.invalidate(mem.ptr(), prelude_info.end_of_prelude);""",
     """    mem.invalidate_all();""",
     dyn("code_cache_charge")),
    # --- mac-mem: patch 0010, the compact block records --------------------------------------------
    ("mac-mem-A1", "A", "0010: relinking stops at the first block that links to the target, so the "
     "others keep branching to a stale translation",
     ARM64 + "address_space.cpp",
     """    u32 index = head->second;
    while (index != no_link) {""",
     """    u32 index = head->second;
    if (index != no_link) {""",
     dyn("bookkeeping")),
    ("mac-mem-A2", "A", "0010: the fastmem handler looks only at a block's first patch site",
     ARM64 + "address_space.cpp",
     """        const auto patch_entry = std::lower_bound(first, last, static_cast<u32>(offset),""",
     """        const auto patch_entry = std::lower_bound(first, first + 1, static_cast<u32>(offset),""",
     dyn("bookkeeping")),
    # --- mac-mem: patch 0011, the guest-range index -----------------------------------------------
    ("mac-mem-A3", "A", "0011: the A64 clear leaves the guest ranges behind (the pin's leak)",
     ARM64 + "a64_address_space.cpp",
     """    decltype(guest_ranges){}.swap(guest_ranges);
    guest_range_pages = {};
    std::vector<u32>{}.swap(wide_guest_ranges);
}""",
     """}""",
     dyn("bookkeeping")),
    ("mac-mem-A4", "A", "0011: a block is indexed under its first page only",
     ARM64 + "a64_address_space.cpp",
     """        guest_range_pages[page].push_back(index);
        if (page == last_page) {""",
     """        guest_range_pages[page].push_back(index);
        if (true) {""",
     dyn("bookkeeping")),
    ("mac-mem-A5", "A", "0011: blocks wider than the page index are never looked at",
     ARM64 + "a64_address_space.cpp",
     """        for (const u32 index : wide_guest_ranges) {
            consider(index);
        }""",
     """""",
     dyn("bookkeeping")),
    ("mac-mem-A6", "A", "0011: the walk of the index for a large invalidation looks at its first page only",
     ARM64 + "a64_address_space.cpp",
     """                if (page >= first_page && page <= last_page) {""",
     """                if (page == first_page) {""",
     dyn("bookkeeping")),
    ("mac-mem-B1", "B", "0011: every block on an invalidated page goes, whether or not its bytes were "
     "written (the range test dropped)",
     ARM64 + "a64_address_space.cpp",
     """            if (range.first <= last && first <= range.last) {""",
     """            if (true) {""",
     dyn("a64_exec")),
    # --- mac-mem: patch 0012, an invalidation that leaves nothing standing is a clear ---------------
    ("mac-mem-A7", "A", "0012 reverted: a jit whose blocks were all invalidated keeps their records and code",
     ARM64 + "a64_address_space.cpp",
     """    if (block_entries.empty()) {
        ClearCache();
    }""",
     """""",
     dyn("bookkeeping")),
    ("mac-mem-B2", "B", "0012 over-reaches: every range invalidation clears the whole cache",
     ARM64 + "a64_address_space.cpp",
     """    if (block_entries.empty()) {
        ClearCache();
    }""",
     """    ClearCache();""",
     dyn("a64_exec")),
    # --- mac-mem: patch 0013, the bookkeeping's large arrays are pages of their own ----------------
    ("mac-mem-A8", "A", "0013 reverted in effect: no array is large enough to be page-backed, so every "
     "freed one goes back to the C++ heap",
     ARM64 + "page_backed_allocator.h",
     """    static constexpr std::size_t threshold = 256 * 1024;""",
     """    static constexpr std::size_t threshold = ~std::size_t{0};""",
     dyn("bookkeeping")),
]
