// armless build: bare metal at EL1, MMU off. Undefined instructions take a
// synchronous exception; the handler flags them and skips the instruction
// without touching any general-purpose register or the flags.

    .section .text.boot, "ax"
    .global _start
_start:
    ldr     x1, =__stack_top
    mov     sp, x1
    ldr     x1, =vectors
    msr     vbar_el1, x1
    mov     x1, #(3 << 20)          // CPACR_EL1.FPEN: no FP/SIMD traps
    msr     cpacr_el1, x1
    isb
    bl      run_cases
    ldr     w0, =0x84000008         // PSCI SYSTEM_OFF
    hvc     #0
    b       .

    .global out
// void out(const char *s, u64 n): polled 16550 at 0xF0100000
out:
    mov     x2, #0xF0100000
1:  cbz     x1, 3f
2:  ldrb    w3, [x2, #5]
    tbz     w3, #5, 2b
    ldrb    w3, [x0], #1
    strb    w3, [x2]
    sub     x1, x1, #1
    b       1b
3:  ret

    .balign 0x800
vectors:
    .rept 4
    .balign 0x80
    b       sync_handler            // current EL, SP0
    .endr
    .balign 0x80
    b       sync_handler            // current EL, SPx: sync
    .balign 0x80
    hlt     #2                      // IRQ
    .balign 0x80
    hlt     #3
    .balign 0x80
    hlt     #4

sync_handler:
    msr     tpidr_el1, x0
    mrs     x0, esr_el1
    lsr     x0, x0, #26
    cmp     x0, #0x25               // data abort: FAULT
    b.eq    2f
    cbnz    x0, 1f                  // otherwise only "unknown" (EC 0) is expected
    adrp    x0, undef_flag
    add     x0, x0, :lo12:undef_flag
    str     x0, [x0]                // any non-zero value other than 2
    b       3f
2:  adrp    x0, undef_flag
    add     x0, x0, :lo12:undef_flag
    msr     tpidr_el0, x1
    mov     x1, #2
    str     x1, [x0]
    mrs     x1, tpidr_el0
3:
    mrs     x0, elr_el1
    add     x0, x0, #4
    msr     elr_el1, x0
    mrs     x0, tpidr_el1
    eret
1:  hlt     #5
