# Measuring armless

Where the time goes, before changing anything for speed: from the cheapest look to real
workloads.

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
elixir bench/run.exs --modes native,wasm   # and through armless.js and the wasm JIT, in node
elixir bench/run.exs --summarize P.json --thread vcpu0   # any samply profile, summarized
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

With `ARMLESS_JIT_PERF_MAP=1` set (for any command, `bench/sites.exs --profile` included), the
native JIT writes `/tmp/perf-<pid>.map`, one line per compiled function: `armless EL0 va
0x0000ffff885b6000 pa 0x6bb6000 [cpu1, 5 entries]`, the exception level, the virtual page it got
hot at (the same physical page can be mapped at others) and the physical page. samply takes the
file for a library, and `--summarize` names compiled code by it, then adds compiled guest code by
exception level and by virtual page. It says how much of the compiled code it could name: on a
java run, 77%. The rest is in host code the map has no line for, mostly late in the run, after
the JIT's flushes, although every compile has its line.

`ARMLESS_JIT_INVAL_STATS=1` prints, for each CPU when it stops, why pages' code was dropped (a
store of its own, its IC IVAU, another CPU's or a device's, a store-exclusive), split into pages
with compiled code and pages with decoded instructions only, and how often the same pages with
compiled code were dropped again. Most drops are of decoded instructions only, which are cheap.

## `bench/sites.exs`: real workloads

```sh
elixir bench/sites.exs ../snowglobe                     # every site built in ../snowglobe/dist
elixir bench/sites.exs ../snowglobe --save main         # keep as build/bench/sites-main.json
elixir bench/sites.exs ../snowglobe --compare main      # after a change
elixir bench/sites.exs ../snowglobe --sites java --profile   # interpreted vs compiled time
```

[snowglobe](https://github.com/filipecabaco/snowglobe) sites are the workloads armless runs for
real: Python with Rich, a JVM's jshell, Elixir's iex and its BEAM JIT, PostgreSQL, Supabase,
Node with TypeScript, Rust and shell tools. This builds snowglobe's native runtime against this
checkout (snowglobe itself is left as it was), restores each built site and feeds it the
commands its `transcript.json` lists, with the native JIT. Per site: the job's time and
instructions, how much compiled code handed to the interpreter, what the JIT compiled (and how
long that took), pages dropped because the guest wrote them, and full flushes; then, over every
site, which instructions compiled code still hands to the interpreter: what to translate next.

Runs start cold (`ARMLESS_JIT_PREWARM=off`). Otherwise snowglobe would prewarm the JIT from the
profile its earlier runs learned (`~/.cache/snowglobe`), which grows with every run, so runs of
the same build would get faster by themselves and comparisons would mean nothing. `--warm`
measures with prewarming, every run from the same copy of that cache. `--armless DIR` measures
another checkout (`main`, say) against which to `--compare`.

What it showed: workloads that run their own JIT (the JVM, the BEAM, V8) write code as they go,
so armless drops what it compiled from those pages and runs the new code interpreted until it is
hot. With `--profile`, a third to half of a vCPU's time is in the interpreter on short commands
(Java, Python, iex): code that runs fewer than `ARMLESS_JIT_THRESHOLD` times never compiles.
Lowering the threshold made every site slower, so that time is the interpreter's to win.

`ARMLESS_JIT_OPT` (Cranelift's `opt_level`, `speed` by default) and `ARMLESS_JIT_REGALLOC`
(`regalloc_algorithm`) change how the native JIT compiles. With `none`, compiles take about 30%
less time and java, elixir and python were 1–4% faster, but the benchmark guest's tight loops ran
7–23% slower; even then a lower threshold did not pay. `single_pass` made every site 21–31% slower.

## `cargo bench --bench machine`

Host-side costs with [criterion](https://github.com/bheisler/criterion.rs): one guest
instruction interpreted and compiled (a fixed loop, as instructions per second), compiling one
page, and saving and restoring a 64 MiB machine. `-- --save-baseline NAME` and
`-- --baseline NAME` compare runs, with criterion's statistics.

`cargo bench --bench zstd` times the zstd decoder (`decompress` and `decompress_each`) on frames
of 64 KiB, 4 MiB and 32 MiB with an 8 MiB window, and `cargo bench -p armless-user --bench
syscalls` what user mode's system calls cost.

Every pull request runs all of them on its base and then on itself, on one runner
([`.github/workflows/bench.yml`](../.github/workflows/bench.yml)), and puts a table of the two in
the run's summary and in a comment on the pull request (`.github/scripts/bench-table.sh` makes it
from criterion's results). It is for information: a regression doesn't fail the run.

## In tests

The fuzzers in `src/jit/native.rs` and the bare-metal suite under the native JIT
(`tests/guest.rs`) fail if a block does not compile, or if fewer than 95% of the fuzzed blocks ran
as compiled code: a change that quietly sends code back to the interpreter is caught as a test
failure, not later as a slowdown. `translated_groups_never_reach_the_interpreter` goes further
for the groups real workloads used to hand back (RBIT, ADC/SBC, DC ZVA, MOVI, max/min, LDR
literal, lane moves, widening shifts): compiled, none of them may call the interpreter, since
results alone can't show it (the interpreter's are the same). `benchmark_guest_stays_compiled`
(`tests/guest.rs`) runs the benchmark guest under the native JIT and checks what the JIT did:
almost everything compiled, almost nothing interpreted, no failed compile.
