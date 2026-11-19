#!/usr/bin/env elixir

# Builds the armless site into site/_out.
#
#   elixir site/build.exs
#
# The docs are not written twice: every page is cut from Readme.md (by its
# `##` headings) or is one of the files under docs/. The landing page is
# site/templates/index.html.eex, written by hand. Each example is a page of its own,
# examples/<slug>.html, from site/examples/<slug>.md (see @examples), listed on
# examples/index.html and the landing page.
#
# SITE_BASE is the path the site is served from ("/armless/" on GitHub Pages).
# Only the 404 page needs it, because it is served from any depth.

Mix.install([{:mdex, "~> 0.13"}])

defmodule Site do
  @repo "https://github.com/filipecabaco/armless"
  # Where the site is published; share cards need absolute URLs
  @site "https://filipecabaco.github.io/armless/"

  # {slug, nav title, group, sources, description}
  # A source is :intro (Readme.md before its first `##`), {:section, "Heading"}
  # or {:file, "docs/x.md"}.
  @pages [
    {"index", "Overview", "Start", [:intro, {:section, "What it has"}],
     "What armless is, and what the machine has: CPU, backends, devices, filesystem, network, snapshots."},
    {"getting-started", "Getting started", "Start",
     [{:section, "Quick start"}, {:section, "Building"}],
     "Run armless natively with armless-run or in a browser page with armless.js, and build the wasm files."},
    {"browser-api", "Browser API", "Use", [{:section, "Browser API", promote: true}],
     "Options, events and calls of the armless.js Armless class."},
    {"native-api", "Native API", "Use", [{:section, "Native API", promote: true}],
     "Using the armless crate from Rust: NativeHost, Runner and the egress proxy settings."},
    {"how-it-works", "How it works", "Internals", [{:section, "How it works", promote: true}],
     "The interpreter, the JIT, hardware backends, the MMU, SMP, devices, snapshots and the browser memory model."},
    {"design", "Design notes", "Internals", [{:file, "docs/armless.md"}],
     "The address map, the host interface, the 9p filesystem, the web relay, threads and the snapshot format."},
    {"map", "Where things are", "Internals", [{:file, "docs/map.md"}],
     "Which module to look in to change something, and which tests say it still works."},
    {"hypervisor", "Hardware virtualization", "Internals", [{:file, "docs/hypervisor.md"}],
     "Running guests on the host's own cores with Hypervisor.framework or KVM."},
    {"profiling", "Measuring", "Internals", [{:file, "docs/profiling.md"}],
     "Where the time goes: armless-run --stats, the benchmark guest, real workloads and criterion."},
    {"testing", "Testing and releases", "Project",
     [{:section, "Filesystem images"}, {:section, "Testing"}, {:section, "Releases"}],
     "Building test filesystems, the test suites, and how a release is published."}
  ]

  # The examples, in the order they are listed: site/examples/<slug>.md, each a page with a
  # machine on it. Its front matter has the title, the description (the page's claim, and its
  # share card's text), the script under site/static that drives the machine, the console pane's
  # name and the caption under the machine, and optionally `side`, the pane beside the console
  # (site/templates/side/<side>.html.eex, metrics by default); its body is what the page says
  # below the machine.
  @examples ~w(trench-run native-snapshot fresh-randomness time-asleep no-kernel egress)

  # The landing page's machine: the trench run, as on its example page
  @landing_caption "Linux 6.18, booted in this tab from an initramfs with busybox, with as many CPUs as this device can spare. One awk per CPU draws a trench run, a new one every time. Ctrl+c stops it. Escape, then Tab, leaves the console."

  def run do
    # ARMLESS_SRC points at another checkout of the repository to cut the docs from
    root = System.get_env("ARMLESS_SRC") || Path.expand("..", __DIR__)
    out = Path.join(__DIR__, "_out")
    File.rm_rf!(out)
    File.mkdir_p!(Path.join(out, "docs"))
    File.cp_r!(Path.join(__DIR__, "static"), out)
    File.write!(Path.join(out, ".nojekyll"), "")

    readme = parse_readme(File.read!(Path.join(root, "Readme.md")))
    version = version(Path.join(root, "Cargo.toml"))
    demo? = File.exists?(Path.join([__DIR__, "static", "demo", "config.json"]))

    pages =
      for {slug, title, group, sources, desc} <- @pages do
        {md, src_dir, edit} = assemble(root, readme, sources)
        {html, toc} = md |> render() |> add_ids()
        %{slug: slug, title: title, group: group, desc: desc, html: html, toc: toc, dir: src_dir, edit: edit}
      end

    owners = for p <- pages, {_, id, _} <- p.toc, into: %{}, do: {id, p.slug}
    files = for p <- pages, f <- source_files(p), into: %{}, do: {f, p.slug}

    pages = Enum.map(pages, &%{&1 | html: rewrite_links(&1, owners, files, root)})

    layout = template("layout.html.eex")

    for {p, i} <- Enum.with_index(pages) do
      prev = if i > 0, do: Enum.at(pages, i - 1)
      next = Enum.at(pages, i + 1)

      html =
        EEx.eval_file(layout,
          assigns: [
            page: p,
            nav: nav(pages, p.slug),
            prev: prev,
            next: next,
            version: version,
            repo: @repo,
            site: @site
          ]
        )

      File.write!(Path.join([out, "docs", p.slug <> ".html"]), html)
    end

    examples = Enum.map(@examples, &example(Path.join([__DIR__, "examples", &1 <> ".md"])))
    File.mkdir_p!(Path.join(out, "examples"))

    for {ex, i} <- Enum.with_index(examples) do
      html =
        EEx.eval_file(template("example.html.eex"),
          assigns: [
            ex: ex,
            n: number(i + 1),
            machine: machine(ex.machine, ex.pane, ex.caption, demo?, ex.side),
            prev: if(i > 0, do: Enum.at(examples, i - 1)),
            next: Enum.at(examples, i + 1),
            demo?: demo?,
            version: version,
            repo: @repo,
            site: @site
          ]
        )

      File.write!(Path.join([out, "examples", ex.slug <> ".html"]), html)
    end

    File.write!(
      Path.join([out, "examples", "index.html"]),
      EEx.eval_file(template("examples.html.eex"),
        assigns: [list: example_list(examples, ""), version: version, repo: @repo, site: @site]
      )
    )

    File.write!(
      Path.join(out, "index.html"),
      EEx.eval_file(template("index.html.eex"),
        assigns: [
          version: version,
          repo: @repo,
          site: @site,
          demo?: demo?,
          embed: embed(readme),
          machine: machine("trench", "/bin/sh", @landing_caption, demo?, "metrics"),
          examples: example_list(examples, "examples/")
        ]
      )
    )

    base = System.get_env("SITE_BASE", "/armless/")

    File.write!(
      Path.join(out, "404.html"),
      EEx.eval_file(template("404.html.eex"), assigns: [base: base, repo: @repo])
    )

    IO.puts("site: #{length(pages)} doc pages, #{length(examples)} examples + landing into #{Path.relative_to_cwd(out)}")
  end

  # ---- examples ------------------------------------------------------------

  @example_keys ~w(title description script pane caption)

  @doc false
  def example(path) do
    {meta, body} = front_matter(File.read!(path))
    slug = Path.basename(path, ".md")

    for key <- @example_keys, not Map.has_key?(meta, key) do
      raise "#{Path.relative_to_cwd(path)}: no #{key} in its front matter"
    end

    {html, _toc} = body |> render() |> add_ids()

    %{
      slug: slug,
      title: meta["title"],
      description: meta["description"],
      script: meta["script"],
      # trench.js drives the machine marked data-machine="trench"
      machine: Path.basename(meta["script"], ".js"),
      pane: meta["pane"],
      caption: meta["caption"],
      side: Map.get(meta, "side", "metrics"),
      html: html
    }
  end

  # `key: value` lines between two `---` lines, then the page's markdown
  @doc false
  def front_matter(text) do
    case Regex.run(~r/\A---\n(.*?)\n---\n(.*)\z/s, text) do
      [_, head, body] ->
        meta =
          for line <- String.split(head, "\n"), String.trim(line) != "", into: %{} do
            case String.split(line, ":", parts: 2) do
              [k, v] -> {String.trim(k), String.trim(v)}
              _ -> raise "front matter line without a key: #{inspect(line)}"
            end
          end

        {meta, String.trim(body)}

      nil ->
        raise "no front matter (--- lines) at the top"
    end
  end

  defp machine(name, pane, caption, demo?, side) do
    side = EEx.eval_file(template(Path.join("side", side <> ".html.eex")), assigns: [demo?: demo?])

    EEx.eval_file(template("machine.html.eex"),
      assigns: [machine: name, pane: pane, caption: h(caption), demo?: demo?, side: side]
    )
  end

  # The list of examples, linked from `dir` (where their pages are, seen from the page it is on)
  @doc false
  def example_list(examples, dir) do
    items =
      examples
      |> Enum.with_index(1)
      |> Enum.map_join("\n", fn {ex, n} ->
        ~s(<li><a href="#{dir}#{ex.slug}.html"><span class="n">#{number(n)}</span><span class="t">#{h(ex.title)}</span><span class="d">#{h(ex.description)}</span></a></li>)
      end)

    ~s(<ol class="examples">\n#{items}\n</ol>)
  end

  defp number(n), do: n |> Integer.to_string() |> String.pad_leading(2, "0")

  # The browser snippet of the Quick start, as the README has it
  @doc false
  def embed(readme) do
    [_, code] = Regex.run(~r/```html\n(.*?)```/s, Map.fetch!(readme, "Quick start"))
    code |> String.trim_trailing() |> h()
  end

  defp template(name), do: Path.join([__DIR__, "templates", name])

  defp version(cargo) do
    case Regex.run(~r/^version\s*=\s*"([^"]+)"/m, File.read!(cargo)) do
      [_, v] -> v
      _ -> raise "no version in Cargo.toml"
    end
  end

  # ---- sources -------------------------------------------------------------

  @doc false
  def parse_readme(text) do
    {done, {name, buf}, _} =
      text
      |> String.split("\n")
      |> Enum.reduce({[], {:intro, []}, false}, fn line, {acc, {name, buf}, fence} ->
        toggled = if String.starts_with?(line, "```"), do: not fence, else: fence

        if not fence and String.starts_with?(line, "## ") do
          heading = line |> String.trim_leading("## ") |> String.trim()
          {[{name, buf} | acc], {heading, []}, toggled}
        else
          {acc, {name, [line | buf]}, toggled}
        end
      end)

    [{name, buf} | done]
    |> Map.new(fn {n, b} -> {n, b |> Enum.reverse() |> Enum.join("\n") |> String.trim()} end)
    |> Map.update!(:intro, fn intro -> intro |> String.replace(~r/\A# armless\s*/, "") end)
  end

  defp assemble(root, _readme, [{:file, path}]) do
    md = File.read!(Path.join(root, path))
    {md |> String.replace(~r/\A# .*\n/, "") |> String.trim(), Path.dirname(path) |> dot(), path}
  end

  defp assemble(_root, readme, sources) do
    md =
      sources
      |> Enum.map(fn
        :intro ->
          Map.fetch!(readme, :intro)

        {:section, name} ->
          section(readme, name, "## " <> name <> "\n\n")

        {:section, name, promote: true} ->
          section(readme, name, "")
      end)
      |> Enum.join("\n\n")

    {md, "", "Readme.md"}
  end

  defp section(readme, name, prefix) do
    body = Map.fetch!(readme, name)
    body = if prefix == "", do: promote(body), else: body
    prefix <> body
  end

  # One section on its own page: its `###` headings become the page's `##`.
  @doc false
  def promote(body) do
    {lines, _} =
      body
      |> String.split("\n")
      |> Enum.map_reduce(false, fn line, fence ->
        toggled = if String.starts_with?(line, "```"), do: not fence, else: fence

        line =
          if not fence and Regex.match?(~r/^[#]{3,4} /, line), do: String.replace_prefix(line, "#", ""), else: line

        {line, toggled}
      end)

    Enum.join(lines, "\n")
  end

  defp dot("."), do: ""
  defp dot(d), do: d

  defp source_files(%{edit: edit}), do: [edit]

  # ---- markdown ------------------------------------------------------------

  @doc false
  def render(md) do
    md
    |> MDEx.to_html!(
      extension: [table: true, strikethrough: true],
      syntax_highlight: nil
    )
    |> String.replace("<table>", ~s(<div class="table-wrap" tabindex="0" role="region" aria-label="Table, scrolls sideways"><table>))
    |> String.replace("</table>", "</table></div>")
    |> String.replace("<pre", ~s(<pre tabindex="0"))
    # Short inline code (a flag, an option) never breaks in the middle
    |> then(&Regex.replace(~r/<code>([^<]{1,32})<\/code>/, &1, "<code class=\"nw\">\\1</code>"))
    # Each option row of an API table can be linked to: #predecode_mb
    |> then(&Regex.replace(~r/<tr>\s*<td><code(?: class="nw")?>([a-z][a-z0-9_]*)<\/code>/, &1, fn whole, name ->
      String.replace(whole, "<tr>", ~s(<tr id="#{name}">), global: false)
    end))
  end

  @doc false
  def add_ids(html) do
    # Walk the headings in order so repeated titles get distinct ids.
    {out, {toc, _}} =
      Regex.split(~r/(<h[23]>.*?<\/h[23]>)/s, html, include_captures: true)
      |> Enum.map_reduce({[], %{}}, fn chunk, {toc, seen} ->
        case Regex.run(~r/\A<h([23])>(.*?)<\/h\1>\z/s, chunk) do
          [_, level, inner] ->
            text = inner |> String.replace(~r/<[^>]+>/, "") |> unescape()
            base = slug(text)
            n = Map.get(seen, base, 0)
            id = if n == 0, do: base, else: "#{base}-#{n}"

            chunk =
              ~s(<h#{level} id="#{id}">#{inner}<a class="anchor" href="##{id}" aria-label="Link to #{h(text)}">#</a></h#{level}>)

            {chunk, {[{String.to_integer(level), id, text} | toc], Map.put(seen, base, n + 1)}}

          nil ->
            {chunk, {toc, seen}}
        end
      end)

    {Enum.join(out), Enum.reverse(toc)}
  end

  defp unescape(s) do
    s
    |> String.replace("&amp;", "&")
    |> String.replace("&lt;", "<")
    |> String.replace("&gt;", ">")
    |> String.replace("&quot;", "\"")
    |> String.replace("&#39;", "'")
  end

  @doc false
  def slug(text) do
    text
    |> String.downcase()
    |> String.replace(~r/[^a-z0-9 \-]/, "")
    |> String.replace(" ", "-")
  end

  # ---- links ---------------------------------------------------------------

  @doc false
  def rewrite_links(page, owners, files, root) do
    Regex.replace(~r/href="([^"]*)"/, page.html, fn whole, href ->
      cond do
        String.starts_with?(href, ["http://", "https://", "mailto:"]) -> whole
        # permalinks added by add_ids
        String.starts_with?(href, "#") -> anchor(page, owners, whole, String.trim_leading(href, "#"))
        true -> ~s(href="#{file_link(page, files, root, href)}")
      end
    end)
  end

  defp anchor(page, owners, whole, id) do
    case Map.get(owners, id) do
      nil ->
        IO.warn("#{page.slug}: no heading for ##{id}")
        whole

      owner when owner == page.slug ->
        whole

      owner ->
        ~s(href="#{owner}.html##{id}")
    end
  end

  defp file_link(page, files, root, href) do
    [path, frag] =
      case String.split(href, "#", parts: 2) do
        [p] -> [p, nil]
        [p, f] -> [p, f]
      end

    full = Path.join(page.dir, path) |> Path.expand("/") |> String.trim_leading("/")
    suffix = if frag, do: "#" <> frag, else: ""

    case Map.get(files, full) do
      nil ->
        abs = Path.join(root, full)
        kind = if File.dir?(abs), do: "tree", else: "blob"
        unless File.exists?(abs), do: IO.warn("#{page.slug}: #{full} does not exist")
        "#{@repo}/#{kind}/main/#{full}#{suffix}"

      slug ->
        "#{slug}.html#{suffix}"
    end
  end

  # ---- navigation ----------------------------------------------------------

  defp nav(pages, current) do
    pages
    |> Enum.chunk_by(& &1.group)
    |> Enum.map_join("\n", fn [%{group: group} | _] = ps ->
      items =
        Enum.map_join(ps, "\n", fn p ->
          cur = if p.slug == current, do: ~s( aria-current="page"), else: ""
          ~s(<li><a href="#{p.slug}.html"#{cur}>#{h(p.title)}</a></li>)
        end)

      ~s(<p class="nav-group">#{h(group)}</p><ul>#{items}</ul>)
    end)
  end

  def h(text) do
    text
    |> to_string()
    |> String.replace("&", "&amp;")
    |> String.replace("<", "&lt;")
    |> String.replace(">", "&gt;")
    |> String.replace("\"", "&quot;")
  end
end

# site/test/site_test.exs loads this file for its functions, without building
unless System.get_env("SITE_BUILD") == "test", do: Site.run()
