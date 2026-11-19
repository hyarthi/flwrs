// The sidebar: expanded (icons and labels) or collapsed to its icons, to
// give the page more room. Remembered in this browser.

const KEY = 'flwrs.sidebar';

function load(): boolean {
  try {
    return localStorage.getItem(KEY) === 'collapsed';
  } catch {
    // Storage blocked (private mode, policies): expanded.
    return false;
  }
}

class SidebarState {
  collapsed = $state(load());

  toggle() {
    this.collapsed = !this.collapsed;
    try {
      if (this.collapsed) localStorage.setItem(KEY, 'collapsed');
      else localStorage.removeItem(KEY);
    } catch {
      // Not remembered; still applied for this page.
    }
  }
}

export const sidebar = new SidebarState();
