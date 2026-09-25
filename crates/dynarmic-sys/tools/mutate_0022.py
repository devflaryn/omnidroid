"""Hand mutations of vendored patch 0022 (D38), each rebuilt and run against its detector, then
restored and the file's SHA-1 checked.

Not rows of `tools/mutate.py`: a row that rebuilds dynarmic leaves the mutated library in the build
directory for the rows after it (D35). This rebuilds the restored tree at the end instead.

usage: python3 crates/dynarmic-sys/tools/mutate_0022.py [ids...]   (from the repository root; honours
OMNIDROID_DYNARMIC_BUILD_DIR). Exclusive: nothing else may build or commit while it runs.
"""
import hashlib
import os
import subprocess
import sys
import time

ROOT = os.getcwd()
X64 = 'crates/dynarmic-sys/vendor/dynarmic/src/dynarmic/backend/x64/'
IFACE = X64 + 'a64_interface.cpp'
EMIT = X64 + 'a64_emit_x64.cpp'
EMITBASE = X64 + 'emit_x64.cpp'
EMITH = X64 + 'a64_emit_x64.h'
PIN = 'crates/dynarmic-sys/vendor/PIN.txt'
SC = ['--test', 'shared_cache']

MUTATIONS = {
    'S1': ('the generation does not empty a thread\'s RSB and fast-dispatch table', IFACE,
           """    if (g != thread.seen_generation) {
        thread.flush_routing(&thread);
        thread.seen_generation = g;
    }""",
           """    if (g != thread.seen_generation) {
        thread.seen_generation = g;
    }""", SC + ['--', 'an_invalidation_through_one_jit']),
    'S2': ('TPIDR_EL0 read from the template\'s box, not the running thread\'s', EMIT,
           """    if (shared_code) {
        // Omnidroid patch 0022: the running thread's box.
        code.mov(result, qword[r15 + offsetof(A64JitState, od_tpidr_el0)]);
        code.mov(result, qword[result]);
    } else if (conf.tpidr_el0) {""",
           """    if (shared_code) {
        code.mov(result, u64(conf.tpidr_el0));
        code.mov(result, qword[result]);
    } else if (conf.tpidr_el0) {""", SC + ['--', 'each_jit_reads_its_own_thread_pointer']),
    'S3': ('an invalidated target\'s link slots are not unlinked', EMITBASE,
           """        const u64 value = target_code_ptr ? reinterpret_cast<u64>(target_code_ptr) : link.unlinked;""",
           """        if (!target_code_ptr) {
            continue;
        }
        const u64 value = reinterpret_cast<u64>(target_code_ptr);""", SC + ['--', 'a_link_to_an_invalidated_block']),
    'S4': ('a translation made outside the lock is published whatever was invalidated meanwhile', IFACE,
           """    const u64 now = invalidation_serial.load(std::memory_order_relaxed);
    if (now == serial) {
        return false;
    }""",
           """    const u64 now = invalidation_serial.load(std::memory_order_relaxed);
    if (now == serial || true) {
        return false;
    }""", SC + ['--', 'a_translation_overtaken']),
    'S5': ('a location another thread is translating is translated again, not waited for', IFACE,
           """        if (in_flight.count(location) == 0) {
            in_flight.insert(location);""",
           """        if (true) {
            in_flight.insert(location);""", SC + ['--', 'eight_jits_on_one_cache']),
    'S6': ('a thread parked in a callback holds its region (no holes)', IFACE,
           """    const bool known = barrier_available && AsymmetricBarrier();""",
           """    const bool known = false;""", SC + ['--', 'a_thread_parked_in_a_callback']),
    'S7': ('retiring a region does not halt the other threads', IFACE,
           """            if (other != &thread) {
                Atomic::Or(&other->jit_state->halt_reason, static_cast<u32>(HaltReason::CacheInvalidation));
            }""",
           """            (void)other;""", SC + ['--', 'a_thread_parked_in_a_callback']),
    'S8': ('attach accepts any configuration', IFACE,
           """    if (c.callbacks == nullptr || t.callbacks == nullptr) {
        return false;
    }""",
           """    if (c.callbacks != nullptr) {
        return true;
    }""", SC + ['--', 'a_jit_that_needs_different_code']),
    'S9': ('Run does not bring the RSB and table up to date at entry (only the dispatcher does)', IFACE,
           """        if (shared) {
            shared->Sync(thread);
        }
        SCOPE_EXIT {
            if (shared) {
                // Patch 0022: outside RunCode, this thread holds no region.""",
           """        SCOPE_EXIT {
            if (shared) {
                // Patch 0022: outside RunCode, this thread holds no region.""", SC + ['--', 'a_resume_through_the_return_stack_buffer']),
    'S10': ('the fast-dispatch handler probes a template-time table, not the running thread\'s', EMIT,
            """            code.mov(r12, qword[r15 + offsetof(A64JitState, od_fast_dispatch_table)]);""",
            """            code.mov(r12, qword[r15 + offsetof(A64JitState, od_tpidr_el0)]);""", SC),
    'S11': ('the global monitor scan skips the template processor\'s slot in shared code', EMITH,
            """    bool SkipOwnMonitorSlot() const { return !shared_code; }""",
            """    bool SkipOwnMonitorSlot() const { return true; }""", SC + ['--', 'another_processor_s_store_clears']),
    'S12': ('the fast-dispatch miss names the location before the lookup (upstream order) in shared code', EMIT,
            """        if (shared_code) {
            // Omnidroid patch 0022: the lookup consults this same table""",
            """        if (false) {
            // Omnidroid patch 0022: the lookup consults this same table""", SC + ['--', 'indirect_calls_that_collide']),
    'S13': ('the dispatcher does not consult the thread table before the lock (every lookup locks)', IFACE,
            """            void* const table = fast_dispatch_table.get();""",
            """            void* const table = nullptr;""", SC + ['--', 'a_thread_finds_what_it_has_looked_up']),
}


