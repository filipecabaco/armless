// Bare-metal entry, exception vectors and EL0 trampolines for the test guest.

    .section .text.boot, "ax"
    .global _start
_start:
    // x0 = DTB address from the loader; keep it for kmain.
    mov     x19, x0
    ldr     x1, =__stack_top
    mov     sp, x1
    // Zero .bss
    ldr     x1, =__bss_start
    ldr     x2, =__bss_end
1:  cmp     x1, x2
    b.hs    2f
    str     xzr, [x1], #8
    b       1b
2:  mov     x0, x19
    bl      kmain
    // kmain returned: power off through PSCI SYSTEM_OFF.
    ldr     w0, =0x84000008
    hvc     #0
3:  wfi
    b       3b

    .global fail_halt
fail_halt:
    hlt     #1

// ---------------------------------------------------------------- vectors
// Frame layout (288 bytes): x0..x30 at 0..240, elr 248, spsr 256, esr 264, far 272

    .macro ventry kind
    .balign 0x80
    sub     sp, sp, #288
    stp     x0, x1, [sp, #0]
    mov     x1, #\kind
    b       exc_common
    .endm

    .text
    .balign 0x800
    .global vectors
vectors:
    ventry 0
    ventry 1
    ventry 2
    ventry 3
    ventry 4
    ventry 5
    ventry 6
    ventry 7
    ventry 8
    ventry 9
    ventry 10
    ventry 11
    ventry 12
    ventry 13
    ventry 14
    ventry 15

exc_common:
    stp     x2, x3, [sp, #16]
    stp     x4, x5, [sp, #32]
    stp     x6, x7, [sp, #48]
    stp     x8, x9, [sp, #64]
    stp     x10, x11, [sp, #80]
    stp     x12, x13, [sp, #96]
    stp     x14, x15, [sp, #112]
    stp     x16, x17, [sp, #128]
    stp     x18, x19, [sp, #144]
    stp     x20, x21, [sp, #160]
    stp     x22, x23, [sp, #176]
    stp     x24, x25, [sp, #192]
    stp     x26, x27, [sp, #208]
    stp     x28, x29, [sp, #224]
    str     x30, [sp, #240]
    mrs     x2, elr_el1
    mrs     x3, spsr_el1
    stp     x2, x3, [sp, #248]
    mrs     x2, esr_el1
    mrs     x3, far_el1
    stp     x2, x3, [sp, #264]
    mov     x0, sp
    bl      exc_handler
    ldp     x2, x3, [sp, #248]
    msr     elr_el1, x2
    msr     spsr_el1, x3
    ldp     x2, x3, [sp, #16]
    ldp     x4, x5, [sp, #32]
    ldp     x6, x7, [sp, #48]
    ldp     x8, x9, [sp, #64]
    ldp     x10, x11, [sp, #80]
    ldp     x12, x13, [sp, #96]
    ldp     x14, x15, [sp, #112]
    ldp     x16, x17, [sp, #128]
    ldp     x18, x19, [sp, #144]
    ldp     x20, x21, [sp, #160]
    ldp     x22, x23, [sp, #176]
    ldp     x24, x25, [sp, #192]
    ldp     x26, x27, [sp, #208]
    ldp     x28, x29, [sp, #224]
    ldr     x30, [sp, #240]
    ldp     x0, x1, [sp, #0]
    add     sp, sp, #288
    eret

// --------------------------------------------------------------- EL0 calls
// long run_el0(long (*fn)(long), long arg, void *user_stack)
// Runs fn(arg) at EL0t. The function returns with `svc #0x99`; the C
// handler then redirects ELR to el0_return with SPSR = EL1h.
    .global run_el0
run_el0:
    ldr     x9, =el1_ctx
    stp     x19, x20, [x9, #0]
    stp     x21, x22, [x9, #16]
    stp     x23, x24, [x9, #32]
    stp     x25, x26, [x9, #48]
    stp     x27, x28, [x9, #64]
    stp     x29, x30, [x9, #80]
    mov     x10, sp
    str     x10, [x9, #96]
    msr     sp_el0, x2
    msr     elr_el1, x0
    msr     spsr_el1, xzr          // EL0t, interrupts unmasked
    mov     x0, x1
    ldr     x30, =el0_exit
    eret

// Lives in the EL0 world: the user function returns here.
    .global el0_exit
el0_exit:
    svc     #0x99
    b       .

    .global el0_return
el0_return:
    ldr     x9, =el1_ctx
    ldp     x19, x20, [x9, #0]
    ldp     x21, x22, [x9, #16]
    ldp     x23, x24, [x9, #32]
    ldp     x25, x26, [x9, #48]
    ldp     x27, x28, [x9, #64]
    ldp     x29, x30, [x9, #80]
    ldr     x10, [x9, #96]
    mov     sp, x10
    ret

    .bss
    .balign 16
el1_ctx:
    .space 112
