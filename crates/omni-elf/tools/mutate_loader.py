"""Mutation testing for the `omni-elf` loader.

    python crates/omni-elf/tools/mutate_loader.py        # from the repository root

Global Constraint 12: a test that does not fail when the logic it covers is reverted is not
evidence. This harness makes that checkable rather than claimed. It applies one mutation at a time
to the loader's source, re-runs the loader test suites, records which tests noticed, and **always**
restores the file — including on a failure, via `try`/`finally`.

Two kinds of mutation, and the second is the point:

* **Direction A** reverts a piece of logic. Correctness must break.
* **Direction B** over-corrects it — seals more than it should, relocates the whole library in one
  window, maps text copy-on-write. These pass a naive correctness reading and destroy a property the
  design depends on, so they are what the "in both directions" half of the constraint is about.

A mutation that does not compile, does not match its pattern, or is caught by nothing is reported as
`MISS`, never as a pass. Two of these were `MISS` on the first run and both were harness bugs.

`--no-fail-fast` is not optional. Without it `cargo test` stops after the first failing test binary,
which silently attributes every mutation to whichever binary happened to run first and never runs the
rest at all. That produced a plausible and completely wrong table on the first attempt.

Requires the real APK at the repository root; without it every loader test skips and every mutation
comes back `NOT CAUGHT`.

Last full run: 18 mutations, 18 caught. The table is in the Task 5 report.
"""
import os
import subprocess

REL = 'crates/omni-elf/src/loader/relocate.rs'
MOD = 'crates/omni-elf/src/loader/mod.rs'
PLAN = 'crates/omni-elf/src/loader/plan.rs'

MUTATIONS = [
    # ---- direction A: revert the logic; correctness must break -------------------------------
    ('A1 ABS64 stored as 32 bits (the earlier plan draft said ABS32)', REL,
     '        unsafe { ptr.cast::<u64>().write_unaligned(value) };',
     '        unsafe { if ty == R_AARCH64_ABS64 { ptr.cast::<u32>().write_unaligned(value as u32) } else { ptr.cast::<u64>().write_unaligned(value) } };'),

    ('A2 relocation window restored writable instead of to its own protection', REL,
     '            Some(range.protection)',
     '            Some(Protection::ReadWrite)'),

    ('A3 R_AARCH64_RELATIVE written without the load base', REL,
     '            R_AARCH64_RELATIVE => (self.base as u64).wrapping_add(addend as u64),',
     '            R_AARCH64_RELATIVE => addend as u64,'),

    ('A4 an unmappable relocation target skipped instead of refused', REL,
     '''        if !self.open.is_some_and(|w| target >= w.start && end <= w.end) {
            self.close()?;
            self.open_window(&r, target, end)?;
        }''',
     '''        if !self.open.is_some_and(|w| target >= w.start && end <= w.end) {
            self.close()?;
            if self.open_window(&r, target, end).is_err() {
                self.open = None;
                return Ok(());
            }
        }'''),

    ('A5 DT_JMPREL table never applied', MOD,
     '''        if let Some(plt) = tables.plt.as_mut() {
            relocator.apply_table(plt)?;
        }''',
     '''        if let Some(plt) = tables.plt.as_mut() {
            let _ = plt;
        }'''),

    ('A6 init_array read from the file image instead of relocated memory', MOD,
     '    let ptr = space.ptr(start, size)?;',
     '    let ptr = elf.slice_at_vaddr(what, array.vaddr, array.size)?.as_ptr() as *mut u8;'),

    ('A7 PT_GNU_RELRO never sealed', MOD,
     '        sealed: seal,',
     '        sealed: false,'),

    ('A8 relro coverage check disabled', MOD,
     '    if end_page < start_page || !plan_covers(plan, start_page, end_page) {',
     '    if false {'),

    ('A9 defined symbols not resolved inside the object', MOD,
     '            bindings.push(SymbolBinding::Value(value as u64));',
     '            bindings.push(SymbolBinding::Unresolved);'),

    ('A10 the .bss tail of the last file page not zeroed', MOD,
     '        unsafe { core::ptr::write_bytes(dst, 0, fill.len) };',
     '        let _ = dst;'),

    ('A11 a failed load does not release its reservation', MOD,
     '    if outcome.is_err() {',
     '    if false {'),

    ('A12 unload releases only the first mapped range', MOD,
     '        space.unmap(self.start, self.span())?;',
     '        space.unmap(self.start, self.ranges[0].len())?;'),

    ('A13 segments not page-rounded before mapping', PLAN,
     '            let mem_start = page_down(seg.p_vaddr, page);',
     '            let mem_start = seg.p_vaddr;'),

    ('A14 p_align below the host page size accepted', PLAN,
     '            if seg.p_align > 1 && seg.p_align < page as u64 {',
     '            if false {'),

    ('A15 the final window never restored to its own protection', REL,
     '        let Some(w) = self.open.take() else { return Ok(()) };',
     '''        let Some(w) = self.open.take() else { return Ok(()) };
        #[allow(unreachable_code, unused_variables)]
        return Ok(());'''),

    # ---- direction B: over-correct; a design property must break -----------------------------
    ('B1 relocate a whole mapped range at once instead of in windows', REL,
     '        let mut win_end = win_start.saturating_add(self.window).min(range.end);',
     '        let mut win_end = range.end;'),

    ('B2 seal every writable range, not just PT_GNU_RELRO', MOD,
     '        None => vec![(start, piece.len, piece.rest)],',
     '        None => vec![(start, piece.len, sealed(piece.rest))],'),

    ('B3 text mapped copy-on-write instead of execute-read', MOD,
     '        PieceSource::File { .. } if piece.rest.is_executable() => Protection::ReadExecute,',
     '        PieceSource::File { .. } if piece.rest.is_executable() => Protection::ReadWrite,'),
]

