// Bare-metal benchmark for armless: phases that each stress one part of the machine, timed with
// the generic timer. Run it with bench/run.exs (or armless-run --raw build-guest/bench.bin
// --stats). Each phase prints "bench <name> <microseconds>"; the MMU is on, as under Linux.
#include <stdint.h>
#include <stddef.h>

#define UART_BASE 0xF0100000UL
#define REG8(a) (*(volatile uint8_t *)(a))
#define SYSREG_READ(name) ({ uint64_t _v; __asm__ volatile("mrs %0, " #name : "=r"(_v)); _v; })
#define SYSREG_WRITE(name, v) __asm__ volatile("msr " #name ", %0; isb" :: "r"((uint64_t)(v)) : "memory")

void fail_halt(void);
extern char vectors[];

static void putc_(char c) {
    while (!(REG8(UART_BASE + 5) & 0x20)) {
    }
    REG8(UART_BASE) = (uint8_t)c;
}
static void puts_(const char *s) {
    while (*s) {
        if (*s == '\n') putc_('\r');
        putc_(*s++);
    }
}
static void put_u64(uint64_t v) {
    char b[21];
    int i = 20;
    b[i] = 0;
    do { b[--i] = (char)('0' + v % 10); v /= 10; } while (v);
    puts_(&b[i]);
}

// Any exception is a bug in the benchmark or the machine: say so and stop
void exc_handler(uint64_t *frame, uint64_t kind) {
    puts_("bench exception kind=");
    put_u64(kind);
    puts_(" esr=");
    put_u64(frame[33]);
    puts_(" elr=");
    put_u64(frame[31]);
    puts_("\n");
    fail_halt();
}

void *memset(void *d, int c, size_t n) {
    uint8_t *p = d;
    while (n--) *p++ = (uint8_t)c;
    return d;
}
void *memcpy(void *d, const void *s, size_t n) {
    uint8_t *a = d;
    const uint8_t *b = s;
    while (n--) *a++ = *b++;
    return d;
}

// ------------------------------------------------------------------- MMU: identity, 16 MiB in 4K pages

static uint64_t l1[512] __attribute__((aligned(4096)));
static uint64_t l2[512] __attribute__((aligned(4096)));
static uint64_t l3[8][512] __attribute__((aligned(4096)));

#define D_VALID 1ULL
#define D_TABLE 3ULL
#define D_PAGE 3ULL
#define D_AF (1ULL << 10)
#define D_SH (3ULL << 8)
#define D_ATTR(n) ((uint64_t)(n) << 2)
// 16 MiB mapped with 4 KiB pages (the rest of the first GiB with 2 MiB blocks)
#define PAGED 0x10000000UL

static void mmu_on(void) {
    l1[0] = (uint64_t)l2 | D_TABLE;
    l1[3] = 0xC0000000ULL | D_VALID | D_AF | D_ATTR(0); // devices
    for (uint64_t i = 0; i < 512; i++) l2[i] = (i << 21) | D_VALID | D_AF | D_SH | D_ATTR(1);
    for (uint64_t t = 0; t < 8; t++) {
        l2[(PAGED >> 21) + t] = (uint64_t)l3[t] | D_TABLE;
        for (uint64_t p = 0; p < 512; p++)
            l3[t][p] = (PAGED + (t << 21) + (p << 12)) | D_PAGE | D_AF | D_SH | D_ATTR(1);
    }
    SYSREG_WRITE(mair_el1, 0xff00);
    uint64_t tcr = 32 | (32ULL << 16) | (2ULL << 30) | (3ULL << 12) | (1ULL << 10) | (1ULL << 8) |
                   (3ULL << 28) | (1ULL << 26) | (1ULL << 24);
    SYSREG_WRITE(tcr_el1, tcr);
    SYSREG_WRITE(ttbr0_el1, l1);
    __asm__ volatile("dsb sy; tlbi vmalle1; dsb sy; isb");
    SYSREG_WRITE(sctlr_el1, SYSREG_READ(sctlr_el1) | 1 | (1 << 2) | (1 << 12));
}

// ------------------------------------------------------------------- phases

static volatile uint64_t sink;
static uint8_t sieve_buf[1 << 20];
static uint8_t bufa[1 << 20], bufb[1 << 20];

// Integer ALU, branches and byte stores: a sieve and a bitwise CRC-32
static uint64_t b_int(void) {
    uint64_t acc = 0;
    for (int rep = 0; rep < 6; rep++) {
        memset(sieve_buf, 0, sizeof sieve_buf);
        for (uint32_t i = 2; i < sizeof sieve_buf; i++) {
            if (sieve_buf[i]) continue;
            acc += i;
            for (uint32_t j = i * 2; j < sizeof sieve_buf; j += i) sieve_buf[j] = 1;
        }
        uint32_t crc = 0xffffffff;
        for (uint32_t i = 0; i < 200000; i++) {
            crc ^= (uint8_t)i;
            for (int k = 0; k < 8; k++) crc = (crc >> 1) ^ (0xedb88320 & -(crc & 1));
        }
        acc += crc;
    }
    return acc;
}

// Sequential loads and stores within a few pages at a time
static uint64_t b_mem(void) {
    uint64_t acc = 0;
    for (int rep = 0; rep < 12; rep++) {
        memset(bufa, rep, sizeof bufa);
        memcpy(bufb, bufa, sizeof bufa);
        const uint64_t *p = (const uint64_t *)bufb;
        for (size_t i = 0; i < sizeof bufb / 8; i++) acc += p[i];
    }
    return acc;
}

