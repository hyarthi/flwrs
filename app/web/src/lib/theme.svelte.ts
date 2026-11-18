// The theme toggle's state: follow the system, or force light / dark.
// public/theme-init.js applies the saved choice before the first paint.

export type Theme = 'system' | 'light' | 'dark';

const KEY = 'flwrs.theme';

function load(): Theme {
  try {
    const saved = localStorage.getItem(KEY);
    if (saved === 'light' || saved === 'dark') return saved;
  } catch {
    // Storage blocked (private mode, policies): follow the system.
  }
  return 'system';
}

class ThemeState {
  current = $state<Theme>(load());

  set(theme: Theme) {
    this.current = theme;
    const root = document.documentElement;
    if (theme === 'system') delete root.dataset.theme;
    else root.dataset.theme = theme;
    try {
      if (theme === 'system') localStorage.removeItem(KEY);
      else localStorage.setItem(KEY, theme);
    } catch {
      // Not remembered; still applied for this page.
    }
  }

  /** system → light → dark → system */
  cycle() {
    this.set(this.current === 'system' ? 'light' : this.current === 'light' ? 'dark' : 'system');
  }
}

export const theme = new ThemeState();
