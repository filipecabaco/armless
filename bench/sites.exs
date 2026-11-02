#!/usr/bin/env elixir
# Real workloads: built snowglobe sites (https://github.com/filipecabaco/snowglobe) running their
# own exercise commands natively, on this armless checkout. Where bench/run.exs measures phases of
# a synthetic guest, this measures what a visitor's commands cost: Python with Rich, a JVM's
# jshell, Elixir's iex and its BEAM JIT, PostgreSQL, Node with TypeScript, Rust and shell tools.
#
#   elixir bench/sites.exs ../snowglobe                       # every site in ../snowglobe/dist
#   elixir bench/sites.exs ../snowglobe --sites java,python --runs 3
#   elixir bench/sites.exs ../snowglobe --save before         # keep as build/bench/sites-before.json
#   elixir bench/sites.exs ../snowglobe --compare before
#   elixir bench/sites.exs ../snowglobe --sites java --profile  # samply: interpreted vs compiled
#   elixir bench/sites.exs ../snowglobe --armless ../armless-main --save main  # another checkout
#
# Runs start cold: snowglobe would otherwise prewarm the JIT from the profile its earlier runs
# learned (in ~/.cache/snowglobe), which grows with every run, so runs would not compare. --warm
# keeps snowglobe's prewarm, from a copy of the cache taken at the start (every run sees the same).
#
# It builds snowglobe-vm (snowglobe's native runtime) against this checkout, without changing
# snowglobe (its Cargo.lock is put back), then for each site restores its snapshot and feeds the
# commands its transcript.json lists to its command (the site's REPL, or a shell), with
# ARMLESS_JIT=native. Each site's report: the job's time and instructions, what the JIT compiled
# and dropped, and what compiled code still hands to the interpreter, by opcode.
#
# Sites built by an older armless restore as they are (snapshots stay compatible); `snowglobe
# build` makes them. See docs/profiling.md.

