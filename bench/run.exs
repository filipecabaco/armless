#!/usr/bin/env elixir
# The armless benchmark: builds armless-run and the benchmark guest (guest/bench.c), runs it under
# the interpreter and the native JIT, and reports each phase's median time with its spread, plus
# what the JIT did (how much ran compiled, how much went back to the interpreter, and which
# instructions those were).
#
#   elixir bench/run.exs                      # 5 runs of each mode
#   elixir bench/run.exs --save before        # ... and keep the results as "before"
#   elixir bench/run.exs --compare before     # ... and compare with them
#   elixir bench/run.exs --profile native     # samply profile of one run: where the host's time goes
#
# Options: --runs N, --modes interp,native, --out DIR (results; default build/bench),
# --profile MODE, --top N (profile rows, default 30), --no-build.
#
# See docs/profiling.md.

defmodule Bench do
  @root Path.expand("..", __DIR__)
  @phases ~w(int mem tlb call fp64 fp32 u-orr u-ext u-cmeq0 u-fadd-s u-fadd-d u-fmla-2d u-ttbr u-isb total)
  @modes %{"interp" => [], "native" => [{"ARMLESS_JIT", "native"}]}

  def main(argv) do
    {opts, _} =
      OptionParser.parse!(argv,
        strict: [
          runs: :integer,
          modes: :string,
          save: :string,
          compare: :string,
          out: :string,
          profile: :string,
          top: :integer,
          build: :boolean
        ]
      )

    runs = Keyword.get(opts, :runs, 5)
    modes = opts |> Keyword.get(:modes, "interp,native") |> String.split(",", trim: true)
    out = Path.expand(Keyword.get(opts, :out, "build/bench"), @root)
    File.mkdir_p!(out)

    Enum.each(modes, &(Map.has_key?(@modes, &1) or die("unknown mode #{&1}: interp or native")))
    if Keyword.get(opts, :build, true), do: build(out)
    run_bin = Path.join(out, "target/release/armless-run")
    guest = Path.join(out, "guest/bench.bin")

    case opts[:profile] do
      nil -> benchmark(run_bin, guest, modes, runs, out, opts)
      mode -> profile(run_bin, guest, mode, out, Keyword.get(opts, :top, 30))
    end
  end

  # ---------------------------------------------------------------- build

  defp build(out) do
    IO.puts(:stderr, "building armless-run (release, with line tables) and the guest ...")
    # Its own target directory: line tables for profiles without rebuilding the usual one
    cmd!("cargo", ~w(build --release --bin armless-run),
      env: [
        {"CARGO_TARGET_DIR", Path.join(out, "target")},
        {"CARGO_PROFILE_RELEASE_DEBUG", "line-tables-only"}
      ]
    )

    cmd!("sh", [Path.join(@root, "guest/build.sh")],
      env: [
        {"OUT", Path.join(out, "guest")},
        {"LD", tool(["ld.lld", "gcc-ld/ld.lld"])},
        {"OBJCOPY", tool(["llvm-objcopy", "rust-objcopy"])}
      ]
    )
  end

  # Installed, or the copy rustc ships (as tests/guest.rs does)
  defp tool(names) do
    shipped = Enum.map(names, &Path.join(rustc_bin(), &1))
    Enum.find(names ++ shipped, &runs?/1) || die("none of #{inspect(names)} found")
  end

  defp runs?(cmd) do
    match?({_, 0}, System.cmd(cmd, ["--version"], stderr_to_stdout: true))
  rescue
    _ -> false
  end

  defp rustc_bin do
    {sysroot, 0} = System.cmd("rustc", ["--print", "sysroot"])
    {version, 0} = System.cmd("rustc", ["-vV"])
    [host] = Regex.run(~r/^host: (\S+)$/m, version, capture: :all_but_first)
    Path.join([String.trim(sysroot), "lib/rustlib", host, "bin"])
  end

  # ---------------------------------------------------------------- benchmark

  defp benchmark(run_bin, guest, modes, runs, out, opts) do
    results =
      Map.new(modes, fn mode ->
        IO.puts(:stderr, "#{mode}: #{runs} runs ...")
        samples = for _ <- 1..runs, do: run(run_bin, guest, mode, out, false)
        {mode, summarize(samples)}
      end)

    baseline = opts[:compare] && load(out, opts[:compare])
    print_phases(results, modes, baseline)
    print_jit(results, modes)

    # Which instructions compiled code hands back to the interpreter: one more run, counting them
    if "native" in modes do
      %{stats: stats} = run(run_bin, guest, "native", out, true)
      print_fallbacks(stats)
    end

    if name = opts[:save] do
      path = Path.join(out, "#{name}.json")
      File.write!(path, JSON.encode!(results))
      IO.puts("\nsaved as #{path}")
    end
  end

  defp run(run_bin, guest, mode, out, fallbacks) do
    stats_path = Path.join(out, "stats-#{mode}.json")
    args = ~w(--raw #{guest} --timeout 600 --stats-json #{stats_path}) ++ if(fallbacks, do: ["--fallbacks"], else: [])
    {output, status} = System.cmd(run_bin, args, env: @modes[mode], stderr_to_stdout: true)
    status == 0 or die("#{mode}: armless-run exited with #{status}:\n#{output}")

    phases =
      Regex.scan(~r/^bench (\S+) (\d+)\r?$/m, output, capture: :all_but_first)
      |> Map.new(fn [name, us] -> {name, String.to_integer(us)} end)

    map_size(phases) > 0 or die("#{mode}: no bench lines in:\n#{output}")
    %{phases: phases, stats: stats_path |> File.read!() |> JSON.decode!()}
  end

  defp summarize(samples) do
    phases =
      Map.new(@phases, fn p ->
        values = samples |> Enum.map(& &1.phases[p]) |> Enum.reject(&is_nil/1) |> Enum.sort()
        {p, %{"median" => median(values), "min" => List.first(values), "max" => List.last(values)}}
      end)

    stats = samples |> List.last() |> Map.fetch!(:stats)
    seconds = samples |> Enum.map(& &1.stats["seconds"]) |> Enum.sort() |> median()
    %{"phases" => phases, "mips" => stats["instructions"] / seconds / 1.0e6, "cpu" => hd(stats["cpus"])}
  end

  defp median([]), do: nil
  defp median(v), do: Enum.at(v, div(length(v), 2))

  defp load(out, name) do
    path = Path.join(out, "#{name}.json")
    File.exists?(path) or die("no saved results #{path}")
    path |> File.read!() |> JSON.decode!()
  end

  # ---------------------------------------------------------------- reports

  defp print_phases(results, modes, baseline) do
    header =
      Enum.flat_map(modes, fn m ->
        ["#{m} ms", "±%"] ++ if(baseline && baseline[m], do: ["vs base"], else: [])
      end)

    rows =
      Enum.map(@phases, fn p ->
        [p | Enum.flat_map(modes, &phase_cells(results[&1]["phases"][p], baseline && baseline[&1], p))]
      end)

    IO.puts("")
    table(["phase" | header], rows)
  end

  defp phase_cells(%{"median" => nil}, _, _), do: ["-", "-"]

  defp phase_cells(%{"median" => m, "min" => lo, "max" => hi}, base, p) do
    spread = if m > 0, do: 100 * (hi - lo) / m / 2, else: 0.0
    cells = [ms(m), "#{Float.round(spread, 1)}"]

    case base && base["phases"][p]["median"] do
      nil -> if base, do: cells ++ ["-"], else: cells
      b -> cells ++ [delta(m, b, spread)]
    end
  end

  # A change smaller than the spread of the runs is noise, and marked as such
  defp delta(now, base, spread) do
    pct = 100 * (now - base) / max(base, 1)
    mark = if abs(pct) <= max(spread, 2.0), do: " ~", else: if(pct < 0, do: " faster", else: " slower")
    "#{if pct >= 0, do: "+", else: ""}#{Float.round(pct, 1)}%#{mark}"
  end

  defp ms(us), do: :erlang.float_to_binary(us / 1000, decimals: 1)

  defp print_jit(results, modes) do
    IO.puts("")

    rows =
      Enum.map(modes, fn m ->
        c = results[m]["cpu"]
        pct = fn a, b -> if b > 0, do: "#{Float.round(100 * a / b, 2)}%", else: "-" end

        [
          m,
          "#{Float.round(results[m]["mips"], 1)}",
          pct.(c["compiled_insns"], c["instructions"]),
          pct.(c["helper_insns"], c["compiled_insns"]),
          "#{c["block_runs"]}",
          "#{c["modules"]} (#{c["native_failures"]} failed)",
          "#{Float.round(c["native_compile_ns"] / 1.0e6, 1)}"
        ]
      end)

    table(["mode", "MIPS", "compiled", "to interp", "block runs", "modules", "compile ms"], rows)
  end

  defp print_fallbacks(stats) do
    case hd(stats["cpus"])["fallbacks"] |> Enum.take(15) do
      [] ->
        IO.puts("\nnothing compiled was handed to the interpreter")

      top ->
        names = disassemble(Enum.map(top, fn [insn, _] -> insn end))
        total = hd(stats["cpus"])["helper_insns"]
        IO.puts("\nhanded to the interpreter by compiled code (native), most first:")

        rows =
          Enum.zip_with(top, names, fn [insn, n], name ->
            [
              "#{n}",
              "#{Float.round(100 * n / max(total, 1), 1)}%",
              "0x" <> String.pad_leading(Integer.to_string(insn, 16), 8, "0"),
              name
            ]
          end)

        table(["count", "share", "encoding", "instruction"], rows)
    end
  end

  # Names for encodings: assembled as .inst and disassembled; "?" if the tools are missing
  defp disassemble(insns) do
    dir = Path.join(System.tmp_dir!(), "armless-bench-#{System.unique_integer([:positive])}")
    File.mkdir_p!(dir)

    try do
      disassemble(insns, dir)
    after
      File.rm_rf(dir)
    end
  end

  defp disassemble(insns, dir) do
    src = Path.join(dir, "i.S")
    obj = Path.join(dir, "i.o")
    File.write!(src, Enum.map_join(insns, "", &"  .inst #{&1}\n"))
    unknown = Enum.map(insns, fn _ -> "?" end)
    objdump = Enum.find([Path.join(rustc_bin(), "llvm-objdump"), "llvm-objdump", "objdump"], &runs?/1)

    with {_, 0} <- System.cmd("clang", ["--target=aarch64-none-elf", "-c", src, "-o", obj], stderr_to_stdout: true),
         true <- objdump != nil,
         {text, 0} <- System.cmd(objdump, ["-d", "--no-show-raw-insn", obj], stderr_to_stdout: true) do
      lines =
        Regex.scan(~r/^\s+[0-9a-f]+:\s+(.+)$/m, text, capture: :all_but_first)
        |> Enum.map(&(&1 |> hd() |> String.trim()))

      if length(lines) == length(insns), do: lines, else: unknown
    else
      _ -> unknown
    end
  end

  # ---------------------------------------------------------------- profile

  # One run under samply; then where the vCPU thread's samples are, by function (self: the
  # function itself; inclusive: with what it calls)
  defp profile(run_bin, guest, mode, out, top) do
    Map.has_key?(@modes, mode) or die("unknown mode #{mode}")
    runs?("samply") or die("samply is not installed: cargo install samply")
    path = Path.join(out, "profile-#{mode}.json")
    IO.puts(:stderr, "recording #{mode} with samply ...")

    cmd!("samply", ~w(record --rate 4000 --save-only --unstable-presymbolicate -o #{path} --) ++ [run_bin, "--raw", guest],
      env: @modes[mode]
    )

    Profile.report(path, "vcpu0", top)
    IO.puts("\nfull profile: samply load #{path}")
  end

  # ---------------------------------------------------------------- helpers

  defp table(header, rows) do
    widths =
      Enum.reduce([header | rows], List.duplicate(0, length(header)), fn row, w ->
        Enum.zip_with(row, w, &max(String.length(&1), &2))
      end)

    line = fn row ->
      row
      |> Enum.zip(widths)
      |> Enum.with_index()
      |> Enum.map_join("  ", fn {{cell, w}, i} ->
        if i == 0 or i == length(row) - 1, do: String.pad_trailing(cell, w), else: String.pad_leading(cell, w)
      end)
      |> String.trim_trailing()
    end

    IO.puts(line.(header))
    IO.puts(Enum.map_join(widths, "  ", &String.duplicate("-", &1)))
    Enum.each(rows, &IO.puts(line.(&1)))
  end

  defp cmd!(cmd, args, opts) do
    {_, status} = System.cmd(cmd, args, Keyword.merge([into: IO.stream(:stderr, :line), stderr_to_stdout: true], opts))
    status == 0 or die("#{cmd} #{Enum.join(args, " ")}: exit #{status}")
  end

  def die(msg) do
    IO.puts(:stderr, "bench: #{msg}")
    System.halt(1)
  end
end

# Self and inclusive samples by function in one thread of a samply profile saved with
# --unstable-presymbolicate (the symbols are in the .syms.json beside it).
defmodule Profile do
  def report(path, thread_name, top) do
    p = path |> File.read!() |> JSON.decode!()
    syms = path |> String.replace_suffix(".json", ".syms.json") |> File.read!() |> JSON.decode!()
    strings = List.to_tuple(syms["string_table"])

    tables =
      Map.new(syms["data"], fn d ->
        t =
          d["symbol_table"]
          |> Enum.sort_by(& &1["rva"])
          |> Enum.map(&{&1["rva"], &1["size"], elem(strings, &1["symbol"])})
          |> List.to_tuple()

        {d["debug_name"], t}
      end)

    libs = p["libs"] |> Enum.map(& &1["debugName"]) |> List.to_tuple()
    th = Enum.find(p["threads"], &(&1["name"] == thread_name)) || Bench.die("no thread #{thread_name}")
    ft = th["frameTable"]
    addr = List.to_tuple(ft["address"])
    func = List.to_tuple(ft["func"])
    resource = List.to_tuple(th["funcTable"]["resource"])
    lib_of = List.to_tuple(th["resourceTable"]["lib"])
    frame_of = List.to_tuple(th["stackTable"]["frame"])
    prefix = List.to_tuple(th["stackTable"]["prefix"])

    name = fn f ->
      res = elem(resource, elem(func, f))
      lib = if is_integer(res) and res >= 0, do: elem(libs, elem(lib_of, res)), else: nil

      case lib && Map.get(tables, lib) do
        nil -> "(JIT-compiled guest code)"
        t -> lookup(t, elem(addr, f)) || "#{lib}+0x#{Integer.to_string(elem(addr, f), 16)}"
      end
    end

    names = Map.new(0..(tuple_size(addr) - 1)//1, fn f -> {f, short(name.(f))} end)
    stacks = Enum.reject(th["samples"]["stack"], &is_nil/1)
    total = length(stacks)

    {self, incl} =
      Enum.reduce(stacks, {%{}, %{}}, fn s, {sc, ic} ->
        frames = walk(s, frame_of, prefix, [])
        leaf = names[elem(frame_of, s)]
        seen = frames |> Enum.map(&names[&1]) |> Enum.uniq()
        {Map.update(sc, leaf, 1, &(&1 + 1)), Enum.reduce(seen, ic, &Map.update(&2, &1, 1, fn x -> x + 1 end))}
      end)

    IO.puts("\n#{thread_name}: #{total} samples\n\nself (time in the function itself):")
    print(self, total, top)
    IO.puts("\ninclusive (with what it calls):")
    print(incl, total, top)
  end

  defp walk(nil, _, _, acc), do: acc
  defp walk(s, frame_of, prefix, acc), do: walk(elem(prefix, s), frame_of, prefix, [elem(frame_of, s) | acc])

  # Binary search for the symbol holding `a`
  defp lookup(t, a), do: lookup(t, a, 0, tuple_size(t) - 1)
  defp lookup(_, _, lo, hi) when lo > hi, do: nil

  defp lookup(t, a, lo, hi) do
    mid = div(lo + hi, 2)
    {rva, size, sym} = elem(t, mid)

    cond do
      a < rva -> lookup(t, a, lo, mid - 1)
      a >= rva + max(size, 1) -> lookup(t, a, mid + 1, hi)
      true -> sym
    end
  end

  # Drop generic arguments and hashes: armless::cpu::Cpu<H>::run::h0123 -> armless::cpu::Cpu::run
  defp short(s) do
    s
    |> String.replace(~r/::h[0-9a-f]{16}$/, "")
    |> strip_generics()
    |> String.replace("::::", "::")
    |> String.replace(~r/::$/, "")
  end

  # Innermost brackets first. After a name or `::` they are generic arguments, and go; anywhere
  # else they are a qualified path (`<Type>::f`, `<Type as Trait>::f`), and leave the type.
  defp strip_generics(s) do
    stripped =
      Regex.replace(~r/(^|.)<([^<>]*)>/, s, fn _, before, inner ->
        # after a name, or `::` (turbofish)
        if before =~ ~r/^[A-Za-z0-9_:]$/ do
          before
        else
          before <> (inner |> String.split(" as ") |> hd())
        end
      end)

    if stripped == s, do: s, else: strip_generics(stripped)
  end

  defp print(counts, total, top) do
    counts
    |> Enum.sort_by(fn {_, n} -> -n end)
    |> Enum.take(top)
    |> Enum.each(fn {name, n} ->
      IO.puts("  #{:io_lib.format("~5.1f", [100 * n / max(total, 1)])}%  #{name}")
    end)
  end
end

Bench.main(System.argv())
