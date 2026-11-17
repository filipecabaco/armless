#!/usr/bin/env elixir

# Tests for the site: how site/build.exs cuts and links the docs, that a full build leaves no
# broken link or anchor, and that site/guest/mandel.awk's CPUs together draw exactly the
# picture one CPU draws.
#
#   elixir site/test/site_test.exs
#
# The awk test runs the host's awk; the guest runs busybox's (site/test/guest.mjs boots it).

System.put_env("SITE_BUILD", "test")
Code.require_file("../build.exs", __DIR__)
ExUnit.start(seed: 0)

defmodule SiteTest do
  use ExUnit.Case

  describe "cutting the README" do
    test "splits at ## headings, keeps the intro, and leaves fenced code alone" do
      readme =
        Site.parse_readme("""
        # armless

        The intro.

        ## One

        Text.

        ```sh
        ## not a heading
        ```

        ## Two

        More.
        """)

      assert readme[:intro] == "The intro."
      assert readme["One"] =~ "## not a heading"
      assert readme["Two"] == "More."
      refute Map.has_key?(readme, "not a heading")
    end

    test "a section on its own page has its ### headings promoted, outside code only" do
      body = "### Options\n\ntext\n\n```md\n### keep\n```\n\n#### Deeper"
      promoted = Site.promote(body)
      assert promoted =~ ~r/^## Options$/m
      assert promoted =~ ~r/^### keep$/m
      assert promoted =~ ~r/^### Deeper$/m
    end
  end

  describe "headings" do
    test "get ids, unique within a page, and a named permalink" do
      {html, toc} = Site.add_ids("<h2>Calls</h2><p>x</p><h2>Calls</h2><h3><code>armless-run --stats</code></h3>")
      assert [{2, "calls", "Calls"}, {2, "calls-1", "Calls"}, {3, "armless-run---stats", _}] = toc
      assert html =~ ~s(<h2 id="calls">)
      assert html =~ ~s(<h2 id="calls-1">)
      assert html =~ ~s(aria-label="Link to Calls")
    end

    test "slugs follow GitHub's" do
      assert Site.slug("Quick start") == "quick-start"
      assert Site.slug("The web relay (dev/web)") == "the-web-relay-devweb"
    end
  end

  describe "rendering" do
    test "tables scroll in a labelled region, code blocks are focusable, options can be linked" do
      html =
        Site.render("""
        | option | meaning |
        |---|---|
        | `predecode_mb` | a cap |

        ```sh
        cargo build
        ```

        Use `--boot-fs` here.
        """)

      assert html =~ ~s(<div class="table-wrap" tabindex="0" role="region")
      assert html =~ ~s(<tr id="predecode_mb">)
      assert html =~ ~s(<pre tabindex="0")
      assert html =~ ~s(<code class="nw">--boot-fs</code>)
    end

    test "the landing page's snippet is the README's browser example, escaped" do
      embed = Site.embed(%{"Quick start" => "### Browser\n\n```html\n<script src=\"armless.js\"></script>\n```\n"})
      assert embed == "&lt;script src=&quot;armless.js&quot;&gt;&lt;/script&gt;"
    end
  end

  describe "links" do
    setup do
      page = %{slug: "index", dir: "", html: nil}
      owners = %{"building" => "getting-started", "what-it-has" => "index"}
      files = %{"docs/armless.md" => "design"}
      {:ok, page: page, owners: owners, files: files}
    end

    test "an anchor on another page goes to that page", ctx do
      page = %{ctx.page | html: ~s(<a href="#building">b</a><a href="#what-it-has">w</a>)}
      out = Site.rewrite_links(page, ctx.owners, ctx.files, Path.expand("../..", __DIR__))
      assert out =~ ~s(href="getting-started.html#building")
      assert out =~ ~s(href="#what-it-has")
    end

    test "a docs file becomes its page, a repository file goes to GitHub", ctx do
      page = %{ctx.page | html: ~s(<a href="docs/armless.md#snapshots">d</a><a href="LICENSE">l</a><a href="lib/zstd">z</a><a href="https://v86.example">x</a>)}
      out = Site.rewrite_links(page, ctx.owners, ctx.files, Path.expand("../..", __DIR__))
      assert out =~ ~s(href="design.html#snapshots")
      assert out =~ ~s(href="https://github.com/filipecabaco/armless/blob/main/LICENSE")
      assert out =~ ~s(href="https://github.com/filipecabaco/armless/tree/main/lib/zstd")
      assert out =~ ~s(href="https://v86.example")
    end
  end

  describe "mandel.awk" do
    @awk Path.join([Path.expand("..", __DIR__), "guest", "mandel.awk"])

    defp frame(cpus, w, h, m) do
      for j <- 0..(cpus - 1), reduce: %{} do
        acc ->
          {out, 0} = System.cmd("awk", ~w(-v W=#{w} -v H=#{h} -v m=#{m} -v J=#{j} -v P=#{cpus} -f #{@awk}))

          for [_, row, text] <- Regex.scan(~r/\e\[(\d+);1H([^\e]*)/, out), into: acc do
            {String.to_integer(row), text}
          end
      end
    end

    test "every CPU count draws every row, the same picture as one CPU" do
      for {w, h} <- [{64, 22}, {84, 26}, {40, 11}], m <- [8, 64] do
        one = frame(1, w, h, m)
        assert Map.keys(one) |> Enum.sort() == Enum.to_list(1..h)
        assert Enum.all?(one, fn {_, text} -> String.length(text) == w end)
        for cpus <- [2, 3, 4, 8], do: assert(frame(cpus, w, h, m) == one, "#{cpus} CPUs, #{w}x#{h}, #{m}")
      end
    end

    test "the bottom half mirrors the top" do
      rows = frame(1, 64, 22, 32)
      for y <- 1..11, do: assert(rows[y] == rows[23 - y])
    end
  end
end

defmodule SiteBuildTest do
  use ExUnit.Case

  @site Path.expand("..", __DIR__)

  setup_all do
    Site.run()
    :ok
  end

  test "every page and asset it links to exists, and every anchor is on its page" do
    out = Path.join(@site, "_out")
    pages = Path.wildcard(Path.join(out, "**/*.html")) |> Enum.reject(&String.ends_with?(&1, "404.html"))
    assert length(pages) > 5

    broken =
      for page <- pages,
          [_, ref] <- Regex.scan(~r/(?:href|src)="([^"]+)"/, File.read!(page)),
          not String.starts_with?(ref, ["http://", "https://", "mailto:", "data:"]),
          problem = check(page, ref),
          do: "#{Path.relative_to(page, out)} -> #{ref}: #{problem}"

    assert broken == []
  end

  defp check(page, ref) do
    [path, frag] =
      case String.split(ref, "#", parts: 2) do
        [p] -> [p, nil]
        [p, f] -> [p, f]
      end

    path = path |> String.split("?") |> hd()
    target = if path == "", do: page, else: Path.expand(path, Path.dirname(page))
    target = if File.dir?(target), do: Path.join(target, "index.html"), else: target

    cond do
      not File.exists?(target) -> "missing"
      frag && not (File.read!(target) =~ ~s(id="#{frag}")) -> "no such anchor"
      true -> nil
    end
  end
end
