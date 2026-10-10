// Self-checking bare-metal test suite for the armless AArch64 core.
// Prints over the 16550 UART and powers off through PSCI when done.

#include <stdint.h>
#include <stddef.h>

#define UART_BASE 0xF0100000UL
#define GICD_BASE 0xF0000000UL
#define GICC_BASE 0xF0010000UL
#define RTC_BASE  0xF0110000UL

#define REG8(a)  (*(volatile uint8_t *)(a))
#define REG32(a) (*(volatile uint32_t *)(a))

#define SYSREG_READ(name) ({ uint64_t _v; __asm__ volatile("mrs %0, " #name : "=r"(_v)); _v; })
#define SYSREG_WRITE(name, v) __asm__ volatile("msr " #name ", %0; isb" :: "r"((uint64_t)(v)) : "memory")

void fail_halt(void);
extern char vectors[];
long run_el0(long (*fn)(long), long arg, void *user_stack);
extern char el0_return[];

// ------------------------------------------------------------------- libc bits

void *memset(void *d, int c, size_t n) {
    uint8_t *p = d;
    while (n--) *p++ = (uint8_t)c;
    return d;
}

void *memcpy(void *d, const void *s, size_t n) {
    uint8_t *dp = d;
    const uint8_t *sp = s;
    while (n--) *dp++ = *sp++;
    return d;
}

int memcmp(const void *a, const void *b, size_t n) {
    const uint8_t *x = a, *y = b;
    for (; n; n--, x++, y++)
        if (*x != *y) return *x - *y;
    return 0;
}

size_t strlen(const char *s) {
    size_t n = 0;
    while (s[n]) n++;
    return n;
}

static void putc_(char c) {
    while (!(REG8(UART_BASE + 5) & 0x20)) {}
    REG8(UART_BASE) = (uint8_t)c;
}

static void puts_(const char *s) {
    while (*s) {
        if (*s == '\n') putc_('\r');
        putc_(*s++);
    }
}

static void put_u64(uint64_t v) {
    char buf[21];
    int i = 20;
    buf[i] = 0;
    do {
        buf[--i] = (char)('0' + v % 10);
        v /= 10;
    } while (v);
    puts_(&buf[i]);
}

static void put_i64(int64_t v) {
    if (v < 0) {
        putc_('-');
        put_u64((uint64_t)(-(v + 1)) + 1);
    } else {
        put_u64((uint64_t)v);
    }
}

static void put_hex(uint64_t v) {
    puts_("0x");
    for (int i = 60; i >= 0; i -= 4) putc_("0123456789abcdef"[(v >> i) & 0xf]);
}

// -------------------------------------------------------------------- harness

static int failures, passes;

static void check(const char *name, uint64_t got, uint64_t want) {
    if (got == want) {
        passes++;
        puts_("  ok   ");
        puts_(name);
        puts_("\n");
    } else {
        failures++;
        puts_("  FAIL ");
        puts_(name);
        puts_(": got ");
        put_hex(got);
        puts_(" want ");
        put_hex(want);
        puts_("\n");
    }
}

// Keep the compiler from constant-folding test inputs.
static volatile int64_t vin[8] = {20, 1000, -100, 7, 0x123456789abcdef0LL, 3, 64, 0};
#define IN(i) (vin[i])

// ---------------------------------------------------------- exception handler

struct frame {
    uint64_t x[31];
    uint64_t elr, spsr, esr, far;
};

static volatile uint64_t last_esr, last_far, last_kind, svc_count, irq_ticks, el0_svc;

static void irq(void) {
    uint32_t iar = REG32(GICC_BASE + 0x0c);
    uint32_t id = iar & 0x3ff;
    if (id == 27) {
        irq_ticks++;
        if (irq_ticks < 5)
            SYSREG_WRITE(cntv_tval_el0, SYSREG_READ(cntfrq_el0) / 1000);
        else
            SYSREG_WRITE(cntv_ctl_el0, 0);
    }
    if (id != 1023) REG32(GICC_BASE + 0x10) = iar;
}

