# Measuring armless

Where the time goes, before changing anything for speed: three tools, from the cheapest to the
most detailed.

## `armless-run --stats`

What one run did, at exit:

```text
armless-run: 383995665 instructions on 1 CPU(s) in 1.13s (340.0 MIPS)
armless-run: cpu0: 383995665 instructions, 99.3% in compiled code (10140528 block runs, 37.6 instructions each), 9.66% of those handed to the interpreter
armless-run: cpu0: 28 modules compiled (28 natively, 0 failed, 25.1 ms compiling, 137 KiB of code), 0 pages invalidated, 2 flushes
```

- **in compiled code**: the share of instructions that ran inside JIT-compiled blocks rather than
  the interpreter. Low means code that never got hot, or got hot and was invalidated.
- **block runs / instructions each**: how often compiled code is entered. Each entry and exit
  costs a few nanoseconds; short blocks (indirect branches, calls) make that add up.
- **handed to the interpreter**: instructions inside compiled blocks that the JIT does not
  translate, run one at a time through a helper (several times slower than translated ones).
- **failed**: compiles that did not translate. Should be 0; the tests insist on it.
- **pages invalidated / flushes**: compiled code dropped because the guest wrote its page, and
  the whole cache dropped (full, reset, or the translation regime changed).

`--fallbacks` adds which instructions were handed to the interpreter, by opcode, most first: what
the JIT should translate next. It hashes every one of them, so leave it off when timing.
`--stats-json FILE` writes all of it as JSON.

## `bench/run.exs`

```sh
elixir bench/run.exs                    # build, then 5 runs under the interpreter and the native JIT
elixir bench/run.exs --save before      # keep the results (build/bench/before.json)
elixir bench/run.exs --compare before   # after a change: each phase against them
elixir bench/run.exs --profile native   # where the host's time goes (needs samply)
```

It builds `armless-run` (release, with line tables, in `build/bench`) and the benchmark guest
(`guest/bench.c`, by `guest/build.sh`), and runs the guest with the MMU on, as under Linux. Each
phase stresses one thing:

| phase | what it exercises |
|---|---|
| `int` | integer ALU, branches, byte stores (a sieve, a bitwise CRC-32) |
| `mem` | sequential loads and stores (memset, memcpy, a sum) |
| `tlb` | random accesses over 16 MiB of 4 KiB pages: the TLB and its fast tables |
| `call` | indirect calls and returns: leaving and entering compiled blocks |
| `fp64`, `fp32` | double- and single-precision multiply-add loops |
| `u-*` | one instruction, 8 million times: what it costs compiled (`u-orr`, `u-fadd-s`) against handed to the interpreter (`u-ext`, `u-cmeq0`, `u-fadd-d`, `u-fmla-2d`) |
| `u-ttbr`, `u-isb` | a TTBR0 write then 32 pages touched (a context switch), against an ISB and the same |

The report gives each phase's median with its spread (half the range over the runs, as a
percentage of the median), the JIT's statistics per mode, and the instructions compiled code hands
to the interpreter, disassembled. With `--compare`, a change within the spread (or 2%) is marked
`~`: noise, not a result. The `tlb` phase is the noisiest (around 10% here); give it more `--runs`.

`--profile MODE` records one run with [samply](https://github.com/mstange/samply) and lists the
functions the vCPU thread spends its samples in, by themselves and with what they call.
`(JIT-compiled guest code)` is time in compiled blocks; time in `native_interp` below it is
instructions handed to the interpreter, and `native_read`/`native_write` are memory accesses that
missed the fast TLB. `samply load build/bench/profile-MODE.json` opens the whole profile in the
Firefox profiler.

## In tests

The fuzzers in `src/jit/native.rs` and the bare-metal suite under the native JIT
(`tests/guest.rs`) fail if a block does not compile, or if fewer than 95% of the fuzzed blocks ran
as compiled code: a change that quietly sends code back to the interpreter is caught as a test
failure, not later as a slowdown.
