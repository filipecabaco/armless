---
title: A Linux program, no kernel
description: busybox's sh runs in user mode, with no guest kernel at all. armless answers its system calls itself, /proc included.
script: nokernel.js
side: user
pane: sh, user mode
caption: sh starts in user mode from a filesystem with a static busybox. There is no guest kernel: armless answers the system calls.
---

## What is running

There is no Linux on this page. The CPU runs busybox, a static arm64 Linux program, at EL0. Each
time it makes a system call, armless answers it directly instead of passing it to a guest kernel.
The `uname` it prints, the processes `ps` lists from `/proc`, and the memory in
`/proc/self/status` are armless's own answers.

User mode covers what real programs need: fork, exec and wait, threads, pipes, signals, epoll,
poll and select, sockets (unix, loopback TCP and UDP, and the host's network behind them), virtual
memory with protections, shared memory, timers, and snapshots of the running processes. A
program's memory is backed only where it touches it.

The commands beside the console each say what armless answers. `uname` is the one that surprises:
armless answers it as Linux `6.6.0-armless-user`, the release a program sees, though no Linux is
running.

## The code

In place of `boot`, `user` names the program and its environment. Its files come from
`filesystem`, fetched as the program reads them:

```js
const vm = new Armless({
  wasm: "armless.wasm",
  memory_mb: 64,
  filesystem: { json: "filesystem.json", baseurl: "blobs/" },
  user: { argv: ["sh"], env: ["PATH=/bin", "HOME=/root", "TERM=xterm-256color"], cwd: "/root" },
});
vm.on("console", (bytes) => term.write(bytes));
term.onData((text) => vm.console_input(text));
vm.on("stop", ({ reason, code }) => console.log(reason, code)); // "exit", 0
```

`armless-exec` does the same natively: `armless-exec --fs DIR sh`. The filesystem here is a
directory with busybox and a few files under `/etc`, made with `tests/fs/fs2json.py` and
`copy-to-sha256.py` ([site/demo.exs](https://github.com/filipecabaco/armless/blob/main/site/demo.exs)).
