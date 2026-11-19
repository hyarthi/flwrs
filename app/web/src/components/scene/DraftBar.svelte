<script lang="ts">
  // Above the canvas: whether we're looking at live or at the draft, what
  // the draft changes, its check, and Review · Discard · Rebase · Submit.
  import GitCompare from '@lucide/svelte/icons/git-compare';
  import Settings from '@lucide/svelte/icons/settings-2';
  import Send from '@lucide/svelte/icons/send';
  import RefreshCw from '@lucide/svelte/icons/refresh-cw';
  import Trash from '@lucide/svelte/icons/trash-2';
  import Undo from '@lucide/svelte/icons/undo';
  import Redo from '@lucide/svelte/icons/redo';
  import ConfirmDialog from '../ConfirmDialog.svelte';
  import ReviewDialog from './ReviewDialog.svelte';
  import SettingsDialog from './SettingsDialog.svelte';
  import { discardDraft, rebaseDraft, redoDraft, submitDraft, undoDraft, type Check, type SceneView } from '../../lib/sceneView';

  let {
    view,
    check,
    onfocus,
    onerror,
  }: {
    view: SceneView;
    check: Check;
    onfocus: (nodeId: string) => void;
    onerror: (title: string, e: unknown) => void;
  } = $props();

  const draft = $derived(view.draft);
  const changes = $derived.by(() => {
    if (!draft) return 0;
    const rows = [...draft.nodes, ...draft.edges].filter((r) => r.change !== 'unchanged').length;
    return rows + (draft.settings.change !== 'unchanged' ? 1 : 0);
  });
  const conflicts = $derived(draft?.conflicts.length ?? 0);

  let reviewing = $state(false);
  let editingSettings = $state(false);
  let discarding = $state(false);
  let confirmingDisable = $state(false);
  let busy = $state(false);

  async function run(title: string, action: () => Promise<unknown>) {
    busy = true;
    try {
      await action();
    } catch (e) {
      onerror(title, e);
    } finally {
      busy = false;
    }
  }

  function submit() {
    if (!draft) return;
    if (view.scene.enabled && !check.launchable) {
      confirmingDisable = true;
      return;
    }
    run("Couldn't submit the draft", () => submitDraft(view.scene.id, draft.base_revision, false));
  }

  const plural = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

  /** Undo (`back`) or redo; the stream brings the draft back. */
  export async function step(back: boolean) {
    if (!draft || busy || !(back ? draft.undo : draft.redo)) return;
    await run(back ? "Couldn't undo" : "Couldn't redo", () => (back ? undoDraft : redoDraft)(view.scene.id));
  }
  const mod = typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform) ? '⌘' : 'Ctrl+';
</script>

<div class="bar">
  <div class="state">
    {#if draft}
      <span class="badge accent">Draft</span>
      <span class="muted">{changes ? plural(changes, 'change') : 'No changes yet'}</span>
      {#if draft.behind}
        <span class="badge warn" title="Live changed since this draft started: rebase it onto live before submitting">Behind live</span>
      {/if}
      {#if conflicts}
        <span class="badge warn">{plural(conflicts, 'conflict')}</span>
      {/if}
    {:else}
      <span class="badge">Live</span>
      <span class="muted">Edits start a draft; live changes only when you submit it.</span>
    {/if}
  </div>

  <div class="checks">
    {#if check.problems.length}
      <span class="badge danger" title={check.problems.join('\n')}>{plural(check.problems.length, 'problem')}</span>
    {/if}
    {#if check.warnings.length}
      <span class="badge warn" title={check.warnings.join('\n')}>{plural(check.warnings.length, 'warning')}</span>
    {/if}
  </div>

  <div class="actions">
    {#if draft}
      <button class="btn ghost icon" type="button" disabled={busy || !draft.undo} title={draft.undo ? `Undo: ${draft.undo} (${mod}Z)` : 'Nothing to undo'} aria-label="Undo" onclick={() => step(true)}>
        <Undo size={16} strokeWidth={1.75} />
      </button>
      <button class="btn ghost icon" type="button" disabled={busy || !draft.redo} title={draft.redo ? `Redo: ${draft.redo} (${mod}⇧Z)` : 'Nothing to redo'} aria-label="Redo" onclick={() => step(false)}>
        <Redo size={16} strokeWidth={1.75} />
      </button>
    {/if}
    <button class="btn ghost" type="button" onclick={() => (editingSettings = true)} title="Scene settings">
      <Settings size={16} strokeWidth={1.75} />Settings
    </button>
    {#if draft}
      <button class="btn ghost" type="button" onclick={() => (reviewing = true)}>
        <GitCompare size={16} strokeWidth={1.75} />Review
      </button>
      <button class="btn ghost" type="button" onclick={() => (discarding = true)} disabled={busy}>
        <Trash size={16} strokeWidth={1.75} />Discard
      </button>
      {#if draft.behind}
        <button class="btn primary" type="button" disabled={busy} onclick={() => run("Couldn't rebase the draft", () => rebaseDraft(view.scene.id))}>
          <RefreshCw size={16} strokeWidth={1.75} />Rebase
        </button>
      {:else}
        <button
          class="btn primary"
          type="button"
          disabled={busy || conflicts > 0 || !draft.has_changes}
          title={conflicts ? 'Resolve the conflicts first (Review)' : !draft.has_changes ? 'Nothing to submit' : 'Apply the draft to live'}
          onclick={submit}
        >
          <Send size={16} strokeWidth={1.75} />Submit
        </button>
      {/if}
    {/if}
  </div>
</div>

{#if reviewing && draft}
  <ReviewDialog
    {view}
    {check}
    onclose={() => (reviewing = false)}
    onfocus={(id) => {
      reviewing = false;
      onfocus(id);
    }}
    {onerror}
  />
{/if}

{#if editingSettings}
  <SettingsDialog {view} onclose={() => (editingSettings = false)} />
{/if}

{#if discarding}
  <ConfirmDialog title="Discard the draft?" confirm="Discard" danger onconfirm={async () => {
      await discardDraft(view.scene.id);
    }} onclose={() => (discarding = false)}>
    <p>Every change in the draft goes; live stays as it is. Positions you moved nodes to are kept.</p>
  </ConfirmDialog>
{/if}

{#if confirmingDisable && draft}
  <ConfirmDialog
    title="Submit and disable?"
    confirm="Submit and disable"
    danger
    onconfirm={async () => {
      await submitDraft(view.scene.id, draft.base_revision, true);
    }}
    onclose={() => (confirmingDisable = false)}
  >
    <p>“{view.scene.name}” is enabled, and the draft has problems, so it couldn't run. Submitting it disables the scene; enable it again once the problems are fixed.</p>
    <ul class="muted">
      {#each check.problems as problem, i (i)}<li>{problem}</li>{/each}
    </ul>
  </ConfirmDialog>
{/if}

<style>
  .bar {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-3);
    min-height: 48px;
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--border);
    background: var(--surface);
    font-size: var(--text-sm);
  }

  .state,
  .checks,
  .actions {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }

  .state {
    flex: 1;
    min-width: 0;
  }

  .state .muted {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  ul {
    margin: var(--space-2) 0 0;
    padding-left: var(--space-4);
    font-size: var(--text-sm);
  }
</style>