def sha1(path):
    return hashlib.sha1(open(path, 'rb').read()).hexdigest()[:8]


def run(args, env_extra=None):
    env = dict(os.environ)
    if env_extra:
        env.update(env_extra)
    r = subprocess.run(['cargo', 'test', '-p', 'dynarmic-sys', '--release'] + args, cwd=ROOT, env=env,
                       capture_output=True, text=True, timeout=1800)
    return r.returncode, r.stdout + r.stderr


def touch(path):
    now = time.time()
    os.utime(path, (now, now))


ids = sys.argv[1:] or list(MUTATIONS)
results = []
for mid in ids:
    what, path, old, new, args = MUTATIONS[mid]
    original = open(path, encoding='utf-8', newline='').read()
    before = sha1(path)
    assert original.count(old) == 1, (mid, 'anchor not unique', original.count(old))
    open(path, 'w', encoding='utf-8', newline='').write(original.replace(old, new))
    touch(PIN)
    try:
        code, out = run(args)
    finally:
        open(path, 'w', encoding='utf-8', newline='').write(original)
        touch(PIN)
    after = sha1(path)
    assert before == after, (mid, before, after)
    caught = code != 0
    failing = [l.strip() for l in out.splitlines() if l.strip().startswith('test ') and l.strip().endswith('FAILED')]
    crashed = 'exit code' in out or 'STATUS_' in out or 'signal' in out
    results.append((mid, what, caught, failing, before, crashed))
    print(f"{mid}: {'CAUGHT' if caught else 'NOT CAUGHT'} -- {what}; failed: {failing or ('(process died)' if caught else '-')}; sha1 {before}", flush=True)

print()
print(f"{sum(1 for r in results if r[2])}/{len(results)} caught")
# rebuild the restored tree
code, out = run(['--no-run'])
print('restored tree rebuilt:', 'ok' if code == 0 else 'FAILED')
