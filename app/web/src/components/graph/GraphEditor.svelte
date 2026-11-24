<script lang="ts">
  // A launch arg that holds a graph (`x-flwrs-graph`), edited on a canvas.
  // The plugin's catalog says what nodes and ports there are; nothing here
  // knows any plugin. The graph is saved to the scene's draft like any other
  // setting; where its nodes sit is layout, saved at once beside the node.
  import { SvelteFlowProvider } from '@xyflow/svelte';
  import GraphCanvas from './GraphCanvas.svelte';
  import { getGraphCatalog, type GraphNode, type Plugin, type SceneView, type Upload } from '../../lib/sceneView';
  import type { GraphCatalog } from '../../lib/graphModel';
  import type { Schema } from '../../lib/schemaForm';

  let {
    view,
    node,
    plugin,
    schema,
    field,
    catalogPath,
    uploads,
    messages,
    active,
    running,
    liveArgs,
    onopensettings,
    onsaved,
  }: {
    view: SceneView;
    node: GraphNode;
    plugin: Plugin;
    schema: Schema;
    field: string;
    catalogPath: string;
    uploads: Upload[];
    /** The check's messages about this node. */
    messages: string[];
    /** The tab is showing: keys are the editor's. */
    active: boolean;
    running: boolean;
    liveArgs: Record<string, unknown> | null;
    onopensettings: () => void;
    onsaved: (saved: GraphNode) => void;
  } = $props();

  let catalog = $state<GraphCatalog | null>(null);
  let error = $state<string | null>(null);
  $effect(() => {
    const { id, version } = plugin;
    getGraphCatalog(id, version, catalogPath).then((text) => {
      if (text === null) {
        error = 'The plugin’s graph catalog couldn’t be loaded.';
        return;
      }
      try {
        catalog = JSON.parse(text) as GraphCatalog;
      } catch (e) {
        error = `The plugin’s graph catalog isn’t valid: ${e instanceof Error ? e.message : e}`;
      }
    });
  });
</script>

{#if error}
  <div class="notice danger" role="alert">{error}</div>
{:else if !catalog}
  <p class="muted">Loading…</p>
{:else}
  <SvelteFlowProvider>
    <GraphCanvas {view} {node} {schema} {field} {catalog} configuredFields={plugin.configured_fields} {uploads} {messages} {active} {running} {liveArgs} {onopensettings} {onsaved} />
  </SvelteFlowProvider>
{/if}
