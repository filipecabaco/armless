# A real build (site/_out, from `elixir site/build.exs`) as site/server.exs serves it: every
# page reached from the landing page, every link and asset on them answered 200 by the server
# itself (never a redirect, so no page links to an old address), every anchor on its page, and
# each example's page with the parts machine.js looks for.
#
#   elixir site/build.exs && elixir site/test/links_test.exs

out = Path.expand("../_out", __DIR__)
File.exists?(Path.join(out, "site.etf")) || raise "no build in #{out}: run elixir site/build.exs first"
System.put_env("SITE_DIR", out)
System.argv(["--no-start"])
Code.require_file("../server.exs", __DIR__)

ExUnit.start()

defmodule SiteLinksTest do
  use ExUnit.Case

  @opts Site.init([])

  defp get(path), do: Plug.Test.conn(:get, path) |> Site.call(@opts)

  defp local_refs(html) do
    for [_, ref] <- Regex.scan(~r/(?:href|src)="([^"]+)"/, html),
        not String.starts_with?(ref, ["http://", "https://", "mailto:", "data:"]),
        uniq: true,
        do: ref
  end

  # Every page reachable from /, by following its links: %{path => html}
  defp crawl(queue, seen \\ %{})
  defp crawl([], seen), do: seen

  defp crawl([path | rest], seen) do
    if Map.has_key?(seen, path) do
      crawl(rest, seen)
    else
      conn = get(path)
      html? = conn.status == 200 and (Plug.Conn.get_resp_header(conn, "content-type") |> List.first("")) =~ "text/html"
      seen = Map.put(seen, path, {conn.status, html? && conn.resp_body})

      next =
        if html?,
          do: for(ref <- local_refs(conn.resp_body), p = page_of(ref, path), p != path, do: p),
          else: []

      crawl(rest ++ next, seen)
    end
  end

  defp page_of(ref, from) do
    case ref |> String.split("#", parts: 2) |> hd() |> String.split("?") |> hd() do
      "" -> from
      "/" <> _ = p -> p
      other -> raise "#{from} links #{ref}: not a path from the site's root (#{other})"
    end
  end

  setup_all do
    {:ok, pages: crawl(["/"])}
  end

  test "every page, link and asset reached from the landing page is answered 200", %{pages: pages} do
    broken = for {path, {status, _}} <- pages, status != 200, do: "#{path}: #{status}"
    assert broken == []
    assert Enum.count(pages, fn {_, {_, html}} -> html end) > 15
  end

  test "every anchor is on its page", %{pages: pages} do
    missing =
      for {from, {_, html}} <- pages,
          html,
          ref <- local_refs(html),
          [path, frag] <- [String.split(ref, "#", parts: 2)],
          target = if(path == "", do: from, else: path),
          {_, target_html} = Map.fetch!(pages, target),
          target_html,
          not (target_html =~ ~s(id="#{frag}")),
          do: "#{from} -> #{ref}"

    assert missing == []
  end

  test "every example has a page, linked from the examples page and the landing page", %{pages: pages} do
    examples = Site.content().examples
    assert examples != []
    {_, list} = pages["/examples"]
    {_, landing} = pages["/"]

    for ex <- examples do
      assert {200, html} = pages["/examples/#{ex.slug}"]
      assert html =~ ~s(data-machine="#{ex.machine}")
      assert list =~ ~s(href="/examples/#{ex.slug}")
      assert landing =~ ~s(href="/examples/#{ex.slug}")
    end
  end

  test "every doc page is reached", %{pages: pages} do
    for p <- Site.content().pages, do: assert({200, _} = pages[Site.doc_path(p.slug)])
  end

  test "a page with a machine has every part machine.js looks for", %{pages: pages} do
    for {path, {_, html}} <- pages, html, html =~ "data-machine=" do
      # Every console, and the figures where a page shows them
      console = ~w(status keys terminal log fallback one-cpu)
      figures = if html =~ ~s(data-part="cpus"), do: ~w(m-mips m-cpus m-cores m-jit m-linear chart), else: []

      for part <- console ++ figures do
        assert html =~ ~s(data-part="#{part}"), "#{path}: no #{part}"
      end
    end
  end
end
