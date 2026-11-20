// Egress the page decides: the guest's wget goes out through the page's fetch(), and every request
// is first put to allow(), which here lets through the hosts ticked in the list and never a
// private address (allow() decides alone, so it checks Armless.private_host itself), and logs it.
(() => {
  const root = document.querySelector('[data-machine="egress"]');
  const list = root.querySelector("[data-requests]");
  const boxes = [...root.querySelectorAll('.allow input[type="checkbox"]')];
  const allowed = (host) => boxes.some((b) => b.checked && b.value === host);

  function log(url, ok, why) {
    list.querySelector(".empty")?.remove();
    const li = document.createElement("li");
    li.dataset.ok = ok;
    const verdict = Object.assign(document.createElement("span"), { className: "verdict-tag", textContent: ok ? "allowed" : "refused" });
    const what = Object.assign(document.createElement("span"), { className: "url", textContent: url });
    const note = Object.assign(document.createElement("span"), { className: "why", textContent: why });
    li.append(verdict, what, note);
    list.prepend(li);
    while (list.childElementCount > 20) list.lastElementChild.remove();
  }

  ArmlessMachine.mount(root, {
    options: () => ({
      network: {
        upgrade_https: true,
        allow: ({ url, host }) => {
          let ok = false, why;
          if (Armless.private_host(host)) why = "a private address";
          else if (allowed(host)) { ok = true; why = "on the list"; }
          else why = "not on the list";
          log(url, ok, why);
          return ok;
        },
      },
    }),
    keys: [
      ["api.github.com", "wget -qO- http://api.github.com/zen; echo\n"],
      ["httpbin.org", "wget -qO- http://httpbin.org/uuid\n"],
      ["10.0.0.1", "wget -qO- http://10.0.0.1/\n"],
    ],
    command: () => "wget -qO- http://api.github.com/zen; echo\n",
    status: "networked. the shell is yours",
  });
})();
