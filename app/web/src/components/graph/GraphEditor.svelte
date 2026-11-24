<script lang="ts">
  // A launch arg that holds a graph (`x-flwrs-graph`), edited on a canvas.
  // The plugin's catalog says what nodes and ports there are; nothing here
  // knows any plugin. The graph is saved to the scene's draft like any other
  // setting; where its nodes sit is layout, saved at once beside the node.
  import { SvelteFlowProvider } from '@xyflow/svelte';
  import { untrack } from 'svelte';
  import GraphCanvas from './GraphCanvas.svelte';
  import { getDefinitions, getGraphCatalog, type GraphNode, type Plugin, type SceneView, type Upload } from '../../lib/sceneView';
  import type { GraphCatalog } from '../../lib/graphModel';
  import type { Schema } from '../../lib/schemaForm';
  import { codeFieldScope } from '../../lib/lua/scope';
  import type { Scope } from '../../lib/lua/types';

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

  // What the graph's code settings see (Lua completion): the definitions
  // files their `x-flwrs-code` names, wherever they sit in the schema.
  let definitions = $state(new Map<string, string>());
  $effect(() => {
    const { id, version } = plugin;
    const paths = new Set<string>();
    const walk = (s: unknown) => {
      if (!s || typeof s !== 'object') return;
      const code = (s as Schema)['x-flwrs-code'];
      if (code?.definitions) paths.add(code.definitions);
      Object.values(s).forEach(walk);
    };
    walk(schema);
    for (const path of paths) {
      getDefinitions(id, version, path).then((text) => {
        if (text !== null) definitions = new Map(untrack(() => definitions)).set(path, text);
      });
    }
  });
  function codeScope(s: Schema): Scope | null {
    const code = s['x-flwrs-code'];
    if (!code || code.language !== 'lua') return null;
    return codeFieldScope(code, code.definitions ? definitions.get(code.definitions) : undefined, null);
  }
</script>

{#if error}
  <div class="notice danger" role="alert">{error}</div>
{:else if !catalog}
  <p class="muted">Loading…</p>
{:else}
  <SvelteFlowProvider>
    <GraphCanvas {view} {node} {schema} {field} {catalog} configuredFields={plugin.configured_fields} {uploads} {messages} {active} {running} {liveArgs} {codeScope} {onopensettings} {onsaved} />
  </SvelteFlowProvider>
{/if}
