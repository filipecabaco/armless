// Makes the site cross-origin isolated, which the landing page's machine needs for more than one
// CPU (its CPUs share memory between Web Workers, through SharedArrayBuffer). GitHub Pages can't
// send the headers, so this worker adds them to every page it serves. Everything the pages load
// comes from this site, so require-corp costs nothing.
self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));

self.addEventListener("fetch", (event) => {
  if (event.request.mode !== "navigate") return;
  event.respondWith(
    fetch(event.request).then((response) => {
      if (response.type === "opaqueredirect") return response;
      const headers = new Headers(response.headers);
      headers.set("Cross-Origin-Opener-Policy", "same-origin");
      headers.set("Cross-Origin-Embedder-Policy", "require-corp");
      return new Response(response.body, { status: response.status, statusText: response.statusText, headers });
    }),
  );
});
