<script lang="ts">
  // What a live view shows beside the canvas: the stats item's summary and
  // the preview. Docked above the inspector by default (it never covers the
  // graph); it can float over the canvas, moved by its title bar.
  import PictureInPicture from '@lucide/svelte/icons/picture-in-picture-2';
  import PanelRight from '@lucide/svelte/icons/panel-right';
  import { summaryOf } from '../../lib/liveFeed';
  import type { LiveView } from '../../lib/liveView.svelte';

  let { live, running, floating = $bindable(false) }: { live: LiveView; running: boolean; floating?: boolean } = $props();

  const summary = $derived(summaryOf(live.stats));
  let at = $state({ x: 24, y: 24 });
  let grab: { x: number; y: number } | null = null;
  let panel: HTMLElement | undefined = $state();

  /** Floats over the canvas's top left corner, clear of the node list beside it. */
  function toggleFloating() {
    if (!floating) {
      const canvas = panel?.closest('.work')?.querySelector<HTMLElement>('.canvas');
      at = { x: (canvas?.offsetLeft ?? 0) + 16, y: 16 };
    }
    floating = !floating;
  }

  function down(e: PointerEvent) {
    if (!floating || (e.target as HTMLElement).closest('button')) return;
    grab = { x: e.clientX - at.x, y: e.clientY - at.y };
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
  }
  function move(e: PointerEvent) {
    if (grab) at = { x: Math.max(0, e.clientX - grab.x), y: Math.max(0, e.clientY - grab.y) };
  }
</script>

{#if running && (live.statsWanted || live.videoWanted)}
  <section class="panel" class:floating style:left={floating ? `${at.x}px` : null} style:top={floating ? `${at.y}px` : null} aria-label="Live view" data-testid="live-panel" bind:this={panel}>
    <!-- svelte-ignore a11y_no_static_element_interactions: the drag handle is a pointer convenience; the dock button is the keyboard way -->
    <header class="head" onpointerdown={down} onpointermove={move} onpointerup={() => (grab = null)}>
      <span>Live</span>
      <span class="spacer"></span>
      <button class="btn ghost icon" type="button" title={floating ? 'Dock beside the canvas' : 'Float over the canvas'} aria-label={floating ? 'Dock' : 'Float'} onclick={toggleFloating}>
        {#if floating}<PanelRight size={14} />{:else}<PictureInPicture size={14} />{/if}
      </button>
    </header>
    {#if live.videoWanted}
      <div class="video">
        {#if live.frame}
          <img src={live.frame} alt="The node's live preview" data-testid="live-preview" onload={(e) => live.shown((e.currentTarget as HTMLImageElement).src)} />
        {:else}
          <span class="muted small">{live.paused ? 'Paused' : live.videoChannel ? 'Waiting for frames…' : 'No preview from the node'}</span>
        {/if}
      </div>
    {/if}
    {#if live.statsWanted}
      {#if summary.length}
        {#each summary as section (section.title)}
          <dl class="rows" aria-label={section.title}>
            {#each section.rows as row (row.label)}<dt>{row.label}</dt><dd class="mono">{row.value}</dd>{/each}
          </dl>
        {/each}
      {:else}
        <span class="muted small">{live.paused ? 'Paused' : live.statsChannel ? 'Waiting for numbers…' : 'No stats from the node'}</span>
      {/if}
    {/if}
  </section>
{/if}

<style>
  .panel {
    display: flex;
    flex: none;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--border);
    background: var(--surface);
  }

  .panel.floating {
    position: absolute;
    z-index: 10;
    width: 420px;
    min-width: 240px;
    max-width: 90%;
    border: 1px solid var(--border-strong);
    border-radius: var(--radius);
    box-shadow: var(--shadow-sm);
    max-height: calc(100% - 2 * var(--space-4));
    resize: horizontal;
    overflow: auto;
  }

  .head {
    display: flex;
    align-items: center;
    gap: var(--space-1);
    font-weight: 600;
    font-size: var(--text-sm);
  }

  .floating .head {
    cursor: move;
  }

  .spacer {
    flex: 1;
  }

  .panel > :global(*) {
    flex: none;
  }

  .video {
    display: grid;
    place-items: center;
    min-height: 120px;
    border-radius: var(--radius-sm);
    background: var(--bg);
  }

  .video img {
    display: block;
    width: 100%;
    border-radius: var(--radius-sm);
  }

  .rows {
    display: grid;
    grid-template-columns: 1fr auto;
    gap: 2px var(--space-2);
    margin: 0;
    font-size: var(--text-xs);
  }

  .rows dt {
    color: var(--text-muted);
  }

  .rows dd {
    margin: 0;
    text-align: right;
  }

  .small {
    font-size: var(--text-xs);
  }
</style>
