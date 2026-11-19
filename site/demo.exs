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

    # armless itself, as released
    release = get!(@release).body
    assets = Map.new(release["assets"], &{&1["name"], &1["browser_download_url"]})

    for name <- ~w(armless.js armless.wasm armless-smp.wasm) do
      url = Map.get(assets, name) || raise "#{release["tag_name"]} has no #{name}"
      save(out, name, get!(url, decode_body: false).body)
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

    # ... and an initramfs: busybox, the init script and the trench run
    guest = Path.join(__DIR__, "guest")

    {_, 0} =
      System.cmd("python3", [
        Path.join([root, "tests", "linux", "mkcpio.py"]),
        Path.join(out, "initrd.cpio"),
        "init=" <> Path.join(guest, "init"),
        "bin/busybox=" <> Path.join(kernel_dir, "busybox"),
        "bin/trench=" <> Path.join(guest, "trench"),
        "usr/share/trench.awk=" <> Path.join(guest, "trench.awk"),
        "proc/",
        "sys/",
        "tmp/",
        "root/"
      ])

    IO.puts("demo: initrd.cpio #{File.stat!(Path.join(out, "initrd.cpio")).size} bytes")

    filesystem(root, out, Path.join(kernel_dir, "busybox"))

    native? =
      case rest do
        [armless_run] -> native_snapshot(out, Path.expand(armless_run), image)
        [] -> false
      end

    File.write!(
      Path.join(out, "config.json"),
      JSON.encode!(%{
        armless: release["tag_name"],
        linux: linux_version(image),
        max_cpus: @max_cpus,
        memory_mb: @memory_mb,
        cmdline: @cmdline,
        native: native?
      })
    )

    IO.puts("demo: ready, #{release["tag_name"]}, up to #{@max_cpus} CPUs#{if native?, do: ", a native snapshot", else: ""}")
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

  defp save(out, name, body) do
    File.write!(Path.join(out, name), body)
    IO.puts("demo: #{name} #{byte_size(body)} bytes")
  end

  defp get!(url, opts \\ []) do
    resp = Req.get!(url, [retry: :transient, max_retries: 2] ++ opts)
    if resp.status != 200, do: raise("#{url}: HTTP #{resp.status}")
    resp
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
