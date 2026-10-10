// Userspace smoke test for armless: glibc (static) exercising FP/SIMD
// (printf, libm, SIMD string functions), threads, signals, fork/exec-free
// process features and timers. Its output must be identical under
// qemu-aarch64 and under Linux on armless.

#include <math.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static uint64_t fnv(const void *p, size_t n) {
    const unsigned char *b = p;
    uint64_t h = 0xcbf29ce484222325ULL;
    while (n--) h = (h ^ *b++) * 0x100000001b3ULL;
    return h;
}

static int cmp_double(const void *a, const void *b) {
    double x = *(const double *)a, y = *(const double *)b;
    return (x > y) - (x < y);
}

static void test_fp(void) {
    printf("fp: %.17g %.17g %.17g\n", sin(1.0), exp(2.5), log(10.0));
    printf("fp: %.9g %.9g %.9g\n", sqrtf(2.0f), powf(1.5f, 7.25f), atan2f(-1.0f, 3.0f));
    printf("fp: %a %a %e\n", 0.1, 1.0 / 3.0, 6.02214076e23);
    printf("fp: %g %g %g %g\n", fmod(10.5, 3.0), round(-2.5), rint(2.5), nearbyint(-3.5));
    printf("fp: %lld %lld %u\n", llround(1e15 + 0.5), (long long)(-7.9), (unsigned)(3e9));
    double acc = 0;
    for (int i = 1; i <= 100000; i++) acc += 1.0 / ((double)i * i);
    printf("fp: basel %.15f\n", acc);
    float v[64];
    for (int i = 0; i < 64; i++) v[i] = sinf((float)i * 0.37f) * 100.0f;
    float dot = 0;
    for (int i = 0; i < 64; i++) dot += v[i] * v[63 - i];
    printf("fp: dot %.6f\n", dot);
    double arr[1000];
    srand(42);
    for (int i = 0; i < 1000; i++) arr[i] = (double)rand() / RAND_MAX - 0.5;
    qsort(arr, 1000, sizeof arr[0], cmp_double);
    printf("fp: sorted %.12f %.12f %.12f\n", arr[0], arr[500], arr[999]);
    printf("fp: strtod %.17g %.17g\n", strtod("3.14159265358979323846", 0), strtod("1e-310", 0));
}

static void test_strings(void) {
    static char a[70000], b[70000];
    for (size_t i = 0; i < sizeof a; i++) a[i] = (char)('A' + (i * 31 + i / 7) % 26);
    a[sizeof a - 1] = 0;
    memcpy(b, a, sizeof a);
    printf("str: memcpy %d len %zu\n", memcmp(a, b, sizeof a), strlen(b));
    memmove(b + 3, b, 50000);
    printf("str: memmove %016llx\n", (unsigned long long)fnv(b, sizeof b));
    memset(b + 100, 'z', 12345);
    printf("str: memset %016llx\n", (unsigned long long)fnv(b, sizeof b));
    printf("str: strchr %td strrchr %td\n", strchr(a, 'Q') - a, strrchr(a, 'Q') - a);
    char *p = strstr(a, "XCH");
    printf("str: strstr %td\n", p ? p - a : -1);
    char buf[256];
    snprintf(buf, sizeof buf, "%08x|%-6s|%+.3e|%c%c", 0xbeef, "ab", -1234.5678, 'o', 'k');
    printf("str: %s\n", buf);
}

static long counter;
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;

static void *worker(void *arg) {
    long n = (long)arg;
    for (long i = 0; i < n; i++) {
        pthread_mutex_lock(&mu);
        counter++;
        pthread_mutex_unlock(&mu);
    }
    double x = 0;
    for (int i = 0; i < 1000; i++) x += sqrt((double)i);
    return (void *)(long)x;
}

static void test_threads(void) {
    pthread_t t[4];
    for (int i = 0; i < 4; i++) pthread_create(&t[i], 0, worker, (void *)20000L);
    long sum = 0;
    for (int i = 0; i < 4; i++) {
        void *r;
        pthread_join(t[i], &r);
        sum += (long)r;
    }
    printf("threads: counter %ld results %ld\n", counter, sum);
}

static jmp_buf jb;
static volatile int got_signal;

static void on_usr1(int sig) {
    got_signal = sig;
    // FP in a signal handler must not corrupt the interrupted context.
    volatile double d = 1.25;
    d *= 3.0;
    (void)d;
}

static void on_segv(int sig) {
    (void)sig;
    longjmp(jb, 1);
}

static void test_signals(void) {
    signal(SIGUSR1, on_usr1);
    double before = 2.0 / 3.0;
    raise(SIGUSR1);
    double after = 2.0 / 3.0;
    printf("signals: usr1 %d fp-preserved %d\n", got_signal, before == after);
    signal(SIGSEGV, on_segv);
    if (setjmp(jb) == 0) {
        *(volatile int *)8 = 1;
        printf("signals: no fault?\n");
    } else {
        printf("signals: caught SIGSEGV\n");
    }
}

static void test_process(void) {
    pid_t pid = fork();
    if (pid == 0) {
        _exit(7);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    printf("process: child exit %d\n", WIFEXITED(status) ? WEXITSTATUS(status) : -1);
    struct timespec t0, t1, d = {0, 50 * 1000 * 1000};
    clock_gettime(CLOCK_MONOTONIC, &t0);
    nanosleep(&d, 0);
    clock_gettime(CLOCK_MONOTONIC, &t1);
    long ms = (t1.tv_sec - t0.tv_sec) * 1000 + (t1.tv_nsec - t0.tv_nsec) / 1000000;
    printf("process: slept >= 50ms %d\n", ms >= 50 && ms < 2000);
    void *big = malloc(64 << 20);
    memset(big, 1, 64 << 20);
    printf("process: 64MiB malloc ok %d\n", ((char *)big)[(64 << 20) - 1] == 1);
    free(big);
}

int main(void) {
    setvbuf(stdout, 0, _IONBF, 0);
    test_fp();
    test_strings();
    test_threads();
    test_signals();
    test_process();
    printf("USERSPACE-DONE\n");
    return 0;
}
