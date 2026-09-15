// xil gui: tab switching, the global refresh, and live stage logs.
(function () {
  "use strict";

  // ── Tabs ─────────────────────────────────────────────────────────────
  // A nav[data-group] of buttons shows the matching .tab of the same group.
  // The open tab of each group survives a reload.
  function remember(group, tab) {
    try { localStorage.setItem("xil-tab-" + group, tab); } catch (e) { /* private window */ }
  }
  function recall(group) {
    try { return localStorage.getItem("xil-tab-" + group); } catch (e) { return null; }
  }
  function show(group, tab) {
    document.querySelectorAll('nav.tabs[data-group="' + group + '"] button').forEach(function (b) {
      b.classList.toggle("active", b.dataset.tab === tab);
    });
    document.querySelectorAll('.tab[data-group="' + group + '"]').forEach(function (s) {
      s.hidden = s.dataset.tab !== tab;
      // Panels that are costly to fill (the Episodes status scan) wait for
      // their first showing instead of running on every page load.
      if (!s.hidden) s.dispatchEvent(new CustomEvent("tab-shown"));
    });
    remember(group, tab);
  }
  // After htmx has wired its triggers, so a restored tab's first "tab-shown"
  // is heard.
  document.addEventListener("DOMContentLoaded", function () {
  document.querySelectorAll("nav.tabs").forEach(function (nav) {
    var group = nav.dataset.group;
    var buttons = nav.querySelectorAll("button");
    var saved = recall(group);
    var first = buttons.length ? buttons[0].dataset.tab : null;
    var valid = Array.prototype.some.call(buttons, function (b) { return b.dataset.tab === saved; });
    show(group, valid ? saved : first);
    nav.addEventListener("click", function (ev) {
      var b = ev.target.closest("button[data-tab]");
      if (b) show(group, b.dataset.tab);
    });
  });
  });

  // ── Global refresh ───────────────────────────────────────────────────
  document.getElementById("global-refresh-btn").addEventListener("click", function () {
    fetch("/choices/invalidate", { method: "POST" }).then(function () {
      htmx.ajax("GET", "/episodes/table?force=1", { target: "#episodes-table" });
      htmx.trigger(document.body, "refresh-all");
    });
  });

  // ── Live logs ────────────────────────────────────────────────────────
  // A swapped-in <pre data-job="N"> streams /jobs/N/stream until the exit line.
  function follow(pre) {
    if (pre.dataset.following) return;
    pre.dataset.following = "1";
    var src = new EventSource("/jobs/" + pre.dataset.job + "/stream");
    function append(text) {
      var atBottom = pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 4;
      pre.textContent += text;
      if (atBottom) pre.scrollTop = pre.scrollHeight;
    }
    src.addEventListener("line", function (ev) { append(ev.data + "\n"); });
    src.addEventListener("exit", function (ev) {
      append("\n" + ev.data);
      src.close();
      htmx.trigger(document.body, "job-done");
    });
    src.onerror = function () { src.close(); };
  }
  function scan(root) {
    (root.querySelectorAll ? root : document).querySelectorAll("pre[data-job]").forEach(follow);
  }
  document.body.addEventListener("htmx:afterSwap", function (ev) { scan(ev.target); });
  scan(document);
})();