void exc_handler(struct frame *f, uint64_t kind) {
    last_kind = kind;
    if ((kind & 3) == 1) { // IRQ
        irq();
        return;
    }
    if ((kind & 3) != 0) {
        puts_("unexpected FIQ/SError\n");
        fail_halt();
    }
    uint64_t ec = f->esr >> 26;
    if (ec == 0x15 && (f->esr & 0xffff) == 0x99 && kind == 8) {
        // EL0 function finished: return to run_el0's caller at EL1h.
        f->elr = (uint64_t)el0_return;
        f->spsr = 0x3c5;
        return;
    }
    last_esr = f->esr;
    last_far = f->far;
    if (ec == 0x15) { // SVC
        uint32_t imm = f->esr & 0xffff;
        if (kind == 8) el0_svc++;
        svc_count++;
        f->x[0] = f->x[0] * 2 + imm; // result visible to the caller
        return;                       // ELR already points past the SVC
    }
    // Faults and undefined instructions: skip the faulting instruction.
    f->elr += 4;
}

// ------------------------------------------------------------------ tests

__attribute__((noinline)) static uint64_t fib(uint64_t n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }

static uint64_t sum_primes(int limit) {
    static uint8_t sieve[1001];
    uint64_t sum = 0;
    for (int i = 2; i <= limit; i++) {
        if (sieve[i]) continue;
        sum += (uint64_t)i;
        for (int j = i * i; j <= limit; j += i) sieve[j] = 1;
    }
    return sum;
}

static uint32_t crc32(const char *s) {
    uint32_t crc = 0xffffffff;
    while (*s) {
        crc ^= (uint8_t)*s++;
        for (int k = 0; k < 8; k++) crc = (crc >> 1) ^ (0xedb88320 & -(crc & 1));
    }
    return ~crc;
}

static void sort(int64_t *a, int n) {
    for (int i = 1; i < n; i++) {
        int64_t v = a[i];
        int j = i - 1;
        while (j >= 0 && a[j] > v) { a[j + 1] = a[j]; j--; }
        a[j + 1] = v;
    }
}

struct node { struct node *next; uint32_t val; uint16_t small; int8_t tiny; };

static void test_integer(void) {
    puts_("integer:\n");
    check("fib(20)", fib((uint64_t)IN(0)), 6765);
    check("sum of primes <= 1000", sum_primes((int)IN(1)), 76127);
    check("crc32", crc32("hello world"), 0x0d4a1185);
    check("sdiv", (uint64_t)(IN(2) / IN(3)), (uint64_t)-14);
    check("srem", (uint64_t)(IN(2) % IN(3)), (uint64_t)-2);
    uint64_t q;
    __asm__ volatile("udiv %0, %1, %2" : "=r"(q) : "r"((uint64_t)IN(1)), "r"((uint64_t)IN(7)));
    check("udiv by zero is 0", q, 0);
    check("udiv 32", (uint32_t)0xfffffff0u / (uint32_t)IN(5), 0x55555550);
    check("umulh", (uint64_t)(((unsigned __int128)(uint64_t)IN(4) * 0xfedcba9876543210ULL) >> 64),
          0x121fa00ad77d7422ULL);
    check("smulh", (uint64_t)(((__int128)IN(2) * (__int128)IN(4)) >> 64), (uint64_t)-8);
    check("clz", (uint64_t)__builtin_clzll((uint64_t)IN(6)), 57);
    check("ctz (rbit+clz)", (uint64_t)__builtin_ctzll((uint64_t)IN(4)), 4);
    check("bswap64", __builtin_bswap64((uint64_t)IN(4)), 0xf0debc9a78563412ULL);
    check("bswap16", __builtin_bswap16((uint16_t)IN(4)), 0xf0de);
    check("popcount", (uint64_t)__builtin_popcountll((uint64_t)IN(4)), 32);
    check("ror", ((uint64_t)IN(4) >> 12) | ((uint64_t)IN(4) << 52), 0xef0123456789abcdULL);
    check("sign extend byte", (uint64_t)(int64_t)(int8_t)IN(4), (uint64_t)-16);
    check("bitfield extract", ((uint64_t)IN(4) >> 20) & 0xfff, 0x9ab);
    uint64_t v = (uint64_t)IN(4);
    v = (v & ~0xff00ULL) | (((uint64_t)IN(3) & 0xff) << 8);
    check("bitfield insert", v, 0x123456789abc07f0ULL);
    int64_t arr[8] = {5, -3, 99, 0, 42, -100, 7, 7};
    sort(arr, 8);
    check("sort min", (uint64_t)arr[0], (uint64_t)-100);
    check("sort max", (uint64_t)arr[7], 99);
    check("sort mid", (uint64_t)arr[4], 7);

    static struct node nodes[4];
    for (int i = 0; i < 4; i++) {
        nodes[i].next = i < 3 ? &nodes[i + 1] : 0;
        nodes[i].val = (uint32_t)(i * 1000 + 1);
        nodes[i].small = (uint16_t)(0xfff0 + i);
        nodes[i].tiny = (int8_t)(-i);
    }
    uint64_t s = 0;
    int64_t t = 0;
    for (struct node *n = &nodes[0]; n; n = n->next) { s += n->val + n->small; t += n->tiny; }
    check("linked list", s, 6004 + 4 * 0xfff0 + 6);
    check("signed byte sum", (uint64_t)t, (uint64_t)-6);

    // 64-bit add with carry chain via __int128
    unsigned __int128 big = ((unsigned __int128)0xffffffffffffffffULL << 64) | 0xffffffffffffffffULL;
    big += (uint64_t)IN(5) - 2;
    check("adc chain", (uint64_t)big, 0);
}