defmodule Sites do
  @root Path.expand("..", __DIR__)
  @dep ~s(patch."https://github.com/filipecabaco/armless".armless.path)

  def main(argv) do
    {opts, args} =
      OptionParser.parse!(argv,
        strict: [
          sites: :string,
          runs: :integer,
          save: :string,
          compare: :string,
          profile: :boolean,
          build: :boolean,
          armless: :string,
          warm: :boolean
        ]
      )

    snowglobe = Path.expand(List.first(args) || die("usage: elixir bench/sites.exs SNOWGLOBE_DIR [options]"))
    dist = Path.join(snowglobe, "dist")
    File.dir?(dist) or die("#{dist}: no built sites (snowglobe build ... --out dist/NAME)")
    out = Path.join(@root, "build/bench")
    File.mkdir_p!(out)
    armless = Path.expand(Keyword.get(opts, :armless, @root))
    # One runtime per armless checkout measured
    target = "sg-target-" <> (armless |> :erlang.md5() |> Base.encode16(case: :lower) |> binary_part(0, 8))
    vm = Path.join([out, target, "release/snowglobe-vm"])
    if Keyword.get(opts, :build, true), do: build(snowglobe, armless, Path.join(out, target))
    Process.put(:env, run_env(opts[:warm], out))

    sites =
      case opts[:sites] do
        nil -> dist |> File.ls!() |> Enum.filter(&File.exists?(Path.join([dist, &1, "transcript.json"]))) |> Enum.sort()
        list -> String.split(list, ",", trim: true)
      end

    runs = Keyword.get(opts, :runs, 1)

    results =
      for site <- sites, into: %{} do
        IO.puts(:stderr, "#{site}: #{runs} run(s) ...")
        samples = for _ <- 1..runs, do: run(vm, dist, site, out)

        if opts[:profile] do
          profile(vm, dist, site, out)
        end

        {site, summarize(samples)}
      end

    baseline = opts[:compare] && load(out, opts[:compare])
    report(results, baseline)

    if name = opts[:save] do
      path = Path.join(out, "sites-#{name}.json")
      File.write!(path, JSON.encode!(results))
      IO.puts("\nsaved as #{path}")
    end
  end

  # ---------------------------------------------------------------- build

  # Cold (no prewarm), or warm from a fixed copy of snowglobe's learned profiles
  defp run_env(warm, out) do
    if warm do
      cache = Path.join(out, "sg-cache")
      File.rm_rf!(cache)
      src = System.get_env("SNOWGLOBE_CACHE") || Path.join(System.user_home!(), ".cache/snowglobe")
      if File.dir?(src), do: File.cp_r!(src, cache), else: File.mkdir_p!(cache)
      # The copy is taken again before each run (runs add to it)
      Process.put(:cache, {src, cache})
      [{"SNOWGLOBE_CACHE", cache}]
    else
      [{"ARMLESS_JIT_PREWARM", "off"}]
    end
  end

  defp build(snowglobe, armless, target) do
    IO.puts(:stderr, "building snowglobe-vm against #{armless} ...")
    runtime = Path.join(snowglobe, "internal/tools/runtime")
    lock = Path.join(runtime, "Cargo.lock")
    saved = File.read!(lock)

    try do
      {_, status} =
        System.cmd("cargo", ["build", "--release", "--config", ~s(#{@dep}="#{armless}")],
          cd: runtime,
          env: [{"CARGO_TARGET_DIR", target}],
          into: IO.stream(:stderr, :line),
          stderr_to_stdout: true
        )

      status == 0 or die("building snowglobe-vm failed")
    after
      # The patch rewrites the lock file; snowglobe's own stays as it was
      File.write!(lock, saved)
    end
  end

  # ---------------------------------------------------------------- one site

  # The site's command (its REPL, or a shell) with the transcript's commands on its input
  defp job(dist, site, ws) do
    run = Path.join([dist, site, "run.json"]) |> File.read!() |> JSON.decode!()
    lines = Path.join([dist, site, "transcript.json"]) |> File.read!() |> JSON.decode!() |> Enum.map(& &1["command"])
    File.rm_rf!(ws)
    File.mkdir_p!(Path.join(ws, "in"))
    File.write!(Path.join(ws, "in/exercise"), Enum.join(lines, "\n") <> "\n")

    case run["command"] do
      # A shell: the commands are its script
      "'/bin/sh' '-c' " <> _ -> "sh in/exercise"
      # Python only runs its startup file (the site's imports) interactively
      "'python3'" -> "python3 -i < in/exercise"
      command -> "#{command} < in/exercise"
    end
  end

  defp run(vm, dist, site, out) do
    ws = Path.join(out, "ws-#{site}")
    cmd = job(dist, site, ws)
    args = ["run", Path.join(dist, site), "--blobs", Path.join(dist, "blobs"), "--workspace", ws, "--cmd", cmd, "--timeout", "600"]
    reset_cache()
    env = [{"ARMLESS_JIT", "native"}, {"ARMLESS_JIT_HELPER_STATS", "1"}, {"SNOWGLOBE_TIMING", "1"}] ++ Process.get(:env)
    {output, _} = System.cmd(vm, args, env: env, stderr_to_stdout: true)
    parse(output, site)
  end

  # Warm runs all start from the same learned profiles
  defp reset_cache do
    with {src, cache} <- Process.get(:cache) do
      File.rm_rf!(cache)
      if File.dir?(src), do: File.cp_r!(src, cache), else: File.mkdir_p!(cache)
    end
  end

  defp parse(output, site) do
    num = fn re -> case Regex.run(re, output, capture: :all_but_first) do
        nil -> nil
        [v] -> String.to_integer(v)
      end
    end

    job_ms = num.(~r/timing: job \S+ in (\d+) ms/) || die("#{site}: the job did not finish:\n#{tail(output)}")

    [modules, compile_ms, invalidations, flushes] =
      Regex.run(~r/jit (\d+) modules \(\d+ compiled in (\d+) ms\), (\d+) page invalidations, (\d+) flushes/, output,
        capture: :all_but_first
      ) || ["0", "0", "0", "0"]

    [insns, helper] =
      Regex.run(~r/([\d.]+)M instructions, ([\d.]+)M of them handed/, output, capture: :all_but_first) || ["0", "0"]

    fallbacks =
      Regex.scan(~r/timing: helper ([0-9a-f]{8})\s+([\d.]+)M\s+e\.g\. ([0-9a-f]{8})/, output, capture: :all_but_first)
      |> Enum.map(fn [_, m, example] -> [String.to_integer(example, 16), String.to_float(m)] end)

    %{
      "job_ms" => job_ms,
      "minsns" => String.to_integer(insns),
      "helper_m" => parse_float(helper),
      "modules" => String.to_integer(modules),
      "compile_ms" => String.to_integer(compile_ms),
      "invalidations" => String.to_integer(invalidations),
      "flushes" => String.to_integer(flushes),
      "fallbacks" => fallbacks
    }
  end

  defp parse_float(s), do: if(String.contains?(s, "."), do: String.to_float(s), else: String.to_integer(s) / 1)
  defp tail(s), do: s |> String.split("\n") |> Enum.take(-15) |> Enum.join("\n")

  defp summarize(samples) do
    times = samples |> Enum.map(& &1["job_ms"]) |> Enum.sort()
    last = List.last(samples)

    Map.merge(last, %{
      "job_ms" => Enum.at(times, div(length(times), 2)),
      "min_ms" => List.first(times),
      "max_ms" => List.last(times)
    })
  end

  # samply on one run: how the vCPU's time splits between compiled code, the interpreter and the rest
  defp profile(vm, dist, site, out) do
    ws = Path.join(out, "ws-#{site}")
    cmd = job(dist, site, ws)
    path = Path.join(out, "profile-site-#{site}.json")
    args = ~w(record --rate 2000 --save-only --unstable-presymbolicate -o #{path} --) ++
      [vm, "run", Path.join(dist, site), "--blobs", Path.join(dist, "blobs"), "--workspace", ws, "--cmd", cmd, "--timeout", "600"]

    reset_cache()
    {_, 0} = System.cmd("samply", args, env: [{"ARMLESS_JIT", "native"} | Process.get(:env)], stderr_to_stdout: true)
    IO.puts("\n#{site}: where vcpu0's time goes (full profile: samply load #{path})")
    System.cmd("elixir", [Path.join(@root, "bench/run.exs"), "--summarize", path, "--thread", "vcpu0", "--top", "12"],
      into: IO.stream(:stdio, :line)
    )
  end

  # ---------------------------------------------------------------- report

  defp report(results, baseline) do
    header = ["site", "job ms", "range"] ++ if(baseline, do: ["vs base"], else: []) ++
      ["Minsns", "MIPS", "to interp", "modules", "compile ms", "invalidated", "flushes"]

    rows =
      results
      |> Enum.sort()
      |> Enum.map(fn {site, r} ->
        base = baseline && baseline[site] && baseline[site]["job_ms"]
        spread = 100 * (r["max_ms"] - r["min_ms"]) / max(r["job_ms"], 1) / 2

        [site, "#{r["job_ms"]}", "#{r["min_ms"]}-#{r["max_ms"]}"] ++
          if(baseline, do: [delta(r["job_ms"], base, spread)], else: []) ++
          [
            "#{r["minsns"]}",
            "#{round(r["minsns"] / max(r["job_ms"], 1) * 1000)}",
            "#{Float.round(r["helper_m"], 1)}M",
            "#{r["modules"]}",
            "#{r["compile_ms"]}",
            "#{r["invalidations"]}",
            "#{r["flushes"]}"
          ]
      end)

    IO.puts("")
    table(header, rows)

    # What compiled code hands to the interpreter, over every site: what to translate next
    total =
      results
      |> Enum.flat_map(fn {site, r} -> Enum.map(r["fallbacks"], fn [insn, m] -> {insn, m, site} end) end)
      |> Enum.group_by(fn {insn, _, _} -> Bitwise.band(insn, 0xFFE0_FC00) end)
      |> Enum.map(fn {_, list} ->
        {insn, _, _} = hd(list)
        m = list |> Enum.map(&elem(&1, 1)) |> Enum.sum()
        sites = list |> Enum.map(&elem(&1, 2)) |> Enum.uniq() |> Enum.join(",")
        {insn, m, sites}
      end)
      |> Enum.sort_by(fn {_, m, _} -> -m end)
      |> Enum.take(15)

    if total != [] do
      names = Bench.Disasm.names(Enum.map(total, &elem(&1, 0)))
      IO.puts("\nhanded to the interpreter by compiled code, over all sites:")
      rows = Enum.zip_with(total, names, fn {_, m, sites}, name -> ["#{Float.round(m, 2)}M", name, sites] end)
      table(["count", "instruction (an example)", "sites"], rows)
    end
  end

  defp delta(_, nil, _), do: "-"

  defp delta(now, base, spread) do
    pct = 100 * (now - base) / max(base, 1)
    mark = if abs(pct) <= max(spread, 5.0), do: " ~", else: if(pct < 0, do: " faster", else: " slower")
    "#{if pct >= 0, do: "+", else: ""}#{Float.round(pct, 1)}%#{mark}"
  end

  defp load(out, name) do
    path = Path.join(out, "sites-#{name}.json")
    File.exists?(path) or die("no saved results #{path}")
    path |> File.read!() |> JSON.decode!()
  end

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

  def die(msg) do
    IO.puts(:stderr, "sites: #{msg}")
    System.halt(1)
  end
end

# Encodings to names: assembled as .inst by clang, disassembled by llvm-objdump (or objdump)
defmodule Bench.Disasm do
  def names(insns) do
    dir = Path.join(System.tmp_dir!(), "armless-sites-#{System.unique_integer([:positive])}")
    File.mkdir_p!(dir)

    try do
      src = Path.join(dir, "i.S")
      obj = Path.join(dir, "i.o")
      File.write!(src, Enum.map_join(insns, "", &"  .inst #{&1}\n"))
      unknown = Enum.map(insns, &("0x" <> Integer.to_string(&1, 16)))

      with {_, 0} <- System.cmd("clang", ["--target=aarch64-none-elf", "-c", src, "-o", obj], stderr_to_stdout: true),
           objdump when objdump != nil <- Enum.find(["llvm-objdump", "objdump"], &System.find_executable/1),
           {text, 0} <- System.cmd(objdump, ["-d", "--no-show-raw-insn", obj], stderr_to_stdout: true) do
        lines = Regex.scan(~r/^\s+[0-9a-f]+:\s+(.+)$/m, text, capture: :all_but_first) |> Enum.map(&(&1 |> hd() |> String.trim()))
        if length(lines) == length(insns), do: lines, else: unknown
      else
        _ -> unknown
      end
    after
      File.rm_rf(dir)
    end
  end
end

Sites.main(System.argv())
