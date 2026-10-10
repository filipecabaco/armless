#!/usr/bin/env python3
"""Generate random A64 test cases for the differential tester.

usage: gen.py --classes int[,simd,...] --count N --seed S > cases.S

Every case loads a random register state, executes one random instruction
from the selected encoding classes and reports which registers changed.
Register fields are limited to x0..x28 (x29 = harness pointer, x30 = link,
31 = SP in some forms), with 31 allowed where it unambiguously means XZR.
"""
import argparse
import random
import struct
import sys

# name: (fixed_mask, fixed_value, reg_field_offsets, zr_ok, extra_rules)
# extra_rules: list of (mask, value, probability) applied after random fill.
INT = {
    "addsub_imm": (0x1F800000, 0x11000000, [0, 5], False, []),
    "logic_imm": (0x1F800000, 0x12000000, [0, 5], False, []),
    "movewide": (0x1F800000, 0x12800000, [0], True, []),
    "bitfield": (0x1F800000, 0x13000000, [0, 5], True, [(1 << 22, 0, 0.0)]),
    "extract": (0x1F800000, 0x13800000, [0, 5, 16], True, []),
    "logic_shift": (0x1F000000, 0x0A000000, [0, 5, 16], True, []),
    "addsub_shift": (0x1F200000, 0x0B000000, [0, 5, 16], True, []),
    "addsub_ext": (0x1F200000, 0x0B200000, [0, 5, 16], False, [(0x00C00000, 0, 0.9)]),
    "adc": (0x1FE00000, 0x1A000000, [0, 5, 16], True, [(0xFC00, 0, 0.9)]),
    "condcmp": (0x1FE00000, 0x1A400000, [5, 16], True, [(0x20000410, 0x20000000, 0.9)]),
    "condsel": (0x1FE00000, 0x1A800000, [0, 5, 16], True, [(0x20000800, 0, 0.9)]),
    "dp2": (0x5FE00000, 0x1AC00000, [0, 5, 16], True, [(0x20000000, 0, 0.9)]),
    "dp1": (0x5FE00000, 0x5AC00000, [0, 5], True, [(0x201F0000 | 0xF000, 0, 0.9)]),
    "dp3": (0x1F000000, 0x1B000000, [0, 5, 16, 10], True, [(0x60000000, 0, 0.9)]),
}

# Scalar floating point. General-register fields are limited (they may be
# Wn/Xn in conversions); V-register fields are left random.
FP = {
    "dp1": (0xFF207C00, 0x1E204000, [], False, []),
    "dp2": (0xFF200C00, 0x1E200800, [], False, []),
    "dp3": (0xFF000000, 0x1F000000, [], False, []),
    "cmp": (0xFF203C00, 0x1E202000, [], False, [(0xC007, 0, 0.9)]),
    "ccmp": (0xFF200C00, 0x1E200400, [], False, []),
    "csel": (0xFF200C00, 0x1E200C00, [], False, []),
    "imm": (0xFF201C00, 0x1E201000, [], False, [(0x3E0, 0, 0.9)]),
    "int": (0x7F20FC00, 0x1E200000, [0, 5], False, []),
    "fixed": (0x7F200000, 0x1E000000, [0, 5], False, []),
}

# Advanced SIMD, vector forms
SIMD = {
    "same": (0x9F200400, 0x0E200400, [], False, []),
    "diff": (0x9F200C00, 0x0E200000, [], False, []),
    "misc": (0x9F3E0C00, 0x0E200800, [], False, []),
    "across": (0x9F3E0C00, 0x0E300800, [], False, []),
    "copy": (0x9FE08400, 0x0E000400, [0, 5], False, []),
    "modimm": (0x9FF80400, 0x0F000400, [], False, [(0x800, 0, 0.9)]),
    "shift": (0x9F800400, 0x0F000400, [], False, []),
    "indexed": (0x9F000400, 0x0F000000, [], False, []),
    "permute": (0xBF208C00, 0x0E000800, [], False, []),
    "ext": (0xBF208400, 0x2E000000, [], False, []),
    "tbl": (0xBF208C00, 0x0E000000, [], False, []),
}

# Advanced SIMD, scalar forms
SIMDS = {
    "same": (0xDF200400, 0x5E200400, [], False, []),
    "diff": (0xDF200C00, 0x5E200000, [], False, []),
    "misc": (0xDF3E0C00, 0x5E200800, [], False, []),
    "pair": (0xDF3E0C00, 0x5E300800, [], False, []),
    "copy": (0xDFE08400, 0x5E000400, [], False, []),
    "shift": (0xDF800400, 0x5F000400, [], False, []),
    "indexed": (0xDF000400, 0x5F000000, [], False, []),
}

