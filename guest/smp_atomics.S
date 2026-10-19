// Every CPU adds to shared counters at once, with the LSE atomics and with exclusives on the
// same words; CPU 0 then checks that no update was lost. Prints "smp atomics ok" or
// "smp atomics FAIL <n>" and powers off. Run with NCPU CPUs (tests/guest.rs).

    .arch armv8.1-a
#define NCPU 4
#define ITER 20000
#define UART 0xF0100000

    .section .text.boot, "ax"
    .global _start
_start:
    mrs     x19, mpidr_el1
    and     x19, x19, #0xff
    cbnz    x19, work
    // CPU 0 starts the others (PSCI CPU_ON) at _start.
    mov     x20, #1
1:  ldr     w0, =0xC4000003
    mov     x1, x20
    adr     x2, _start
    mov     x3, #0
    hvc     #0
    add     x20, x20, #1
    cmp     x20, #NCPU
    b.lo    1b

work:
    adr     x21, counters
    add     x23, x21, #8
    add     x24, x21, #16
    add     x25, x21, #32
    add     x26, x21, #48
    ldr     x22, =ITER
    mov     x9, #1
2:  // c0: LDADD
    stadd   x9, [x21]
    // c1: a CAS loop
    ldr     x0, [x21, #8]
3:  add     x1, x0, #1
    mov     x2, x0
    casal   x2, x1, [x23]
    cmp     x2, x0
    mov     x0, x2
    b.ne    3b
    // c2: LDXR/STXR and LDADDAL on the same word
4:  ldaxr   x0, [x24]
    add     x0, x0, #1
    stlxr   w1, x0, [x24]
    cbnz    w1, 4b
    ldaddal x9, x0, [x24]
    // c3/c4: both halves by CASP and by LDXP/STXP
    ldp     x4, x5, [x21, #32]
5:  add     x6, x4, #1
    add     x7, x5, #1
    mov     x2, x4
    mov     x3, x5
    caspal  x2, x3, x6, x7, [x25]
    cmp     x2, x4
    ccmp    x3, x5, #0, eq
    mov     x4, x2
    mov     x5, x3
    b.ne    5b
6:  ldaxp   x4, x5, [x25]
    add     x4, x4, #1
    add     x5, x5, #1
    stlxp   w1, x4, x5, [x25]
    cbnz    w1, 6b
    // c5/c6: the 32-bit halves of one doubleword, by CASP of W registers
    ldr     w4, [x21, #48]
    ldr     w5, [x21, #52]
7:  add     w6, w4, #1
    add     w7, w5, #2
    mov     w2, w4
    mov     w3, w5
    casp    w2, w3, w6, w7, [x26]
    cmp     w2, w4
    ccmp    w3, w5, #0, eq
    mov     w4, w2
    mov     w5, w3
    b.ne    7b
    subs    x22, x22, #1
    b.ne    2b

    // Done; CPUs other than 0 park.
    add     x0, x21, #64
    stadd   x9, [x0]
    cbz     x19, check
8:  wfi
    b       8b

check:
    add     x0, x21, #64
9:  ldar    x1, [x0]
    cmp     x1, #NCPU
    b.ne    9b
    ldr     x3, =(NCPU * ITER)
    mov     x4, #0          // failure bits
    ldr     x1, [x21]
    cmp     x1, x3
    cset    x5, ne
    orr     x4, x4, x5
    ldr     x1, [x21, #8]
    cmp     x1, x3
    cset    x5, ne
    orr     x4, x4, x5, lsl #1
    ldr     x1, [x21, #16]
    cmp     x1, x3, lsl #1
    cset    x5, ne
    orr     x4, x4, x5, lsl #2
    ldp     x1, x2, [x21, #32]
    cmp     x1, x3, lsl #1
    cset    x5, ne
    orr     x4, x4, x5, lsl #3
    cmp     x2, x3, lsl #1
    cset    x5, ne
    orr     x4, x4, x5, lsl #4
    ldp     w1, w2, [x21, #48]
    cmp     w1, w3
    cset    x5, ne
    orr     x4, x4, x5, lsl #5
    cmp     w2, w3, lsl #1
    cset    x5, ne
    orr     x4, x4, x5, lsl #6
    adr     x0, msg
    bl      puts
    cbz     x4, 10f
    adr     x0, fail
    bl      puts
    lsr     x0, x4, #4      // which counters were wrong, in hex
    bl      hexdigit
    and     x0, x4, #15
    bl      hexdigit
    b       11f
10: adr     x0, ok
    bl      puts
11: mov     w0, #'\n'
    bl      putc
    ldr     w0, =0x84000008 // SYSTEM_OFF
    hvc     #0
12: wfi
    b       12b

hexdigit:
    cmp     x0, #10
    add     x1, x0, #'0'
    add     x2, x0, #('a' - 10)
    csel    x0, x1, x2, lo
    // falls through to putc
putc:
    ldr     x1, =UART
13: ldrb    w2, [x1, #5]
    tbz     w2, #5, 13b
    strb    w0, [x1]
    ret

puts:
    mov     x10, x30
    mov     x11, x0
14: ldrb    w0, [x11], #1
    cbz     w0, 15f
    bl      putc
    b       14b
15: ret     x10

msg:  .asciz "smp atomics "
ok:   .asciz "ok"
fail: .asciz "FAIL "
    .balign 8
    .ltorg

    .data
    .balign 4096        // not on a code page, which every store would invalidate
counters:
    .fill 9, 8, 0
