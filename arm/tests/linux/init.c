// Minimal PID 1 for boot tests: raw syscalls, no libc, no FP/SIMD
// (built with -mgeneral-regs-only so it runs before the FP unit exists).

typedef unsigned long u64;
typedef long i64;

#define SYS_openat 56
#define SYS_close 57
#define SYS_read 63
#define SYS_write 64
#define SYS_exit 93
#define SYS_nanosleep 101
#define SYS_clock_gettime 113
#define SYS_reboot 142
#define SYS_uname 160
#define SYS_mount 40
#define SYS_mkdirat 34

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
#define sys(n, a, b, c) sys6(n, (i64)(a), (i64)(b), (i64)(c), 0, 0, 0)

static u64 slen(const char *s) {
    u64 n = 0;
    while (s[n]) n++;
    return n;
}

static void out(const char *s) { sys(SYS_write, 1, s, slen(s)); }

static void out_num(u64 v) {
    char b[24];
    int i = 23;
    b[i] = 0;
    do { b[--i] = (char)('0' + v % 10); v /= 10; } while (v);
    out(&b[i]);
}

static void cat(const char *path) {
    char buf[512];
    i64 fd = sys6(SYS_openat, -100, (i64)path, 0, 0, 0, 0);
    if (fd < 0) { out("  (cannot open "); out(path); out(")\n"); return; }
    for (;;) {
        i64 n = sys(SYS_read, fd, buf, sizeof buf);
        if (n <= 0) break;
        sys(SYS_write, 1, buf, n);
    }
    sys(SYS_close, fd, 0, 0);
}

struct utsname { char f[6][65]; };
struct timespec { i64 sec, nsec; };

void _start(void) {
    out("\n[init] hello from armless userspace (EL0)\n");

    struct utsname u;
    if (sys(SYS_uname, &u, 0, 0) == 0) {
        out("[init] uname: "); out(u.f[0]); out(" "); out(u.f[2]); out(" "); out(u.f[4]); out("\n");
    }

    sys6(SYS_mkdirat, -100, (i64)"/proc", 0555, 0, 0, 0);
    if (sys6(SYS_mount, (i64)"proc", (i64)"/proc", (i64)"proc", 0, 0, 0) == 0) {
        out("[init] /proc/cpuinfo:\n");
        cat("/proc/cpuinfo");
        out("[init] /proc/interrupts:\n");
        cat("/proc/interrupts");
    }

    struct timespec t0, t1, d = {0, 200 * 1000 * 1000};
    sys(SYS_clock_gettime, 0, &t0, 0); // CLOCK_REALTIME
    out("[init] realtime seconds: "); out_num((u64)t0.sec); out("\n");
    sys(SYS_clock_gettime, 1, &t0, 0); // CLOCK_MONOTONIC
    sys(SYS_nanosleep, &d, 0, 0);
    sys(SYS_clock_gettime, 1, &t1, 0);
    u64 ms = (u64)((t1.sec - t0.sec) * 1000 + (t1.nsec - t0.nsec) / 1000000);
    out("[init] slept "); out_num(ms); out(" ms\n");

    out("[init] ARMLESS-BOOT-OK\n");
    // LINUX_REBOOT_CMD_POWER_OFF -> PSCI SYSTEM_OFF
    sys6(SYS_reboot, 0xfee1dead, 672274793, 0x4321fedc, 0, 0, 0);
    for (;;) {}
}
