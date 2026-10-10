// Small progressive enhancements. Every page works without this script.
(() => {
  "use strict";

  const copyText = async (text, button) => {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      const area = document.createElement("textarea");
      area.value = text;
      document.body.appendChild(area);
      area.select();
      document.execCommand("copy");
      area.remove();
    }
    const original = button.innerHTML;
    button.innerHTML =
      '<svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 6 9 17l-5-5"/></svg>';
    button.classList.add("copied");
    setTimeout(() => {
      button.innerHTML = original;
      button.classList.remove("copied");
    }, 1500);
  };

  document.addEventListener("click", (event) => {
    const target = event.target.closest("[data-copy], [data-copy-text]");
    if (target) {
      event.preventDefault();
      const text = target.dataset.copyText ?? document.querySelector(target.dataset.copy)?.value ?? "";
      copyText(text, target);
      return;
    }

    const toggle = event.target.closest("[data-toggle]");
    if (toggle) {
      const el = document.getElementById(toggle.dataset.toggle);
      if (el) {
        el.hidden = !el.hidden;
        if (!el.hidden) el.querySelector("input, textarea")?.focus();
      }
      return;
    }

    const collapse = event.target.closest("[data-collapse]");
    if (collapse) {
      collapse.closest(".diff-file")?.classList.toggle("collapsed");
      return;
    }

    const cloneTab = event.target.closest("[data-clone-tab]");
    if (cloneTab) {
      const panel = cloneTab.closest(".clone-panel");
      panel.querySelectorAll("[data-clone-tab]").forEach((t) => t.classList.toggle("selected", t === cloneTab));
      panel.querySelectorAll("[data-clone-pane]").forEach((p) => {
        p.hidden = p.dataset.clonePane !== cloneTab.dataset.cloneTab;
      });
      return;
    }

    // Close open dropdowns when clicking elsewhere.
    document.querySelectorAll("details.dropdown[open]").forEach((d) => {
      if (!d.contains(event.target)) d.removeAttribute("open");
    });
  });

  // Only one dropdown open at a time; focus the filter when opening.
  document.querySelectorAll("details.dropdown").forEach((details) => {
    details.addEventListener("toggle", () => {
      if (!details.open) return;
      document.querySelectorAll("details.dropdown[open]").forEach((other) => {
        if (other !== details) other.removeAttribute("open");
      });
      details.querySelector("[data-filter]")?.focus();
    });
  });

  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      document.querySelectorAll("details.dropdown[open]").forEach((d) => {
        d.removeAttribute("open");
        d.querySelector("summary")?.focus();
      });
    }
  });

  document.addEventListener("input", (event) => {
    const filter = event.target.closest("[data-filter]");
    if (!filter) return;
    const query = filter.value.trim().toLowerCase();
    filter
      .closest(".dropdown-menu")
      .querySelectorAll("[data-filter-item]")
      .forEach((item) => {
        item.hidden = query !== "" && !item.textContent.toLowerCase().includes(query);
      });
  });

  document.addEventListener("change", (event) => {
    const select = event.target.closest("select[data-autosubmit]");
    if (select) select.form.submit();
  });

  document.addEventListener("submit", (event) => {
    const message = event.target.dataset.confirm;
    if (message && !window.confirm(message)) event.preventDefault();
  });

  // Highlight #L10 or #L10-L20 in file views.
  const highlightLines = () => {
    document.querySelectorAll("tr.highlighted").forEach((row) => row.classList.remove("highlighted"));
    const match = location.hash.match(/^#L(\d+)(?:-L(\d+))?$/);
    if (!match) return;
    const start = Number(match[1]);
    const end = Number(match[2] || match[1]);
    for (let n = Math.min(start, end); n <= Math.max(start, end); n++) {
      document.getElementById(`L${n}`)?.classList.add("highlighted");
    }
  };

  // Shift-click a line number to select a range.
  let anchorLine = null;
  document.addEventListener("click", (event) => {
    const link = event.target.closest(".code-view .num a");
    if (!link) return;
    const line = Number(link.getAttribute("href").slice(2));
    if (event.shiftKey && anchorLine) {
      event.preventDefault();
      const [a, b] = [Math.min(anchorLine, line), Math.max(anchorLine, line)];
      history.replaceState(null, "", `#L${a}-L${b}`);
      highlightLines();
    } else {
      anchorLine = line;
    }
  });

  window.addEventListener("hashchange", highlightLines);
  highlightLines();
  const first = document.querySelector("tr.highlighted");
  if (first) first.scrollIntoView({ block: "center" });
})();