static void test_atomics(void) {
    puts_("atomics:\n");
    static volatile uint64_t counter;
    for (int i = 0; i < 100; i++) __atomic_fetch_add(&counter, 3, __ATOMIC_SEQ_CST);
    check("ldxr/stxr add", counter, 300);
    uint64_t expected = 300;
    int ok = __atomic_compare_exchange_n(&counter, &expected, 7, 0, __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST);
    check("cmpxchg", (uint64_t)ok * 1000 + counter, 1007);
    uint32_t w = 5;
    check("xchg 32", __atomic_exchange_n(&w, 9, __ATOMIC_ACQ_REL) * 10 + w, 59);
}

static void test_exceptions(void) {
    puts_("exceptions:\n");
    SYSREG_WRITE(vbar_el1, vectors);
    register uint64_t x0 __asm__("x0") = 10;
    __asm__ volatile("svc #5" : "+r"(x0) :: "memory");
    check("svc round trip", x0, 25);
    check("svc esr", last_esr >> 26, 0x15);
    check("svc from current EL uses SPx vector", last_kind, 4);
    __asm__ volatile(".inst 0x00000000" ::: "memory"); // permanently undefined
    check("undefined -> EC 0", last_esr >> 26, 0);
    __asm__ volatile("brk #0x42" ::: "memory");
    check("brk esr", last_esr, (0x3cULL << 26) | (1 << 25) | 0x42);
    check("CurrentEL", SYSREG_READ(CurrentEL), 4);
}

static long user_fn(long arg) {
    register long x0 __asm__("x0") = arg;
    __asm__ volatile("svc #1" : "+r"(x0) :: "memory");
    return x0 + 1;
}

static long user_reads_el(long arg) {
    (void)arg;
    // Reading CurrentEL at EL0 is undefined; the handler skips it.
    uint64_t v = 0x55;
    __asm__ volatile("mrs %0, CurrentEL" : "+r"(v));
    return (long)v;
}

static uint8_t user_stack[4096] __attribute__((aligned(16)));

static void test_el0(void) {
    puts_("EL0:\n");
    el0_svc = 0;
    long r = run_el0(user_fn, 20, user_stack + sizeof user_stack);
    check("svc from EL0", (uint64_t)r, 20 * 2 + 1 + 1);
    check("EL0 svc counted", el0_svc, 1);
    check("back at EL1", SYSREG_READ(CurrentEL), 4);
    r = run_el0(user_reads_el, 0, user_stack + sizeof user_stack);
    check("EL0 mrs CurrentEL traps", (uint64_t)r, 0x55);
    check("trap came from lower EL", last_kind, 8);
}

// ---------------------------------------------------------------------- MMU

static uint64_t l1_lo[512] __attribute__((aligned(4096)));
static uint64_t l2_lo[512] __attribute__((aligned(4096)));
static uint64_t l3_test[512] __attribute__((aligned(4096)));
static uint64_t l1_hi[512] __attribute__((aligned(4096)));

#define D_VALID  1ULL
#define D_TABLE  3ULL
#define D_PAGE   3ULL
#define D_AF     (1ULL << 10)
#define D_SH     (3ULL << 8)
#define D_ATTR(i) ((uint64_t)(i) << 2)
#define D_AP_EL0 (1ULL << 6)
#define D_AP_RO  (2ULL << 6)
#define D_UXN    (1ULL << 54)

#define TEST_REGION 0x19000000UL // 2 MiB region remapped with 4K pages

