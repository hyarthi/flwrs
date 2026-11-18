// A small history-API router: the current location as reactive state,
// `navigate`, and path patterns with `:name` segments. Plain <a href> links
// work everywhere: App.svelte intercepts same-origin clicks.

class Location {
  path = $state(window.location.pathname);
  search = $state(window.location.search);

  sync() {
    this.path = window.location.pathname;
    this.search = window.location.search;
  }
}

export const location = new Location();

window.addEventListener('popstate', () => location.sync());

export function navigate(to: string, options: { replace?: boolean } = {}) {
  if (options.replace) history.replaceState(null, '', to);
  else history.pushState(null, '', to);
  location.sync();
}

export type Params = Record<string, string>;

/** Matches `path` against a pattern such as `/scenes/by-id/:id`. */
export function match(pattern: string, path: string): Params | null {
  const want = pattern.split('/').filter(Boolean);
  const got = path.split('/').filter(Boolean);
  if (want.length !== got.length) return null;
  const params: Params = {};
  for (let i = 0; i < want.length; i++) {
    if (want[i].startsWith(':')) {
      try {
        params[want[i].slice(1)] = decodeURIComponent(got[i]);
      } catch {
        return null;
      }
    } else if (want[i] !== got[i]) {
      return null;
    }
  }
  return params;
}

/** Whether a click on `anchor` should be handled by the router. */
export function isRouterLink(event: MouseEvent, anchor: HTMLAnchorElement): boolean {
  if (event.defaultPrevented || event.button !== 0) return false;
  if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return false;
  if (anchor.target && anchor.target !== '_self') return false;
  if (anchor.hasAttribute('download') || anchor.dataset.external !== undefined) return false;
  const url = new URL(anchor.href);
  if (url.origin !== window.location.origin) return false;
  // The server's own pages (API, API docs, Swagger UI) aren't the UI's.
  return !/^\/(api|api-docs|swagger-ui)(\/|$)/.test(url.pathname);
}
