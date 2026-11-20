# site/server.exs over a build of its own: a route and template per page, the old static
# addresses sent on to them, the isolation headers on every response, gzip when there is a .gz
# and the client takes it, pages not cached and the machines' files an hour, and the 404 page
# for what isn't there.
#
#   elixir site/test/server_test.exs
#
# site/test/links_test.exs crawls a real build.

dir = Path.join(System.tmp_dir!(), "armless-site-server-#{System.unique_integer([:positive])}")
static = Path.join(dir, "static")
File.mkdir_p!(Path.join(static, "demo"))
File.write!(Path.join([static, "demo", "armless.wasm"]), "wasm")
File.write!(Path.join([static, "demo", "armless.wasm.gz"]), :zlib.gzip("wasm"))
File.write!(Path.join(static, "race.js"), "js")

page = fn slug, title -> %{slug: slug, title: title, group: "Start", desc: "d", html: "<p>#{title} text</p>", toc: [], dir: "", edit: "Readme.md"} end

example = fn slug, title ->
  %{slug: slug, title: title, description: "d", script: "race.js", machine: "race", pane: "/bin/sh", caption: "c", side: "metrics", html: "<h2>#{title} body</h2>"}
end

content = %{
  version: "9.9.9",
  demo?: true,
  embed: "&lt;script&gt;",
  pages: [page.("index", "Overview"), page.("design", "Design notes")],
  examples: [example.("one", "First example"), example.("two", "Second example")]
}

File.write!(Path.join(dir, "site.etf"), :erlang.term_to_binary(content))
System.put_env("SITE_DIR", dir)
System.argv(["--no-start"])
Code.require_file("../server.exs", __DIR__)

ExUnit.start()

defmodule SiteServerTest do
  use ExUnit.Case, async: true
  import Plug.Test
  import Plug.Conn

  @opts Site.init([])

  defp get(path, headers \\ []) do
    Enum.reduce(headers, conn(:get, path), fn {k, v}, c -> put_req_header(c, k, v) end)
    |> Site.call(@opts)
  end

  defp header(conn, name), do: conn |> get_resp_header(name) |> List.first()

  test "every response is cross-origin isolated, a missing page's and a redirect's too" do
    for path <- ["/", "/examples/one", "/demo/armless.wasm", "/nope", "/examples/"] do
      conn = get(path)
      assert header(conn, "cross-origin-opener-policy") == "same-origin", path
      assert header(conn, "cross-origin-embedder-policy") == "require-corp", path
      assert header(conn, "cross-origin-resource-policy") == "same-origin", path
    end
  end

  test "each page is its route, rendered from its template, and isn't cached" do
    for {path, body} <- [
          {"/", "armless 9.9.9"},
          {"/examples", "Second example"},
          {"/examples/one", "First example body"},
          {"/examples/two", "Second example body"},
          {"/docs", "Overview text"},
          {"/docs/design", "Design notes text"}
        ] do
      conn = get(path)
      assert conn.status == 200, path
      assert conn.resp_body =~ body, path
      assert header(conn, "cache-control") == "no-cache", path
      assert header(conn, "content-type") =~ "text/html", path
    end
  end

  test "a page links from the site's root, so it reads the same with or without a trailing slash" do
    list = get("/examples").resp_body
    assert list =~ ~s(href="/examples/one")
    assert list =~ ~s(href="/style.css")
    refute list =~ ~r/(href|src)="(?!\/|#|https?:)/

    ex = get("/examples/one").resp_body
    assert ex =~ ~s(href="/examples/two")
    assert ex =~ ~s(src="/race.js")
    assert ex =~ ~s(data-machine="race")
    refute ex =~ ~r/(href|src)="(?!\/|#|https?:)/

    doc = get("/docs").resp_body
    assert doc =~ ~s(href="/docs/design")
    refute doc =~ ~r/(href|src)="(?!\/|#|https?:)/
  end

  test "the site's old addresses, as static files, go on to its routes (query string too)" do
    for {from, to} <- [
          {"/index.html", "/"},
          {"/examples/", "/examples"},
          {"/examples/index.html", "/examples"},
          {"/examples/one.html", "/examples/one"},
          {"/examples/one/", "/examples/one"},
          {"/docs/", "/docs"},
          {"/docs/index.html", "/docs"},
          {"/docs/index", "/docs"},
          {"/docs/design.html", "/docs/design"},
          {"/examples/?from=x", "/examples?from=x"}
        ] do
      conn = get(from)
      assert {conn.status, header(conn, "location")} == {301, to}, from
    end
  end

  test "the machines' files are kept an hour, and go gzipped to a client that takes it" do
    conn = get("/demo/armless.wasm", [{"accept-encoding", "gzip"}])
    assert conn.status == 200
    assert header(conn, "content-encoding") == "gzip"
    assert :zlib.gunzip(conn.resp_body) == "wasm"
    assert header(conn, "cache-control") == "public, max-age=3600"
    assert header(conn, "content-type") == "application/wasm"
    plain = get("/demo/armless.wasm")
    assert {plain.resp_body, header(plain, "content-encoding")} == {"wasm", nil}
  end

  test "a page's script is looked at again each visit, not kept from the deploy before" do
    conn = get("/race.js")
    assert conn.status == 200
    assert header(conn, "cache-control") == "no-cache"
  end

  test "what isn't there is the 404 page with a 404: no route, no example, no doc" do
    for path <- ["/nope", "/examples/nope", "/docs/nope", "/examples/one/more"] do
      conn = get(path)
      assert conn.status == 404, path
      assert conn.resp_body =~ "That page is not here", path
    end
  end

  test "a path out of the directory is refused (Bandit answers Plug.Static's error with 400)" do
    e = assert_raise(Plug.Static.InvalidPathError, fn -> get("/../server.exs") end)
    assert e.plug_status == 400
  end
end
