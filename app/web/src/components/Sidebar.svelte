<script lang="ts">
  import Workflow from '@lucide/svelte/icons/workflow';
  import LayoutDashboard from '@lucide/svelte/icons/layout-dashboard';
  import Puzzle from '@lucide/svelte/icons/puzzle';
  import Server from '@lucide/svelte/icons/server';
  import logo from '../assets/logo.svg';

  let { section }: { section?: string } = $props();

  const items = [
    { id: 'scenes', label: 'Scenes', href: '/scenes', icon: Workflow },
    { id: 'overview', label: 'Overview', href: '/overview', icon: LayoutDashboard },
    { id: 'plugins', label: 'Plugins', href: '/plugins', icon: Puzzle },
    { id: 'server', label: 'Server', href: '/server', icon: Server },
  ];
</script>

<nav class="sidebar" aria-label="Main">
  <a class="brand" href="/">
    <img src={logo} alt="" width="28" height="28" />
    <span>flwrs</span>
  </a>
  <ul>
    {#each items as item (item.id)}
      <li>
        <a href={item.href} class:active={section === item.id} aria-current={section === item.id ? 'page' : undefined}>
          <item.icon size={18} strokeWidth={1.75} />
          {item.label}
        </a>
      </li>
    {/each}
  </ul>
</nav>

<style>
  .sidebar {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
    padding: var(--space-3);
    background: var(--surface);
    border-right: 1px solid var(--border);
  }

  .brand {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    height: calc(var(--topbar-height) - 2 * var(--space-3));
    padding: 0 var(--space-2);
    color: var(--text);
    font-size: var(--text-lg);
    font-weight: 650;
    letter-spacing: -0.02em;
  }

  .brand img {
    border-radius: 6px;
  }

  ul {
    display: flex;
    flex-direction: column;
    gap: 2px;
    margin: 0;
    padding: 0;
    list-style: none;
  }

  li a {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    height: 34px;
    padding: 0 var(--space-3);
    border-radius: var(--radius);
    color: var(--text-muted);
    font-weight: 500;
    transition:
      background var(--dur-fast) var(--ease),
      color var(--dur-fast) var(--ease);
  }

  li a:hover {
    background: var(--surface-hover);
    color: var(--text);
  }

  li a.active {
    background: var(--accent-soft);
    color: var(--accent);
  }
</style>
