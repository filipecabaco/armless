// PID 1 for the free page reporting test (tests/linux/free-pages.sh): writes 200 MiB, frees it,
// waits long enough for the kernel to report it (page reporting runs 2 s after pages free up, and
// 6.18 reports a free list in passes 2 s apart: 8 s left 145 MiB of 200 reported on CI), then
// says so and idles, for armless-run --save-after to snapshot the machine. Raw syscalls, no libc.

typedef unsigned long u64;
typedef long i64;

static i64 sys6(i64 n, i64 a, i64 b, i64 c, i64 d, i64 e, i64 f) {
    register i64 x8 __asm__("x8") = n;
    register i64 x0 __asm__("x0") = a;
    register i64 x1 __asm__("x1") = b;
    register i64 x2 __asm__("x2") = c;
    register i64 x3 __asm__("x3") = d;
    register i64 x4 __asm__("x4") = e;
    register i64 x5 __asm__("x5") = f;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2), "r"(x3), "r"(x4), "r"(x5) : "memory");
    return x0;
}

static void out(const char *s) {
    u64 n = 0;
    while (s[n]) n++;
    sys6(64, 1, (i64)s, n, 0, 0, 0);
}

static void nap(i64 secs) {
    i64 ts[2] = {secs, 0};
    sys6(101, (i64)ts, 0, 0, 0, 0, 0);
}

void _start(void) {
    const u64 len = 200ul << 20;
    char *p = (char *)sys6(222, 0, len, 3, 0x22, -1, 0); // mmap(RW, PRIVATE | ANONYMOUS)
    if ((i64)p < 0) {
        out("free-pages: mmap failed\n");
        for (;;) nap(60);
    }
    for (u64 i = 0; i < len; i += 4096) p[i] = (char)((i >> 12) | 1);
    sys6(215, (i64)p, len, 0, 0, 0, 0); // munmap
    out("free-pages: wrote and freed 200 MiB\n");
    nap(30);
    out("ARMLESS-FREED-AND-WAITED\n");
    for (;;) nap(60);
}
