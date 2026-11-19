// A machine on one of the site's pages: Linux booted from an initramfs on several CPUs, its console
// in xterm.js, and armless's own report of what each CPU is doing beside it. Nothing here is
// simulated: the numbers come from vm.memory_report(), the text is what the guest writes, and the
// status line only says what the page has seen happen.
//
//   const m = await ArmlessMachine.mount(root, {
//     keys: [["^C", "\x03", "Send ctrl+c"], ["nproc", "nproc\n"]],  // buttons that type into the console
//     command: (cols, rows) => "trench 80 24\n",  // typed at the first prompt, unless the visitor got there first
//     running: /frame 1:/,                         // what the guest prints once the command runs
//     status: (cpus) => "flying the trench",       // the status line then
//     // Armless options over the defaults (the demo's kernel and initramfs, booted on as many
//     // CPUs as the device can spare): a snapshot, user mode, a network...
//     options: (at, config) => ({ cpus: 1, initial_state: at("demo/native.bin.zst") }),
//     restored: true,                              // no prompt comes: type the command once it runs
//     fetching: "fetching armless and a snapshot", // what the console says while it loads
//     starting: "restoring",                       // the status line once it runs
//     needs: "native",                             // a flag of demo/config.json the example needs
//   });
//   (and `on: { blob: fn }`: listeners on the machine from the moment it is made)
//   m.vm, m.term, m.set(state, text), m.type(text), m.yours(), m.startedAt (performance.now())
//
// `root` holds the markup of site/templates/machine.html.eex, whose parts are found by data-part;
// the metrics are drawn where the page has them. Paths are taken relative to this file, so a
// page at any depth finds demo/, cpus.js and sw.js.
(() => {
  const BASE = new URL(".", document.currentScript.src);
  const at = (path) => new URL(path, BASE).href;
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

  // More than one CPU needs a cross-origin isolated page. GitHub Pages can't send the headers, so
  // sw.js adds them: the first visit installs it and reloads once. Where that can't happen the
  // machine runs on one CPU, and says so.
  let isolating = null;
  function isolate() {
    return (isolating ??= (async () => {
      const key = "armless-isolate";
      if (crossOriginIsolated) { sessionStorage.removeItem(key); return true; }
      if (!("serviceWorker" in navigator) || !isSecureContext || sessionStorage.getItem(key)) return false;
      try {
        await navigator.serviceWorker.register(at("sw.js"));
        await navigator.serviceWorker.ready;
        sessionStorage.setItem(key, "1");
        location.reload();
        return new Promise(() => {});
      } catch {
        return false;
      }
    })());
  }

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
      const config = await fetch(at("demo/config.json")).then((r) => (r.ok ? r.json() : Promise.reject(new Error("the machine is not part of this build"))));
      // An asset only some builds have (config.json says): the native snapshot
      if (opts.needs && !config[opts.needs]) throw new Error("this example's snapshot is not part of this build");
      await Promise.all([load(at("cpus.js")), load(at("demo/xterm.js")), load(at("demo/addon-fit.js")), load(at("demo/armless.js"))]);
      const extra = opts.options ? opts.options(at, config) : {};
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
      const term = new Terminal({
        fontFamily: '"Space Mono", ui-monospace, monospace',
        fontSize: 13,
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
      term.write(`\x1b[2m${opts.fetching || "fetching armless, Linux and its initramfs"}\x1b[0m\r\n`);

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

      // What the guest printed, as text: for the prompt and status checks and the screen reader log
      const decoder = new TextDecoder();
      let tail = "";
      let started = false;   // the machine is running
      let typed = false;     // the page gave the shell its command
      let running = false;   // the guest printed what says the command runs
      let yours = false;     // the visitor took over
      let promptSeen = false;

      // What is left to type of the page's command. The visitor taking over sends it at once, so
      // what they type or a key sends starts on a prompt of its own instead of running into it
      let rest = "";
      const takeOver = () => {
        if (rest) { vm.console_input(rest); rest = ""; }
        if (yours) return;
        yours = true;
        if (started) set("ready", "the shell is yours");
      };

      vm.on("console", (bytes) => {
        term.write(bytes);
        tail = (tail + decoder.decode(bytes, { stream: true })).slice(-4000);
        if (!promptSeen && /#\s*$/.test(tail.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, ""))) {
          promptSeen = true;
          setTimeout(start, 300);
        }
        check();
        announce();
      });
      // Whether the command runs yet: on each output, and once it is typed (its output can come
      // back before the last key's pause is over)
      function check() {
        if (!typed || running || yours) return;
        if (opts.running?.test(tail)) {
          running = true;
          set("ready", opts.status ? opts.status(cpus) : "running");
        } else if (/not found|rror/.test(tail)) {
          takeOver();
        }
      }
      vm.on("error", (e) => fail("the machine stopped: " + (e.message || e)));
      vm.on("stop", ({ reason }) => set("stopped", "stopped (" + reason + ")"));

      // The terminal also answers the guest's queries through onData, so only keys and pastes count
      term.onKey(({ domEvent }) => {
        if (domEvent.ctrlKey && domEvent.key.toLowerCase() === "c") takeOver();
        else if (!typed) takeOver();
      });
      term.textarea?.addEventListener("paste", takeOver);
      term.onData((data) => { if (started) vm.console_input(data); });

      const resize = () => {
        fit.fit();
        if (started) vm.console_resize(term.cols, term.rows);
      };
      new ResizeObserver(resize).observe($("terminal"));

      // The page types the example's command, unless the visitor got there first
      async function start() {
        if (yours) return;
        if (!opts.command) return takeOver();
        set("loading", "starting");
        rest = opts.command(term.cols, term.rows);
        if (reduced) { vm.console_input(rest); rest = ""; }
        while (rest && !yours) {
          vm.console_input(rest[0]);
          rest = rest.slice(1);
          await new Promise((r) => setTimeout(r, 45));
        }
        typed = true;
        // A command with nothing in particular to wait for runs once it is typed
        if (!opts.running && !yours) set("ready", opts.status ? opts.status(cpus) : "running");
        check();
      }

      // A polite log of the guest's last line, for screen readers, at most every two seconds
      const log = $("log");
      let said = "", announceTimer = 0;
      function announce() {
        if (announceTimer) return;
        announceTimer = setTimeout(() => {
          announceTimer = 0;
          const lines = tail.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, "").split(/\r?\n/).map((l) => l.trim()).filter(Boolean);
          const last = lines.at(-1) ?? "";
          if (log && last && last !== said) log.textContent = said = last;
        }, 2000);
      }

      await vm.ready;
      vm.run();
      const startedAt = performance.now();
      started = true;
      resize();
      set("loading", opts.starting || "booting Linux");
      if (opts.restored) {
        // A restored shell already printed its prompt, before it was saved
        promptSeen = true;
        setTimeout(start, 500);
      } else {
        // If the prompt went by unrecognised, start anyway once the shell has had time to settle
        setTimeout(() => { if (!promptSeen) { promptSeen = true; start(); } }, 30000);
      }

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

      if ($("cpus")) report(vm, cpus);
      return {
        vm,
        term,
        set,
        startedAt,
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
        if (document.hidden) { last = null; return; }
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

  window.ArmlessMachine = { mount };
})();
