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

CLASSES = {"int": INT}

# Encodings QEMU's cortex-a53 implements but armless does not advertise.
def excluded(enc):
    # CRC32/CRC32C: data-processing (2 source), opcode 010xxx
    if enc & 0x5FE00000 == 0x1AC00000 and (enc >> 13) & 7 == 0b010:
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
        for off in regs:
            r = 31 if (zr_ok and rng.random() < 0.08) else rng.randrange(29)
            enc = (enc & ~(31 << off)) | (r << off)
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


def state_block(rng):
    words = [rand_x(rng) for _ in range(29)]
    words.append(rng.getrandbits(4) << 28)  # nzcv
    words.append(0)  # fpsr
    words.append(0)  # fpcr
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
        w("    bl load_state\n    .inst 0x%08x\n    bl save_state\n" % enc)
        w("    ldr x0, =%d\n    ldr w1, =0x%08x\n    mov x2, sp\n    mov x3, x29\n    bl report\n" % (i, enc))
        if i % 64 == 63:
            w("    b 1f\n    .ltorg\n1:\n")
    w("    add sp, sp, #768\n    ldp x29, x30, [sp], #16\n    ret\n    .ltorg\n")
    w("    .data\n    .balign 16\n")
    for b in range(args.blocks):
        w("init_%d:\n" % b)
        for word in state_block(rng):
            w("    .quad 0x%016x\n" % word)


if __name__ == "__main__":
    main()
