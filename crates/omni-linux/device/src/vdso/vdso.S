// omnidroid's vDSO: the arm64 Linux kernel's user-mode entry points (`__kernel_clock_gettime`,
// `__kernel_gettimeofday`, `__kernel_clock_getres`, `__kernel_rt_sigreturn`), which bionic calls
// instead of the system calls when AT_SYSINFO_EHDR names them (bionic's `__libc_init_vdso`).
//
// Time is read as Linux's vDSO reads it: the architectural counter `CNTVCT_EL0`, scaled by the
// counter's frequency and offset by what the kernel keeps in the data page (`crate::vdso`):
//
//   vvar + 0   u64  1 when the page is filled (0: every call is the system call)
//   vvar + 8   u64  the counter's frequency, Hz
//   vvar + 16  i64  CLOCK_MONOTONIC at counter 0, ns
//   vvar + 24  i64  CLOCK_REALTIME - CLOCK_MONOTONIC, ns
//
// Any clock but REALTIME(_COARSE), MONOTONIC(_RAW, _COARSE) and BOOTTIME is the system call, as in
// Linux's vDSO. The data page is the last page of this image: the kernel writes it after mapping.
    .text

// ns (x0) from the counter, per the data page at x9; clobbers x10-x13.
.macro counter_ns
    isb
    mrs x10, cntvct_el0
    ldr x11, [x9, #8]               // frequency
    udiv x12, x10, x11              // whole seconds
    msub x13, x12, x11, x10         // remainder ticks
    movz x10, #0xca00
    movk x10, #0x3b9a, lsl #16      // 1e9
    mul x13, x13, x10
    udiv x13, x13, x11              // remainder ns
    madd x0, x12, x10, x13          // ns
.endm

    .globl __kernel_clock_gettime
    .type __kernel_clock_gettime, %function
    .balign 16
__kernel_clock_gettime:
    adrp x9, .Lvvar
    add x9, x9, :lo12:.Lvvar
    ldr x10, [x9]
    cbz x10, 9f
    cmp w0, #0
    b.eq 1f                         // REALTIME
    cmp w0, #5
    b.eq 1f                         // REALTIME_COARSE
    cmp w0, #1
    b.eq 2f                         // MONOTONIC
    cmp w0, #4
    b.eq 2f                         // MONOTONIC_RAW
    cmp w0, #6
    b.eq 2f                         // MONOTONIC_COARSE
    cmp w0, #7
    b.eq 2f                         // BOOTTIME
    b 9f
1:  mov x14, x1
    counter_ns
    ldr x10, [x9, #16]
    add x0, x0, x10
    ldr x10, [x9, #24]
    add x0, x0, x10
    b 3f
2:  mov x14, x1
    counter_ns
    ldr x10, [x9, #16]
    add x0, x0, x10
3:  movz x10, #0xca00
    movk x10, #0x3b9a, lsl #16      // 1e9
    udiv x11, x0, x10
    msub x12, x11, x10, x0
    stp x11, x12, [x14]
    mov w0, #0
    ret
9:  mov x8, #113                    // clock_gettime
    svc #0
    ret
    .size __kernel_clock_gettime, . - __kernel_clock_gettime

    .globl __kernel_gettimeofday
    .type __kernel_gettimeofday, %function
    .balign 16
__kernel_gettimeofday:
    adrp x9, .Lvvar
    add x9, x9, :lo12:.Lvvar
    ldr x10, [x9]
    cbz x10, 9f
    mov x14, x0
    mov x15, x1
    cbz x14, 2f
    counter_ns
    ldr x10, [x9, #16]
    add x0, x0, x10
    ldr x10, [x9, #24]
    add x0, x0, x10
    movz x10, #0xca00
    movk x10, #0x3b9a, lsl #16      // 1e9
    udiv x11, x0, x10               // seconds
    msub x12, x11, x10, x0          // ns
    mov x10, #1000
    udiv x12, x12, x10              // us
    stp x11, x12, [x14]
2:  cbz x15, 3f
    stp wzr, wzr, [x15]             // struct timezone: UTC, no DST
3:  mov w0, #0
    ret
9:  mov x8, #169                    // gettimeofday
    svc #0
    ret
    .size __kernel_gettimeofday, . - __kernel_gettimeofday

    .globl __kernel_clock_getres
    .type __kernel_clock_getres, %function
    .balign 16
__kernel_clock_getres:
    cmp w0, #0
    b.eq 1f
    cmp w0, #1
    b.eq 1f
    cmp w0, #4
    b.eq 1f
    cmp w0, #5
    b.eq 1f
    cmp w0, #6
    b.eq 1f
    cmp w0, #7
    b.eq 1f
    mov x8, #114                    // clock_getres
    svc #0
    ret
1:  cbz x1, 2f
    mov x10, #1
    stp xzr, x10, [x1]              // 1 ns, as the kernel answers
2:  mov w0, #0
    ret
    .size __kernel_clock_getres, . - __kernel_clock_getres

// The signal trampoline, where a handler returns when its action has no SA_RESTORER. Unwinders
// know it by these two instructions.
    .globl __kernel_rt_sigreturn
    .type __kernel_rt_sigreturn, %function
    .balign 16
    nop
__kernel_rt_sigreturn:
    mov x8, #139                    // rt_sigreturn
    svc #0
    .size __kernel_rt_sigreturn, . - __kernel_rt_sigreturn

// The data page: the last page of the image, filled by the kernel.
    .section .vvar, "a"
    .balign 4096
    .globl __omni_vvar
    .type __omni_vvar, %object
__omni_vvar:
.Lvvar:
    .zero 4096
    .size __omni_vvar, 4096