# Loads and stores. The base register is always x28 (pointing into a scratch
# buffer), register offsets use x27 (a small value), and transfer registers
# are limited to x0..x26 so the base is never overwritten. Immediate offsets
# are kept small so every access stays inside the buffer.
SMALL_IMM12 = (0x003C0000, 0, 1.0)
LDST = {
    "v_uimm": (0x3F000000, 0x3D000000, [0], False, [SMALL_IMM12]),
    "v_imm9": (0x3F200000, 0x3C000000, [], False, []),
    "v_reg": (0x3F200C00, 0x3C200800, [], False, []),
    "v_pair": (0x3E000000, 0x2C000000, [], False, []),
    "v_multi": (0xBFBF0000, 0x0C000000, [], False, []),
    "v_multi_post": (0xBFA00000, 0x0C800000, [], False, []),
    "v_single": (0xBF9F0000, 0x0D000000, [], False, []),
    "v_single_post": (0xBF800000, 0x0D800000, [], False, []),
    "x_uimm": (0x3F000000, 0x39000000, [0], False, [SMALL_IMM12]),
    "x_imm9": (0x3F200000, 0x38000000, [0], False, []),
    "x_reg": (0x3F200C00, 0x38200800, [0], False, []),
    "x_pair": (0x3E000000, 0x28000000, [0, 10], False, []),
    "x_excl": (0x3F000000, 0x08000000, [0, 10, 16], False, [(0x00007C00, 0x00007C00, 0.5)]),
}
LDST_BASE = 5      # Rn field offset: forced to x28
LDST_INDEX = 16    # Rm field offset for register-offset and post-index forms

CLASSES = {"int": INT, "fp": FP, "simd": SIMD, "simds": SIMDS, "ldst": LDST}

# Encodings QEMU's cortex-a53 implements but armless does not advertise.
def excluded(enc):
    # CRC32/CRC32C: data-processing (2 source), opcode 010xxx
    if enc & 0x5FE00000 == 0x1AC00000 and (enc >> 13) & 7 == 0b010:
        return True
    # Crypto: AES, SHA1/SHA256 (2- and 3-register), PMULL with 64-bit elements
    if enc & 0xFF3E0C00 == 0x4E280800:
        return True
    if enc & 0xFF208C00 == 0x5E000000 or enc & 0xFF3E0C00 == 0x5E280800:
        return True
    if enc & 0xBF20FC00 == 0x0E20E000 and (enc >> 22) & 3 == 3:
        return True
    return False


def gen_insn(rng, templates):
    while True:
        name = rng.choice(sorted(templates))
        mask, value, regs, zr_ok, rules = templates[name]
        enc = (rng.getrandbits(32) & ~mask) | value
        for m, v, p in rules:
            if rng.random() < p:
                enc = (enc & ~m) | v
        limit = 27 if name.startswith("ldst:") else 29
        for off in regs:
            r = 31 if (zr_ok and rng.random() < 0.08) else rng.randrange(limit)
            enc = (enc & ~(31 << off)) | (r << off)
        if name.startswith("ldst:"):
            enc = (enc & ~(31 << LDST_BASE)) | (28 << LDST_BASE)
            if name.endswith(("_reg", "_post")):
                rm = 31 if (name.endswith("_post") and rng.random() < 0.5) else 27
                enc = (enc & ~(31 << LDST_INDEX)) | (rm << LDST_INDEX)
        if not excluded(enc):
            return enc


SPECIAL64 = [0, 1, 2, 0xFFFFFFFFFFFFFFFF, 0x7FFFFFFFFFFFFFFF, 0x8000000000000000,
             0xFFFFFFFF, 0x7FFFFFFF, 0x80000000, 0xFFFFFFFF80000000, 0x100000000, 63, 64, 31, 32]

FP_SPECIAL32 = [0x00000000, 0x80000000, 0x3F800000, 0xBF800000, 0x7F800000, 0xFF800000,
                0x7FC00000, 0x7FA00000, 0xFFC12345, 0x00000001, 0x807FFFFF, 0x4B000000,
                0x4F000000, 0xCF000000, 0x3F000000, 0x7F7FFFFF]
