<script lang="ts">
  // The graph editor's live toggles: Stats and Video open and close the
  // node's streams (watching costs the device work, so both start off).
  // Shown while the node runs; when it announces nothing, a hint says how to
  // turn its live view on, if the plugin names that setting.
  import Activity from '@lucide/svelte/icons/activity';
  import Video from '@lucide/svelte/icons/video';
  import type { LiveView, FeedState } from '../../lib/liveView.svelte';

  let {
    live,
    running,
    settingTitle,
    settingOn,
    onopensettings,
  }: {
    live: LiveView;
    running: boolean;
    /** The plugin's setting that turns live channels on, if it names one. */
    settingTitle: string | null;
    /** Whether the running node has it on. */
    settingOn: boolean;
    onopensettings: () => void;
  } = $props();

  const label = (s: FeedState, wanted: boolean, has: boolean) => {
    if (!wanted) return '';
    if (!has) return 'waiting for the node';
    if (live.paused) return 'paused';
    return { off: '', connecting: 'connecting', waiting: 'waiting', live: '' }[s];
  };
  const any = $derived(live.channels.some((c) => c.display));
</script>

{#if running && live.listed}
  {#if any || live.statsWanted || live.videoWanted}
    <div class="live" role="group" aria-label="Live view">
      <span class="muted small">Live</span>
      {#if live.statsChannel || live.statsWanted}
        <button class="btn toggle" type="button" aria-pressed={live.statsWanted} onclick={() => live.toggleStats()} title="Per-node numbers while the node runs (the node works on them only while shown)">
          <Activity size={14} strokeWidth={2} /> Stats
          {#if label(live.statsFeed.state, live.statsWanted, !!live.statsChannel)}<span class="state">{label(live.statsFeed.state, live.statsWanted, !!live.statsChannel)}</span>{/if}
        </button>
      {/if}
      {#if live.videoChannel || live.videoWanted}
        <button class="btn toggle" type="button" aria-pressed={live.videoWanted} onclick={() => live.toggleVideo()} title="The node's preview (the node draws it only while shown)">
          <Video size={14} strokeWidth={2} /> Video
          {#if label(live.videoFeed.state, live.videoWanted, !!live.videoChannel)}<span class="state">{label(live.videoFeed.state, live.videoWanted, !!live.videoChannel)}</span>{/if}
        </button>
      {/if}
      {#if live.paused}
        <span class="muted small" role="status">Open in another tab.</span>
        <button class="btn ghost" type="button" onclick={() => live.resume()}>Watch here</button>
      {/if}
    </div>
  {:else if settingTitle && !settingOn}
    <span class="hint muted small" data-testid="live-off">
      Live view is off for this node. Turn on <strong>{settingTitle}</strong> in
      <button class="link" type="button" onclick={onopensettings}>Settings</button>, then submit the draft.
    </span>
  {:else if settingTitle}
    <span class="hint muted small">Waiting for the node to report…</span>
  {/if}
{/if}

<style>
  .live {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
  }

  .toggle {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
  }

  .toggle[aria-pressed='true'] {
    border-color: var(--accent);
    background: var(--accent-soft);
    color: var(--text);
  }

  .state {
    color: var(--text-muted);
    font-size: var(--text-xs);
  }

  .small {
    font-size: var(--text-xs);
  }

  .link {
    padding: 0;
    border: 0;
    background: none;
    color: var(--accent);
    font: inherit;
    text-decoration: underline;
    cursor: pointer;
  }
</style>
