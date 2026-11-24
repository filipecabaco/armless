#!/usr/bin/env elixir
# armless's site. Each page is a route rendered from its template under site/templates, with
# the content site/build.exs gathered ($SITE_DIR/site.etf); scripts, styles, fonts and the
# machines' files are $SITE_DIR/static. Every response carries the headers that make a page
# cross-origin isolated: a machine of several CPUs needs that (its CPUs share memory between Web
# Workers through SharedArrayBuffer), so a page doesn't first reload itself through sw.js as it
# must on a host that can't send them.
#
#   /                    the landing page          templates/index.html.eex
#   /examples            the examples              templates/examples.html.eex
#   /examples/:slug      an example, its machine   templates/example.html.eex
#   /docs, /docs/:slug   a docs page               templates/doc.html.eex
#   anything else        404                       templates/404.html.eex
#
# Every link on a page is a path from the site's root, so a page reads the same at any address.
# The addresses the site had as static files (/examples/index.html, /docs/design.html,
# /examples/) are sent on to these, so links already shared keep working.
#
#   elixir site/server.exs              serves on $PORT (default 8080)
#   elixir site/server.exs --no-start   defines the server and returns (site/test/server_test.exs)
#
# Static files with a .gz beside them (the Dockerfile makes those) go compressed to a client
# that takes gzip. The machines' files (demo/) are kept an hour; the rest is looked at again
# each visit.

Mix.install([{:francis, "~> 0.4.0"}])

defmodule Site.Templates do
  require EEx
  @dir Path.expand("templates", __DIR__)

  for {name, file} <- [
        index: "index.html.eex",
        examples: "examples.html.eex",
        example: "example.html.eex",
        doc: "doc.html.eex",
        not_found: "404.html.eex",
        machine: "machine.html.eex"
      ] do
    EEx.function_from_file(:def, name, Path.join(@dir, file), [:assigns])
  end

  # The pane beside a machine's console: side("metrics", assigns) is templates/side/metrics.html.eex
  # (Not every pane looks at its assigns)
  for file <- Path.wildcard(Path.join([@dir, "side", "*.html.eex"])) do
    def side(unquote(Path.basename(file, ".html.eex")), var!(assigns)) do
      _ = var!(assigns)
      unquote(EEx.compile_file(file))
    end
  end
end

