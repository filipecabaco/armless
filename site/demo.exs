#!/usr/bin/env elixir

# Puts what the site's machines need into site/static/demo (git-ignored):
#
# - armless.js, armless.wasm and armless-smp.wasm from armless's latest release
# - xterm.js, for the console
# - the guest: a Linux kernel and an initramfs with a static busybox and site/guest/*, from a
#   directory where tests/linux/build.sh and build-rootfs.sh put Image and busybox
# - fs/: a filesystem with that busybox, for the user-mode examples (tests/fs's tools)
# - native.bin.zst, given ARMLESS_RUN: the guest booted and saved by a native armless-run at its
#   shell prompt, for the examples that restore it. Build armless-run from the release's tag, so
#   the snapshot is the format the release's wasm reads.
#
#   elixir site/demo.exs KERNEL_DIR [ARMLESS_RUN]
#
# Without the release files or the kernel the build goes on: the pages say the machine is not
# part of it. Without ARMLESS_RUN the examples that restore the native snapshot say so.

Mix.install([{:req, "~> 0.5"}])
Logger.configure(level: :info)

defmodule Demo do
  @release "https://api.github.com/repos/filipecabaco/armless/releases/latest"
  @xterm "https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0"
  @fit "https://cdn.jsdelivr.net/npm/@xterm/addon-fit@0.10.0"
  # armless's limit; the page gives the machine as many of these as the device can spare (cpus.js)
  @max_cpus 8
  @memory_mb 256
  @cmdline "console=hvc0 quiet rdinit=/init"

  def run([kernel_dir | rest]) when length(rest) <= 1 do
    root = Path.expand("..", __DIR__)
    out = Path.join([__DIR__, "static", "demo"])
    File.rm_rf!(out)
    File.mkdir_p!(out)

    # armless itself: as built from this checkout (ARMLESS_BUILD: a directory with armless.js,
    # armless.wasm and armless-smp.wasm, as build-wasm.sh makes them, and armless-gpu.js), or as
    # released
    version =
      case System.get_env("ARMLESS_BUILD") do
        dir when is_binary(dir) and dir != "" ->
          for name <- ~w(armless.js armless.wasm armless-smp.wasm armless-gpu.js) do
            save(out, name, File.read!(Path.join(dir, name)))
          end

          System.get_env("ARMLESS_VERSION", "this checkout")

        _ ->
          release = get!(@release).body
          assets = Map.new(release["assets"], &{&1["name"], &1["browser_download_url"]})

          for name <- ~w(armless.js armless.wasm armless-smp.wasm armless-gpu.js) do
            url = Map.get(assets, name) || raise "#{release["tag_name"]} has no #{name}"
            save(out, name, get!(url, decode_body: false).body)
          end

          release["tag_name"]
      end

    for {url, name} <- [
          {@xterm <> "/lib/xterm.js", "xterm.js"},
          {@xterm <> "/css/xterm.css", "xterm.css"},
          {@fit <> "/lib/addon-fit.js", "addon-fit.js"}
        ] do
      save(out, name, get!(url, decode_body: false).body)
    end

    # The guest: the kernel, gzipped (armless boots Image, gzip, zstd or EFI zboot) ...
    image = File.read!(Path.join(kernel_dir, "Image"))
    save(out, "Image.gz", :zlib.gzip(image))

    # ... and an initramfs: busybox, the init script and the trench run, and the GPU example's
    # program when it was built (GPU_DEMO: site/guest/gpu.cpp compiled for the guest, pages.yml)
    guest = Path.join(__DIR__, "guest")

    gpu =
      case System.get_env("GPU_DEMO") do
        path when is_binary(path) and path != "" -> ["bin/gpu=" <> Path.expand(path)]
        _ -> []
      end

    {_, 0} =
      System.cmd(
        "python3",
        [
          Path.join([root, "tests", "linux", "mkcpio.py"]),
          Path.join(out, "initrd.cpio"),
          "init=" <> Path.join(guest, "init"),
          "bin/busybox=" <> Path.join(kernel_dir, "busybox"),
          "bin/trench=" <> Path.join(guest, "trench"),
          "usr/share/trench.awk=" <> Path.join(guest, "trench.awk")
        ] ++ gpu ++ ["proc/", "mnt/", "sys/", "tmp/", "root/"]
      )

    IO.puts("demo: initrd.cpio #{File.stat!(Path.join(out, "initrd.cpio")).size} bytes")

    filesystem(root, out, Path.join(kernel_dir, "busybox"))
    python? = python_filesystem(root, out, Path.join(kernel_dir, "busybox"))

    native? =
      case rest do
        [armless_run] -> native_snapshot(out, Path.expand(armless_run), image)
        [] -> false
      end

    File.write!(
      Path.join(out, "config.json"),
      JSON.encode!(%{
        armless: version,
        linux: linux_version(image),
        max_cpus: @max_cpus,
        memory_mb: @memory_mb,
        cmdline: @cmdline,
        native: native?,
        python: python?,
        gpu: gpu != []
      })
    )

    IO.puts("demo: ready, #{version}, up to #{@max_cpus} CPUs#{if native?, do: ", a native snapshot", else: ""}")
  end

  def run(_), do: raise("usage: elixir site/demo.exs KERNEL_DIR [ARMLESS_RUN]")

  # Applets the user-mode examples call by name: each a link to busybox, as `busybox --install`
  # would make (the build can't run an arm64 busybox to ask it for its list)
  @applets ~w(sh ash cat date echo env free grep head hostname id kill ls mkdir od ps rm
              sleep tail top touch uname uptime wc wget)

  # The user-mode examples' filesystem: busybox and the few files a shell and wget look for, as
  # fs/filesystem.json and its blobs in fs/blobs (tests/fs's fs2json.py and copy-to-sha256.py)
  defp filesystem(root, out, busybox) do
    tree = Path.join(System.tmp_dir!(), "armless-site-fs-#{System.unique_integer([:positive])}")
    File.rm_rf!(tree)

    try do
      for dir <- ~w(bin etc root tmp), do: File.mkdir_p!(Path.join(tree, dir))
      File.cp!(busybox, Path.join([tree, "bin", "busybox"]))
      File.chmod!(Path.join([tree, "bin", "busybox"]), 0o755)
      for applet <- @applets, do: File.ln_s!("busybox", Path.join([tree, "bin", applet]))

      File.write!(Path.join([tree, "etc", "passwd"]), "root:x:0:0:root:/root:/bin/sh\n")
      File.write!(Path.join([tree, "etc", "group"]), "root:x:0:\n")
      File.write!(Path.join([tree, "etc", "hosts"]), "127.0.0.1 localhost\n")
      # Any address: armless answers the program's DNS queries itself
      File.write!(Path.join([tree, "etc", "resolv.conf"]), "nameserver 10.0.2.3\n")

      fs = Path.join(out, "fs")
      File.mkdir_p!(Path.join(fs, "blobs"))
      tools = Path.join([root, "tests", "fs"])
      {_, 0} = System.cmd("python3", [Path.join(tools, "fs2json.py"), "--zstd", "--out", Path.join(fs, "filesystem.json"), tree], stderr_to_stdout: true)
      {_, 0} = System.cmd("python3", [Path.join(tools, "copy-to-sha256.py"), "--zstd", tree, Path.join(fs, "blobs")], stderr_to_stdout: true)
      IO.puts("demo: fs/ #{length(File.ls!(Path.join(fs, "blobs")))} blobs")
    after
      File.rm_rf!(tree)
    end
  end

  # The guest booted natively, on one CPU, and saved at its shell prompt once it has written
  # where and when that happened to /root/origin. The pages restore it as it was saved: one CPU,
  # the same memory, a console and no network.
  defp native_snapshot(out, armless_run, image) do
    dir = Path.join(System.tmp_dir!(), "armless-site-native-#{System.unique_integer([:positive])}")
    File.mkdir_p!(dir)

    try do
      kernel = Path.join(dir, "Image")
      File.write!(kernel, image)
      raw = Path.join(dir, "native.bin")
      host = :erlang.system_info(:system_architecture) |> to_string()
      at = DateTime.utc_now() |> Calendar.strftime("%Y-%m-%d %H:%M UTC")
      # SA''VED: the shell echoes the line as typed, so only the command's output says SAVED
      input = "echo 'Booted and saved by a native armless-run on #{host}, #{at}.' > /root/origin; echo SA''VED\n"

      {log, status} =
        System.cmd(
          armless_run,
          ["--kernel", kernel, "--initrd", Path.join(out, "initrd.cpio"), "--append", @cmdline,
           "-m", "#{@memory_mb}", "--smp", "1", "--console-after", "# ", "--console-input", input,
           "--save", raw, "--save-after", "SAVED", "--timeout", "300"],
          stderr_to_stdout: true
        )

      if status != 0, do: raise("armless-run exited #{status}: #{String.slice(log, -800, 800)}")
      {_, 0} = System.cmd("zstd", ["-q", "-19", "-o", Path.join(out, "native.bin.zst"), raw])
      IO.puts("demo: native.bin.zst #{File.stat!(Path.join(out, "native.bin.zst")).size} bytes (#{host})")
      true
    rescue
      e ->
        IO.warn("demo: no native snapshot (#{Exception.message(e)}); its examples will say so")
        false
    after
      File.rm_rf!(dir)
    end
  end

  defp linux_version(image) do
    case Regex.run(~r/Linux version (\d+\.\d+(?:\.\d+)?)/, image) do
      [_, v] -> v
      nil -> nil
    end
  end

  # The race's filesystem (examples/no-kernel): Alpine's python3 with what it needs, as its
  # packages have them, and busybox's sh for a subprocess, as py/filesystem.json and its blobs.
  # The same files for both machines: the one with no kernel runs /usr/bin/python3 from it, and
  # the one that boots Linux mounts it over 9p and runs the same file (site/guest/init).
  @alpine "https://dl-cdn.alpinelinux.org/alpine/v3.22/main/aarch64"
  @python ~w(python3 python3-pycache-pyc0)

  defp python_filesystem(root, out, busybox) do
    tree = Path.join(System.tmp_dir!(), "armless-site-py-#{System.unique_integer([:positive])}")
    File.rm_rf!(tree)

    try do
      packages = Alpine.resolve(Alpine.index(get!(@alpine <> "/APKINDEX.tar.gz", decode_body: false).body), @python)

      for {name, version} <- packages do
        Alpine.extract(get!("#{@alpine}/#{name}-#{version}.apk", decode_body: false).body, tree)
      end

      for dir <- ~w(bin etc root tmp proc dev sys), do: File.mkdir_p!(Path.join(tree, dir))
      File.cp!(busybox, Path.join([tree, "bin", "busybox"]))
      File.chmod!(Path.join([tree, "bin", "busybox"]), 0o755)
      for applet <- ~w(sh cat echo ls), do: File.ln_s!("busybox", Path.join([tree, "bin", applet]))
      File.write!(Path.join([tree, "etc", "passwd"]), "root:x:0:0:root:/root:/bin/sh\n")
      File.write!(Path.join([tree, "etc", "group"]), "root:x:0:\n")

      fs = Path.join(out, "py")
      File.mkdir_p!(Path.join(fs, "blobs"))
      tools = Path.join([root, "tests", "fs"])
      {_, 0} = System.cmd("python3", [Path.join(tools, "fs2json.py"), "--zstd", "--out", Path.join(fs, "filesystem.json"), tree], stderr_to_stdout: true)
      {_, 0} = System.cmd("python3", [Path.join(tools, "copy-to-sha256.py"), "--zstd", tree, Path.join(fs, "blobs")], stderr_to_stdout: true)
      names = Enum.map_join(packages, " ", fn {n, v} -> "#{n}-#{v}" end)
      IO.puts("demo: py/ #{length(File.ls!(Path.join(fs, "blobs")))} blobs (#{names})")
      true
    rescue
      e ->
        IO.warn("demo: no python filesystem (#{Exception.message(e)}); the race says so")
        File.rm_rf!(Path.join(out, "py"))
        false
    after
      File.rm_rf!(tree)
    end
  end

  defp save(out, name, body) do
    File.write!(Path.join(out, name), body)
    IO.puts("demo: #{name} #{byte_size(body)} bytes")
  end

  defp get!(url, opts \\ []) do
    # api.github.com refuses a runner's anonymous requests once its address has used its quota
    auth =
      case {URI.parse(url).host, System.get_env("GITHUB_TOKEN")} do
        {"api.github.com", token} when is_binary(token) and token != "" ->
          [auth: {:bearer, token}]

        _ ->
          []
      end

    resp = Req.get!(url, [retry: :transient, max_retries: 2] ++ auth ++ opts)
    if resp.status != 200, do: raise("#{url}: HTTP #{resp.status}")
    resp
  end
