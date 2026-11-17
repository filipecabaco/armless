// The machine-mode counterpart of user/benches/syscalls.rs: a static /init (no libc) that times
// getppid, a 64-byte read of /dev/zero, a 1-byte pipe write and read, and fork + exit + wait in a
// Linux guest, prints "bench NAME N ns" for each and powers off. Built and run by
// bench/machine-syscalls.sh.
typedef unsigned long u64;
static long sys6(long n, long a, long b, long c, long d, long e, long f) {
    register long x8 __asm__("x8") = n, x0 __asm__("x0") = a, x1 __asm__("x1") = b, x2 __asm__("x2") = c,
                  x3 __asm__("x3") = d, x4 __asm__("x4") = e, x5 __asm__("x5") = f;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2), "r"(x3), "r"(x4), "r"(x5) : "memory");
    return x0;
}
#define sys(n, ...) sys6(n, ##__VA_ARGS__, 0, 0, 0, 0, 0, 0)
#define S0(n) sys6(n, 0, 0, 0, 0, 0, 0)
static void say(const char *s) { long n = 0; while (s[n]) n++; sys6(64, 1, (long)s, n, 0, 0, 0); }
static void num(u64 v) { char b[24]; int i = 23; b[i] = 0; do { b[--i] = '0' + v % 10; v /= 10; } while (v); say(b + i); }
static u64 now(void) { long ts[2]; sys6(113, 1, (long)ts, 0, 0, 0, 0); return (u64)ts[0] * 1000000000ul + ts[1]; }
static char buf[64];
static int pfd[2];
static long zfd;
static void op_getppid(void) { sys6(173, 0, 0, 0, 0, 0, 0); }
static void op_read(void) { sys6(63, zfd, (long)buf, 64, 0, 0, 0); }
static void op_pipe(void) { sys6(64, pfd[1], (long)buf, 1, 0, 0, 0); sys6(63, pfd[0], (long)buf, 1, 0, 0, 0); }
static void op_fork(void) {
    long pid = sys6(220, 17, 0, 0, 0, 0, 0); /* clone(SIGCHLD): fork */
    if (pid == 0) sys6(93, 0, 0, 0, 0, 0, 0);
    sys6(260, -1, 0, 0, 0, 0, 0);
}
static void bench(const char *name, void (*f)(void), u64 n) {
    u64 best = ~0ul;
    for (int r = 0; r < 3; r++) {
        u64 t = now();
        for (u64 i = 0; i < n; i++) f();
        u64 d = now() - t;
        if (d < best) best = d;
    }
    say("bench "); say(name); say(" "); num(best / n); say(" ns\n");
}
void _start(void) {
    zfd = sys6(56, -100, (long)"/dev/zero", 0, 0, 0, 0);
    sys6(59, (long)pfd, 0, 0, 0, 0, 0);
    say("bench start\n");
    bench("getppid", op_getppid, 100000);
    bench("read-64", op_read, 100000);
    bench("pipe-1", op_pipe, 50000);
    bench("fork-exit-wait", op_fork, 1000);
    say("bench done\n");
    sys6(142, 0xfee1dead, 0x28121969, 0x4321fedc, 0, 0, 0); /* reboot(POWER_OFF) */
    for (;;) S0(124);
}
