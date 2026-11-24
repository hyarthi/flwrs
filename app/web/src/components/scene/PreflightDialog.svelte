<script lang="ts">
  // "Check this machine": runs the nodes' pre-flight checks without starting
  // anything (on the draft if there is one), and lists what they found.
  import Modal from '../Modal.svelte';
  import { runPreflight, type PreflightRun } from '../../lib/sceneView';

  let {
    sceneId,
    draft,
    nameOf,
    onclose,
  }: {
    sceneId: string;
    draft: boolean;
    nameOf: (nodeId: string) => string;
    onclose: () => void;
  } = $props();

  let run = $state<PreflightRun | null>(null);
  let error = $state<string | null>(null);
  let busy = $state(false);

  async function check() {
    busy = true;
    error = null;
    try {
      run = await runPreflight(sceneId, draft);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  $effect(() => {
    void check();
  });

  const checked = $derived(run?.nodes.filter((n) => n.checked) ?? []);
  const skipped = $derived(run?.nodes.filter((n) => !n.checked) ?? []);
</script>

<Modal title="Check this machine" {onclose} size="md">
  <p class="muted small">Can the {draft ? 'draft’s' : 'scene’s'} nodes run here right now? Nothing is started.</p>
  {#if error}
    <div class="notice danger" role="alert">{error}</div>
  {:else if busy && !run}
    <p class="muted" role="status">Checking…</p>
  {:else if run}
    {#if !checked.length}
      <div class="notice" role="status">No node here has a pre-flight check.</div>
    {:else if run.ok}
      <div class="notice ok" role="status">Every checked node can run on this machine.</div>
    {:else if run.permanent}
      <div class="notice danger" role="status">Some nodes can’t run here until something changes: enabling is refused.</div>
    {:else}
      <div class="notice warn" role="status">Some nodes aren’t ready yet: the scene would wait for them, and retry.</div>
    {/if}
    <ul class="nodes">
      {#each checked as node (node.node_id)}
        <li>
          <strong>{nameOf(node.node_id)}</strong>
          {#if !node.problems.length}<span class="badge ok">Ready</span>{/if}
          {#if node.problems.length || node.warnings.length}
            <ul>
              {#each node.problems as p, i (i)}
                <li>
                  <span class="badge {p.retryable ? 'warn' : 'danger'}">{p.retryable ? 'Retryable' : 'Permanent'}</span>
                  <span class="message">{p.message}</span>
                </li>
              {/each}
              {#each node.warnings as w, i (i)}
                <li><span class="badge">Warning</span> <span class="message">{w}</span></li>
              {/each}
            </ul>
          {/if}
        </li>
      {/each}
    </ul>
    {#if skipped.length}
      <p class="muted small">
        Not checked:
        {skipped.map((n) => `${nameOf(n.node_id)} (${n.skipped})`).join(', ')}
      </p>
    {/if}
  {/if}
  <div class="dialog-actions">
    <button class="btn" type="button" onclick={check} disabled={busy}>{busy ? 'Checking…' : 'Check again'}</button>
    <button class="btn primary" type="button" onclick={onclose}>Close</button>
  </div>
</Modal>

<style>
  .small {
    font-size: var(--text-sm);
  }

  .nodes,
  .nodes ul {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    margin: 0;
    padding: 0;
    list-style: none;
  }

  .nodes {
    margin-block: var(--space-3);
  }

  .nodes ul {
    margin-top: var(--space-1);
  }

  .message {
    overflow-wrap: anywhere;
    white-space: pre-wrap;
  }
</style>
