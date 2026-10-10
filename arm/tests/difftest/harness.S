// State load/save shared by the qemu-user (reference) and bare-metal (armless)
// builds. Layout of a state block (768 bytes):
//   0..232   x0..x28        232 nzcv      240 fpsr      248 fpcr
//   256..768 q0..q31

    .text
    .global load_state
// x29 = init block
load_state:
#ifdef WITH_SIMD
    ldp     q0, q1, [x29, #256]
    ldp     q2, q3, [x29, #288]
    ldp     q4, q5, [x29, #320]
    ldp     q6, q7, [x29, #352]
    ldp     q8, q9, [x29, #384]
    ldp     q10, q11, [x29, #416]
    ldp     q12, q13, [x29, #448]
    ldp     q14, q15, [x29, #480]
    ldp     q16, q17, [x29, #512]
    ldp     q18, q19, [x29, #544]
    ldp     q20, q21, [x29, #576]
    ldp     q22, q23, [x29, #608]
    ldp     q24, q25, [x29, #640]
    ldp     q26, q27, [x29, #672]
    ldp     q28, q29, [x29, #704]
    ldp     q30, q31, [x29, #736]
    ldr     x0, [x29, #240]
    msr     fpsr, x0
    ldr     x0, [x29, #248]
    msr     fpcr, x0
#endif
    ldr     x0, [x29, #232]
    msr     nzcv, x0
    ldp     x0, x1, [x29, #0]
    ldp     x2, x3, [x29, #16]
    ldp     x4, x5, [x29, #32]
    ldp     x6, x7, [x29, #48]
    ldp     x8, x9, [x29, #64]
    ldp     x10, x11, [x29, #80]
    ldp     x12, x13, [x29, #96]
    ldp     x14, x15, [x29, #112]
    ldp     x16, x17, [x29, #128]
    ldp     x18, x19, [x29, #144]
    ldp     x20, x21, [x29, #160]
    ldp     x22, x23, [x29, #176]
    ldp     x24, x25, [x29, #192]
    ldp     x26, x27, [x29, #208]
    ldr     x28, [x29, #224]
    ret

    .global save_state
// Saves into the 768-byte block at sp (set up by run_cases).
save_state:
    stp     x0, x1, [sp, #0]
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
    str     x28, [sp, #224]
    mrs     x0, nzcv
    str     x0, [sp, #232]
#ifdef WITH_SIMD
    mrs     x0, fpsr
    str     x0, [sp, #240]
    mrs     x0, fpcr
    str     x0, [sp, #248]
    stp     q0, q1, [sp, #256]
    stp     q2, q3, [sp, #288]
    stp     q4, q5, [sp, #320]
    stp     q6, q7, [sp, #352]
    stp     q8, q9, [sp, #384]
    stp     q10, q11, [sp, #416]
    stp     q12, q13, [sp, #448]
    stp     q14, q15, [sp, #480]
    stp     q16, q17, [sp, #512]
    stp     q18, q19, [sp, #544]
    stp     q20, q21, [sp, #576]
    stp     q22, q23, [sp, #608]
    stp     q24, q25, [sp, #640]
    stp     q26, q27, [sp, #672]
    stp     q28, q29, [sp, #704]
    stp     q30, q31, [sp, #736]
#endif
    ret
