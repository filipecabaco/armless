// The smallest probe: one write and exit_group, no libc.
static long sys(long n, long a, long b, long c) {
	register long x8 __asm__("x8") = n;
	register long x0 __asm__("x0") = a;
	register long x1 __asm__("x1") = b;
	register long x2 __asm__("x2") = c;
	__asm__ volatile("svc #0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2) : "memory");
	return x0;
}

void _start(void) {
	static const char msg[] = "hello: no libc\n";
	sys(64, 1, (long)msg, sizeof msg - 1);
	sys(94, 0, 0, 0);
	for (;;) {}
}
