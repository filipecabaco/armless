// The trench run, on the landing page and on its example page: site/guest/trench, a shell script
// in the initramfs that runs one awk per CPU, typed at the first prompt.
ArmlessMachine.mount(document.querySelector('[data-machine="trench"]'), {
  keys: [["^C", "\x03", "Send ctrl+c"], ["nproc", "nproc\n"], ["cpuinfo", "cat /proc/cpuinfo\n"]],
  command: (cols, rows) => `trench ${Math.min(cols - 1, 110)} ${Math.min(rows - 2, 40)}\n`,
  running: /frame 1:/,
  status: (cpus) => `flying the trench on ${cpus} CPU${cpus > 1 ? "s" : ""}. ctrl+c stops it`,
});
