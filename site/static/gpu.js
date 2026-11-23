// The GPU example (site/examples/gpu.md): site/guest/gpu.cpp in the initramfs, a 3D scene raymarched in real time, run at the first
// prompt. The machine has a network device for the GPU's connection alone (allow() lets nothing
// out), and armless-gpu.js answers the guest's connections to gpu.internal:7070 with this page's
// WebGPU. Without WebGPU they are refused, and the program says so.
(async () => {
  const root = document.querySelector('[data-machine="gpu"]');
  if (!root) return;
  const run = (cols, rows) => `gpu ${Math.min(cols - 1, 140)} ${Math.min(rows - 2, 44)}\n`;
  const m = await ArmlessMachine.mount(root, {
    needs: "gpu",
    scripts: ["demo/armless-gpu.js"],
    options: () => ({ network: { allow: () => false } }),
    made: (vm) => ArmlessGPU.attach(vm, { max_memory: 256 * 1048576 }),
    keys: [["c", "c", "Render on the GPU or on the emulated CPU"], ["q", "q", "Stop it"], ["gpu", "gpu\n", "Run it again"]],
    command: run,
    running: /GPU: |No GPU answered/,
    status: "asking for a GPU",
  });
  // What the program found, as it says (a browser can have navigator.gpu and still no GPU to give)
  const said = await m.waitFor(/^GPU: (.+)$|No GPU answered/m);
  // After mount's own status for the same line (set as the command is seen running)
  await new Promise((resolve) => setTimeout(resolve, 0));
  m.set("ready", said?.[1] ? `raymarching on ${said[1].trim()}. c: the same on the CPU, q: stop` : "no WebGPU in this browser, the program says. the shell is yours");
})();
