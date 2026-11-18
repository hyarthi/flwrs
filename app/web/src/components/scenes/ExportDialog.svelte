<script lang="ts">
  import Download from '@lucide/svelte/icons/download';
  import Modal from '../Modal.svelte';
  import { exportUrl } from '../../lib/scenes';

  let { id, name, onclose }: { id: string; name: string; onclose: () => void } = $props();

  let format = $state<'flwr' | 'far'>('flwr');
  let secrets = $state(false);
</script>

<Modal title={`Export “${name}”`} {onclose} size="sm">
  <fieldset>
    <legend class="field">Format</legend>
    <label class="check">
      <input type="radio" name="format" value="flwr" bind:group={format} />
      <span><strong>.flwr</strong> <span class="muted">— the scene: nodes, edges, mappings, settings</span></span>
    </label>
    <label class="check">
      <input type="radio" name="format" value="far" bind:group={format} />
      <span><strong>.far</strong> <span class="muted">— the scene and the files uploaded to its nodes</span></span>
    </label>
  </fieldset>
  <label class="check">
    <input type="checkbox" bind:checked={secrets} />
    Include secrets (tokens, secret settings)
  </label>
  {#if secrets}
    <p class="notice warn">The file will hold secrets: store and share it like a password.</p>
  {/if}
  <div class="dialog-actions">
    <button class="btn" type="button" onclick={onclose}>Cancel</button>
    <a class="btn primary" href={exportUrl(id, format, secrets)} download data-external onclick={onclose}>
      <Download size={16} strokeWidth={1.75} /> Download
    </a>
  </div>
</Modal>

<style>
  fieldset {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    margin: 0;
    padding: 0;
    border: 0;
  }

  legend {
    margin-bottom: var(--space-1);
  }

  .btn.primary:hover {
    text-decoration: none;
  }
</style>
