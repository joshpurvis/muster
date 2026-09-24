const CACHE = "muster-mobile-v1";
const SHELL = [
  "/m/",
  "/m/app.js",
  "/m/app.css",
  "/m/manifest.webmanifest",
  "/m/icon.svg",
  "/m/icon-192.png",
  "/m/icon-512.png",
  "/m/icon-maskable-512.png",
];

self.addEventListener("install", (event) => {
  event.waitUntil(caches.open(CACHE).then((cache) => cache.addAll(SHELL)));
});
self.addEventListener("activate", (event) => {
  event.waitUntil((async () => {
    for (const key of await caches.keys()) {
      if (key.startsWith("muster-mobile-") && key !== CACHE) await caches.delete(key);
    }
    await self.clients.claim();
  })());
});
self.addEventListener("fetch", (event) => {
  const request = event.request;
  const url = new URL(request.url);
  if (request.method !== "GET" || url.origin !== self.location.origin || !url.pathname.startsWith("/m/")) {
    return;
  }
  event.respondWith((async () => {
    const cache = await caches.open(CACHE);
    try {
      const response = await fetch(request);
      if (response.ok) await cache.put(request, response.clone()).catch(() => {});
      return response;
    } catch {
      const saved = await cache.match(request) ??
        (request.mode === "navigate" ? await cache.match("/m/") : null);
      return saved ?? Response.error();
    }
  })());
});
