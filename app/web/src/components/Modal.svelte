<script lang="ts">
  // A modal on the native <dialog>: focus trapping, Esc and the backdrop come
  // from the browser.
  import type { Snippet } from 'svelte';
  import X from '@lucide/svelte/icons/x';

  let {
    title,
    onclose,
    children,
    size = 'md',
    fixed = false,
  }: {
    title: string;
    onclose: () => void;
    children: Snippet;
    size?: 'sm' | 'md' | 'lg' | 'xl' | 'full';
    /**
     * Full height, and the dialog itself doesn't scroll: the content lays
     * out its own scrolling area (e.g. tabs above, actions below, fixed).
     */
    fixed?: boolean;
  } = $props();

  let dialog: HTMLDialogElement;

  $effect(() => {
    dialog.showModal();
    return () => dialog.close();
  });
</script>

<dialog
  class={size}
  class:fixed
  bind:this={dialog}
  onclose={onclose}
  onclick={(e) => {
    if (e.target === dialog) dialog.close();
  }}
>
  <div class="body">
    <header>
      <h2>{title}</h2>
      <button class="btn ghost icon" type="button" aria-label="Close" onclick={() => dialog.close()}>
        <X size={18} strokeWidth={1.75} />
      </button>
    </header>
    <!-- The title stays; only this scrolls (a `fixed` modal's content
         arranges its own scrolling). -->
    <div class="content">
      {@render children()}
    </div>
  </div>
</dialog>

<style>
  dialog {
    width: min(720px, calc(100vw - 2 * var(--space-5)));
    max-height: calc(100vh - 2 * var(--space-6));
    padding: 0;
    border: 1px solid var(--border);
    border-radius: var(--radius-lg);
    background: var(--surface);
    color: var(--text);
    box-shadow: var(--shadow);
  }

  dialog.lg {
    width: min(960px, calc(100vw - 2 * var(--space-5)));
  }

  dialog.full {
    width: min(1800px, calc(100vw - 2 * var(--space-4)));
    max-width: none;
  }

  dialog.xl {
    width: min(1280px, calc(100vw - 2 * var(--space-5)));
  }

  dialog.sm {
    width: min(440px, calc(100vw - 2 * var(--space-5)));
  }

  dialog[open] {
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }

  dialog.fixed {
    height: calc(100vh - 2 * var(--space-6));
  }

  dialog::backdrop {
    background: light-dark(#0f172a40, #00000099);
  }

  .body {
    display: flex;
    flex: 1;
    flex-direction: column;
    gap: var(--space-4);
    min-height: 0;
    padding: var(--space-4) var(--space-5) var(--space-5);
  }

  header {
    display: flex;
    flex: none;
    align-items: center;
    justify-content: space-between;
  }

  .content {
    display: flex;
    flex: 1;
    flex-direction: column;
    gap: var(--space-4);
    min-height: 0;
    overflow-y: auto;
    /* Room for focus rings and the scrollbar, without shifting the content. */
    margin: 0 calc(-1 * var(--space-2));
    padding: var(--space-1) var(--space-2);
  }

  /* A fixed modal's content lays out its own scrolling area. */
  .fixed .content {
    overflow: hidden;
  }
</style>
