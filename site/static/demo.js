// The landing page's machine: a program in armless's user mode (busybox's sh from an Alpine
// filesystem, no kernel), its console in xterm.js, and armless's own memory report beside it.
// Nothing here is simulated: the numbers are vm.memory_report() and the text is what the
// guest writes. The status line only says what the page has seen happen.
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

  const LOOP =
    'n=0; while :; do n=$((n+1)); dd if=/dev/zero of=/tmp/ballast bs=1M count=$((n%4*16+16)) 2>/dev/null; ' +
    'echo "pass $n: $(wc -c </tmp/ballast) bytes"; rm /tmp/ballast; done\n';

  (async () => {
    set("loading", "loading");
    const config = await fetch("demo/config.json").then((r) => (r.ok ? r.json() : Promise.reject(new Error("the machine is not part of this build"))));
    await Promise.all([load("demo/xterm.js"), load("demo/addon-fit.js"), load("demo/armless.js")]);

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
    term.write("\x1b[2mfetching armless.wasm and filesystem.json\x1b[0m\r\n");

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
      memory_mb: config.memory_mb,
      cpus: 1,
      debug: false,
      autostart: false,
      filesystem: { json: "demo/filesystem.json", baseurl: config.baseurl },
      user: {
        argv: ["/bin/sh"],
        env: ["PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin", "TERM=xterm-256color", "HOME=/root"],
        cwd: "/",
        cols: term.cols,
        rows: term.rows,
      },
    });
    window.armless = vm;

    // What the guest printed, as text: for the prompt and loop checks and the screen reader log
    const decoder = new TextDecoder();
    let tail = "";
    let started = false;    // the machine is running
    let loopTyped = false;  // the page finished typing the loop
    let loopRan = false;    // the guest printed its first pass
    let yours = false;      // the visitor took over
    let promptSeen = false;

    const takeOver = () => {
      if (yours) return;
      yours = true;
      if (started) set("ready", "the shell is yours");
    };

    vm.on("console", (bytes) => {
      term.write(bytes);
      tail = (tail + decoder.decode(bytes, { stream: true })).slice(-4000);
      // The shell's prompt, with any control sequences it wrote after it taken off
      if (!promptSeen && /#\s*$/.test(tail.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, ""))) {
        promptSeen = true;
        setTimeout(startLoop, 150);
      }
      if (loopTyped && !loopRan && !yours) {
        if (/pass 1:/.test(tail)) {
          loopRan = true;
          set("ready", "running a loop. ctrl+c stops it");
        } else if (/syntax error|not found/.test(tail)) {
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
      else if (!loopTyped) takeOver();
    });
    term.textarea?.addEventListener("paste", takeOver);
    term.onData((data) => { if (started) vm.console_input(data); });

    const resize = () => {
      fit.fit();
      if (started) vm.console_resize(term.cols, term.rows);
    };
    new ResizeObserver(resize).observe($("terminal"));

    // The page types the loop into the shell, as keystrokes, unless the visitor got there first
    async function startLoop() {
      if (yours) return;
      set("loading", "typing a loop");
      if (reduced) vm.console_input(LOOP);
      else {
        for (const ch of LOOP) {
          if (yours) return;
          vm.console_input(ch);
          await new Promise((r) => setTimeout(r, ch === "\n" ? 200 : 18));
        }
      }
      loopTyped = true;
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
    set("loading", "starting the shell");
    // If the prompt went by unrecognised, start anyway once the shell has had time to settle
    setTimeout(() => { if (!promptSeen) { promptSeen = true; startLoop(); } }, 6000);
    $("keys").hidden = false;
    for (const b of document.querySelectorAll("[data-send]")) {
      b.addEventListener("click", () => {
        takeOver();
        vm.console_input(b.dataset.send.replaceAll("\\n", "\n").replaceAll("\\x03", "\x03"));
        term.focus({ preventScroll: true });
      });
    }

    memory(vm);
  })().catch((e) => fail(e.message || String(e)));

  function memory(vm) {
    const started = performance.now();
    const history = [];
    let linear0 = null;
    const fig = (id, v) => ($(id).textContent = v);
    const mib = (n) => (n / MIB).toFixed(1);

    const sample = () => {
      if (document.hidden) return;
      let r;
      try { r = vm.memory_report({ scan: false }); } catch { return; }
      const t = (performance.now() - started) / 1000;
      linear0 ??= r.memory;
      history.push({ t, live: r.heap.live, peak: r.heap.peak });
      while (history.length && history[0].t < t - WINDOW_S) history.shift();

      fig("m-linear", mib(r.memory));
      fig("m-grown", r.memory > linear0 ? "+" + mib(r.memory - linear0) : "0.0");
      fig("m-live", mib(r.heap.live));
      fig("m-peak", mib(r.heap.peak));
      fig("m-written", mib(r.fs.written_bytes));
      fig("m-jit", String(r.jit.compiled));
      draw(history, t);
    };
    setInterval(sample, 1000);
    sample();
  }

  function draw(history, now) {
    const W = 520, H = 230, L = 50, R = 12, T = 26, B = 32;
    const peak = Math.max(...history.map((p) => p.peak), 8 * MIB);
    const top = Math.ceil(peak / (16 * MIB)) * 16 * MIB;
    const span = Math.max(30, Math.min(now, WINDOW_S));
    const x = (t) => L + ((t - (now - span)) / span) * (W - L - R);
    const y = (v) => T + (1 - v / top) * (H - T - B);
    // A gap in the samples (the tab was hidden) is a gap in the line, not a straight stretch
    const line = (key) => history.map((p, i) => (i && p.t - history[i - 1].t < 3 ? "L" : "M") + x(p.t).toFixed(1) + " " + y(p[key]).toFixed(1)).join("");

    let ticks = "";
    const step = top / 4;
    for (let i = 0; i <= 4; i++) {
      const v = step * i;
      ticks += `<line class="grid" x1="${L}" x2="${W - R}" y1="${y(v)}" y2="${y(v)}"/>` +
        `<text class="axis" x="${L - 8}" y="${y(v) + 4}" text-anchor="end">${Math.round(v / MIB)}</text>`;
    }
    let xticks = "";
    for (let k = 0; k <= 4; k++) {
      const s = (span / 4) * k;
      const t = now - span + s;
      xticks += `<text class="axis" x="${x(t)}" y="${H - 8}" text-anchor="${k === 0 ? "start" : k === 4 ? "end" : "middle"}">${k === 4 ? "now" : "-" + Math.round(span - s) + " s"}</text>`;
    }
    $("chart").innerHTML =
      ticks + xticks +
      `<text class="axis" x="${L - 8}" y="${T - 12}" text-anchor="end">MiB</text>` +
      `<path class="peak" d="${line("peak")}"/><path class="live" d="${line("live")}"/>`;
  }
})();
