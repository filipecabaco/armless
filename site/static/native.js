// Started natively, resumed in the tab: demo/native.bin.zst is the site's guest, booted by a native
// armless-run when the site was built and saved at its shell prompt, once it had written where
// and when to /root/origin (site/demo.exs). The page restores it, on the one CPU it was saved with.
ArmlessMachine.mount(document.querySelector('[data-machine="native"]'), {
  needs: "native",
  options: (at) => ({ cpus: 1, initial_state: at("demo/native.bin.zst") }),
  keys: [["origin", "cat /root/origin\n"], ["uname", "uname -a\n"], ["uptime", "uptime\n"]],
  command: () => "cat /root/origin\n",
  running: /Booted and saved/,
  status: "resumed in this tab. the shell is yours",
});
