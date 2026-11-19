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
  }: { title: string; onclose: () => void; children: Snippet; size?: 'sm' | 'md' | 'lg' | 'xl' } = $props();

  let dialog: HTMLDialogElement;

  $effect(() => {
    dialog.showModal();
    return () => dialog.close();
  });
</script>

<dialog
  class={size}
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
    {@render children()}
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

  dialog.xl {
    width: min(1280px, calc(100vw - 2 * var(--space-5)));
  }

  dialog.sm {
    width: min(440px, calc(100vw - 2 * var(--space-5)));
  }

  dialog::backdrop {
    background: light-dark(#0f172a40, #00000099);
  }

  .body {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
    padding: var(--space-4) var(--space-5) var(--space-5);
  }

  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
</style>
