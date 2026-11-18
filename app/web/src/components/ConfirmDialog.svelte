<script lang="ts">
  // "Are you sure?" for actions that can't be undone. `onconfirm` may be
  // async: the dialog stays open (busy) until it settles, and shows its error.
  import type { Snippet } from 'svelte';
  import Modal from './Modal.svelte';

  let {
    title,
    confirm,
    danger = false,
    onconfirm,
    onclose,
    children,
  }: {
    title: string;
    /** The confirm button's label. */
    confirm: string;
    danger?: boolean;
    onconfirm: () => Promise<void> | void;
    onclose: () => void;
    children: Snippet;
  } = $props();

  let busy = $state(false);
  let error = $state<string | null>(null);

  async function run() {
    busy = true;
    error = null;
    try {
      await onconfirm();
      onclose();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }
</script>

<Modal {title} {onclose} size="sm">
  <div class="text">{@render children()}</div>
  {#if error}
    <p class="notice danger" role="alert">{error}</p>
  {/if}
  <div class="dialog-actions">
    <button class="btn" type="button" onclick={onclose} disabled={busy}>Cancel</button>
    <button class="btn" class:primary={!danger} class:solid-danger={danger} type="button" onclick={run} disabled={busy}>
      {confirm}
    </button>
  </div>
</Modal>

<style>
  .text {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    font-size: var(--text-sm);
  }

  .solid-danger {
    border-color: var(--danger);
    background: var(--danger);
    color: var(--on-accent);
  }

  .solid-danger:hover {
    background: var(--danger);
    filter: brightness(1.08);
  }
</style>
