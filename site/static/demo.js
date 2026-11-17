// The landing page's machine: Linux booted from an initramfs on several CPUs, its console in
// xterm.js, and armless's own report of what each CPU is doing beside it. Nothing here is
// simulated: the numbers come from vm.memory_report() and vm.smp_report(), the text is what the
// guest writes, and the status line only says what the page has seen happen.
(() => {
  const $ = (id) => document.getElementById(id);
  const status = $("status");
  const MIB = 1048576;
  const WINDOW_S = 120;
  const reduced = matchMedia("(prefers-reduced-motion: reduce)").matches;

  const set = (state, text) => {
    status.dataset.state = state;
    status.textContent = text;
  };
  const fail = (text) => {
    set("error", text);
    const box = $("console-fallback");
    box.hidden = false;
    box.textContent = text + ". ";
    const again = Object.assign(document.createElement("button"), { type: "button", className: "btn", textContent: "reload" });
    again.addEventListener("click", () => location.reload());
    box.append(again);
  };

  if (!("WebAssembly" in window)) return fail("this browser has no WebAssembly");

  const load = (src) => new Promise((resolve, reject) => {
    const s = Object.assign(document.createElement("script"), { src });
    s.onload = resolve;
    s.onerror = () => reject(new Error("could not load " + src));
    document.head.append(s);
  });

  // More than one CPU needs a cross-origin isolated page. GitHub Pages can't send the headers, so
  // sw.js adds them: the first visit installs it and reloads once. Where that can't happen the
  // machine runs on one CPU, and says so.
  async function isolate() {
    const key = "armless-isolate";
    if (crossOriginIsolated) { sessionStorage.removeItem(key); return true; }
    if (!("serviceWorker" in navigator) || !isSecureContext || sessionStorage.getItem(key)) return false;
    try {
      await navigator.serviceWorker.register("sw.js");
      await navigator.serviceWorker.ready;
      sessionStorage.setItem(key, "1");
      location.reload();
      return new Promise(() => {});
    } catch {
      return false;
    }
  }

  (async () => {
    set("loading", "loading");
    const isolated = await isolate();
    const config = await fetch("demo/config.json").then((r) => (r.ok ? r.json() : Promise.reject(new Error("the machine is not part of this build"))));
    await Promise.all([load("cpus.js"), load("demo/xterm.js"), load("demo/addon-fit.js"), load("demo/armless.js")]);
    // As many CPUs as this device can spare (cpus.js), up to armless's limit
    const cores = navigator.hardwareConcurrency || 0;
    const cpus = cpuCount({
      isolated,
      cores,
      memoryGb: navigator.deviceMemory,
      coarse: matchMedia("(pointer: coarse)").matches,
      max: config.max_cpus,
    });
    if (!isolated) $("one-cpu").hidden = false;
    $("m-cpus").textContent = String(cpus);
    if (cores) $("m-cores").textContent = `of ${cores} core${cores > 1 ? "s" : ""} on this device`;

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
    term.write(`\x1b[2mfetching armless, Linux and its initramfs\x1b[0m\r\n`);

    // A terminal must not keep the keyboard: Escape then Tab leaves it, as the caption says
    let escAt = 0;
    term.attachCustomKeyEventHandler((e) => {
      if (e.type !== "keydown") return true;
      if (e.key === "Escape") escAt = performance.now();
      else if (e.key === "Tab" && performance.now() - escAt < 1500) return false;
      return true;
    });

    const vm = new Armless({
      wasm: "demo/armless.wasm",
      wasm_smp: "demo/armless-smp.wasm",
      memory_mb: config.memory_mb,
      cpus,
      autostart: false,
      boot: { kernel: "demo/Image.gz", initrd: "demo/initrd.cpio", cmdline: config.cmdline },
    });
    window.armless = vm;

    // What the guest printed, as text: for the prompt and status checks and the screen reader log
    const decoder = new TextDecoder();
    let tail = "";
    let started = false;   // the machine is running
    let typed = false;     // the page gave the shell its command
    let drawing = false;   // the guest printed its first frame
    let yours = false;     // the visitor took over
    let promptSeen = false;

    const takeOver = () => {
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
      if (typed && !drawing && !yours) {
        if (/frame 1:/.test(tail)) {
          drawing = true;
          set("ready", `flying the trench on ${cpus} CPU${cpus > 1 ? "s" : ""}. ctrl+c stops it`);
        } else if (/not found|rror/.test(tail)) {
          takeOver();
        }
      }
      announce();
    });
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

    // The page types the command, unless the visitor got there first: trench, a shell script in
    // the initramfs that runs one awk per CPU (site/guest/trench)
    async function start() {
      if (yours) return;
      set("loading", "starting");
      const line = `trench ${Math.min(term.cols - 1, 110)} ${Math.min(term.rows - 2, 40)}\n`;
      for (const ch of reduced ? [line] : line) {
        if (yours) return;
        vm.console_input(ch);
        await new Promise((r) => setTimeout(r, 45));
      }
      typed = true;
    }

    // A polite log of the guest's last line, for screen readers, at most every two seconds
    const log = $("console-log");
    let said = "", announceTimer = 0;
    function announce() {
      if (announceTimer) return;
      announceTimer = setTimeout(() => {
        announceTimer = 0;
        const lines = tail.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, "").split(/\r?\n/).map((l) => l.trim()).filter(Boolean);
        const last = lines.at(-1) ?? "";
        if (last && last !== said) log.textContent = said = last;
      }, 2000);
    }

    await vm.ready;
    vm.run();
    started = true;
    resize();
    set("loading", "booting Linux");
    // If the prompt went by unrecognised, start anyway once the shell has had time to settle
    setTimeout(() => { if (!promptSeen) { promptSeen = true; start(); } }, 30000);
    $("keys").hidden = false;
    for (const b of document.querySelectorAll("[data-send]")) {
      b.addEventListener("click", () => {
        takeOver();
        vm.console_input(b.dataset.send.replaceAll("\\n", "\n").replaceAll("\\x03", "\x03"));
        term.focus({ preventScroll: true });
      });
    }

    report(vm, cpus);
  })().catch((e) => fail(e.message || String(e)));

  // What each CPU did in the last second, from armless's own counters
  function report(vm, cpus) {
    const t0 = performance.now();
    const history = [];
    let last = null;
    const bars = $("cpus");
    bars.innerHTML = Array.from({ length: cpus }, (_, i) =>
      `<li><span class="cpu-name">cpu${i}</span><span class="cpu-bar"><span id="cpu-bar-${i}"></span></span><span class="cpu-rate" id="cpu-rate-${i}">-</span></li>`).join("");

    const sample = () => {
      if (document.hidden) { last = null; return; }
      let r;
      try { r = vm.memory_report({ scan: false }); } catch { return; }
      const now = performance.now();
      const insns = r.smp?.insns ?? [vm.instruction_count];
      if (last) {
        const dt = (now - last.at) / 1000;
        const rates = insns.map((n, i) => Math.max(0, (n - (last.insns[i] ?? 0)) / dt / 1e6));
        const total = rates.reduce((a, b) => a + b, 0);
        const top = Math.max(...rates, 1);
        rates.forEach((m, i) => {
          $(`cpu-bar-${i}`).style.width = (100 * m / top).toFixed(1) + "%";
          $(`cpu-rate-${i}`).textContent = m.toFixed(0) + " MIPS";
        });
        $("m-mips").textContent = total.toFixed(0);
        history.push({ t: (now - t0) / 1000, mips: total });
        while (history.length && history[0].t < (now - t0) / 1000 - WINDOW_S) history.shift();
        draw(history, (now - t0) / 1000);
      }
      last = { at: now, insns };
      $("m-linear").textContent = (r.memory / MIB).toFixed(1);
      $("m-jit").textContent = String(r.jit.compiled);
    };
    setInterval(sample, 1000);
    sample();
  }

  function draw(history, now) {
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
    $("chart").innerHTML = ticks + xticks +
      `<text class="axis" x="${L - 8}" y="${T - 12}" text-anchor="end">MIPS</text>` +
      `<path class="live" d="${line}"/>`;
  }
})();
