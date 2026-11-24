// The race: the same python3, Alpine's, from the same filesystem (demo/py, site/demo.exs), on two
// machines of one CPU each. One runs it with no kernel, armless answering its system calls; the
// other boots Linux, which mounts that filesystem over 9p and runs the same python3
// (site/guest/init).
//
// First everything each needs before its first >>> is downloaded (demo/py/startup.json lists
// python3's files, site/py-startup.mjs; Linux also needs its kernel and initramfs), and the table
// says how much. Then both start at the same moment, given what was downloaded from memory, and
// each is timed from its start to its first >>>: what the clocks measure is booting and starting
// python3, not the network. The two share this tab's CPU while both run. The buttons type the
// same line into both.
(() => {
  const BASE = new URL(".", document.currentScript.src);
  // The deploy this page is from (?v= on this script): every file asked for carries it, so no
  // cache hands a page a file of the deploy before. (A directory, a base for others, doesn't.)
  const V = new URL(document.currentScript.src).searchParams.get("v");
  const at = (path) => {
    const url = new URL(path, BASE);
    if (V && !path.endsWith("/")) url.searchParams.set("v", V);
    return url.href;
  };
  const page = document.querySelector('[data-machine="race"]');
  const second = document.querySelector('[data-machine="race-b"]');
  const race = page.querySelector("[data-race]");
  const cell = (name) => race.querySelector(`[data-score="${name}"]`);
  const verdict = race.querySelector("[data-verdict]");
  const mb = (n) => `${(n / 1048576).toFixed(1)} MB`;
  const seconds = (ms) => `${(ms / 1000).toFixed(2)} s`;
  const bytes = (url) => fetch(url).then((r) => (r.ok ? r.arrayBuffer() : Promise.reject(new Error(`${url}: ${r.status}`)))).then((b) => new Uint8Array(b));
  const total = (list) => list.reduce((n, b) => n + b.length, 0);

  // python3's files, downloaded once for both: a blob not listed is fetched when it is read
  const blobs = new Map();
  const blob = (name) => blobs.get(name) ?? bytes(at("demo/py/blobs/" + name));
  const filesystem = { json: at("demo/py/filesystem.json"), baseurl: at("demo/py/blobs/"), fetch_blob: blob };

  async function download() {
    const listed = await fetch(at("demo/py/startup.json")).then((r) => (r.ok ? r.json() : { user: [], kernel: [] }));
    const names = [...new Set([...listed.user, ...listed.kernel])];
    const [kernel, initrd] = await Promise.all([
      bytes(at("demo/Image.gz")),
      bytes(at("demo/initrd.cpio")),
      ...names.map((n) => bytes(at("demo/py/blobs/" + n)).then((b) => blobs.set(n, b))),
    ]);
    const of = (list) => total(list.map((n) => blobs.get(n)).filter(Boolean));
    cell("bytes-u").textContent = mb(of(listed.user));
    cell("bytes-k").textContent = mb(of(listed.kernel) + kernel.length + initrd.length);
    return { kernel, initrd, less: kernel.length + initrd.length + of(listed.kernel) - of(listed.user) };
  }

  // A side's machine, started: it times itself from its start to its first >>>
  async function run(key, root, options) {
    const m = await ArmlessMachine.mount(root, { needs: "python", options });
    await m.waitFor(/>>> $/);
    const time = performance.now() - m.startedAt;
    m.set("ready", "python3, yours to type in");
    cell(`ready-${key}`).textContent = seconds(time);
    return { m, time };
  }

  for (const root of [page, second]) {
    const status = root.querySelector('[data-part="status"]');
    status.dataset.state = "loading";
    status.textContent = "downloading what it needs";
  }

  (async () => {
    const got = await download();
    const [u, k] = await Promise.all([
      run("u", page, () => ({
        cpus: 1,
        filesystem,
        user: { argv: ["python3"], env: ["PATH=/usr/bin:/bin", "HOME=/root", "TERM=xterm-256color"], cwd: "/root" },
      })),
      run("k", second, (_, config) => ({
        cpus: 1,
        filesystem,
        boot: { kernel: got.kernel, initrd: got.initrd, cmdline: `${config.cmdline} python` },
      })),
    ]);
    const times = k.time / u.time;
    verdict.dataset.state = "different";
    verdict.textContent = times >= 1.1
      ? `With no kernel, python3 was ready in ${(100 / times).toFixed(0)}% of the time Linux took, and needed ${mb(got.less)} less downloaded: no kernel, no initramfs, no boot.`
      : `This time the two were ready within a moment of each other; the one with no kernel needed ${mb(got.less)} less downloaded.`;
    for (const b of race.querySelectorAll("[data-send]")) {
      b.disabled = false;
      b.addEventListener("click", () => [u, k].forEach(({ m }) => m.type(b.dataset.send + "\n")));
    }
  })();
})();
