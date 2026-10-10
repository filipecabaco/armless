// Prints, for each test case, the registers whose value changed. The same code
// runs on qemu-aarch64 (reference) and on armless; the outputs are diffed.

typedef unsigned long u64;
typedef unsigned int u32;

void out(const char *s, u64 n);
volatile u64 undef_flag;

static char line[4096];
static u64 pos;

static void put(const char *s) {
    while (*s) line[pos++] = *s++;
}

static void hex(u64 v, int digits) {
    for (int i = (digits - 1) * 4; i >= 0; i -= 4) line[pos++] = "0123456789abcdef"[(v >> i) & 15];
}

static void dec(u64 v) {
    char b[24];
    int i = 23;
    b[i] = 0;
    do { b[--i] = (char)('0' + v % 10); v /= 10; } while (v);
    put(&b[i]);
}

struct state {
    u64 x[29];
    u64 nzcv, fpsr, fpcr;
    u64 q[32][2];
};

void report(u64 idx, u32 enc, const struct state *s, const struct state *init) {
    pos = 0;
    dec(idx);
    put(" ");
    hex(enc, 8);
    if (undef_flag) {
        undef_flag = 0;
        put(" UNDEF\n");
        out(line, pos);
        return;
    }
    for (int i = 0; i < 29; i++) {
        if (s->x[i] != init->x[i]) {
            put(" x");
            dec((u64)i);
            put("=");
            hex(s->x[i], 16);
        }
    }
    if (s->nzcv != init->nzcv) {
        put(" nzcv=");
        hex(s->nzcv >> 28, 1);
    }
#ifdef WITH_SIMD
    if (s->fpsr != init->fpsr) {
        put(" fpsr=");
        hex(s->fpsr, 8);
    }
    for (int i = 0; i < 32; i++) {
        if (s->q[i][0] != init->q[i][0] || s->q[i][1] != init->q[i][1]) {
            put(" v");
            dec((u64)i);
            put("=");
            hex(s->q[i][1], 16);
            hex(s->q[i][0], 16);
        }
    }
#endif
    put("\n");
    out(line, pos);
}
