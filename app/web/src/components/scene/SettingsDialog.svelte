<script lang="ts">
  // The scene's settings (name, hop limit, node defaults), edited in the
  // draft like the graph: they reach live when the draft is submitted.
  import { untrack } from 'svelte';
  import Modal from '../Modal.svelte';
  import { ApiError } from '../../lib/api';
  import { updateDraftSettings, type SceneView } from '../../lib/sceneView';
  import { getInfo, type ServerInfo } from '../../lib/server';

  let { view, onclose }: { view: SceneView; onclose: () => void } = $props();

  // The form starts from the settings as they are when it opens.
  const current = untrack(() => view.draft?.settings ?? view.scene);
  let name = $state(current.name);
  let maxHops = $state<number | null | undefined>(current.max_hops);
  let durable = $state(current.defaults.durable ?? false);
  let remoteAuth = $state<'' | 'token' | 'mtls'>(current.defaults.remote_auth ?? '');
  /** '' = the server's default (TLS required when the hub has a TLS listener). */
  let requireTls = $state<'' | 'true' | 'false'>(current.defaults.require_tls === undefined ? '' : String(current.defaults.require_tls) as 'true' | 'false');

  // The server's own defaults, to say what leaving a setting out means.
  let info = $state<ServerInfo | null>(null);
  getInfo()
    .then((i) => (info = i))
    .catch(() => {});

  let busy = $state(false);
  let error = $state<ApiError | null>(null);

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    busy = true;
    error = null;
    try {
      await updateDraftSettings(view.scene.id, {
        name: name.trim(),
        max_hops: typeof maxHops === 'number' ? maxHops : null,
        defaults: {
          ...(durable ? { durable } : {}),
          ...(remoteAuth ? { remote_auth: remoteAuth } : {}),
          ...(requireTls ? { require_tls: requireTls === 'true' } : {}),
        },
      });
      onclose();
    } catch (e) {
      error = e instanceof ApiError ? e : new ApiError(0, String(e));
    } finally {
      busy = false;
    }
  }
</script>

<Modal title="Scene settings" {onclose} size="sm">
  <form onsubmit={submit}>
    <label class="field">
      Name
      <input class="input" bind:value={name} maxlength="200" required />
    </label>
    <label class="field">
      Hop limit
      <input class="input" type="number" min="1" max="10000" step="1" placeholder={info ? `Default (${info.max_hops})` : 'Default'} bind:value={maxHops} />
      <span class="hint">Events are dropped after this many handovers (stops runaway loops). Empty = the server’s default (<code>[director] max_hops</code>).</span>
    </label>
    <fieldset>
      <legend>Defaults for the scene’s nodes</legend>
      <label class="check"><input type="checkbox" bind:checked={durable} />Durable queues (events survive a restart) <span class="muted">(default: off)</span></label>
      <label class="field">
        Remote nodes authenticate with
        <select class="input" bind:value={remoteAuth}>
          <option value="">The server’s default (token)</option>
          <option value="token">A pre-shared token</option>
          <option value="mtls">A TLS client certificate (mTLS)</option>
        </select>
      </label>
      <label class="field">
        Remote nodes with a token must connect over TLS
        <select class="input" bind:value={requireTls}>
          <option value="">The server’s default{info ? (info.hub_tls_address ? ' (yes)' : ' (no)') : ''}</option>
          <option value="true">Yes</option>
          <option value="false">No (plain TCP allowed)</option>
        </select>
        <span class="hint">The server requires TLS when its hub has a TLS listener (<code>[hub.tls]</code>){info ? (info.hub_tls_address ? ': it has one.' : ': it has none.') : '.'}</span>
      </label>
      <span class="hint muted">A node’s own setting wins over these.</span>
    </fieldset>
    {#if error}
      <div class="notice danger" role="alert">
        <div>
          {error.message}
          {#if error.details.length}
            <ul>{#each error.details as detail, i (i)}<li>{detail}</li>{/each}</ul>
          {/if}
        </div>
      </div>
    {/if}
    <p class="muted hint">Saved to the draft: live changes when you submit it.</p>
    <div class="dialog-actions">
      <button class="btn" type="button" onclick={onclose}>Cancel</button>
      <button class="btn primary" type="submit" disabled={busy || !name.trim()}>Save to draft</button>
    </div>
  </form>
</Modal>

<style>
  form,
  fieldset {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  fieldset {
    margin: 0;
    padding: var(--space-3);
    border: 1px solid var(--border);
    border-radius: var(--radius);
  }

  legend {
    padding: 0 var(--space-1);
    font-size: var(--text-sm);
    font-weight: 500;
  }

  .hint {
    font-size: var(--text-xs);
  }

  ul {
    margin: var(--space-1) 0 0;
    padding-left: var(--space-4);
  }
</style>
