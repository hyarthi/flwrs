// Runs before the first paint: applies the theme saved by the theme toggle
// (src/lib/theme.svelte.ts), so a dark-mode user never sees a light flash.
try {
  var t = localStorage.getItem('flwrs.theme');
  if (t === 'light' || t === 'dark') document.documentElement.dataset.theme = t;
} catch (e) {}
