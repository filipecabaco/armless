// The GPU example (site/examples/gpu.md): site/guest/gpu.cpp in the initramfs, run at the first
// prompt. The machine has a network device for the GPU's connection alone (allow() lets nothing
// out), and armless-gpu.js answers the guest's connections to gpu.internal:7070 with this page's
// WebGPU. Without WebGPU they are refused, and the program says so.
(() => {
  const root = document.querySelector('[data-machine="gpu"]');
  if (!root) return;
  const run = (cols, rows) => `gpu ${Math.min(cols - 1, 140)} ${Math.min(rows - 2, 44)}\n`;
  ArmlessMachine.mount(root, {
    needs: "gpu",
    scripts: ["demo/armless-gpu.js"],
    options: () => ({ network: { allow: () => false } }),
    made: (vm) => ArmlessGPU.attach(vm, { max_memory: 256 * 1048576 }),
    keys: [["^C", "\x03", "Send ctrl+c"], ["gpu", "gpu\n", "Run it again"], ["cpuinfo", "cat /proc/cpuinfo\n"]],
    command: run,
    running: /GPU: |No GPU answered/,
    status: () => ("gpu" in navigator ? "computed on this page's GPU. the shell is yours" : "this browser has no WebGPU: the guest says so"),
  });
})();