defmodule Site do
  @dir System.get_env("SITE_DIR", Path.expand("_out", __DIR__))
  @repo "https://github.com/filipecabaco/armless"
  # Where the site is; share cards need absolute URLs
  @site "https://armless.dev"
  # The landing page's machine: the trench run, as on its example page
  @landing_caption "Linux 6.18, booted in this tab from an initramfs with busybox, with as many CPUs as this device can spare. One awk per CPU draws a trench run, a new one every time. Ctrl+c stops it. Escape, then Tab, leaves the console."
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

  alias Site.Templates

  plug(:headers)
  plug(:old_addresses)

  plug(Plug.Static,
    at: "/",
    from: Path.join(@dir, "static"),
    gzip: true,
    cache_control_for_etags: "public, max-age=3600"
  )

  get("/healthz", fn _ -> "ok" end)

  get("/", fn conn ->
    c = content()

    html(conn, Templates.index(common(c) ++ [
      demo?: c.demo?,
      embed: c.embed,
      machine: machine(c.demo?, "trench", "/bin/sh", @landing_caption, "metrics"),
      examples: example_list(c.examples)
    ]))
  end)

  get("/examples", fn conn ->
    c = content()
    html(conn, Templates.examples(common(c) ++ [list: example_list(c.examples)]))
  end)

  get("/examples/:slug", fn conn ->
    c = content()

    case Enum.find_index(c.examples, &(&1.slug == conn.params["slug"])) do
      nil ->
        not_found(conn)

      i ->
        ex = Enum.at(c.examples, i)

        html(conn, Templates.example(common(c) ++ [
          ex: ex,
          n: number(i + 1),
          machine: machine(c.demo?, ex.machine, ex.pane, ex.caption, ex.side),
          prev: if(i > 0, do: Enum.at(c.examples, i - 1)),
          next: Enum.at(c.examples, i + 1),
          demo?: c.demo?
        ]))
    end
  end)

  get("/docs", fn conn -> doc(conn, "index") end)
  # /docs/index is /docs (it redirects, in old_addresses)
  get("/docs/:slug", fn conn -> doc(conn, conn.params["slug"]) end)

  unmatched(fn conn -> not_found(conn) end)

  def dir, do: @dir

  # The content site/build.exs gathered, read once
  def content do
    case :persistent_term.get({__MODULE__, :content}, nil) do
      nil ->
        c = Path.join(@dir, "site.etf") |> File.read!() |> :erlang.binary_to_term()
        :persistent_term.put({__MODULE__, :content}, c)
        c

      c ->
        c
    end
  end

  defp doc(conn, slug) do
    c = content()

    case Enum.find_index(c.pages, &(&1.slug == slug)) do
      nil ->
        not_found(conn)

      i ->
        page = Enum.at(c.pages, i)

        html(conn, Templates.doc(common(c) ++ [
          page: page,
          nav: nav(c.pages, slug),
          prev: if(i > 0, do: Enum.at(c.pages, i - 1)),
          next: Enum.at(c.pages, i + 1)
        ]))
    end
  end

  defp not_found(conn), do: html(conn, 404, Templates.not_found(repo: @repo))

  defp common(c), do: [version: c.version, v: version_query(c), repo: @repo, site: @site]

  # ?v=<build> on the scripts and styles a page loads (a build without the id has none)
  defp version_query(%{build: build}) when is_binary(build) and build != "", do: "?v=" <> build
  defp version_query(_), do: ""

  defp machine(demo?, name, pane, caption, side) do
    Templates.machine(
      machine: name,
      pane: pane,
      caption: h(caption),
      demo?: demo?,
      side: Templates.side(side, demo?: demo?)
    )
  end

  defp example_list(examples) do
    items =
      examples
      |> Enum.with_index(1)
      |> Enum.map_join("\n", fn {ex, n} ->
        ~s(<li><a href="/examples/#{ex.slug}"><span class="n">#{number(n)}</span><span class="t">#{h(ex.title)}</span><span class="d">#{h(ex.description)}</span></a></li>)
      end)

    ~s(<ol class="examples">\n#{items}\n</ol>)
  end

  defp nav(pages, current) do
    pages
    |> Enum.chunk_by(& &1.group)
    |> Enum.map_join("\n", fn [%{group: group} | _] = ps ->
      items =
        Enum.map_join(ps, "\n", fn p ->
          cur = if p.slug == current, do: ~s( aria-current="page"), else: ""
          ~s(<li><a href="#{doc_path(p.slug)}"#{cur}>#{h(p.title)}</a></li>)
        end)

      ~s(<p class="nav-group">#{h(group)}</p><ul>#{items}</ul>)
    end)
  end

  defp number(n), do: n |> Integer.to_string() |> String.pad_leading(2, "0")

  # A doc page's path: the overview is /docs itself
  def doc_path("index"), do: "/docs"
  def doc_path(slug), do: "/docs/" <> slug

  def h(text) do
    text
    |> to_string()
    |> String.replace("&", "&amp;")
    |> String.replace("<", "&lt;")
    |> String.replace(">", "&gt;")
    |> String.replace("\"", "&quot;")
  end

  # The addresses the site had when it was static files, sent on (301) to its routes:
  # /index.html, /examples/index.html, /examples/x.html, /docs/index.html, /docs/x.html and a
  # trailing slash (/examples/). The query string goes along.
  def old_addresses(%Plug.Conn{method: m, request_path: path} = conn, _) when m in ["GET", "HEAD"] do
    case new_address(path) do
      nil ->
        conn

      to ->
        redirect(conn, 301, if(conn.query_string == "", do: to, else: to <> "?" <> conn.query_string))
    end
  end

  def old_addresses(conn, _), do: conn

  @doc false
  def new_address("/"), do: nil
  def new_address("/index.html"), do: "/"

  def new_address(path) do
    case String.split(path, "/", trim: true) do
      [section, "index.html"] when section in ["examples", "docs"] -> "/" <> section
      ["docs", "index"] -> "/docs"
      [section, page] when section in ["examples", "docs"] ->
        cond do
          String.ends_with?(page, ".html") -> "/#{section}/#{Path.rootname(page)}"
          String.ends_with?(path, "/") -> "/#{section}/#{page}"
          true -> nil
        end

      [section] when section in ["examples", "docs"] ->
        if String.ends_with?(path, "/"), do: "/" <> section

      _ ->
        nil
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
  # The content read now, so a build without it fails here and not on the first visit
  Site.content()
  {:ok, _} = Supervisor.start_link([Site], strategy: :one_for_one)
  IO.puts("site: #{Site.dir()} on port #{System.get_env("PORT", "8080")}")
  Process.sleep(:infinity)
end
