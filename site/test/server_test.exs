# site/server.exs over a directory of its own: the isolation headers on every response, a
# directory's index.html, gzip when there is a .gz and the client takes it, pages not cached and
# the rest an hour, and 404.html for what isn't there.
#
#   elixir site/test/server_test.exs

dir = Path.join(System.tmp_dir!(), "armless-site-server-#{System.unique_integer([:positive])}")
File.mkdir_p!(Path.join(dir, "examples"))
File.write!(Path.join(dir, "index.html"), "<h1>landing</h1>")
File.write!(Path.join(dir, "404.html"), "<h1>not here</h1>")
File.write!(Path.join([dir, "examples", "index.html"]), "<h1>examples</h1>")
File.mkdir_p!(Path.join(dir, "demo"))
File.write!(Path.join([dir, "demo", "armless.wasm"]), "wasm")
File.write!(Path.join([dir, "demo", "armless.wasm.gz"]), :zlib.gzip("wasm"))
File.write!(Path.join(dir, "race.js"), "js")
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

  test "every response is cross-origin isolated, a missing file's too" do
    for path <- ["/", "/demo/armless.wasm", "/nope"] do
      conn = get(path)
      assert header(conn, "cross-origin-opener-policy") == "same-origin", path
      assert header(conn, "cross-origin-embedder-policy") == "require-corp", path
      assert header(conn, "cross-origin-resource-policy") == "same-origin", path
    end
  end

  test "a directory is its index.html, and a page isn't cached" do
    for {path, body} <- [{"/", "landing"}, {"/examples/", "examples"}, {"/examples", "examples"}] do
      conn = get(path)
      assert conn.status == 200, path
      assert conn.resp_body =~ body, path
      assert header(conn, "cache-control") == "no-cache", path
      assert header(conn, "content-type") =~ "text/html", path
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

  test "what isn't there is 404.html with a 404" do
    conn = get("/examples/nope.html")
    assert conn.status == 404
    assert conn.resp_body =~ "not here"
  end

  test "a path out of the directory is refused (Bandit answers Plug.Static's error with 400)" do
    e = assert_raise(Plug.Static.InvalidPathError, fn -> get("/../server.exs") end)
    assert e.plug_status == 400
  end
end
