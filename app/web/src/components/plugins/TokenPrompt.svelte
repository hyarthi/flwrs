<script lang="ts">
  // Asks for `[plugins] admin_token` when a plugin change needs it. Mounted
  // by the plugin pages; the token stays in memory for this tab.
  import { onDestroy } from 'svelte';
  import Modal from '../Modal.svelte';
  import { setTokenPrompt } from '../../lib/plugins';

  let pending = $state<{ retry: boolean; resolve: (token: string | null) => void } | null>(null);
  let token = $state('');

  setTokenPrompt((retry) => new Promise((resolve) => (pending = { retry, resolve })));
  onDestroy(() => setTokenPrompt(null));

  function finish(value: string | null) {
    pending?.resolve(value);
    pending = null;
    token = '';
  }
</script>

{#if pending}
  <Modal title="Admin token" size="sm" onclose={() => finish(null)}>
    <form
      onsubmit={(e) => {
        e.preventDefault();
        finish(token.trim() || null);
      }}
    >
      <p class="muted">
        {pending.retry ? 'That token was refused. ' : ''}Plugin changes on this server need its admin token (<code>[plugins] admin_token</code>).
        It’s kept for this tab only.
      </p>
      <label class="field">
        Token
        <!-- svelte-ignore a11y_autofocus -->
        <input class="input" type="password" autocomplete="off" bind:value={token} autofocus />
      </label>
      <div class="dialog-actions">
        <button class="btn" type="button" onclick={() => finish(null)}>Cancel</button>
        <button class="btn primary" type="submit" disabled={!token.trim()}>Continue</button>
      </div>
    </form>
  </Modal>
{/if}

<style>
  form {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
</style>