FP_SPECIAL64 = [0x0000000000000000, 0x8000000000000000, 0x3FF0000000000000, 0xBFF0000000000000,
                0x7FF0000000000000, 0xFFF0000000000000, 0x7FF8000000000000, 0x7FF4000000000000,
                0x0000000000000001, 0x800FFFFFFFFFFFFF, 0x4330000000000000, 0x43E0000000000000,
                0xC3E0000000000000, 0x3FE0000000000000, 0x7FEFFFFFFFFFFFFF]


def rand_x(rng):
    k = rng.random()
    if k < 0.3:
        return rng.choice(SPECIAL64)
    if k < 0.45:
        return rng.getrandbits(8)
    if k < 0.6:
        v = rng.getrandbits(32)
        return v | (0xFFFFFFFF00000000 if v & 0x80000000 and rng.random() < 0.5 else 0)
    return rng.getrandbits(64)


def rand_q(rng):
    k = rng.random()
    if k < 0.25:
        lanes = [rng.choice(FP_SPECIAL32) if rng.random() < 0.6 else
                 struct.unpack("<I", struct.pack("<f", rng.uniform(-1000, 1000)))[0] for _ in range(4)]
        return sum(l << (32 * i) for i, l in enumerate(lanes))
    if k < 0.5:
        lanes = [rng.choice(FP_SPECIAL64) if rng.random() < 0.6 else
                 struct.unpack("<Q", struct.pack("<d", rng.uniform(-1e6, 1e6)))[0] for _ in range(2)]
        return lanes[0] | (lanes[1] << 64)
    if k < 0.6:
        return rng.choice([0, (1 << 128) - 1, 0x0102030405060708090A0B0C0D0E0F10])
    return rng.getrandbits(128)


def state_block(rng, ldst=False):
    words = [rand_x(rng) for _ in range(29)]
    if ldst:
        words[27] = rng.randrange(0, 512) & ~7   # register offset
        words[28] = 0                            # base: added to scratch+8192 at run time
    words.append(rng.getrandbits(4) << 28)  # nzcv
    words.append(0)  # fpsr
    # FPCR: mostly round-to-nearest, sometimes other rounding modes, FZ or DN
    words.append(rng.choice([0, 0, 0, 0, 0x400000, 0x800000, 0xC00000, 0x1000000, 0x2000000, 0x3000000]))
    qs = [rand_q(rng) for _ in range(32)]
    for q in qs:
        words.append(q & ((1 << 64) - 1))
        words.append(q >> 64)
    return words


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--classes", default="int")
    ap.add_argument("--count", type=int, default=2000)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--blocks", type=int, default=16)
    args = ap.parse_args()
    rng = random.Random(args.seed)
    templates = {}
    ldst = "ldst" in args.classes.split(",")
    for c in args.classes.split(","):
        for k, v in CLASSES[c].items():
            templates[c + ":" + k] = v

    w = sys.stdout.write
    w("// generated by gen.py --classes %s --count %d --seed %d\n" % (args.classes, args.count, args.seed))
    w("    .text\n    .global run_cases\nrun_cases:\n")
    w("    stp x29, x30, [sp, #-16]!\n    sub sp, sp, #768\n")
    for i in range(args.count):
        enc = gen_insn(rng, templates)
        blk = i % args.blocks
        w("    adrp x29, init_%d\n    add x29, x29, :lo12:init_%d\n" % (blk, blk))
        if ldst:
            w("    bl reset_scratch\n")
            w("    adrp x29, init_%d\n    add x29, x29, :lo12:init_%d\n" % (blk, blk))
        w("    bl load_state\n")
        if ldst:
            w("    adrp x28, scratch\n    add x28, x28, :lo12:scratch\n    add x28, x28, #2, lsl #12\n")
        w("    .inst 0x%08x\n    bl save_state\n" % enc)
        w("    ldr x0, =%d\n    ldr w1, =0x%08x\n    mov x2, sp\n    mov x3, x29\n    bl report\n" % (i, enc))
        if i % 64 == 63:
            w("    b 1f\n    .ltorg\n1:\n")
    w("    add sp, sp, #768\n    ldp x29, x30, [sp], #16\n    ret\n    .ltorg\n")
    w("    .data\n    .balign 16\n")
    for b in range(args.blocks):
        w("init_%d:\n" % b)
        for word in state_block(rng, ldst):
            w("    .quad 0x%016x\n" % word)


if __name__ == "__main__":
    main()
