---
title: The same Python, with no kernel
description: The same python3 on two machines. One runs it with no kernel at all, armless answering its system calls; the other boots Linux to run it. The one with no kernel is ready first.
script: race.js
side: race
pane: python3, no kernel
caption: Both machines have one CPU and the same filesystem, Alpine's python3 and what it needs. What each reads before its first prompt is downloaded first, and Linux's kernel and initramfs with it; then both start at the same moment, sharing this tab's CPU, and each is timed from its start to its first >>>.
---

## What is racing

Both consoles run the same file: `/usr/bin/python3`, Alpine's own build for arm64 Linux, not
changed for armless and not compiled to WebAssembly. Its libraries, its standard library and its
compiled `.pyc` files come from one filesystem that both machines read, file by file, as Python
opens them.

The machine on the left has no kernel. armless loads python3 and its dynamic loader into memory
the way `execve` would, and runs it. Each system call Python makes is answered by armless itself:
open, mmap, futex, clone, poll, and the `/proc` files Python reads as it starts. There is nothing
to download for it but Python's own files, and nothing to boot.

The machine on the right does what a computer does. It boots Linux 6.18 from a kernel and an
initramfs, mounts that filesystem over 9p and starts python3. Every system call Python makes
traps into Linux, and Linux answers it.

The page downloads what each needs before either starts, so the clocks time booting and starting
Python, not the network: python3's files for both, and the kernel and initramfs for Linux. Then
both start at the same moment.

## Then try the same line in both

The buttons type the same line into both consoles:

- **who answers**: `os.uname().release` is `6.6.0-armless-user` on the left, the release armless
  tells a program it runs on, and `6.18.60` on the right.
- **a million squares**: pure computation takes the same time in both. The CPU is the same.
- **20,000 system calls**: `os.stat` of python3's own file, in a loop. Under Linux each call goes
  into the guest kernel and its 9p filesystem; with no kernel, armless answers it directly. In
  Chrome on a laptop the left takes about a quarter of the time the right does.
- **a child process**: `subprocess` forks and execs `sh`. armless runs both processes.
- **sqlite3**: an in-memory database, through Python's own `sqlite3` module and Alpine's
  `libsqlite3`.

## What it is for

A Linux program, unmodified, ready in a fraction of a second: a REPL in a documentation page, a
command-line tool in a browser, a job that starts per request. The program keeps what makes
Linux programs useful (files, processes, threads, sockets, signals) without the cost of a kernel
underneath, and with memory backed only where it touches it.

## The code

In place of `boot`, `user` names the program and its environment. Its files come from
`filesystem`, fetched as the program reads them:

```js
const vm = new Armless({
  wasm: "armless.wasm",
  filesystem: { json: "filesystem.json", baseurl: "blobs/" },
  user: { argv: ["python3"], env: ["PATH=/usr/bin:/bin", "HOME=/root"], cwd: "/root" },
});
vm.on("console", (bytes) => term.write(bytes));
term.onData((text) => vm.console_input(text));
```

`armless-exec` does the same natively: `armless-exec --fs DIR python3`. The filesystem is
Alpine's `python3` and `python3-pycache-pyc0` packages with what they depend on, unpacked from
Alpine's own `.apk` files by
[site/demo.exs](https://github.com/filipecabaco/armless/blob/main/site/demo.exs).
