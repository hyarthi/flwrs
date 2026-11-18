<script lang="ts">
  import LogOut from '@lucide/svelte/icons/log-out';
  import ThemeToggle from './ThemeToggle.svelte';
  import { session } from '../lib/session.svelte';

  let { title }: { title: string } = $props();
</script>

<header class="topbar">
  <h1>{title}</h1>
  <div class="actions">
    <ThemeToggle />
    {#if session.info?.can_log_out}
      <button class="btn ghost icon" type="button" title="Log out" aria-label="Log out" onclick={() => session.logout()}>
        <LogOut size={18} strokeWidth={1.75} />
      </button>
    {/if}
  </div>
</header>

<style>
  .topbar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    height: var(--topbar-height);
    padding: 0 var(--space-5);
    border-bottom: 1px solid var(--border);
    background: var(--surface);
  }

  h1 {
    font-size: var(--text-lg);
  }

  .actions {
    display: flex;
    gap: var(--space-2);
  }
</style>