static void test_mmu(void) {
    puts_("MMU:\n");
    // TTBR0: 4 GiB of VA, level 1 start. GiB 0 via 2 MiB blocks, GiB 3 = devices.
    l1_lo[0] = (uint64_t)l2_lo | D_TABLE;
    l1_lo[3] = 0xC0000000ULL | D_VALID | D_AF | D_ATTR(0) | D_UXN;
    for (uint64_t i = 0; i < 256; i++) // 512 MiB of RAM
        l2_lo[i] = (i << 21) | D_VALID | D_AF | D_SH | D_ATTR(1);
    l2_lo[TEST_REGION >> 21] = (uint64_t)l3_test | D_TABLE;
    l3_test[0] = TEST_REGION | D_PAGE | D_AF | D_SH | D_ATTR(1) | D_AP_RO;            // read-only
    l3_test[1] = (TEST_REGION + 0x1000) | D_PAGE | D_AF | D_SH | D_ATTR(1);           // RW, EL1 only
    // l3_test[2] invalid
    l3_test[3] = (TEST_REGION + 0x3000) | D_PAGE | D_SH | D_ATTR(1);                  // AF clear
    l3_test[4] = (TEST_REGION + 0x1000) | D_PAGE | D_AF | D_SH | D_ATTR(1) | D_AP_EL0; // alias, EL0 RW
    // TTBR1: top 4 GiB, GiB 0 aliases physical 0..1 GiB
    l1_hi[0] = 0x0ULL | D_VALID | D_AF | D_SH | D_ATTR(1);

    *(volatile uint64_t *)(TEST_REGION + 0x1000) = 0x1111;

    SYSREG_WRITE(mair_el1, 0xff00); // attr0 device-nGnRnE, attr1 normal WB
    uint64_t tcr = 32               // T0SZ: 4 GiB
                 | (32ULL << 16)    // T1SZ
                 | (2ULL << 30)     // TG1 = 4K
                 | (3ULL << 12) | (1ULL << 10) | (1ULL << 8)
                 | (3ULL << 28) | (1ULL << 26) | (1ULL << 24);
    SYSREG_WRITE(tcr_el1, tcr);
    SYSREG_WRITE(ttbr0_el1, l1_lo);
    SYSREG_WRITE(ttbr1_el1, l1_hi);
    __asm__ volatile("dsb sy; tlbi vmalle1; dsb sy; isb");
    SYSREG_WRITE(sctlr_el1, SYSREG_READ(sctlr_el1) | 1 | (1 << 2) | (1 << 12));
    check("MMU on, still running", SYSREG_READ(sctlr_el1) & 1, 1);

    volatile uint64_t *ro = (volatile uint64_t *)TEST_REGION;
    volatile uint64_t *rw = (volatile uint64_t *)(TEST_REGION + 0x1000);
    check("RW page read", *rw, 0x1111);
    *rw = 0x2222;
    check("RW page write", *rw, 0x2222);
    last_esr = 0;
    *ro = 5;
    check("RO write faults (EC)", last_esr >> 26, 0x25);
    check("RO write faults (WnR, perm L3)", last_esr & 0x7f, 0x40 | 0x0f);
    check("RO write FAR", last_far, TEST_REGION);
    last_esr = 0;
    (void)*(volatile uint64_t *)(TEST_REGION + 0x2000);
    check("unmapped read: translation fault L3", last_esr & 0x7f, 0x07);
    last_esr = 0;
    (void)*(volatile uint64_t *)(TEST_REGION + 0x3000);
    check("AF clear: access flag fault L3", last_esr & 0x7f, 0x0b);

    // High alias through TTBR1 sees the same physical memory.
    volatile uint64_t *alias = (volatile uint64_t *)(0xFFFFFFFF00000000ULL + TEST_REGION + 0x1000);
    check("TTBR1 alias read", *alias, 0x2222);
    *alias = 0x3333;
    check("TTBR1 alias write", *rw, 0x3333);

    // AT S1E1R/W
    __asm__ volatile("at s1e1r, %0; isb" :: "r"(TEST_REGION + 0x1008));
    check("AT S1E1R pa", SYSREG_READ(par_el1) & ~0xfffULL, TEST_REGION + 0x1000);
    __asm__ volatile("at s1e1w, %0; isb" :: "r"(TEST_REGION));
    check("AT S1E1W on RO page fails", SYSREG_READ(par_el1) & 1, 1);

    // Top-byte-ignore for TTBR0 data accesses.
    SYSREG_WRITE(tcr_el1, tcr | (1ULL << 37));
    volatile uint64_t *tagged = (volatile uint64_t *)((0x5aULL << 56) | (TEST_REGION + 0x1000));
    check("TBI0 tagged pointer", *tagged, 0x3333);
    SYSREG_WRITE(tcr_el1, tcr);

    // DC ZVA zeroes a 64-byte block.
    static uint64_t zblock[16] __attribute__((aligned(64)));
    for (int i = 0; i < 16; i++) zblock[i] = 0xaa;
    __asm__ volatile("dc zva, %0" :: "r"(&zblock[8]) : "memory");
    check("DC ZVA", zblock[7] + zblock[8] + zblock[15], 0xaa);
}

