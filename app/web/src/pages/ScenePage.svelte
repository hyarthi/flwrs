<script lang="ts">
  // A scene: its header (name, state, enable switch) and the Layout / Stats
  // tabs, fed by the scene's stream (`…/by-id/{id}/stream`).
  import { SvelteFlowProvider } from '@xyflow/svelte';
  import ArrowLeft from '@lucide/svelte/icons/arrow-left';
  import MonitorCheck from '@lucide/svelte/icons/monitor-check';
  import Page from '../components/Page.svelte';
  import SceneStats from '../components/scene/SceneStats.svelte';
  import Tabs from '../components/Tabs.svelte';
  import NodeModal from '../components/node/NodeModal.svelte';
  import Switch from '../components/Switch.svelte';
  import LayoutView from '../components/scene/LayoutView.svelte';
  import PreflightDialog from '../components/scene/PreflightDialog.svelte';
  import { ApiError } from '../lib/api';
  import { navigate } from '../lib/router.svelte';
  import { openStream } from '../lib/stream.svelte';
  import { setEnabled } from '../lib/scenes';
  import type { SceneStatusEvent, SceneView } from '../lib/sceneView';

  let { id, tab, nodeId }: { id: string; tab: 'layout' | 'stats'; nodeId?: string } = $props();

  const base = $derived(`/scenes/by-id/${encodeURIComponent(id)}`);

  // ---- live data ----

  let view = $state<SceneView | null>(null);
  let status = $state<SceneStatusEvent | null>(null);
  let gone = $state(false);
  let streamError = $state<string | null>(null);

  $effect(() => {
    const stream = openStream(`/scenes/by-id/${encodeURIComponent(id)}/stream`, {
      scene: (event: SceneView) => {
        view = event;
        streamError = null;
      },
      status: (event: SceneStatusEvent) => (status = event),
      deleted: () => {
        gone = true;
        stream.close();
      },
      stream_error: (event: { error: string }) => (streamError = event.error),
    });
    return () => stream.close();
  });

  $effect(() => {
    if (view) document.title = `${view.scene.name} · flwrs`;
  });

  // ---- header ----

  const shownState = $derived.by(() => {
    if (!view) return null;
    if (status?.state === 'FAILED') return { label: 'Failed', tone: 'danger', detail: status.errors.join('\n') };
    if (status?.state === 'WAITING') {
      const retry = status.preflight_retry;
      const attempt = retry ? ` (check ${retry.attempt} of ${retry.max_attempts})` : '';
      return {
        label: 'Waiting',
        tone: 'warn',
        detail: status.errors.length ? `${status.errors.join('\n')}${attempt}` : `Pre-flight check running${attempt}`,
      };
    }
    if (!view.scene.enabled) return status?.state === 'RUNNING' ? { label: 'Stopping', tone: 'warn' } : { label: 'Disabled', tone: '' };
    if (status?.state === 'RUNNING') {
      const running = status.nodes.filter((n) => n.status.state === 'RUNNING').length;
      return running < status.nodes.length
        ? { label: `Running ${running}/${status.nodes.length}`, tone: 'warn', detail: 'Not every node is connected' }
        : status.failed_update
          ? { label: 'Running (update failed)', tone: 'warn', detail: status.failed_update.errors.join('\n') }
          : { label: 'Running', tone: 'ok' };
    }
    return { label: 'Starting', tone: 'warn' };
  });

  let checking = $state(false);
  function nodeName(nodeId: string): string {
    const nodes = view?.draft?.nodes ?? view?.nodes ?? [];
    return nodes.find((n) => n.id === nodeId)?.name ?? nodeId;
  }

  let toggling = $state(false);
  let toggleError = $state<ApiError | null>(null);
  async function toggle(enabled: boolean) {
    if (!view) return;
    toggling = true;
    toggleError = null;
    try {
      await setEnabled(view.scene.id, enabled);
    } catch (e) {
      toggleError = e instanceof ApiError ? e : new ApiError(0, String(e));
    } finally {
      toggling = false;
    }
  }
</script>

<Page full>
  {#if gone}
    <div class="card gone">
      <h2>This scene was deleted</h2>
      <p class="muted">Someone deleted it while you had it open.</p>
      <a class="btn" href="/scenes"><ArrowLeft size={16} strokeWidth={1.75} />All scenes</a>
    </div>
  {:else if !view}
    <div class="card gone muted">{streamError ?? 'Loading the scene…'}</div>
  {:else}
    <header>
      <div class="title">
        <a class="back btn ghost icon" href="/scenes" aria-label="All scenes" title="All scenes"><ArrowLeft size={16} strokeWidth={1.75} /></a>
        <h1>{view.scene.name}</h1>
        {#if shownState}
          <span class="badge {shownState.tone}" title={shownState.detail}>{shownState.label}</span>
        {/if}
      </div>
      <div class="enable">
        <button class="btn" type="button" onclick={() => (checking = true)}>
          <MonitorCheck size={16} strokeWidth={1.75} />Check this machine
        </button>
        {#if view.check.problems.length && !view.scene.enabled}
          <span class="muted small" title={view.check.problems.join('\n')}>Live has problems: fix them to enable it</span>
        {/if}
        <Switch
          checked={view.scene.enabled}
          label={view.scene.enabled ? 'Disable the scene' : 'Enable the scene'}
          disabled={!view.scene.enabled && view.check.problems.length > 0}
          busy={toggling}
          onchange={toggle}
        />
        <span class="small">{view.scene.enabled ? 'Enabled' : 'Disabled'}</span>
      </div>
    </header>
    {#if toggleError}
      <div class="notice danger" role="alert">
        <div>
          {toggleError.message}
          {#if toggleError.details.length}
            <ul>{#each toggleError.details as d, i (i)}<li>{d}</li>{/each}</ul>
          {/if}
        </div>
      </div>
    {/if}
    {#if streamError}
      <div class="notice danger" role="alert">{streamError}</div>
    {/if}
    {#if status?.failed_update}
      <div class="notice warn" role="status">
        <div>
          The latest update failed its pre-flight check, so the scene keeps running its previous version.
          <ul>{#each status.failed_update.errors as e, i (i)}<li>{e}</li>{/each}</ul>
        </div>
      </div>
    {/if}

    <Tabs
      tabs={[
        { id: 'layout', label: 'Layout', href: base },
        { id: 'stats', label: 'Stats', href: `${base}/stats` },
      ]}
      active={tab}
    />
    {#if tab === 'layout'}
      <SvelteFlowProvider>
        <LayoutView {view} {status} />
      </SvelteFlowProvider>
    {:else}
      <SceneStats {view} {status} />
    {/if}
  {/if}
</Page>

{#if checking && view}
  <PreflightDialog sceneId={view.scene.id} draft={!!view.draft} nameOf={nodeName} onclose={() => (checking = false)} />
{/if}

{#if nodeId && view}
  <NodeModal {view} {status} {nodeId} onclose={() => navigate(base)} />
{/if}

<style>
  header {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
  }

  .title,
  .enable {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-width: 0;
  }

  h1 {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .small {
    font-size: var(--text-sm);
  }

  .gone {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: var(--space-2);
    padding: var(--space-5);
  }

  ul {
    margin: var(--space-1) 0 0;
    padding-left: var(--space-4);
  }
</style>
