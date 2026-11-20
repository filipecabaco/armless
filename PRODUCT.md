# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Stack

Delegated. Chosen: a small Elixir build script (`site/build.exs`, MDEx via `Mix.install`, with `site/demo.exs` for the landing page machine) that renders the markdown in `docs/` and the README into `site/_out`, served on Fly by a Francis server (`site/server.exs`) with a route and EEx template per page. The pages are hand-written HTML/CSS templates. No JavaScript framework.

## Users

Two audiences, both confirmed:
- Developers who want to embed armless: web developers loading `armless.js` and `armless.wasm` into a page, and Rust developers using the crate or `armless-run`. They arrive to answer "can this run my guest, and how do I wire it up".
- People curious what an in-browser arm64 Linux machine is, who need the idea explained before the API.

## Product Purpose

armless is an AArch64 machine emulator written in Rust. It boots unmodified arm64 Linux in a browser tab (WebAssembly, driven by `js/armless.js`) and natively (a Rust crate and the `armless-run` command). The whole machine, devices included, is Rust; the host only provides time, file contents and the other end of network connections.

## Positioning

snowglobe uses armless to turn Docker images into static sites that restore a running app in the visitor's browser in about a second. armless started as a fork of v86. The whole machine is Rust and the host does not run device code.

## Operating Context

Source and releases live at github.com/filipecabaco/armless (remote `origin`). A push to `main` that bumps the version in `Cargo.toml` publishes a release `armless-v<version>` with `armless.js`, `armless.wasm`, `armless-smp.wasm` and `armless-web.tar.gz`. The crate is not on crates.io (`publish = false`); native users depend on it as a git dependency on the tag. License: BSD 2-Clause. Multi-CPU in the browser needs a cross-origin isolated page (COOP/COEP), which GitHub Pages cannot send as headers.

## Capabilities and Constraints

Facts live in `Readme.md` and `docs/`. Do not state a number, speed or compatibility claim that is not in them. The landing page boots a live machine: Linux 6.8 (the CI test kernel) from an initramfs with busybox, on 4 CPUs where the page can be cross-origin isolated (a service worker adds the headers GitHub Pages can't send) and on 1 CPU otherwise. The Pages workflow builds the kernel and busybox; `site/demo.exs` takes armless from its latest release and packs the initramfs. No benchmark figures should be quoted without checking `docs/profiling.md`.

## Brand Commitments

Name is lowercase: `armless`. Copy must read as written by the maintainer: plain, specific, no marketing filler and no AI cadence (no "seamless", "powerful", "unlock", "blazing", "revolutionary", rule-of-three lists, or "not just X, but Y" constructions).

## Evidence on Hand

`Readme.md`, `docs/armless.md`, `docs/hypervisor.md`, `docs/profiling.md`, the test table in the README. Absent: testimonials, users, benchmark charts, a hosted demo. Do not invent them.

## Product Principles

1. Say what it does and what it does not, with the numbers the repo already has.
2. The docs are the product for embedders: complete, searchable by eye, correct.
3. Explain the idea to the curious reader in a screen, then get out of the way.
4. Show real output (a console transcript, real option names), never mock results.

## Accessibility & Inclusion

WCAG AA contrast, keyboard navigable, visible focus, reduced-motion respected, readable without JavaScript.
