<script lang="ts">
  import { location, match, navigate, isRouterLink } from './lib/router.svelte';
  import { routes, HOME } from './routes';
  import Sidebar from './components/Sidebar.svelte';
  import { sidebar } from './lib/sidebar.svelte';
  import Topbar from './components/Topbar.svelte';
  import NotFoundPage from './pages/NotFoundPage.svelte';
  import AccessPage from './pages/AccessPage.svelte';
  import LoginPage from './pages/LoginPage.svelte';
  import { session, loginPath, nextPath } from './lib/session.svelte';

  const resolved = $derived.by(() => {
    for (const route of routes) {
      const params = match(route.pattern, location.path);
      if (params) return { route, props: { ...route.props, ...params } };
    }
    return null;
  });

  const onLogin = $derived(location.path === '/login');

  session.load();

  // Logged out: to the login page (and back afterwards). Logged in: away
  // from it. `/` goes home.
  $effect(() => {
    if (session.status === 'out' && !onLogin) {
      navigate(loginPath(location.path + location.search), { replace: true });
    } else if (session.status === 'in' && onLogin) {
      const next = nextPath(location.search);
      navigate(next === '/' ? HOME : next, { replace: true });
    } else if (session.status === 'in' && location.path === '/') {
      navigate(HOME, { replace: true });
    }
  });

  $effect(() => {
    document.title = resolved ? `${resolved.route.title} · flwrs` : 'flwrs';
  });

  // Same-origin <a href> clicks go through the router (no page reload).
  function onclick(event: MouseEvent) {
    const anchor = (event.target as Element | null)?.closest?.('a');
    if (anchor instanceof HTMLAnchorElement && isRouterLink(event, anchor)) {
      event.preventDefault();
      const url = new URL(anchor.href);
      if (url.pathname + url.search !== location.path + location.search) {
        navigate(url.pathname + url.search + url.hash);
      }
    }
  }
</script>

<svelte:document {onclick} />

{#if session.status === 'loading'}
  <!-- The first check is quick: nothing rather than a flash of the frame. -->
{:else if session.status === 'refused' || session.status === 'unreachable'}
  <AccessPage />
{:else if session.status === 'out' || onLogin}
  {#if onLogin}
    <LoginPage />
  {/if}
{:else if resolved?.route.bare}
  <resolved.route.component {...resolved.props} />
{:else}
  <div class="frame" class:collapsed={sidebar.collapsed}>
    <Sidebar section={resolved?.route.section} />
    <div class="main">
      <Topbar title={resolved?.route.title ?? 'Not found'} />
      <main class="content">
        {#if resolved}
          {#key resolved.route.group ? `${resolved.route.group}:${resolved.props.id ?? ''}` : resolved.route.pattern}
            <resolved.route.component {...resolved.props} />
          {/key}
        {:else if location.path !== '/'}
          <NotFoundPage />
        {/if}
      </main>
    </div>
  </div>
{/if}

<style>
  /* The page takes whatever the sidebar leaves, as it collapses or expands. */
  .frame {
    display: grid;
    grid-template-columns: var(--sidebar-width) minmax(0, 1fr);
    height: 100%;
    transition: grid-template-columns var(--dur) var(--ease);
  }

  .frame.collapsed {
    grid-template-columns: var(--sidebar-collapsed-width) minmax(0, 1fr);
  }

  .main {
    display: flex;
    flex-direction: column;
    min-width: 0;
    min-height: 0;
  }

  .content {
    flex: 1;
    min-height: 0;
    overflow: auto;
  }
</style>
