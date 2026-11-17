// A copy button on every code block. Without JavaScript the blocks are still selectable text.
(() => {
  if (!navigator.clipboard) return;
  for (const pre of document.querySelectorAll("pre")) {
    const wrap = document.createElement("div");
    wrap.className = "code";
    pre.replaceWith(wrap);
    wrap.append(pre);
    const button = Object.assign(document.createElement("button"), { type: "button", className: "btn copy", textContent: "copy" });
    button.setAttribute("aria-label", "Copy this code");
    button.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(pre.innerText.replace(/\n$/, ""));
        button.textContent = "copied";
      } catch {
        button.textContent = "select it to copy";
      }
      setTimeout(() => (button.textContent = "copy"), 1800);
    });
    wrap.append(button);
  }
})();
