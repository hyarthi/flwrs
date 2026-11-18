<script lang="ts">
  import Modal from '../Modal.svelte';
  import { createScene, type Scene } from '../../lib/scenes';
  import { ApiError } from '../../lib/api';

  let { onclose, oncreated }: { onclose: () => void; oncreated: (scene: Scene) => void } = $props();

  let name = $state('');
  let busy = $state(false);
  let error = $state<ApiError | null>(null);

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (!name.trim() || busy) return;
    busy = true;
    error = null;
    try {
      oncreated(await createScene(name.trim()));
    } catch (e) {
      error = e instanceof ApiError ? e : new ApiError(0, String(e));
    } finally {
      busy = false;
    }
  }
</script>

<Modal title="New scene" {onclose} size="sm">
  <form onsubmit={submit}>
    <label class="field">
      Name
      <!-- svelte-ignore a11y_autofocus -->
      <input class="input" bind:value={name} maxlength="200" required autofocus />
      <span class="hint">New scenes start disabled: add nodes, then enable it.</span>
    </label>
    {#if error}
      <div class="notice danger" role="alert">
        <div>
          {error.message}
          {#if error.details.length}
            <ul>
              {#each error.details as detail, i (i)}<li>{detail}</li>{/each}
            </ul>
          {/if}
        </div>
      </div>
    {/if}
    <div class="dialog-actions">
      <button class="btn" type="button" onclick={onclose}>Cancel</button>
      <button class="btn primary" type="submit" disabled={busy || !name.trim()}>Create</button>
    </div>
  </form>
</Modal>

<style>
  form {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
</style>
