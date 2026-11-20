---
title: Two copies, two kinds of random
description: Restore one snapshot twice and each copy draws its own random numbers, because armless gives every restore a new VM generation ID and Linux reseeds on it.
script: clones.js
side: clone
pane: copy 1
caption: Both copies restore the same snapshot, the same memory to the byte, and read 16 bytes from /dev/urandom.
---

## Why this matters

A snapshot holds the guest's random number generator as well as everything else. Restore the same
snapshot twice and, unless something tells Linux, both copies continue from one generator state.
They hand out the same "random" session keys, TLS nonces and UUIDs, and the copies can't tell.

armless gives each restored machine a VM generation ID, a 128-bit value in the device tree
(`microsoft,vmgenid`) that is new on every restore. Linux 6.10 and later, built with
`CONFIG_VMGENID`, watches it, and when it changes it mixes the new value into its generator and
reseeds it.

## What is running

The two consoles above are two machines in this tab. Both restore the snapshot from
[the native example](/examples/native-snapshot), saved at a shell prompt. Each one reads
`head -c 16 /dev/urandom` as soon as it is restored. The line under the second copy compares
the two reads.

## The code

Nothing to turn on. Every restore gets its own ID, whether it starts from `initial_state` or a
later `restore_state()`:

```js
const copy = () => new Armless({ wasm: "armless.wasm", memory_mb: 256, cpus: 1, initial_state: "native.bin.zst" });
const [a, b] = [copy(), copy()];
// each: head -c 16 /dev/urandom | od -An -tx1  → different bytes
```

The guest needs a kernel that reads the ID: Linux 6.10 or later with `CONFIG_VMGENID`
(`CONFIG_VIRT_DRIVERS` on arm64). This site's kernel is
[tests/linux/armless_defconfig](https://github.com/filipecabaco/armless/blob/main/tests/linux/armless_defconfig)
on Linux 6.18.
