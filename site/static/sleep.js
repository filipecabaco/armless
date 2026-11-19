// Time asleep, in three steps. (1) sh in user mode starts a 60-second timer, a background sleep that
// says when it ends. (2) "save and stop" saves the machine and the process (save_state(),
// save_process()) into this browser's IndexedDB, with how long the timer had left. (3) A restore
// either tells the process how long it was saved (user_asleep_ms), so the timer fires when it was
// due, at once if that has gone by, or tells it nothing, so the timer has the time it had left.
// Each button says beforehand which it will be.
(async () => {
  const SECONDS = 60;
  const root = document.querySelector('[data-machine="sleep"]');
  const q = (sel) => root.querySelector(sel);
  const timerNote = q("[data-timer]");
  const savedNote = q("[data-saved]");
  const saveButton = q("[data-save]");
  const restoreButtons = [...root.querySelectorAll("[data-restore]")];
  const predict = { asleep: q('[data-predict="asleep"]'), none: q('[data-predict="none"]') };

  // ---- the snapshot, kept in IndexedDB so it outlives the tab
  const db = () => new Promise((resolve, reject) => {
    const open = indexedDB.open("armless-site", 1);
    open.onupgradeneeded = () => open.result.createObjectStore("snapshots");
    open.onsuccess = () => resolve(open.result);
    open.onerror = () => reject(open.error);
  });
  const store = async (mode, fn) => {
    const d = await db();
    return new Promise((resolve, reject) => {
      const req = fn(d.transaction("snapshots", mode).objectStore("snapshots"));
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error);
    });
  };
  const KEY = "time-asleep-v2";
  const load = () => store("readonly", (s) => s.get(KEY)).catch(() => null);
  const keep = (snap) => store("readwrite", (s) => s.put(snap, KEY));
  const forget = () => store("readwrite", (s) => s.delete(KEY)).catch(() => {});

  const clock = (t) => new Date(t).toLocaleTimeString([], { hour12: false });
  const span = (ms) => {
    const s = Math.max(0, Math.round(ms / 1000));
    if (s < 60) return `${s} s`;
    if (s < 3600) return `${Math.floor(s / 60)} min ${s % 60} s`;
    return `${Math.floor(s / 3600)} h ${Math.floor((s % 3600) / 60)} min`;
  };

  let snap = await load();   // { at, left, state, user }: saved at, with `left` ms on the timer
  let timerAt = 0;           // when the timer in the running process was set
  let machine = null;

  function show() {
    const now = Date.now();
    if (timerAt) {
      const due = timerAt + SECONDS * 1000;
      timerNote.textContent = now < due
        ? `set at ${clock(timerAt)}, due at ${clock(due)}: in ${span(due - now)}`
        : `set at ${clock(timerAt)}, due at ${clock(due)}: it has fired`;
    }
    restoreButtons.forEach((b) => (b.disabled = !snap || !machine));
    if (!snap) return;
    const due = snap.at + snap.left;
    savedNote.textContent = `Saved at ${clock(snap.at)}, ${span(now - snap.at)} ago, with ${span(snap.left)} left on its timer.`;
    predict.asleep.textContent = now >= due
      ? `It was due at ${clock(due)}: it fires as soon as it is restored.`
      : `It fires at ${clock(due)}, when it was due: ${span(due - now)} after a restore now.`;
    predict.none.textContent = `It fires ${span(snap.left)} after the restore, the time it had left when it was saved.`;
  }
  setInterval(show, 1000);
  show();

  const env = ["PATH=/bin", "HOME=/root", "TERM=xterm-256color", "PS1=~ # "];
  // A background sleep that says, when it ends, when it began
  const timer = `t=$(date +%T); (sleep ${SECONDS}; echo "the ${SECONDS}-second timer set at $t fired at $(date +%T)") & echo "timer set at $t"`;
  machine = await ArmlessMachine.mount(root, {
    options: (at) => ({
      cpus: 1,
      memory_mb: 64,
      filesystem: { json: at("demo/fs/filesystem.json"), baseurl: at("demo/fs/blobs/") },
      user: { argv: ["sh"], env, cwd: "/root" },
    }),
    fetching: "fetching armless and busybox",
    starting: "starting sh, no kernel",
    keys: [["date", "date +%T\n"], ["jobs", "jobs\n"]],
    command: () => `${timer}\n`,
    running: /^timer set at/m,
    status: () => "a timer is set. step 2: save it",
  });
  const { vm, term, set } = machine;
  saveButton.disabled = false;
  show();

  // The page's clock for when the timer was set: the moment the shell said so
  const decoder = new TextDecoder();
  let out = "";
  vm.on("console", (b) => {
    if (timerAt) return;
    out += decoder.decode(b, { stream: true });
    if (/^timer set at/m.test(out.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, ""))) { timerAt = Date.now(); show(); }
  });

  const note = (text) => term.write(`\r\n\x1b[2m-- ${text} --\x1b[0m\r\n`);

  saveButton.addEventListener("click", async () => {
    saveButton.disabled = true;
    try {
      await vm.stop();
      const state = await vm.save_state();
      const user = await vm.save_process();
      const at = Date.now();
      const left = timerAt ? Math.max(0, timerAt + SECONDS * 1000 - at) : 0;
      snap = { at, left, state, user };
      await keep(snap);
      timerAt = 0;
      timerNote.textContent = "saved with the process";
      set("off", "saved and stopped. step 3: restore it, now or later");
      note(`saved at ${clock(at)} with ${span(left)} left on the timer, and stopped`);
    } catch (e) {
      set("error", "could not save: " + (e.message || e));
      vm.run();
      saveButton.disabled = false;
    }
    // Saved, the machine is stopped: there is nothing to save again until a restore runs it
    show();
  });

  for (const button of restoreButtons) {
    button.addEventListener("click", async () => {
      if (!snap) return;
      const told = button.dataset.restore === "asleep";
      const asleep = told ? Date.now() - snap.at : 0;
      const expect = (told ? predict.asleep : predict.none).textContent;
      restoreButtons.forEach((b) => (b.disabled = true));
      try {
        await vm.restore_state(snap.state, { user_state: snap.user, user_asleep_ms: asleep });
        // What the console showed before is another process's: this one is the one saved
        term.reset();
        note(told
          ? `restored the process saved at ${clock(snap.at)}, told it was saved for ${span(asleep)}`
          : `restored the process saved at ${clock(snap.at)}, told nothing`);
        term.write(`\x1b[2m   ${expect}\x1b[0m\r\n`);
        if (!vm.running) vm.run();
        machine.yours();
        set("ready", told ? "restored, told how long it was saved" : "restored, told nothing");
        saveButton.disabled = false;
      } catch (e) {
        // A snapshot from an older build of the site may not restore on this one
        set("error", "could not restore: " + (e.message || e));
        snap = null;
        await forget();
        savedNote.textContent = "That snapshot didn't restore on this build, and is forgotten. Save a new one.";
      }
      show();
    });
  }
})();
