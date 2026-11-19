// No kernel: busybox's sh runs in user mode, from demo/fs (a filesystem with busybox, site/demo.exs).
// The CPU runs it at EL0 and armless answers its system calls, /proc included. The pane beside it
// says what the machine has, how sh is doing (vm.user_status) and what armless answers to a few
// commands.
(async () => {
  const root = document.querySelector('[data-machine="nokernel"]');
  const q = (sel) => root.querySelector(sel);
  let files = 0;
  // When sh first wrote to its terminal, from when the machine started running
  let wroteAt = 0;
  const console_ = () => { wroteAt ||= performance.now(); };

  const m = await ArmlessMachine.mount(root, {
    options: (at) => ({
      cpus: 1,
      memory_mb: 64,
      filesystem: { json: at("demo/fs/filesystem.json"), baseurl: at("demo/fs/blobs/") },
      user: { argv: ["sh"], env: ["PATH=/bin", "HOME=/root", "TERM=xterm-256color", "PS1=~ # "], cwd: "/root" },
    }),
    fetching: "fetching armless and busybox. no kernel to boot",
    starting: "starting sh, no kernel",
    command: () => "uname -a\n",
    running: /armless-user/,
    status: () => "sh, with no kernel. the shell is yours",
    on: {
      console: console_,
      blob: () => { q("[data-files]").textContent = String(++files); },
    },
  });

  setInterval(() => {
    if (wroteAt) q("[data-started]").textContent = String(Math.max(0, Math.round(wroteAt - m.startedAt)));
    const s = m.vm.user_status;
    q("[data-sh]").textContent = !s ? "-" : s.state === "idle" ? (s.waiting_for_input ? "waiting for you" : "asleep") : s.state;
  }, 500);

  for (const b of root.querySelectorAll("[data-send]")) {
    b.disabled = false;
    b.addEventListener("click", () => {
      m.type(b.dataset.send);
      m.term.focus({ preventScroll: true });
    });
  }
})();
