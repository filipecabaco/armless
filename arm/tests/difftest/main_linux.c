// Reference build: runs under qemu-aarch64 (user mode). Undefined instructions
// raise SIGILL; the handler flags them and skips the instruction.

typedef unsigned long u64;

extern volatile u64 undef_flag;
void run_cases(void);

static long sys(long n, long a, long b, long c, long d) {
    register long x8 __asm__("x8") = n;
    register long x0 __asm__("x0") = a;
    register long x1 __asm__("x1") = b;
    register long x2 __asm__("x2") = c;
    register long x3 __asm__("x3") = d;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2), "r"(x3) : "memory");
    return x0;
}

void out(const char *s, u64 n) { sys(64, 1, (long)s, (long)n, 0); }

// struct ucontext (arm64): uc_mcontext.pc lives at byte offset 440.
#define UC_PC_OFFSET 440

static void on_sigill(int sig, void *info, void *uc) {
    (void)info;
    undef_flag = sig == 4 ? 1 : 2;
    *(u64 *)((char *)uc + UC_PC_OFFSET) += 4;
}

void sig_restorer(void);
__asm__(".text\n.global sig_restorer\nsig_restorer:\n mov x8, #139\n svc #0\n");

struct k_sigaction {
    void (*handler)(int, void *, void *);
    u64 flags;
    void (*restorer)(void);
    u64 mask;
};

void _start(void) {
    struct k_sigaction sa = {on_sigill, 4 /* SA_SIGINFO */ | 0x04000000 /* SA_RESTORER */ | 0x40000000 /* SA_NODEFER */,
                             sig_restorer, 0};
    sys(134, 4 /* SIGILL */, (long)&sa, 0, 8);
    sys(134, 7 /* SIGBUS */, (long)&sa, 0, 8);
    sys(134, 11 /* SIGSEGV */, (long)&sa, 0, 8);
    run_cases();
    sys(93, 0, 0, 0, 0);
}
