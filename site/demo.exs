#!/usr/bin/env elixir

# Puts what the landing page's machine needs into site/static/demo (git-ignored):
#
# - armless.js, armless.wasm and armless-smp.wasm from armless's latest release
# - xterm.js, for the console
# - the guest: a Linux kernel and an initramfs with a static busybox and site/guest/*, from a
#   directory where tests/linux/build.sh and build-rootfs.sh put Image and busybox
#
#   elixir site/demo.exs KERNEL_DIR
#
# Without the release files or the kernel the build goes on: the landing page says the machine
# is not part of it.

Mix.install([{:req, "~> 0.5"}])
Logger.configure(level: :info)

defmodule Demo do
  @release "https://api.github.com/repos/filipecabaco/armless/releases/latest"
  @xterm "https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0"
  @fit "https://cdn.jsdelivr.net/npm/@xterm/addon-fit@0.10.0"
  # armless's limit; the page gives the machine as many of these as the device can spare (cpus.js)
  @max_cpus 8
  @memory_mb 256

  def run([kernel_dir]) do
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

    File.write!(
      Path.join(out, "config.json"),
      JSON.encode!(%{
        armless: release["tag_name"],
        max_cpus: @max_cpus,
        memory_mb: @memory_mb,
        cmdline: "console=hvc0 quiet rdinit=/init"
      })
    )

    IO.puts("demo: ready, #{release["tag_name"]}, up to #{@max_cpus} CPUs")
  end

  def run(_), do: raise("usage: elixir site/demo.exs KERNEL_DIR")

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