static long user_read(long addr) {
    return (long)*(volatile uint64_t *)addr;
}

static void test_mmu_el0(void) {
    puts_("MMU EL0:\n");
    // The EL0 stack and code live in EL1-only pages: give EL0 a stack in the
    // EL0-accessible alias page (l3_test[4]) and run code through the UXN=0
    // EL1 mapping (execute-only for EL0).
    void *stack = (void *)(TEST_REGION + 0x4000 + 0x1000);
    last_esr = 0;
    long r = run_el0(user_read, (long)(TEST_REGION + 0x4000), stack);
    check("EL0 reads EL0 page", (uint64_t)r, 0x3333);
    last_esr = 0;
    r = run_el0(user_read, (long)(TEST_REGION + 0x1000), stack);
    check("EL0 read of EL1 page faults", last_esr >> 26, 0x24);
    check("  as permission fault L3", last_esr & 0x3f, 0x0f);
}

// --------------------------------------------------------------- GIC + timer

static void test_timer(void) {
    puts_("GIC + generic timer:\n");
    check("CNTFRQ", SYSREG_READ(cntfrq_el0), 62500000);
    uint64_t t0 = SYSREG_READ(cntvct_el0);
    for (volatile int i = 0; i < 20000; i++) {}
    check("counter advances", SYSREG_READ(cntvct_el0) > t0, 1);

    REG32(GICD_BASE + 0x000) = 1;
    REG32(GICC_BASE + 0x004) = 0xff;
    REG32(GICC_BASE + 0x000) = 1;
    REG32(GICD_BASE + 0x100) = 1u << 27; // enable PPI 27 (virtual timer)
    check("GICD_TYPER lines", REG32(GICD_BASE + 0x004) & 0x1f, 3);

    irq_ticks = 0;
    SYSREG_WRITE(cntv_tval_el0, SYSREG_READ(cntfrq_el0) / 1000);
    SYSREG_WRITE(cntv_ctl_el0, 1);
    __asm__ volatile("msr daifclr, #2");
    for (int spins = 0; irq_ticks < 5 && spins < 1000; spins++) __asm__ volatile("wfi");
    __asm__ volatile("msr daifset, #2");
    check("5 timer interrupts", irq_ticks, 5);
    check("IRQ vector (current EL, SPx)", last_kind, 5);
    check("nothing left pending", REG32(GICC_BASE + 0x18), 1023);
}

static void test_rtc(void) {
    puts_("RTC:\n");
    uint32_t now = REG32(RTC_BASE);
    check("PL031 time after 2024", now > 1704067200u, 1);
    check("PL031 PrimeCell id", REG32(RTC_BASE + 0xff0) | REG32(RTC_BASE + 0xff4) << 8, 0xf00d);
}

static void test_psci(void) {
    puts_("PSCI:\n");
    register uint64_t x0 __asm__("x0") = 0x84000000;
    __asm__ volatile("hvc #0" : "+r"(x0) :: "memory");
    check("PSCI_VERSION", x0, 0x10000);
}

static int check_dtb(const uint8_t *dtb) {
    uint32_t magic = (uint32_t)dtb[0] << 24 | (uint32_t)dtb[1] << 16 | (uint32_t)dtb[2] << 8 | dtb[3];
    return magic == 0xd00dfeed;
}

int kmain(const uint8_t *dtb) {
    puts_("armless bare-metal tests\n");
    check("DTB in x0", (uint64_t)check_dtb(dtb), 1);
    test_integer();
    test_atomics();
    test_exceptions();
    test_el0();
    test_psci();
    test_rtc();
    test_timer();
    test_mmu();
    test_mmu_el0();
    puts_("passed ");
    put_u64((uint64_t)passes);
    puts_(", failed ");
    put_i64(failures);
    puts_("\n");
    if (failures) fail_halt();
    return 0;
}
