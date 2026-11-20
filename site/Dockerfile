# The site as Fly serves it: site/server.exs (Francis) renders each page from its template under
# site/templates, with what site/build.exs gathered (site/_out: site.etf and static/), and sends
# the headers that make its pages cross-origin isolated.
FROM elixir:1.20-alpine

RUN mix local.hex --force && mix local.rebar --force
ENV MIX_INSTALL_DIR=/opt/mix-installs SITE_DIR=/srv PORT=8080

WORKDIR /app
COPY server.exs .
COPY templates templates
# Its dependencies and templates compiled now, not as a machine starts
RUN elixir server.exs --no-start

COPY _out /srv
# A gzipped copy beside each static file that compresses (the blobs, Image.gz and snapshots
# already are)
RUN find /srv/static -type f \( -name '*.html' -o -name '*.js' -o -name '*.css' -o -name '*.json' \
      -o -name '*.wasm' -o -name '*.svg' -o -name '*.cpio' -o -name '*.md' \) -exec gzip -k9 {} \;

# A VM for a machine of 256 MB and a shared CPU: one scheduler (and one dirty CPU one), no
# busy-waiting for work, memory given back to the system as soon as a carrier empties, and room
# for a few thousand processes (one a connection), not a million
ENV ERL_FLAGS="+S 1:1 +SDcpu 1:1 +SDio 2 +sbwt none +sbwtdcpu none +sbwtdio none +MBacul 0 +MHacul 0 +P 16384 +Q 4096"

EXPOSE 8080
CMD ["elixir", "server.exs"]
