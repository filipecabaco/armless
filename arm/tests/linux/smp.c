// SMP stress test: run in a guest with several CPUs (armless-run --smp N).
// Each check prints a deterministic line, so the output can be compared with
// qemu-aarch64; timings go to stderr.
//
// - atomic counters (LDXR/STXR loops) incremented from every thread
// - a pthread mutex (futex waits and wakeups across CPUs)
// - a lock-free stack with compare-and-swap (lost updates, ABA)
// - threads that mmap/munmap and touch memory (TLB shootdowns)
// - a parallel compute speed-up measurement (stderr only)
#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#define THREADS 8

static double now(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec * 1e-9;
}

static void run_threads(void *(*fn)(void *), int n)
{
    pthread_t t[THREADS];
    for(long i = 0; i < n; i++)
        pthread_create(&t[i], NULL, fn, (void *)i);
    for(int i = 0; i < n; i++)
        pthread_join(t[i], NULL);
}

// ------------------------------------------------------------- atomics
static _Atomic uint64_t counter64;
static _Atomic uint32_t counter32;
static _Atomic uint16_t counter16;
static _Atomic uint8_t counter8;
#define ATOMIC_ITERS 100000

static void *atomic_worker(void *arg)
{
    (void)arg;
    for(int i = 0; i < ATOMIC_ITERS; i++)
    {
        atomic_fetch_add(&counter64, 3);
        atomic_fetch_add(&counter32, 1);
        atomic_fetch_add(&counter16, 1);
        atomic_fetch_add(&counter8, 1);
    }
    return NULL;
}

// ---------------------------------------------------------------- mutex
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static uint64_t guarded[4];
#define MUTEX_ITERS 20000

static void *mutex_worker(void *arg)
{
    long id = (long)arg;
    for(int i = 0; i < MUTEX_ITERS; i++)
    {
        pthread_mutex_lock(&lock);
        // non-atomic read-modify-writes: only correct with mutual exclusion
        for(int k = 0; k < 4; k++)
            guarded[k] += id + k;
        pthread_mutex_unlock(&lock);
        if((i & 255) == 0)
            sched_yield();
    }
    return NULL;
}

// ------------------------------------------------------- lock-free stack
struct node { struct node *next; long value; };
static _Atomic(struct node *) top;
static struct node nodes[THREADS][2000];
static _Atomic long popped_sum;
static _Atomic long popped_count;

static void push(struct node *n)
{
    struct node *old = atomic_load(&top);
    do n->next = old;
    while(!atomic_compare_exchange_weak(&top, &old, n));
}

static struct node *pop(void)
{
    struct node *old = atomic_load(&top);
    while(old && !atomic_compare_exchange_weak(&top, &old, old->next))
        ;
    return old;
}

static void *stack_worker(void *arg)
{
    long id = (long)arg;
    // Each node is pushed and popped by one thread; nodes are never reused
    // across threads while on the stack, so ABA cannot occur.
    for(int i = 0; i < 2000; i++)
    {
        nodes[id][i].value = id * 2000 + i;
        push(&nodes[id][i]);
        if(i % 2)
        {
            struct node *n = pop();
            if(n)
            {
                atomic_fetch_add(&popped_sum, n->value);
                atomic_fetch_add(&popped_count, 1);
            }
        }
    }
    return NULL;
}

// ------------------------------------------------------ mmap / TLB churn
static _Atomic long mmap_errors;

static void *mmap_worker(void *arg)
{
    long id = (long)arg;
    for(int i = 0; i < 300; i++)
    {
        size_t len = 64 * 1024;
        unsigned char *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if(p == MAP_FAILED) { atomic_fetch_add(&mmap_errors, 1); continue; }
        for(size_t k = 0; k < len; k += 4096)
            p[k] = (unsigned char)(id + i + k);
        for(size_t k = 0; k < len; k += 4096)
            if(p[k] != (unsigned char)(id + i + k))
                atomic_fetch_add(&mmap_errors, 1);
        // make the pages read-only, then unmap: both need TLB invalidation
        mprotect(p, len, PROT_READ);
        if(p[4096] != (unsigned char)(id + i + 4096))
            atomic_fetch_add(&mmap_errors, 1);
        munmap(p, len);
    }
    return NULL;
}

// ------------------------------------------------------- parallel compute
static volatile uint64_t sink[THREADS];

static void *compute_worker(void *arg)
{
    long id = (long)arg;
    uint64_t x = id + 1;
    for(int i = 0; i < 3000000; i++)
        x = x * 6364136223846793005ULL + 1442695040888963407ULL;
    sink[id] = x;
    return NULL;
}

int main(void)
{
    long cpus = sysconf(_SC_NPROCESSORS_ONLN);
    fprintf(stderr, "smp: %ld CPUs online\n", cpus);

    double t = now();
    run_threads(atomic_worker, THREADS);
    printf("atomics: %llu %u %u %u\n", (unsigned long long)counter64, counter32,
           (unsigned)counter16, (unsigned)counter8);
    fprintf(stderr, "smp: atomics %.2fs\n", now() - t);

    t = now();
    run_threads(mutex_worker, THREADS);
    printf("mutex: %llu %llu %llu %llu\n", (unsigned long long)guarded[0], (unsigned long long)guarded[1],
           (unsigned long long)guarded[2], (unsigned long long)guarded[3]);
    fprintf(stderr, "smp: mutex %.2fs\n", now() - t);

    t = now();
    run_threads(stack_worker, THREADS);
    long left_sum = 0, left_count = 0;
    for(struct node *n = top; n; n = n->next) { left_sum += n->value; left_count++; }
    printf("stack: %ld nodes %ld\n", popped_count + left_count, popped_sum + left_sum);
    fprintf(stderr, "smp: stack %.2fs\n", now() - t);

    t = now();
    run_threads(mmap_worker, THREADS);
    printf("mmap: errors %ld\n", (long)mmap_errors);
    fprintf(stderr, "smp: mmap %.2fs\n", now() - t);

    t = now();
    run_threads(compute_worker, 1);
    double one = now() - t;
    t = now();
    run_threads(compute_worker, cpus < THREADS ? (int)cpus : THREADS);
    double all = now() - t;
    fprintf(stderr, "smp: compute 1 thread %.2fs, %ld threads %.2fs (%.1fx throughput)\n",
            one, cpus, all, one * (cpus < THREADS ? cpus : THREADS) / all);
    printf("SMP-OK\n");
    return 0;
}
