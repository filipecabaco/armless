#!/usr/bin/env elixir
# armless's site: what site/build.exs made (site/_out, or $SITE_DIR), served with the headers
# that make its pages cross-origin isolated. A machine of several CPUs needs that (its CPUs share
# memory between Web Workers through SharedArrayBuffer), so a page doesn't first reload itself
# through sw.js as it must on a host that can't send them.
#
#   elixir site/server.exs              serves on $PORT (default 8080)
#   elixir site/server.exs --no-start   defines the server and returns (site/test/server_test.exs)
#
# Files with a .gz beside them (the Dockerfile makes those) go compressed to a client that takes
# gzip. The machines' files (demo/) are kept an hour; the rest is looked at again each visit.

Mix.install([{:francis, "~> 0.4.0"}])

defmodule Site do
  @dir System.get_env("SITE_DIR", Path.expand("_out", __DIR__))
  @isolation [
    {"cross-origin-opener-policy", "same-origin"},
    {"cross-origin-embedder-policy", "require-corp"},
    {"cross-origin-resource-policy", "same-origin"},
    {"x-content-type-options", "nosniff"},
    {"referrer-policy", "strict-origin-when-cross-origin"}
  ]

  use Francis,
    bandit_opts: [
      port: String.to_integer(System.get_env("PORT", "8080")),
      ip: :any,
      # (One small machine: a few acceptors are plenty)
      thousand_island_options: [num_acceptors: 4]
    ],
    # (Each request's line, below the logger's level: a busy page makes a lot of them)
    log_level: :debug

  plug(:index)
  plug(:headers)

  plug(Plug.Static,
    at: "/",
    from: @dir,
    gzip: true,
    cache_control_for_etags: "public, max-age=3600"
  )

  get("/healthz", fn _ -> "ok" end)

  unmatched(fn conn ->
    page = Path.join(@dir, "404.html")

    conn
    |> Plug.Conn.put_resp_content_type("text/html")
    |> Plug.Conn.send_file(404, page)
  end)

  def dir, do: @dir

  # A directory's path is its index.html, as a static host serves it: /, /examples/, /docs
  def index(%Plug.Conn{path_info: path} = conn, _) do
    cond do
      path == [] -> %{conn | path_info: ["index.html"]}
      List.last(path) =~ "." -> conn
      File.regular?(Path.join([@dir | path] ++ ["index.html"])) -> %{conn | path_info: path ++ ["index.html"]}
      true -> conn
    end
  end

  # The isolation headers on everything. Only the machines' files (demo/: the kernel, the wasm,
  # snapshots, blobs) are kept an hour; pages, their scripts and styles are looked at again each
  # visit (an ETag makes that cheap), so a page never runs with a script of the deploy before.
  # Set as the response is sent, after Plug.Static's own.
  def headers(conn, _) do
    conn
    |> Plug.Conn.merge_resp_headers(@isolation)
    |> Plug.Conn.register_before_send(fn conn ->
      if machine?(conn), do: conn, else: Plug.Conn.put_resp_header(conn, "cache-control", "no-cache")
    end)
  end

  defp machine?(%Plug.Conn{path_info: ["demo" | _]}), do: true
  defp machine?(_), do: false
end

Logger.configure(level: :info)

if "--no-start" not in System.argv() do
  {:ok, _} = Supervisor.start_link([Site], strategy: :one_for_one)
  IO.puts("site: #{Site.dir()} on port #{System.get_env("PORT", "8080")}")
  Process.sleep(:infinity)
end