end

defmodule Alpine do
  @moduledoc false
  # Alpine's packages, as apk installs them, without apk: the index's records, the packages a
  # set needs (what each depends on, by name or by what another provides), and a package's files.

  # APKINDEX.tar.gz's APKINDEX: one record a package, a blank line between, `K:value` lines
  # (Like an .apk, two gzip streams: its signature, then the index)
  def index(tar_gz) do
    file = Path.join(System.tmp_dir!(), "armless-#{System.unique_integer([:positive])}.tar.gz")
    File.write!(file, tar_gz)

    {text, 0} =
      try do
        System.cmd("tar", ["-xzOf", file, "APKINDEX"])
      after
        File.rm!(file)
      end

    text
    |> String.split("\n\n", trim: true)
    |> Enum.map(fn record ->
      record
      |> String.split("\n", trim: true)
      |> Enum.reduce(%{}, fn line, acc ->
        case String.split(line, ":", parts: 2) do
          [k, v] -> Map.put(acc, k, v)
          _ -> acc
        end
      end)
    end)
  end

  # The packages `wanted` needs, theirs included: [{name, version}], each once
  def resolve(index, wanted) do
    by_name = Map.new(index, &{&1["P"], &1})

    provides =
      for pkg <- index, token <- String.split(pkg["p"] || ""), reduce: %{} do
        acc -> Map.put_new(acc, bare(token), pkg["P"])
      end

    walk(wanted, MapSet.new(), [], by_name, provides)
    |> Enum.reverse()
    |> Enum.map(&{&1, by_name[&1]["V"]})
  end

  defp walk([], _seen, acc, _by_name, _provides), do: acc

  defp walk([want | rest], seen, acc, by_name, provides) do
    name = if Map.has_key?(by_name, want), do: want, else: provides[want]

    cond do
      String.starts_with?(want, "!") -> walk(rest, seen, acc, by_name, provides)
      name == nil -> raise "no Alpine package is or provides #{want}"
      MapSet.member?(seen, name) -> walk(rest, seen, acc, by_name, provides)
      true ->
        deps = (by_name[name]["D"] || "") |> String.split() |> Enum.map(&bare/1)
        walk(deps ++ rest, MapSet.put(seen, name), [name | acc], by_name, provides)
    end
  end

  # A dependency's name, without its version constraint: so:libz.so.1=1.3.1-r0 -> so:libz.so.1
  defp bare(token), do: token |> String.split(~r/[<>=~]/, parts: 2) |> hd()

  # An .apk: gzip streams one after another (signature, control, data), each a piece of one tar
  # with no end in between, which tar reads as one. Its files under `dir`, but apk's own
  # (.PKGINFO, .SIGN.*, ...)
  def extract(apk, dir) do
    File.mkdir_p!(dir)
    file = Path.join(System.tmp_dir!(), "armless-#{System.unique_integer([:positive])}.apk")
    File.write!(file, apk)

    try do
      {_, 0} = System.cmd("tar", ["-xzf", file, "-C", dir], stderr_to_stdout: true)
    after
      File.rm!(file)
    end

    for dot <- File.ls!(dir), String.starts_with?(dot, "."), do: File.rm_rf!(Path.join(dir, dot))
    :ok
  end
end

try do
  Demo.run(System.argv())
rescue
  e ->
    IO.warn("demo: not built (#{Exception.message(e)}); the landing page will say the machine is unavailable")
    File.rm_rf!(Path.join([__DIR__, "static", "demo"]))
    System.halt(0)
end
