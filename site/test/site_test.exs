#!/usr/bin/env elixir

# Tests for the site: how site/build.exs cuts and links the docs, that a full build leaves no
# broken link or anchor, and that site/guest/trench.awk's CPUs together draw exactly the
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

  describe "examples" do
    test "front matter is key: value lines, the rest is the page's markdown" do
      {meta, body} = Site.front_matter("---\ntitle: A: b\nscript: x.js\n\n---\n\n## Body\n")
      assert meta == %{"title" => "A: b", "script" => "x.js"}
      assert body == "## Body"
      assert_raise RuntimeError, ~r/no front matter/, fn -> Site.front_matter("## Body\n") end
    end

    test "every example has its front matter, and the script it names is in site/static" do
      for path <- Path.wildcard(Path.join([Path.expand("..", __DIR__), "examples", "*.md"])) do
        ex = Site.example(path)
        assert File.exists?(Path.join([Path.expand("..", __DIR__), "static", ex.script])), "#{ex.slug}: no #{ex.script}"
        assert ex.html =~ "<h2"
        # the script mounts the machine marked with its own name
        assert File.read!(Path.join([Path.expand("..", __DIR__), "static", ex.script])) =~ ~s(data-machine="#{ex.machine}")
      end
    end

    test "the list links each example from where it is shown, numbered, escaped" do
      list = Site.example_list([%{slug: "a", title: "A <b>", description: "d"}, %{slug: "b", title: "B", description: "e"}], "examples/")
      assert list =~ ~s(href="examples/a.html")
      assert list =~ ~s(<span class="n">02</span>)
      assert list =~ "A &lt;b&gt;"
    end
  end

  describe "trench.awk" do
    @awk Path.join([Path.expand("..", __DIR__), "guest", "trench.awk"])
    @run 120

    defp frame(cpus, w, h, f, seed) do
      for j <- 0..(cpus - 1), reduce: %{} do
        acc ->
          {out, 0} = System.cmd("awk", ~w(-v W=#{w} -v H=#{h} -v J=#{j} -v P=#{cpus} -v f=#{f} -v F=#{@run} -v SEED=#{seed} -f #{@awk}))

          for [_, row, text] <- Regex.scan(~r/\e\[(\d+);1H((?:[^\e]|\e\[[0-9;]*m)*)/, out), into: acc do
            {String.to_integer(row), text}
          end
      end
    end

    defp plain(text), do: String.replace(text, ~r/\e\[[0-9;]*m/, "")

    test "every CPU count draws every row, the same picture as one CPU" do
      # frames at the start, among the fighters, with the torpedo going in and in the explosion
      for {w, h} <- [{64, 22}, {84, 26}, {40, 11}], f <- [0, 37, 100, 110, 116], seed <- [1, 424_242] do
        one = frame(1, w, h, f, seed)
        assert Map.keys(one) |> Enum.sort() == Enum.to_list(1..h)
        assert Enum.all?(one, fn {_, text} -> String.length(plain(text)) == w end)
        for cpus <- [2, 3, 8], do: assert(frame(cpus, w, h, f, seed) == one, "#{cpus} CPUs, #{w}x#{h}, frame #{f}, seed #{seed}")
      end
    end

    test "every run is a different trench, and a frame is the same each time it is drawn" do
      assert frame(1, 64, 22, 30, 1) != frame(1, 64, 22, 30, 2)
      assert frame(1, 64, 22, 30, 7) == frame(1, 64, 22, 30, 7)
      assert frame(1, 64, 22, 30, 7) != frame(1, 64, 22, 31, 7)
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

  test "every example's side pane is a template, and each page's scripts are in the build" do
    out = Path.join(@site, "_out")

    for path <- Path.wildcard(Path.join([@site, "examples", "*.md"])) do
      ex = Site.example(path)
      assert File.exists?(Path.join([@site, "templates", "side", ex.side <> ".html.eex"])), "#{ex.slug}: no side #{ex.side}"
      html = File.read!(Path.join([out, "examples", ex.slug <> ".html"]))
      assert html =~ ~s(data-machine="#{ex.machine}")
    end
  end

  test "every example has a page, listed on the examples page and the landing page" do
    out = Path.join(@site, "_out")
    examples = Path.wildcard(Path.join([@site, "examples", "*.md"])) |> Enum.map(&Path.basename(&1, ".md"))
    assert examples != []
    list = File.read!(Path.join([out, "examples", "index.html"]))
    landing = File.read!(Path.join(out, "index.html"))

    for slug <- examples do
      assert File.exists?(Path.join([out, "examples", slug <> ".html"]))
      assert list =~ ~s(href="#{slug}.html")
      assert landing =~ ~s(href="examples/#{slug}.html")
    end
  end

  test "a page with a machine has every part machine.js looks for" do
    out = Path.join(@site, "_out")

    for page <- [Path.join(out, "index.html") | Path.wildcard(Path.join([out, "examples", "*.html"]))],
        html = File.read!(page),
        html =~ "data-machine=" do
      # Every console, and the figures where a page shows them
      console = ~w(status keys terminal log fallback one-cpu)
      figures = if html =~ ~s(data-part="cpus"), do: ~w(m-mips m-cpus m-cores m-jit m-linear chart), else: []

      for part <- console ++ figures do
        assert html =~ ~s(data-part="#{part}"), "#{Path.relative_to(page, out)}: no #{part}"
      end
    end
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