// Random 8-byte loads and stores over 16 MiB of 4 KiB pages: the TLB and its fast tables
static uint64_t b_tlb(void) {
    uint64_t x = 88172645463325252ULL, acc = 0;
    volatile uint64_t *base = (volatile uint64_t *)PAGED;
    for (int i = 0; i < 3000000; i++) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        size_t idx = (x >> 3) & ((16u << 20) / 8 - 1);
        acc += base[idx];
        base[idx] = acc;
    }
    return acc;
}

typedef uint64_t (*op_fn)(uint64_t, uint64_t);
static uint64_t f_add(uint64_t a, uint64_t b) { return a + b; }
static uint64_t f_xor(uint64_t a, uint64_t b) { return a ^ (b << 1); }
static uint64_t f_mul(uint64_t a, uint64_t b) { return a * (b | 1); }
static uint64_t f_sub(uint64_t a, uint64_t b) { return a - (b >> 3); }
static op_fn ops[4] = {f_add, f_xor, f_mul, f_sub};

// Indirect calls and returns: leaving and entering compiled blocks
static uint64_t __attribute__((noinline)) b_call(void) {
    uint64_t acc = 1;
    for (uint32_t i = 0; i < 4000000; i++) acc = ops[(i ^ (i >> 3)) & 3](acc, i);
    return acc;
}

// Double- and single-precision multiply-add loops
static double b_fp64(void) {
    double x[256], y[256], s = 0;
    for (int i = 0; i < 256; i++) { x[i] = i * 0.5; y[i] = 1.0 / (i + 1); }
    for (int rep = 0; rep < 4000; rep++) {
        for (int i = 0; i < 256; i++) { s += x[i] * y[i]; y[i] = y[i] * 0.999 + 0.001; }
        s = s / 1.0001;
    }
    return s;
}

static float b_fp32(void) {
    float x[256], y[256], s = 0;
    for (int i = 0; i < 256; i++) { x[i] = i * 0.5f; y[i] = 1.0f / (i + 1); }
    for (int rep = 0; rep < 4000; rep++) {
        for (int i = 0; i < 256; i++) { s += x[i] * y[i]; y[i] = y[i] * 0.999f + 0.001f; }
        s = s / 1.0001f;
    }
    return s;
}

// Micro: one instruction, four at a time, 8 million times. What it costs compiled, against what
// it costs handed to the interpreter.
#define MICRO(name, insn)                                                                         \
    static uint64_t __attribute__((noinline)) name(void) {                                        \
        for (int i = 0; i < 2000000; i++)                                                        \
            __asm__ volatile(insn "\n" insn "\n" insn "\n" insn ::: "v0", "v1", "v2", "v3");    \
        return 0;                                                                                \
    }
MICRO(u_orr, "orr v0.16b, v1.16b, v2.16b")
MICRO(u_ext, "ext v0.16b, v1.16b, v2.16b, #8")
MICRO(u_cmeq0, "cmeq v0.4s, v1.4s, #0")
MICRO(u_fadd_s, "fadd s0, s1, s2")
MICRO(u_fadd_d, "fadd d0, d1, d2")
MICRO(u_fmla_2d, "fmla v0.2d, v1.2d, v2.2d")

// A context switch's cost: writing TTBR0 then touching 32 pages, against an ISB and the same
static uint64_t __attribute__((noinline)) u_ttbr(void) {
    uint64_t acc = 0;
    volatile uint64_t *b = (volatile uint64_t *)PAGED;
    for (int i = 0; i < 100000; i++) {
        SYSREG_WRITE(ttbr0_el1, l1);
        for (int p = 0; p < 32; p++) acc += b[p * 512];
    }
    return acc;
}
static uint64_t __attribute__((noinline)) u_isb(void) {
    uint64_t acc = 0;
    volatile uint64_t *b = (volatile uint64_t *)PAGED;
    for (int i = 0; i < 100000; i++) {
        __asm__ volatile("isb");
        for (int p = 0; p < 32; p++) acc += b[p * 512];
    }
    return acc;
}

// ------------------------------------------------------------------- driver

static uint64_t freq, total;

static void report(const char *name, uint64_t ticks) {
    total += ticks;
    puts_("bench ");
    puts_(name);
    puts_(" ");
    put_u64(ticks * 1000000 / freq);
    puts_("\n");
}

#define RUN(name, expr)                                    \
    do {                                                   \
        uint64_t t0 = SYSREG_READ(cntvct_el0);             \
        sink = (uint64_t)(expr);                           \
        report(name, SYSREG_READ(cntvct_el0) - t0);        \
    } while (0)

int kmain(const uint8_t *dtb) {
    (void)dtb;
    SYSREG_WRITE(vbar_el1, vectors);
    freq = SYSREG_READ(cntfrq_el0);
    mmu_on();
    RUN("int", b_int());
    RUN("mem", b_mem());
    RUN("tlb", b_tlb());
    RUN("call", b_call());
    RUN("fp64", b_fp64() * 1000);
    RUN("fp32", b_fp32() * 1000);
    __asm__ volatile("fmov s1, #1.0\n fmov s2, #2.0\n fmov d1, #1.0\n fmov d2, #2.0" ::: "v1", "v2");
    RUN("u-orr", u_orr());
    RUN("u-ext", u_ext());
    RUN("u-cmeq0", u_cmeq0());
    RUN("u-fadd-s", u_fadd_s());
    RUN("u-fadd-d", u_fadd_d());
    RUN("u-fmla-2d", u_fmla_2d());
    RUN("u-ttbr", u_ttbr());
    RUN("u-isb", u_isb());
    puts_("bench total ");
    put_u64(total * 1000000 / freq);
    puts_("\n");
    return 0;
}
