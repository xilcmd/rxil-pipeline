// Render ```mermaid blocks. Mermaid loads from a CDN; offline, the diagrams
// stay readable as their source text.
(function () {
  var blocks = document.querySelectorAll("code.language-mermaid");
  if (!blocks.length) return;
  var s = document.createElement("script");
  s.src = "https://cdn.jsdelivr.net/npm/mermaid@11.4.1/dist/mermaid.min.js";
  s.onload = function () {
    blocks.forEach(function (code) {
      var div = document.createElement("pre");
      div.className = "mermaid";
      div.textContent = code.textContent;
      code.parentElement.replaceWith(div);
    });
    var dark = /(coal|navy|ayu)/.test(document.documentElement.className);
    window.mermaid.initialize({ startOnLoad: false, theme: dark ? "dark" : "default" });
    window.mermaid.run();
  };
  document.head.appendChild(s);
})();
