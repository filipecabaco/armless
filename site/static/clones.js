// Two copies of one snapshot, side by side: both restore demo/native.bin.zst, the same memory to
// the byte, and read 16 bytes from /dev/urandom. armless gives each restore a new VM generation
// ID, and Linux (6.10 and later, with CONFIG_VMGENID) reseeds its generator when it changes.
(() => {
  const first = document.querySelector('[data-machine="clones"]');
  const second = document.querySelector('[data-machine="clone-b"]');
  const verdict = second.querySelector("[data-verdict]");
  const read = "head -c 16 /dev/urandom | od -An -tx1\n";
  const line = /^((?: [0-9a-f]{2}){16})\s*$/m;
  const plain = (s) => s.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, "");
  const bytes = [null, null];

  const copy = (root, i) =>
    ArmlessMachine.mount(root, {
      needs: "native",
      options: (at) => ({ cpus: 1, initial_state: at("demo/native.bin.zst") }),
      restored: true,
      fetching: `fetching copy ${i + 1} of the snapshot`,
      starting: "restoring",
      keys: i === 0 ? [["urandom", read]] : [],
      command: () => read,
      running: line,
      status: () => "restored, and read 16 bytes",
    }).then((m) => {
      const decoder = new TextDecoder();
      let out = "";
      m.vm.on("console", (b) => {
        if (bytes[i]) return;
        out += decoder.decode(b, { stream: true });
        const found = line.exec(plain(out));
        if (found) {
          bytes[i] = found[1].trim();
          judge();
        }
      });
    });

  function judge() {
    if (!bytes[0] || !bytes[1]) return;
    verdict.dataset.state = bytes[0] === bytes[1] ? "same" : "different";
    verdict.textContent = bytes[0] === bytes[1]
      ? "Both copies read the same bytes: their generators were not reseeded."
      : "The copies read different bytes: each reseeded when it was restored.";
  }

  copy(first, 0);
  copy(second, 1);
})();
