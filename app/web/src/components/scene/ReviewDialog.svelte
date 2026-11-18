<script lang="ts">
  // The draft's diff as a list: what submitting would change, the
  // conflicts a rebase left (keep the draft's version or take live's), and
  // the draft's check. A node's row focuses it on the canvas.
  import Modal from '../Modal.svelte';
  import { resolveConflict, type Change, type Check, type Conflict, type SceneView } from '../../lib/sceneView';

  let {
    view,
    check,
    onclose,
    onfocus,
    onerror,
  }: {
    view: SceneView;
    check: Check;
    onclose: () => void;
    onfocus: (nodeId: string) => void;
    onerror: (title: string, e: unknown) => void;
  } = $props();

  const draft = $derived(view.draft!);
  const names = $derived(new Map(draft.nodes.map((n) => [n.id, n.name ?? n.plugin_id])));
  const nodes = $derived(draft.nodes.filter((n) => n.change !== 'unchanged' || n.conflict));
  const edges = $derived(draft.edges.filter((e) => e.change !== 'unchanged' || e.conflict));
  const settingsChanged = $derived(draft.settings.change !== 'unchanged' || draft.settings.conflict);

  const CHANGE: Record<Change, string> = { unchanged: 'unchanged', added: 'new', changed: 'changed', removed: 'removed' };
  const CONFLICT: Record<Conflict, string> = {
    changed_both: 'changed here and live',
    removed_live: 'removed live, changed here',
    changed_live: 'changed live, removed here',
  };
  const tone = (c: Change) => (c === 'added' ? 'accent' : c === 'removed' ? 'danger' : c === 'changed' ? 'warn' : '');

  let busy = $state(false);
  async function resolve(target: 'settings' | 'node' | 'edge', id: string | null, keep: 'draft' | 'live') {
    busy = true;
    try {
      await resolveConflict(view.scene.id, target, id, keep);
    } catch (e) {
      onerror("Couldn't resolve the conflict", e);
    } finally {
      busy = false;
    }
  }
</script>

{#snippet resolver(target: 'settings' | 'node' | 'edge', id: string | null, conflict: Conflict)}
  <span class="badge warn">{CONFLICT[conflict]}</span>
  <span class="resolve">
    <button class="btn" type="button" disabled={busy} onclick={() => resolve(target, id, 'draft')}>Keep the draft’s</button>
    <button class="btn" type="button" disabled={busy} onclick={() => resolve(target, id, 'live')}>Take live’s</button>
  </span>
{/snippet}

<Modal title="Review the draft" {onclose}>
  {#if !nodes.length && !edges.length && !settingsChanged}
    <p class="muted">The draft doesn’t change anything yet.</p>
  {/if}

  {#if settingsChanged}
    <section>
      <h3>Scene settings</h3>
      <div class="row">
        <span class="badge warn">changed</span>
        <span class="what">name “{draft.settings.name}”, hop limit {draft.settings.max_hops ?? 'default'}</span>
        {#if draft.settings.conflict}{@render resolver('settings', null, draft.settings.conflict)}{/if}
      </div>
    </section>
  {/if}

  {#if nodes.length}
    <section>
      <h3>Nodes</h3>
      {#each nodes as node (node.id)}
        <div class="row">
          <span class="badge {tone(node.change)}">{CHANGE[node.change]}</span>
          <button class="link what" type="button" onclick={() => onfocus(node.id)} title="Show on the canvas">
            {node.name ?? node.plugin_id}
            <span class="mono muted">{node.id}</span>
          </button>
          {#if node.parts.length}<span class="muted parts">{node.parts.join(', ')}</span>{/if}
          {#if node.conflict}{@render resolver('node', node.id, node.conflict)}{/if}
        </div>
      {/each}
    </section>
  {/if}

  {#if edges.length}
    <section>
      <h3>Edges</h3>
      {#each edges as edge (edge.id)}
        <div class="row">
          <span class="badge {tone(edge.change)}">{CHANGE[edge.change]}</span>
          <span class="what">{names.get(edge.source_node_id) ?? edge.source_node_id} → {names.get(edge.target_node_id) ?? edge.target_node_id}</span>
          {#if edge.parts.length}<span class="muted parts">{edge.parts.join(', ')}</span>{/if}
          {#if edge.conflict}{@render resolver('edge', edge.id, edge.conflict)}{/if}
        </div>
      {/each}
    </section>
  {/if}

  {#if check.problems.length || check.warnings.length}
    <section>
      <h3>Check</h3>
      {#if check.problems.length}
        <div class="notice danger">
          <div>
            {view.scene.enabled ? 'The scene is enabled: these block submitting it (unless you disable it).' : 'These would keep the scene from being enabled.'}
            <ul>{#each check.problems as p, i (i)}<li>{p}</li>{/each}</ul>
          </div>
        </div>
      {/if}
      {#if check.warnings.length}
        <div class="notice warn">
          <ul>{#each check.warnings as w, i (i)}<li>{w}</li>{/each}</ul>
        </div>
      {/if}
    </section>
  {/if}
</Modal>

<style>
  section {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  h3 {
    margin: 0 0 var(--space-1);
    color: var(--text-muted);
    font-size: var(--text-xs);
    font-weight: 600;
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }

  .row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-1) 0;
    border-bottom: 1px solid var(--border);
    font-size: var(--text-sm);
  }

  .what {
    display: inline-flex;
    gap: var(--space-2);
    align-items: baseline;
  }

  .link {
    padding: 0;
    border: 0;
    background: none;
    color: var(--accent);
    font: inherit;
    cursor: pointer;
  }

  .parts {
    font-size: var(--text-xs);
  }

  .resolve {
    display: inline-flex;
    gap: var(--space-1);
    margin-left: auto;
  }

  .resolve .btn {
    height: 26px;
    font-size: var(--text-xs);
  }

  ul {
    margin: var(--space-1) 0 0;
    padding-left: var(--space-4);
  }
</style>
