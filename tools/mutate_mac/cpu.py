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



# The native backend's rows (`mac-hvf-*`, docs/ports/macos-hvf.md). Every command runs its test binary
# through `tools/hvf_run.sh`, which signs it with `com.apple.security.hypervisor`; without that the
# suites fail at `hv_vm_create` (HV_DENIED), which the pre-flight would report as a failing command.
HVF_PLATFORM = "crates/omni-platform/src/hypervisor/macos.rs"
HVF_VM = "crates/omni-platform/src/vm/macos.rs"
HVF_CPU = "crates/omni-cpu/src/native/mod.rs"
HVF_SYSTEM = "crates/omni-cpu/src/native/system.rs"
HVF_THREADS = "crates/omni-android/src/bionic/threads.rs"
HVF_BIONIC = "crates/omni-android/src/bionic/mod.rs"


def hvf(package, feature, target, only=""):
    """A signed `cargo test` of one target with the native backend's feature on."""
    return ["sh", "-c",
            "CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER=$PWD/tools/hvf_run.sh CARGO_BUILD_JOBS=4 "
            f"cargo test -p {package} --release --features {feature} --test {target} "
            f"--no-fail-fast -- --test-threads=1 {only}"]


HVF_PLAT_TESTS = hvf("omni-platform", "hypervisor", "hypervisor_macos")
HVF_CPU_TESTS = hvf("omni-cpu", "native-hvf", "native")
HVF_INIT_GATE = hvf("omni-android", "native-hvf", "native_initializers")
# The translating backend's own runaway-thread test: the B row below must not touch dynarmic.
BIONIC_RUNAWAY = ["sh", "-c",
                  "CARGO_BUILD_JOBS=4 cargo test -p omni-android --release --test bionic "
                  "--no-fail-fast -- --test-threads=1 a_runaway_guest_thread_stops_at_a_window_boundary"]

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
    # --- the native backend (Hypervisor.framework), docs/ports/macos-hvf.md ----------------------
    ("mac-hvf-P1", "A", "stage-2 update maps without unmapping first: a replaced object is not re-bound",
     HVF_PLATFORM,
     """        let result = unmap(from, to - from).and_then(|()| {""",
     """        let result = Ok(()).and_then(|()| {""",
     HVF_PLAT_TESTS),
    ("mac-hvf-P2", "A", "a PROT_NONE host page is mapped readable at stage 2",
     HVF_PLATFORM,
     """    let mut flags = 0;
    if prot & libc::PROT_READ != 0 {""",
     """    let mut flags = HV_MEMORY_READ;
    if prot & libc::PROT_READ != 0 {""",
     HVF_PLAT_TESTS),
    ("mac-hvf-P3", "A", "vm/macos.rs's mprotect is not mirrored: a commit or protect never reaches stage 2",
     HVF_VM,
     """        return Err(errno());
    }
    mirrored(address, size, protection);
    Ok(())""",
     """        return Err(errno());
    }
    Ok(())""",
     HVF_PLAT_TESTS),
    ("mac-hvf-P4", "A", "the decommit primitive (fresh MAP_FIXED) is not mirrored: the guest keeps the old pages",
     HVF_VM,
     """    debug_assert_eq!(mapped as usize, address, "MAP_FIXED returns the requested base");
    mirrored(address, size, libc::PROT_NONE);""",
     """    debug_assert_eq!(mapped as usize, address, "MAP_FIXED returns the requested base");""",
     HVF_PLAT_TESTS),
    ("mac-hvf-P5", "A", "detach leaves the range mapped at stage 2",
     HVF_PLATFORM,
     """        mirror.overlays.remove(&key);
    }
    let _ = unmap(base, len);""",
     """        mirror.overlays.remove(&key);
    }""",
     HVF_PLAT_TESTS),
    ("mac-hvf-P6", "A", "a host change to an overlaid page overwrites the overlay",
     HVF_PLATFORM,
     """    for (&overlay, _) in mirror.overlays.range(start as u64..end as u64) {""",
     """    for (&overlay, _) in mirror.overlays.range(0..0) {""",
     HVF_PLAT_TESTS),
    ("mac-hvf-P7", "A", "the vCPU limit is not checked: the 65th is the framework's error, not VcpuLimit",
     HVF_PLATFORM,
     """    if live >= max {""",
     """    if false && live >= max {""",
     HVF_PLAT_TESTS),
    ("mac-hvf-P8", "A", "Q registers written byte-reversed through the asm trampoline",
     HVF_PLATFORM,
     """    let bytes = value.to_le_bytes();""",
     """    let bytes = value.to_be_bytes();""",
     HVF_PLAT_TESTS),
    ("mac-hvf-P9", "A", "attach does not check the IPA size",
     HVF_PLATFORM,
     """    if limits.ipa_bits == 0 || (end as u64) > (1u64 << limits.ipa_bits) {""",
     """    if limits.ipa_bits == 0 {""",
     HVF_PLAT_TESTS),
    ("mac-hvf-C1", "A", "TPIDR_EL0 is not saved back: a guest MSR to it is lost",
     HVF_CPU,
     """        self.regs.tpidr_el0 = cpu.sys_reg(SysReg::TpidrEl0)?;""",
     """        let _ = cpu.sys_reg(SysReg::TpidrEl0)?;""",
     HVF_CPU_TESTS),
    ("mac-hvf-C2", "A", "a register set between runs on the same thread is not loaded",
     HVF_CPU,
     """            if full || self.dirty.x & (1 << i) != 0 {""",
     """            if full {""",
     HVF_CPU_TESTS),
    ("mac-hvf-C3", "A", "the vector file is not saved at an exit",
     HVF_CPU,
     """        for i in 0..32u8 {
            self.regs.v[i as usize] = cpu.simd(i)?;
        }""",
     """        for i in 0..0u8 {
            self.regs.v[i as usize] = cpu.simd(i)?;
        }""",
     HVF_CPU_TESTS),
    ("mac-hvf-C4", "A", "a stage-2 abort is never paged in: every lazy first touch is a fault",
     HVF_CPU,
     """        omni_mem::admit(&self.shared.space, address, 1, access).is_ok()""",
     """        omni_mem::admit(&self.shared.space, address, 1, access).is_ok() && false""",
     HVF_CPU_TESTS),
    ("mac-hvf-C5", "B", "the demand-paging answer skips the policy: any address in the space is admitted",
     HVF_CPU,
     """        omni_mem::admit(&self.shared.space, address, 1, access).is_ok()""",
     """        { let _ = access; self.shared.space.ensure_committed(address & !(self.shared.space.page_size() - 1), 1).is_ok() }""",
     HVF_CPU_TESTS),
    ("mac-hvf-C6", "A", "a registered thunk's BRK is not recognised as the thunk",
     HVF_CPU,
     """        if traps_at_address && self.thunks.contains(&at) {""",
     """        if false && traps_at_address && self.thunks.contains(&at) {""",
     HVF_CPU_TESTS),
    ("mac-hvf-C7", "A", "a thunk on a writable data page is not reported when the fetch aborts",
     HVF_CPU,
     """                                } else if self.thunks.contains(&at) {
                                    Some(ExitReason::Thunk { pc: at })""",
     """                                } else if false {
                                    Some(ExitReason::Thunk { pc: at })""",
     HVF_CPU_TESTS),
    ("mac-hvf-C8", "A", "a counted run is not refused: it runs unbounded",
     HVF_CPU,
     """        if limit.instructions().is_some() {""",
     """        if false && limit.instructions().is_some() {""",
     HVF_CPU_TESTS),
    ("mac-hvf-C9", "A", "WnR read backwards: a refused store is a read",
     HVF_CPU,
     """                            } else if iss & ISS_WNR != 0 {
                                AccessKind::Write""",
     """                            } else if iss & ISS_WNR == 0 {
                                AccessKind::Write""",
     HVF_CPU_TESTS),
    ("mac-hvf-C10", "A", "the trapped CNTPCT_EL0 returns zero",
     HVF_CPU,
     """                            let value = hv::counter_now().wrapping_sub(thread.vtimer_offset);""",
     """                            let value = 0u64;""",
     HVF_CPU_TESTS),
    ("mac-hvf-C11", "A", "an EL1-vector exit saves the vCPU's own PC (the vector) rather than ELR_EL1",
     HVF_CPU,
     """        if traps_at_address && self.sentinel == Some(at) {
            self.save(thread, elr, spsr)""",
     """        if traps_at_address && self.sentinel == Some(at) {
            self.save(thread, cpu.reg(Reg::Pc).unwrap_or(0), spsr)""",
     HVF_CPU_TESTS),
    ("mac-hvf-C12", "A", "a veneer BRK at an unregistered address is reported as the guest's own BRK",
     HVF_CPU,
     """            ec::BRK64 if (iss & 0xFFFF) as u32 == 0xF00D && self.is_veneer(at) => {""",
     """            ec::BRK64 if false => {""",
     HVF_CPU_TESTS),
    ("mac-hvf-C13", "A", "the veneer page is overlaid on a page of real code",
     HVF_CPU,
     """            Some(region) if region.protection.is_executable() => {""",
     """            Some(region) if false && region.protection.is_executable() => {""",
     HVF_CPU_TESTS),
    ("mac-hvf-C14", "A", "EL1 is configured without FP/SIMD enabled at EL0",
     HVF_SYSTEM,
     """const CPACR_EL1: u64 = 3 << 20;""",
     """const CPACR_EL1: u64 = 0;""",
     HVF_CPU_TESTS),
    ("mac-hvf-A1", "A", "guest threads on a backend that cannot count are given counted windows",
     HVF_THREADS,
     """    let limit = if counted { RunLimit::Instructions(window) } else { RunLimit::Unlimited };""",
     """    let limit = RunLimit::Instructions(window);""",
     HVF_INIT_GATE),
    ("mac-hvf-A2", "B", "guest threads on a counting backend run unbounded, losing their window boundaries",
     HVF_THREADS,
     """    let limit = if counted { RunLimit::Instructions(window) } else { RunLimit::Unlimited };""",
     """    let limit = if false { RunLimit::Instructions(window) } else { RunLimit::Unlimited };""",
     BIONIC_RUNAWAY),
    ("mac-hvf-A3", "A", "stop_guest_threads does not halt a thread that has no run windows",
     HVF_BIONIC,
     """        for halt in self.uncounted_halts.lock().values() {
            halt.request();
        }""",
     """        for halt in self.uncounted_halts.lock().values() {
            let _ = halt;
        }""",
     HVF_INIT_GATE),
]
