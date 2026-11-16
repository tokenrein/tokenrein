// Tokenrein dashboard: light unless this browser chose dark. Loaded before the page renders, so it
// never flashes the other theme.
"use strict";
(function () {
  let theme = null;
  try { theme = localStorage.getItem("tokenrein.theme"); } catch (_) { /* storage blocked: light */ }
  document.documentElement.dataset.theme = theme === "dark" ? "dark" : "light";
})();