CMD = [
    'cargo', 'test', '-p', 'omni-elf', '--release',
    '--no-fail-fast', '--test', 'loader_m1', '--test', 'loader_commit', '--test', 'loader_hostile', '--lib',
    '--', '--test-threads=1',
]


def run():
    p = subprocess.run(CMD, capture_output=True, text=True, errors='replace')
    out = p.stdout + p.stderr
    failed = [l.split(' ... ')[0].replace('test ', '', 1).strip()
              for l in out.splitlines() if ' ... FAILED' in l]
    crashed = 'STATUS_ACCESS_VIOLATION' in out or '0xc0000005' in out
    if 'error[E' in out or ('error: could not compile' in out and not failed):
        return 'DID NOT COMPILE', []
    if crashed and not failed:
        return 'CRASHED (access violation)', []
    if not failed:
        return 'NOT CAUGHT', []
    return 'caught', failed


def main():
    # Run from the repository root whichever directory the harness was invoked from.
    os.chdir(os.path.dirname(os.path.dirname(os.path.dirname(os.path.dirname(
        os.path.abspath(__file__))))))
    results = []
    for name, path, old, new in MUTATIONS:
        original = open(path, encoding='utf-8').read()
        if old not in original:
            results.append((name, 'PATTERN NOT FOUND', []))
            print(f'!! {name}: pattern not found', flush=True)
            continue
        try:
            open(path, 'w', encoding='utf-8', newline='').write(original.replace(old, new, 1))
            verdict, failed = run()
        finally:
            open(path, 'w', encoding='utf-8', newline='').write(original)
        results.append((name, verdict, failed))
        print(f'{name}\n    -> {verdict}: {", ".join(failed) if failed else "-"}', flush=True)

    print('\n==== summary ====')
    for name, verdict, failed in results:
        mark = 'OK  ' if verdict == 'caught' else 'MISS'
        print(f'{mark} {name}: {verdict} ({len(failed)} tests)')
    print('\n==== detail ====')
    for name, verdict, failed in results:
        print(f'- {name}: {verdict}')
        for t in failed:
            print(f'    {t}')


if __name__ == '__main__':
    main()
