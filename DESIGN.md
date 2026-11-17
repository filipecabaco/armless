# Design

Minimalist and dark, after vitro.run: one monospace typeface, a warm near-black ground, thin lines, state by brightness, colour only for failure. Source: `site/static/style.css`.

## Color

| Role | Value | Use |
|---|---|---|
| ground | `oklch(0.13 0.008 70)` | page |
| surface | `oklch(0.17 0.009 70)` | code blocks |
| raised | `oklch(0.23 0.009 70)` | inline code, hover |
| line | `oklch(0.28 0.010 70)` | every rule and border |
| edge | `oklch(0.36 0.010 70)` | underlines, selection |
| text | `oklch(0.96 0.012 70)` | text, the live state, the heap-now line |
| muted | `oklch(0.74 0.010 70)` | secondary text |
| faint | `oklch(0.66 0.009 70)` | captions, inactive links, the heap-peak line |
| error | `oklch(0.66 0.17 28)` | failure only |
| console | `oklch(0.1 0.008 70)` | the terminal |

## Type

Space Mono 400 and 700 for everything, including the console. Self-hosted subset in `site/static/fonts` (SIL OFL, licence beside the files). Headlines are tight (-0.03em).

## Components

- **Landing:** a short hero (headline, one sentence, two buttons), then the machine: the console and a memory pane in one thin-ruled box, a one-line caption, and the embed snippet. Everything else is in the docs.
- **Memory pane:** `vm.memory_report()` once a second: linear memory, heap now and peak, files written, JIT blocks, and a chart of heap now (solid) and peak (dashed).
- **Docs:** plain list of pages on the left (current page underlined), the page, "on this page" on the right from 76rem. Tables scroll inside a ruled region.
- **Buttons:** quiet until pointed at; primary is solid text colour. 44px tall on touch screens.
- **Code blocks:** Space Mono like everything else, with a quiet copy button (added by `site.js`; without JavaScript the text is still selectable). Short inline code never breaks mid-token.
- **Console:** the guest redraws the Mandelbrot set in place with busybox awk, from 4 to 64 iterations and over again, about two frames a second once warm, shaded by the log of the escape time. Each frame adds 4 MiB to a file that is deleted when the set starts over, so the memory chart climbs as the picture sharpens. The status line says only what the page has seen happen (starting, drawing, the shell is yours). Escape, then Tab, leaves the terminal; a polite live region reads the guest's last line to screen readers.
- **Small screens:** the page list and "on this page" fold into two collapsed rows above the content.

## Rules

- Nothing on the page is simulated. The console is the guest's output, the figures are armless's report.
- No cards, no shadows, no gradients, no accent colour.
- Little copy on the landing page. Claims come from `Readme.md` and `docs/`.
