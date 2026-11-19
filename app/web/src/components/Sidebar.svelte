<script lang="ts">
  // The main navigation. Collapsible to its icons (the button at the
  // bottom): the labels fade out while every icon stays where it is, and
  // while collapsed each item names itself in a tooltip.
  import Workflow from '@lucide/svelte/icons/workflow';
  import LayoutDashboard from '@lucide/svelte/icons/layout-dashboard';
  import Puzzle from '@lucide/svelte/icons/puzzle';
  import Server from '@lucide/svelte/icons/server';
  import ChevronLeft from '@lucide/svelte/icons/chevron-left';
  import logo from '../assets/logo.svg';
  import { sidebar } from '../lib/sidebar.svelte';

  let { section }: { section?: string } = $props();

  const items = [
    { id: 'overview', label: 'Overview', href: '/overview', icon: LayoutDashboard },
    { id: 'scenes', label: 'Scenes', href: '/scenes', icon: Workflow },
    { id: 'plugins', label: 'Plugins', href: '/plugins', icon: Puzzle },
    { id: 'server', label: 'Server', href: '/server', icon: Server },
  ];

  // The tooltip: fixed-positioned, so the sidebar's clipping doesn't cut it.
  let tip = $state<{ label: string; x: number; y: number } | null>(null);
  function show(e: Event, label: string) {
    if (!sidebar.collapsed) return;
    const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
    tip = { label, x: r.right + 8, y: r.top + r.height / 2 };
  }
  const hide = () => (tip = null);

  function toggle() {
    tip = null;
    sidebar.toggle();
  }
</script>

<nav class="sidebar" class:collapsed={sidebar.collapsed} aria-label="Main">
  <a class="brand" href="/" onmouseenter={(e) => show(e, 'flwrs')} onmouseleave={hide} onfocus={(e) => show(e, 'flwrs')} onblur={hide}>
    <img src={logo} alt="" width="28" height="28" />
    <span class="label">flwrs</span>
  </a>
  <ul>
    {#each items as item (item.id)}
      <li>
        <a
          href={item.href}
          class:active={section === item.id}
          aria-current={section === item.id ? 'page' : undefined}
          onmouseenter={(e) => show(e, item.label)}
          onmouseleave={hide}
          onfocus={(e) => show(e, item.label)}
          onblur={hide}
        >
          <item.icon size={18} strokeWidth={1.75} />
          <span class="label">{item.label}</span>
        </a>
      </li>
    {/each}
  </ul>
  <button
    class="toggle"
    type="button"
    aria-expanded={!sidebar.collapsed}
    aria-label={sidebar.collapsed ? 'Expand the sidebar' : 'Collapse the sidebar'}
    onclick={toggle}
    onmouseenter={(e) => show(e, 'Expand')}
    onmouseleave={hide}
    onfocus={(e) => show(e, 'Expand')}
    onblur={hide}
  >
    <span class="chevron"><ChevronLeft size={18} strokeWidth={1.75} /></span>
    <span class="label">Collapse</span>
  </button>
</nav>

{#if tip}
  <div class="tip" role="tooltip" style:left="{tip.x}px" style:top="{tip.y}px">{tip.label}</div>
{/if}

<style>
  .sidebar {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
    min-width: 0;
    padding: var(--space-3);
    overflow: hidden;
    background: var(--surface);
    border-right: 1px solid var(--border);
  }

  /* Labels fade as the sidebar narrows; they never wrap or push the icons. */
  .label {
    white-space: nowrap;
    transition: opacity var(--dur) var(--ease);
  }

  .collapsed .label {
    opacity: 0;
    pointer-events: none;
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
    flex: none;
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

  li a,
  .toggle {
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

  li a :global(svg),
  .toggle :global(svg) {
    flex: none;
  }

  li a:hover,
  .toggle:hover {
    background: var(--surface-hover);
    color: var(--text);
  }

  li a.active {
    background: var(--accent-soft);
    color: var(--accent);
  }

  .toggle {
    width: 100%;
    margin-top: auto;
    border: 0;
    background: none;
    font: inherit;
    font-size: var(--text-sm);
    cursor: pointer;
  }

  /* The chevron points where the button goes: left to collapse, right to expand. */
  .chevron {
    display: flex;
    flex: none;
    transition: transform var(--dur) var(--ease);
  }

  .collapsed .chevron {
    transform: rotate(180deg);
  }

  .tip {
    position: fixed;
    z-index: 100;
    padding: var(--space-1) var(--space-2);
    border-radius: var(--radius-sm);
    background: var(--text);
    color: var(--surface);
    font-size: var(--text-xs);
    font-weight: 500;
    white-space: nowrap;
    pointer-events: none;
    transform: translateY(-50%);
    box-shadow: var(--shadow);
  }
</style>
