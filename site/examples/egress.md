---
title: The page decides what the guest reaches
description: Every HTTP request and WebSocket a guest makes goes through the page's allow(). Tick a host to let it through, and private addresses never are.
script: egress.js
side: egress
pane: /bin/sh, networked
caption: Linux 6.18 with a network. The guest's wget reaches the internet through this page's fetch(), and only where allow() says yes.
---

## What is running

This machine boots with `network` on. Its virtio-net card is backed by a TCP/IP stack inside
armless: DHCP and DNS are answered locally, and the guest's HTTP on port 80 and HTTPS on 443 end
inside the machine. armless turns each request into one the page makes with `fetch()`, upgraded to
`https://` on an https page. The guest never gets a raw socket to the internet.

Before any request goes out, armless asks the page's `allow()`. The one here lets through the
hosts ticked in the list beside the console and refuses everything else. It also refuses private
and loopback addresses, which matter most in a browser on someone's home or office network. A
refused request gets a 502 Bad Gateway inside the guest, and the log beside the console lists
every decision.

The requests also go out under the browser's own rules: never with the visitor's cookies, HTTP
authentication or referrer, and a host has to allow cross-origin requests (CORS). Both hosts in
the list do.

## The code

```js
const allowed = new Set(["api.github.com"]);
const vm = new Armless({
  wasm: "armless.wasm",
  memory_mb: 256,
  boot: { kernel: "Image.gz", initrd: "initrd.cpio", cmdline: "console=hvc0 rdinit=/init" },
  network: {
    upgrade_https: true,
    // allow() decides alone: check private addresses yourself
    allow: ({ url, host, port, kind }) => !Armless.private_host(host) && allowed.has(host),
  },
});
```

Without `allow`, a guest may reach any public address and the page's own origin. A native host
gets the same controls and more: `ports` limits the TCP ports the guest can reach at all, and a
write budget caps what it can write to its filesystem (`ENOSPC`, with a real `statfs`). See the
[native API](../docs/native-api.html).
