#!/usr/bin/env elixir

# Fetches what the landing page's machine needs into site/static/demo (git-ignored):
# armless.js and armless.wasm, the filesystem tree of a published snowglobe site, and
# xterm.js. The machine is a program in armless's user mode (busybox's sh from that
# site's Alpine filesystem); the filesystem's blobs stay where snowglobe keeps them and
# are fetched by the page on first read.
#
#   elixir site/demo.exs [SNOWGLOBE_SITE_URL]
#
# A failed download leaves the demo out and the build goes on: the landing page says so.

Mix.install([{:req, "~> 0.5"}])

defmodule Demo do
  @site "https://filipecabaco.github.io/snowglobe/curl/"
  @xterm "https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0"
  @fit "https://cdn.jsdelivr.net/npm/@xterm/addon-fit@0.10.0"

  def run(argv) do
    site = argv |> List.first(@site) |> String.trim_trailing("/") |> Kernel.<>("/")
    out = Path.join([__DIR__, "static", "demo"])
    File.mkdir_p!(out)

    index = get!(site).body
    baseurl = capture!(index, ~r/baseurl:\s*"([^"]+)"/, "the filesystem's blob address")
    memory_mb = capture!(index, ~r/memory_mb:\s*(\d+)/, "memory_mb") |> String.to_integer()

    files = [
      {site <> "armless/armless.js", "armless.js"},
      {site <> "armless/armless.wasm", "armless.wasm"},
      {site <> "system/filesystem.json", "filesystem.json"},
      {@xterm <> "/lib/xterm.js", "xterm.js"},
      {@xterm <> "/css/xterm.css", "xterm.css"},
      {@fit <> "/lib/addon-fit.js", "addon-fit.js"}
    ]

    for {url, name} <- files do
      body = get!(url, decode_body: false).body
      File.write!(Path.join(out, name), body)
      IO.puts("demo: #{name} #{byte_size(body)} bytes")
    end

    # One blob has to answer, or the machine could not start
    probe = baseurl <> (File.read!(Path.join(out, "filesystem.json")) |> first_blob!())
    %{status: 200} = Req.head!(probe)

    File.write!(
      Path.join(out, "config.json"),
      JSON.encode!(%{source: site, baseurl: baseurl, memory_mb: memory_mb, fetched: Date.utc_today()})
    )

    IO.puts("demo: ready, blobs from #{baseurl}")
  end

  defp get!(url, opts \\ []) do
    resp = Req.get!(url, [retry: :transient, max_retries: 2] ++ opts)
    if resp.status != 200, do: raise("#{url}: HTTP #{resp.status}")
    resp
  end

  defp capture!(text, regex, what) do
    case Regex.run(regex, text) do
      [_, v] -> v
      _ -> raise "demo: could not find #{what} in the site's page"
    end
  end

  defp first_blob!(json) do
    case Regex.run(~r/"([0-9a-f]{10}\.bin\.zst)"/, json) do
      [_, blob] -> blob
      _ -> raise "demo: no blob in filesystem.json"
    end
  end
end

try do
  Demo.run(System.argv())
rescue
  e ->
    IO.warn("demo: not fetched (#{Exception.message(e)}); the landing page will say the machine is unavailable")
    File.rm_rf!(Path.join([__DIR__, "static", "demo"]))
    System.halt(0)
end
