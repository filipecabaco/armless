// How many CPUs the landing page's machine gets on this device. Each guest CPU is a Web Worker
// that runs flat out while the guest is busy, so the machine leaves one of the device's cores to
// the page itself, stays at four on phones and tablets (heat and battery), and at two where the
// browser says the device has 2 GB of memory or less. Without cross-origin isolation the CPUs
// can't share memory, so there is one. site/test/cpus.test.mjs covers it.
(function (root) {
  function cpuCount({ isolated, cores, memoryGb, coarse, max }) {
    if (!isolated) return 1;
    let n = Math.max(1, (cores || 2) - 1);
    if (coarse) n = Math.min(n, 4);
    if (memoryGb && memoryGb <= 2) n = Math.min(n, 2);
    return Math.max(1, Math.min(n, max));
  }
  if (typeof module === "object" && module.exports) module.exports = { cpuCount };
  else root.cpuCount = cpuCount;
})(this);
