// A machine on one of the site's pages: Linux booted from an initramfs on several CPUs, a snapshot
// restored, or a program with no kernel; its console in xterm.js, and armless's own report of what
// each CPU is doing beside it. Nothing here is simulated: the numbers come from
// vm.memory_report(), the text is what the guest writes, and the status line only says what the
// page has seen happen.
//
//   const m = await ArmlessMachine.mount(root, {
//     keys: [["^C", "\x03", "Send ctrl+c"], ["nproc", "nproc\n"]],  // buttons that type into the console
//     command: (cols, rows) => "trench 80 24\n",  // typed once it is up, unless the visitor got there first
//     running: /frame 1:/,                         // what the guest prints once the command runs
//     status: (cpus) => "flying the trench",       // the status line then (or a string)
//     // Armless options over the defaults (the demo's kernel and initramfs, booted on as many
//     // CPUs as the device can spare): a snapshot, user mode, a network...
//     options: (at, config) => ({ cpus: 1, initial_state: at("demo/native.bin.zst") }),
//     needs: "native",                             // a flag of demo/config.json the example needs
//     on: { blob: fn },                            // listeners on the machine from the moment it is made
//     scripts: ["demo/armless-gpu.js"],            // more of the build's scripts, loaded with armless.js
//     fontSize: 9,                                 // the console's (13 unless said)
//     made: (vm) => ArmlessGPU.attach(vm),         // the machine, as soon as it is made (before it runs)
//   });
//   m.vm, m.term, m.set(state, text), m.type(text), m.yours(), m.startedAt (performance.now()),
//   m.waitFor(regex, { fresh }): the first match in the console's text (from now on, with fresh)
//
// `root` holds the markup of site/templates/machine.html.eex, whose parts are found by data-part;
// the metrics are drawn where the page has them. Paths are taken relative to this file, so a
// page at any depth finds demo/, cpus.js and sw.js.
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
  const MIB = 1048576;
  const WINDOW_S = 120;
  const reduced = matchMedia("(prefers-reduced-motion: reduce)").matches;

  // Each script once, however many machines a page has
  const scripts = new Map();
  const load = (src) => {
    if (!scripts.has(src)) {
      scripts.set(src, new Promise((resolve, reject) => {
        const s = Object.assign(document.createElement("script"), { src });
        s.onload = resolve;
        s.onerror = () => reject(new Error("could not load " + src));
        document.head.append(s);
      }));
    }
    return scripts.get(src);
  };

  // More than one CPU needs a cross-origin isolated page. site/server.exs sends the headers; on a
  // host that can't, sw.js adds them: the first visit installs it and reloads once. Where neither
  // can happen the machine runs on one CPU, and says so.
  let isolating = null;
  function isolate() {
    return (isolating ??= (async () => {
      const key = "armless-isolate";
      if (crossOriginIsolated) { sessionStorage.removeItem(key); return true; }
      if (!("serviceWorker" in navigator) || !isSecureContext || sessionStorage.getItem(key)) return false;
      try {
        await navigator.serviceWorker.register(new URL("sw.js", BASE).href);
        await navigator.serviceWorker.ready;
        sessionStorage.setItem(key, "1");
        location.reload();
        return new Promise(() => {});
      } catch {
        return false;
      }
    })());
  }

  const plain = (s) => s.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, "");
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  // What the console says while a machine loads, and the status line once it runs, by what the
  // machine is: a program with no kernel, a restored snapshot, or Linux to boot
  const phrases = (extra) =>
    extra.user ? ["fetching armless. no kernel to fetch", "starting, no kernel"]
    : extra.initial_state ? ["fetching armless and a snapshot", "restoring"]
    : ["fetching armless, Linux and its initramfs", "booting Linux"];

  function mount(root, opts = {}) {
    const $ = (part) => root.querySelector(`[data-part="${part}"]`);
    const text = (part, value) => { const el = $(part); if (el) el.textContent = value; };
    const status = $("status");

    const set = (state, text) => {
      status.dataset.state = state;
      status.textContent = text;
    };
    const fail = (text) => {
      set("error", text);
      const box = $("fallback");
      box.hidden = false;
      box.textContent = text + ". ";
      const again = Object.assign(document.createElement("button"), { type: "button", className: "btn", textContent: "reload" });
      again.addEventListener("click", () => location.reload());
      box.append(again);
    };

    if (!("WebAssembly" in window)) {
      fail("this browser has no WebAssembly");
      return new Promise(() => {});
    }

    return (async () => {
      set("loading", "loading");
      const [config] = await Promise.all([
        fetch(at("demo/config.json")).then((r) => (r.ok ? r.json() : Promise.reject(new Error("the machine is not part of this build")))),
        load(at("cpus.js")), load(at("demo/xterm.js")), load(at("demo/addon-fit.js")), load(at("demo/armless.js")),
        ...(opts.scripts || []).map((path) => load(at(path))),
      ]);
      // An asset only some builds have (config.json says): the native snapshot, python3's files
      if (opts.needs && !config[opts.needs]) throw new Error("this example's files are not part of this build");
      const extra = opts.options ? opts.options(at, config) : {};
      const [fetching, starting] = phrases(extra);
      const cores = navigator.hardwareConcurrency || 0;
      let cpus = extra.cpus;
      if (!cpus) {
        // As many CPUs as this device can spare (cpus.js), up to armless's limit
        const isolated = await isolate();
        cpus = cpuCount({
          isolated,
          cores,
          memoryGb: navigator.deviceMemory,
          coarse: matchMedia("(pointer: coarse)").matches,
          max: config.max_cpus,
        });
        if (!isolated && $("one-cpu")) $("one-cpu").hidden = false;
      }
      text("m-cpus", String(cpus));
      if (cores) text("m-cores", `of ${cores} core${cores > 1 ? "s" : ""} on this device`);

      const css = getComputedStyle(document.documentElement);
      // The console's font, loaded before the terminal measures its cells: measured with the
      // fallback, a row is shorter than Space Mono's, so the terminal fits a row too many, tells
      // the guest so, and a program that fills it scrolls its top line away once the font comes
      const fontSize = opts.fontSize ?? 13;
      await Promise.race([document.fonts?.load(`${fontSize}px "Space Mono"`).catch(() => {}), sleep(3000)]);
      const term = new Terminal({
        fontFamily: '"Space Mono", ui-monospace, monospace',
        fontSize,
        cursorBlink: !reduced,
        scrollback: 2000,
        theme: {
          background: css.getPropertyValue("--console").trim(),
          foreground: css.getPropertyValue("--console-ink").trim(),
          cursor: css.getPropertyValue("--console-ink").trim(),
          selectionBackground: css.getPropertyValue("--console-select").trim(),
        },
      });
      const fit = new FitAddon.FitAddon();
      term.loadAddon(fit);
      term.open($("terminal"));
      fit.fit();
      term.write(`\x1b[2m${fetching}\x1b[0m\r\n`);

      // A terminal must not keep the keyboard: Escape then Tab leaves it, as the caption says
      let escAt = 0;
      term.attachCustomKeyEventHandler((e) => {
        if (e.type !== "keydown") return true;
        if (e.key === "Escape") escAt = performance.now();
        else if (e.key === "Tab" && performance.now() - escAt < 1500) return false;
        return true;
      });

      const booted = !extra.initial_state && !extra.user;
      const vm = new Armless({
        wasm: at("demo/armless.wasm"),
        wasm_smp: at("demo/armless-smp.wasm"),
        memory_mb: config.memory_mb,
        ...(booted ? { boot: { kernel: at("demo/Image.gz"), initrd: at("demo/initrd.cpio"), cmdline: config.cmdline } } : {}),
        ...extra,
        cpus,
        autostart: false,
      });
      // For the console of the browser's developer tools; the first machine of a page
      window.armless ??= vm;
      // The page's own listeners, on from the start (a user-mode program's files are fetched
      // before it is ready)
      for (const [event, fn] of Object.entries(opts.on || {})) vm.on(event, fn);
      opts.made?.(vm);

      // The console as text, without its escape sequences: its last few thousand characters, and
      // what is waited for in them. `base` counts what came before `seen`.
      const decoder = new TextDecoder();
      let seen = "", base = 0;
      const waits = [];
      // A wait that matched is resolved and done (false); one that didn't goes on waiting (true)
      const pending = (w) => {
        const m = w.re.exec(seen.slice(Math.max(0, w.from - base)));
        if (m) w.resolve(m);
        return !m;
      };
      const waitFor = (re, { fresh = false } = {}) => new Promise((resolve) => {
        const w = { re, resolve, from: fresh ? base + seen.length : 0 };
        if (pending(w)) waits.push(w);
      });
      vm.on("console", (bytes) => {
        term.write(bytes);
        seen += plain(decoder.decode(bytes, { stream: true }));
        if (seen.length > 8000) { base += seen.length - 4000; seen = seen.slice(-4000); }
        for (let i = waits.length - 1; i >= 0; i--) if (!pending(waits[i])) waits.splice(i, 1);
        announce();
      });
      vm.on("error", (e) => fail("the machine stopped: " + (e.message || e)));
      vm.on("stop", ({ reason }) => set("stopped", "stopped (" + reason + ")"));

      // The visitor taking over: the page types nothing more of its command. What is left of it
      // goes at once, so what they type starts on a prompt of its own instead of running into it
      let yours = false;
      let rest = "";
      // The page's command is typed (or it has none): from then on only ctrl+c takes over
      let typed = !opts.command;
      const takeOver = () => {
        if (rest) { vm.console_input(rest); rest = ""; }
        if (yours) return;
        yours = true;
        set("ready", "the shell is yours");
      };

      // A polite log of the guest's last line, for screen readers, at most every two seconds
      const log = $("log");
      let said = "", announceTimer = 0;
      function announce() {
        if (announceTimer || !log) return;
        announceTimer = setTimeout(() => {
          announceTimer = 0;
          const last = seen.slice(-400).split(/\r?\n/).map((l) => l.trim()).filter(Boolean).at(-1) ?? "";
          if (last && last !== said) log.textContent = said = last;
        }, 2000);
      }

      await vm.ready;
      vm.run();
      const startedAt = performance.now();
      set("loading", starting);

      // The visitor's keys, once it runs. The terminal also answers the guest's queries through
      // onData, so only keys and pastes count as taking over.
      term.onKey(({ domEvent }) => {
        if (!typed || (domEvent.ctrlKey && domEvent.key.toLowerCase() === "c")) takeOver();
      });
      term.textarea?.addEventListener("paste", takeOver);
      term.onData((data) => vm.console_input(data));
      let fitting = 0;
      new ResizeObserver(() => {
        if (fitting) return;
        fitting = requestAnimationFrame(() => {
          fitting = 0;
          fit.fit();
          vm.console_resize(term.cols, term.rows);
        });
      }).observe($("terminal"));
      fit.fit();
      vm.console_resize(term.cols, term.rows);

      const keys = $("keys") || document.createElement("span");
      for (const [label, send, aria] of opts.keys || []) {
        const b = Object.assign(document.createElement("button"), { type: "button", className: "btn", textContent: label });
        if (aria) b.setAttribute("aria-label", aria);
        b.addEventListener("click", () => {
          takeOver();
          vm.console_input(send);
          term.focus({ preventScroll: true });
        });
        keys.append(b);
      }
      keys.hidden = !keys.childElementCount;

      // The page types its command once the machine is up (a shell's prompt; a restored one
      // printed its prompt before it was saved), at most 30 s on, unless the visitor got there
      // first; then the status says it runs once the guest says so, or gives up the console on
      // an error
      if (opts.command) (async () => {
        await Promise.race([extra.initial_state ? sleep(500) : waitFor(/#\s*$/).then(() => sleep(300)), sleep(30000)]);
        if (yours) return;
        set("loading", "starting");
        const ran = Promise.race([
          waitFor(opts.running ?? /(?:)/, { fresh: true }).then(() => true),
          waitFor(/not found|rror/, { fresh: true }).then(() => false),
        ]);
        rest = opts.command(term.cols, term.rows);
        while (rest && !yours) {
          const n = reduced ? rest.length : 1;
          vm.console_input(rest.slice(0, n));
          rest = rest.slice(n);
          await sleep(45);
        }
        typed = true;
        const ok = await ran;
        if (yours) return;
        if (!ok) return takeOver();
        set("ready", typeof opts.status === "function" ? opts.status(cpus) : opts.status ?? "running");
      })();

      if ($("cpus")) report(vm, cpus);
      return {
        vm,
        term,
        set,
        startedAt,
        waitFor,
        // Type into the console as the visitor, who has the shell from then on
        type: (data) => { takeOver(); vm.console_input(data); },
        yours: takeOver,
      };
    })().catch((e) => {
      fail(e.message || String(e));
      return new Promise(() => {});
    });

    // What each CPU did in the last second, from armless's own counters
    function report(vm, cpus) {
      const t0 = performance.now();
      const history = [];
      let last = null;
      $("cpus").innerHTML = Array.from({ length: cpus }, (_, i) =>
        `<li><span class="cpu-name">cpu${i}</span><span class="cpu-bar"><span data-cpu-bar="${i}"></span></span><span class="cpu-rate" data-cpu-rate="${i}">-</span></li>`).join("");
      const bars = [...root.querySelectorAll("[data-cpu-bar]")];
      const rates = [...root.querySelectorAll("[data-cpu-rate]")];

      const sample = () => {
        // (Nothing to show of a hidden tab or a stopped machine)
        if (document.hidden || !vm.running) { last = null; return; }
        let r;
        try { r = vm.memory_report({ scan: false }); } catch { return; }
        const now = performance.now();
        const insns = r.smp?.insns ?? [Number(vm.instruction_count)];
        if (last) {
          const dt = (now - last.at) / 1000;
          const mips = insns.map((n, i) => Math.max(0, (n - (last.insns[i] ?? 0)) / dt / 1e6));
          const total = mips.reduce((a, b) => a + b, 0);
          const top = Math.max(...mips, 1);
          mips.forEach((m, i) => {
            bars[i].style.width = (100 * m / top).toFixed(1) + "%";
            rates[i].textContent = m.toFixed(0) + " MIPS";
          });
          $("m-mips").textContent = total.toFixed(0);
          history.push({ t: (now - t0) / 1000, mips: total });
          while (history.length && history[0].t < (now - t0) / 1000 - WINDOW_S) history.shift();
          draw($("chart"), history, (now - t0) / 1000);
        }
        last = { at: now, insns };
        $("m-linear").textContent = (r.memory / MIB).toFixed(1);
        $("m-jit").textContent = String(r.jit.compiled);
      };
      setInterval(sample, 1000);
      sample();
    }
  }

  function draw(svg, history, now) {
    const W = 520, H = 200, L = 50, R = 12, T = 26, B = 32;
    const peak = Math.max(...history.map((p) => p.mips), 50);
    const step = peak > 400 ? 200 : peak > 200 ? 100 : 50;
    const top = Math.ceil(peak / step) * step;
    const span = Math.max(30, Math.min(now, WINDOW_S));
    const x = (t) => L + ((t - (now - span)) / span) * (W - L - R);
    const y = (v) => T + (1 - v / top) * (H - T - B);
    // A gap in the samples (the tab was hidden) is a gap in the line, not a straight stretch
    const line = history.map((p, i) => (i && p.t - history[i - 1].t < 3 ? "L" : "M") + x(p.t).toFixed(1) + " " + y(p.mips).toFixed(1)).join("");

    let ticks = "";
    for (let i = 0; i <= 4; i++) {
      const v = (top / 4) * i;
      ticks += `<line class="grid" x1="${L}" x2="${W - R}" y1="${y(v)}" y2="${y(v)}"/>` +
        `<text class="axis" x="${L - 8}" y="${y(v) + 4}" text-anchor="end">${Math.round(v)}</text>`;
    }
    let xticks = "";
    for (let k = 0; k <= 4; k++) {
      const s = (span / 4) * k;
      const t = now - span + s;
      xticks += `<text class="axis" x="${x(t)}" y="${H - 8}" text-anchor="${k === 0 ? "start" : k === 4 ? "end" : "middle"}">${k === 4 ? "now" : "-" + Math.round(span - s) + " s"}</text>`;
    }
    svg.innerHTML = ticks + xticks +
      `<text class="axis" x="${L - 8}" y="${T - 12}" text-anchor="end">MIPS</text>` +
      `<path class="live" d="${line}"/>`;
  }

  window.ArmlessMachine = { mount, plain };
})();
