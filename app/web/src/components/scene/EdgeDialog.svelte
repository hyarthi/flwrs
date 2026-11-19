<script lang="ts">
  // An edge's settings, saved to the draft: its name and its mapping, in a
  // visual editor (the source's fields on the left, the rules in the
  // middle, the target's fields on the right; click a field to use its
  // path) or as JSON. Below, a test: a sample payload in, what the edge
  // sends out (or that it filters it, or the error), re-run as you edit.
  // The queue events wait in is the target node's: shown here, edited there.
  import { untrack } from 'svelte';
  import Plus from '@lucide/svelte/icons/plus';
  import ArrowUp from '@lucide/svelte/icons/arrow-up';
  import ArrowDown from '@lucide/svelte/icons/arrow-down';
  import X from '@lucide/svelte/icons/x';
  import RefreshCw from '@lucide/svelte/icons/refresh-cw';
  import Modal from '../Modal.svelte';
  import Tabs from '../Tabs.svelte';
  import CodeEditor from '../form/CodeEditor.svelte';
  import SchemaTree from '../mapping/SchemaTree.svelte';
  import { mappingScope } from '../../lib/lua/scope';
  import { ApiError } from '../../lib/api';
  import {
    FIELD_DRAG_TYPE,
    fromMapping,
    newRow,
    readDraggedField,
    samplePayload,
    toMapping,
    type EditorState,
    type RuleKind,
  } from '../../lib/mapping';
  import { edgeMessages, testMapping, updateDraftEdge, type Mapping, type MappingTest, type SceneView } from '../../lib/sceneView';

  let {
    view,
    edgeId,
    onclose,
    onopennode,
  }: { view: SceneView; edgeId: string; onclose: () => void; onopennode: (nodeId: string) => void } = $props();

  const edge = $derived((view.draft?.edges ?? view.edges).find((e) => e.id === edgeId));
  const removed = $derived(!!edge && 'change' in edge && edge.change === 'removed');
  const nodes = $derived(view.draft?.nodes ?? view.nodes);
  const nameOf = (id: string) => {
    const n = nodes.find((n) => n.id === id);
    return n ? (n.name ?? n.plugin_id) : id;
  };
  const queue = $derived(edge ? ((view.draft_queues ?? view.queues)[edge.target_node_id] ?? null) : null);
  const check = $derived(view.draft_check ?? view.check);
  const checkProblems = $derived(edge ? edgeMessages(check.problems, edge.source_node_id, edge.target_node_id) : []);

  // ---- the editor's state ----

  const initial = untrack(() => edge);
  let name = $state(initial?.name ?? '');
  let editor = $state<EditorState>(fromMapping(initial?.mapping ?? null));
  let tab = $state<'visual' | 'json'>('visual');
  let jsonText = $state('');
  let jsonError = $state<string | null>(null);
  let dirty = $state(false);

  /** The mapping being edited (from whichever tab is open). */
  const current = $derived.by((): { mapping: Mapping | null; errors: string[] } => {
    if (tab === 'json') {
      if (!jsonText.trim()) return { mapping: null, errors: [] };
      try {
        const value = JSON.parse(jsonText);
        if (!value || typeof value !== 'object' || Array.isArray(value)) return { mapping: null, errors: ['A mapping is a JSON object'] };
        return { mapping: value as Mapping, errors: [] };
      } catch (e) {
        return { mapping: null, errors: [`Not valid JSON: ${e instanceof Error ? e.message : e}`] };
      }
    }
    return toMapping(editor);
  });

  function switchTab(next: string) {
    if (next === tab) return;
    if (next === 'json') {
      jsonText = current.mapping ? JSON.stringify(current.mapping, null, 2) : '';
      jsonError = null;
      tab = 'json';
    } else {
      if (current.errors.length) {
        jsonError = current.errors[0];
        return;
      }
      editor = fromMapping(current.mapping);
      tab = 'visual';
    }
  }

  function change() {
    dirty = true;
  }

  // ---- the test: schemas, problems, each sample's outcome ----

  interface Sample {
    id: number;
    text: string;
  }
  type Result = { outcome: MappingTest['result'] } | { error: string };

  // Samples are kept per edge in this browser (a convenience).
  const storeKey = untrack(() => `flwrs.samples.${view.scene.id}.${edgeId}`);
  function loadSamples(): Sample[] {
    try {
      const saved = JSON.parse(localStorage.getItem(storeKey) ?? '[]');
      if (Array.isArray(saved) && saved.length && saved.every((t) => typeof t === 'string')) {
        return saved.map((text, i) => ({ id: i + 1, text }));
      }
    } catch {
      // Storage blocked or unreadable: start fresh.
    }
    return [{ id: 1, text: '' }];
  }
  let samples = $state<Sample[]>(loadSamples());
  let active = $state(0);
  let nextSample = untrack(() => samples.length) + 1;
  $effect(() => {
    const texts = samples.map((s) => s.text);
    try {
      localStorage.setItem(storeKey, JSON.stringify(texts));
    } catch {
      // Not kept.
    }
  });

  let test = $state<MappingTest | null>(null);
  let testError = $state<string | null>(null);
  let results = $state(new Map<number, Result>());
  let running = $state(false);

  const parse = (text: string): { payload?: Record<string, unknown>; error?: string } => {
    if (!text.trim()) return {};
    try {
      return { payload: JSON.parse(text) };
    } catch (e) {
      return { error: `Not valid JSON: ${e instanceof Error ? e.message : e}` };
    }
  };

  async function runTest() {
    if (!edge || current.errors.length) return;
    const request = (payload?: Record<string, unknown>) =>
      testMapping(view.scene.id, {
        source_node_id: edge!.source_node_id,
        target_node_id: edge!.target_node_id,
        mapping: current.mapping,
        payload,
      });
    running = true;
    try {
      const runs = await Promise.all(
        samples.map(async (s): Promise<[number, Result, MappingTest | null]> => {
          const { payload, error } = parse(s.text);
          if (error) return [s.id, { error }, null];
          if (!payload) return [s.id, { outcome: null }, null];
          const t = await request(payload);
          return [s.id, { outcome: t.result }, t];
        }),
      );
      // Schemas and problems from any run (or a run without a sample).
      test = runs.find(([, , t]) => t)?.[2] ?? (await request());
      results = new Map(runs.map(([id, r]) => [id, r]));
      testError = null;
      // The first time: a sample from the source's schema.
      if (test.source && samples.length === 1 && !samples[0].text.trim()) {
        samples[0].text = JSON.stringify(samplePayload(test.source), null, 2);
      }
    } catch (e) {
      testError = e instanceof Error ? e.message : String(e);
    } finally {
      running = false;
    }
  }

  function addSample() {
    samples = [...samples, { id: nextSample++, text: test?.source ? JSON.stringify(samplePayload(test.source), null, 2) : '' }];
    active = samples.length - 1;
  }
  function removeSample(i: number) {
    samples = samples.filter((_, j) => j !== i);
    active = Math.min(active, samples.length - 1);
  }
  const resultOf = (id: number) => results.get(id);
  const toneOf = (r: Result | undefined) =>
    !r ? '' : 'error' in r ? 'bad' : !r.outcome ? '' : r.outcome.outcome === 'output' ? 'ok' : r.outcome.outcome === 'filtered' ? 'filtered' : 'bad';

  // Re-run as the mapping or the sample changes (debounced).
  let timer: ReturnType<typeof setTimeout> | undefined;
  $effect(() => {
    void JSON.stringify(current.mapping);
    void samples.map((s) => s.text).join('\u0000');
    clearTimeout(timer);
    timer = setTimeout(() => untrack(runTest), untrack(() => test) ? 400 : 0);
    return () => clearTimeout(timer);
  });

  /** What the mapping's Lua sees: `payload` typed by the source's schema, `meta`. */
  const luaScope = $derived(mappingScope(test?.source ?? null));

  // ---- rules ----

  /** The path field last focused: a schema click fills it. */
  let focus = $state<{ row: number; field: 'source' | 'target' } | null>(null);
  const hasSource = (kind: RuleKind) => kind === 'copy' || kind === 'decode' || kind === 'encode';

  function addRule(kind: RuleKind, target = '', source = '') {
    const row = newRow(kind, target, source);
    editor.rows = [...editor.rows, row];
    focus = { row: row.id, field: kind === 'copy' && !source ? 'source' : 'target' };
    change();
  }

  // Paths a schema has (to give a source field's rule the same target).
  const collect = (fields: { key: string; fields?: unknown[] }[] | null | undefined, prefix = ''): string[] =>
    (fields ?? []).flatMap((f) => {
      const path = prefix ? `${prefix}.${f.key}` : f.key;
      return [path, ...collect(f.fields as { key: string }[] | undefined, path)];
    });
  const targetPaths = $derived(new Set(collect(test?.target)));
  const usedSources = $derived(new Set(editor.rows.map((r) => r.source).filter(Boolean)));
  const usedTargets = $derived(new Set(editor.rows.map((r) => r.target).filter(Boolean)));

  function pick(side: 'source' | 'target', path: string) {
    if (editor.useScript) return;
    const row = focus ? editor.rows.find((r) => r.id === focus!.row) : undefined;
    if (row && focus!.field === side && (side === 'target' || hasSource(row.kind))) {
      row[side] = path;
      change();
      return;
    }
    // Nothing focused for that side: a new rule from the field.
    if (side === 'source') addRule('copy', targetPaths.has(path) ? path : '', path);
    else addRule('copy', path);
  }

  /** A field dropped on the rules area: a new copy rule from it. */
  function dropOnRules(e: DragEvent) {
    const dropped = readDraggedField(e);
    if (!dropped || editor.useScript) return;
    e.preventDefault();
    if (dropped.side === 'source') addRule('copy', targetPaths.has(dropped.path) ? dropped.path : '', dropped.path);
    else addRule('copy', dropped.path);
  }

  /** A field dropped on a rule's path field of its side: that path. */
  function dropOnPath(e: DragEvent, row: (typeof editor.rows)[number], field: 'source' | 'target') {
    const dropped = readDraggedField(e);
    if (!dropped || dropped.side !== field) return;
    e.preventDefault();
    e.stopPropagation();
    row[field] = dropped.path;
    change();
  }

  const accepts = (e: DragEvent) => {
    if (e.dataTransfer?.types.includes(FIELD_DRAG_TYPE)) e.preventDefault();
  };

  function moveRule(i: number, by: number) {
    const rows = [...editor.rows];
    const [row] = rows.splice(i, 1);
    rows.splice(i + by, 0, row);
    editor.rows = rows;
    change();
  }

  const KINDS: { kind: RuleKind; label: string; hint: string }[] = [
    { kind: 'copy', label: 'Copy', hint: 'the source field’s value (converted to the target’s type); a missing source changes nothing' },
    { kind: 'set', label: 'Set', hint: 'a constant (JSON)' },
    { kind: 'lua', label: 'Lua', hint: 'function(payload, meta) returning the value; nil removes the field' },
    { kind: 'delete', label: 'Delete', hint: 'removes the field' },
    { kind: 'decode', label: 'Decode JSON', hint: 'parses a BYTES / STRING source as JSON into the target (`.` = the whole output, mode replace)' },
    { kind: 'encode', label: 'Encode JSON', hint: 'writes the source (`.` = the whole input) as JSON into a BYTES / STRING target' },
  ];

  // ---- save ----

  let busy = $state(false);
  let error = $state<ApiError | null>(null);

  async function save() {
    if (!edge || current.errors.length) return;
    busy = true;
    error = null;
    try {
      await updateDraftEdge(view.scene.id, edge.id, {
        name: name.trim() || null,
        source_node_id: edge.source_node_id,
        target_node_id: edge.target_node_id,
        mapping: current.mapping,
      });
      onclose();
    } catch (e) {
      error = e instanceof ApiError ? e : new ApiError(0, String(e));
    } finally {
      busy = false;
    }
  }

  const KIND_LABEL = { memory: 'in memory', spill: 'in memory, spilling to disk', durable: 'durable (on disk until delivered)' };
  const pretty = (v: unknown) => JSON.stringify(v, null, 2);
</script>

<Modal title={edge ? `${nameOf(edge.source_node_id)} → ${nameOf(edge.target_node_id)}` : 'Edge'} {onclose} size="xl">
  {#if !edge}
    <p class="muted">This edge isn’t in the scene any more.</p>
  {:else}
    <div class="top">
      <label class="field name">
        Name
        <input class="input" bind:value={name} maxlength="200" placeholder="Optional" oninput={change} />
      </label>
      {#if queue}
        <div class="queue">
          <span><strong>Queue:</strong> {KIND_LABEL[queue.kind]}; when full: {queue.when_full.replace('_', ' ')} <span class="muted">(the target node’s, shared by every edge into it)</span></span>
          <button class="btn ghost" type="button" onclick={() => onopennode(edge!.target_node_id)}>Open {nameOf(edge.target_node_id)}</button>
        </div>
      {/if}
    </div>

    <Tabs tabs={[{ id: 'visual', label: 'Mapping' }, { id: 'json', label: 'JSON' }]} active={tab} onselect={switchTab} />

    {#if tab === 'json'}
      <textarea class="input code json" rows="16" spellcheck="false" bind:value={jsonText} oninput={change} placeholder="Empty: events pass through unchanged."></textarea>
      {#if jsonError}<p class="error">{jsonError}</p>{/if}
    {:else}
      <div class="editor">
        <aside class="schema">
          <h3>{nameOf(edge.source_node_id)} sends</h3>
          {#if test?.source}
            <SchemaTree fields={test.source} side="source" used={usedSources} onpick={(p) => pick('source', p)} ondropfield={(path, dropped) => addRule('copy', dropped, path)} />
          {:else}
            <p class="muted small">{test ? 'Its schema isn’t known (its plugin is missing?): type paths by hand.' : 'Loading…'}</p>
          {/if}
        </aside>

        <section class="rules" role="list" ondragover={accepts} ondrop={dropOnRules}>
          <div class="row">
            <label class="field">
              Start from
              <select class="input" bind:value={editor.mode} onchange={change}>
                <option value="merge">The input (merge: rules change it)</option>
                <option value="replace">Nothing (replace: only what rules set)</option>
              </select>
            </label>
            <label class="field">
              Build the output with
              <select class="input" value={editor.useScript ? 'script' : 'rules'} onchange={(e) => { editor.useScript = e.currentTarget.value === 'script'; change(); }}>
                <option value="rules">Rules</option>
                <option value="script">A Lua script</option>
              </select>
            </label>
          </div>

          <div class="block">
            <label class="check">
              <input type="checkbox" checked={editor.filter !== null} onchange={(e) => { editor.filter = e.currentTarget.checked ? 'return true' : null; change(); }} />
              Filter: only events for which this returns true go on
            </label>
            {#if editor.filter !== null}
              <CodeEditor compact scope={luaScope} params={['payload', 'meta']} language="lua" value={editor.filter} onchange={(v) => { editor.filter = v; change(); }} />
              <span class="hint mono">function(payload, meta) → boolean</span>
            {/if}
          </div>

          {#if editor.useScript}
            <div class="block">
              <CodeEditor scope={luaScope} params={['payload', 'meta']} language="lua" value={editor.script} onchange={(v) => { editor.script = v; change(); }} />
              <span class="hint mono">function(payload, meta) → the whole output table (nil: dropped)</span>
            </div>
          {:else}
            {#each editor.rows as row, i (row.id)}
              <div class="rule">
                <div class="rule-head">
                  <select class="input kind" bind:value={row.kind} onchange={() => { if (row.kind === 'set' && !row.value) row.value = '""'; change(); }} title={KINDS.find((k) => k.kind === row.kind)?.hint}>
                    {#each KINDS as k (k.kind)}<option value={k.kind}>{k.label}</option>{/each}
                  </select>
                  {#if hasSource(row.kind)}
                    <input class="input mono path" placeholder="source.path" bind:value={row.source} class:focused={focus?.row === row.id && focus.field === 'source'} onfocus={() => (focus = { row: row.id, field: 'source' })} oninput={change} ondragover={accepts} ondrop={(e) => dropOnPath(e, row, 'source')} />
                    <span class="arrow">→</span>
                  {/if}
                  <input class="input mono path" placeholder="target.path" bind:value={row.target} class:focused={focus?.row === row.id && focus.field === 'target'} onfocus={() => (focus = { row: row.id, field: 'target' })} oninput={change} ondragover={accepts} ondrop={(e) => dropOnPath(e, row, 'target')} />
                  {#if row.kind === 'set'}
                    <span class="arrow">=</span>
                    <input class="input mono value" placeholder="JSON value" bind:value={row.value} oninput={change} />
                  {/if}
                  <span class="rule-actions">
                    <button class="btn ghost icon" type="button" disabled={i === 0} aria-label="Move up" onclick={() => moveRule(i, -1)}><ArrowUp size={14} strokeWidth={1.75} /></button>
                    <button class="btn ghost icon" type="button" disabled={i === editor.rows.length - 1} aria-label="Move down" onclick={() => moveRule(i, 1)}><ArrowDown size={14} strokeWidth={1.75} /></button>
                    <button class="btn ghost icon" type="button" aria-label="Remove the rule" onclick={() => { editor.rows = editor.rows.filter((r) => r.id !== row.id); change(); }}><X size={14} strokeWidth={1.75} /></button>
                  </span>
                </div>
                {#if row.kind === 'lua'}
                  <CodeEditor compact scope={luaScope} params={['payload', 'meta']} language="lua" value={row.lua} onchange={(v) => { row.lua = v; change(); }} />
                {/if}
              </div>
            {:else}
              <p class="muted small">No rules: {editor.mode === 'merge' ? 'events go on as they are' : 'the output is empty'}. Click a field on either side to start one, or add one:</p>
            {/each}
            <div class="add">
              {#each KINDS as k (k.kind)}
                <button class="btn ghost" type="button" title={k.hint} onclick={() => addRule(k.kind)}><Plus size={14} strokeWidth={1.75} />{k.label}</button>
              {/each}
            </div>
          {/if}
        </section>

        <aside class="schema">
          <h3>{nameOf(edge.target_node_id)} takes</h3>
          {#if test?.target}
            <SchemaTree fields={test.target} side="target" used={usedTargets} onpick={(p) => pick('target', p)} ondropfield={(path, dropped) => addRule('copy', path, dropped)} />
          {:else}
            <p class="muted small">{test ? 'Its schema isn’t known: type paths by hand.' : 'Loading…'}</p>
          {/if}
        </aside>
      </div>
    {/if}

    {#each current.errors as e, i (i)}<p class="error">{e}</p>{/each}
    {#if test?.problems.length}
      <div class="notice danger"><ul>{#each test.problems as p, i (i)}<li>{p}</li>{/each}</ul></div>
    {:else if !dirty && checkProblems.length}
      <div class="notice danger"><ul>{#each checkProblems as p, i (i)}<li>{p}</li>{/each}</ul></div>
    {/if}

    <div class="samples" role="tablist">
      {#each samples as sample, i (sample.id)}
        {@const tone = toneOf(resultOf(sample.id))}
        <span class="sample-tab" class:active={i === active}>
          <button type="button" role="tab" aria-selected={i === active} onclick={() => (active = i)}>
            <span class="dot {tone}"></span>Sample {i + 1}
          </button>
          {#if samples.length > 1}
            <button type="button" class="close" aria-label={`Remove sample ${i + 1}`} onclick={() => removeSample(i)}><X size={12} strokeWidth={2} /></button>
          {/if}
        </span>
      {/each}
      <button class="btn ghost add-sample" type="button" onclick={addSample} title="Another sample"><Plus size={14} strokeWidth={1.75} />Sample</button>
      {#if running}<span class="muted small">Running…</span>{/if}
    </div>

    {#if samples[active]}
      {@const sample = samples[active]}
      {@const result = resultOf(sample.id)}
      <section class="test">
        <div class="test-col">
          <div class="test-head">
            <h3>From {nameOf(edge.source_node_id)}</h3>
            {#if test?.source}
              <button class="btn ghost" type="button" title="A fresh sample from the schema" onclick={() => (sample.text = JSON.stringify(samplePayload(test!.source!), null, 2))}>
                <RefreshCw size={14} strokeWidth={1.75} />Reset
              </button>
            {/if}
          </div>
          <textarea class="input code" rows="9" spellcheck="false" bind:value={sample.text} placeholder="A payload as JSON (bytes as base64)"></textarea>
        </div>
        <div class="test-col">
          <div class="test-head">
            <h3>Sent to {nameOf(edge.target_node_id)}</h3>
          </div>
          {#if testError}
            <p class="error">{testError}</p>
          {:else if result && 'error' in result}
            <pre class="out error">{result.error}</pre>
          {:else if !result?.outcome}
            <pre class="out muted">{test?.problems.length ? 'Fix the problems to try it.' : 'Give a sample to try it.'}</pre>
          {:else if result.outcome.outcome === 'output'}
            <pre class="out">{pretty(result.outcome.payload)}</pre>
          {:else if result.outcome.outcome === 'filtered'}
            <pre class="out muted">Filtered out: nothing is sent.</pre>
          {:else}
            <pre class="out error">{result.outcome.message}</pre>
          {/if}
        </div>
      </section>
    {/if}

    {#if error}
      <div class="notice danger" role="alert">
        <div>
          {error.message}
          {#if error.details.length}<ul>{#each error.details as d, i (i)}<li>{d}</li>{/each}</ul>{/if}
        </div>
      </div>
    {/if}

    <div class="dialog-actions">
      <span class="muted small grow">Saved to the draft: live changes when you submit it.</span>
      <button class="btn" type="button" onclick={onclose}>Cancel</button>
      <button class="btn primary" type="button" disabled={busy || current.errors.length > 0 || removed} onclick={save}>Save to draft</button>
    </div>
  {/if}
</Modal>

<style>
  .top {
    display: flex;
    flex-wrap: wrap;
    align-items: flex-end;
    gap: var(--space-3);
  }

  .name {
    flex: 1;
    min-width: 220px;
  }

  .queue {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-1) var(--space-1) var(--space-1) var(--space-3);
    border-radius: var(--radius);
    background: var(--surface-2);
    font-size: var(--text-sm);
  }

  .editor {
    display: grid;
    grid-template-columns: minmax(180px, 1fr) minmax(0, 2.4fr) minmax(180px, 1fr);
    gap: var(--space-3);
    min-height: 260px;
  }

  .schema {
    max-height: 420px;
    padding: var(--space-2);
    overflow: auto;
    border: 1px solid var(--border);
    border-radius: var(--radius);
  }

  h3 {
    margin: 0 0 var(--space-2);
    color: var(--text-muted);
    font-size: var(--text-xs);
    font-weight: 600;
    letter-spacing: 0.04em;
    text-transform: uppercase;
  }

  .rules {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    max-height: 420px;
    overflow: auto;
  }

  .row {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: var(--space-2);
  }

  .block {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .rule {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    padding: var(--space-2);
    border: 1px solid var(--border);
    border-radius: var(--radius);
  }

  .rule-head {
    display: flex;
    align-items: center;
    gap: var(--space-1);
  }

  .kind {
    width: 118px;
    flex: none;
  }

  .path {
    flex: 1;
    min-width: 80px;
  }

  .value {
    flex: 1;
    min-width: 60px;
  }

  .path.focused {
    border-color: var(--accent);
  }

  .arrow {
    color: var(--text-faint);
  }

  .rule-actions {
    display: flex;
    flex: none;
  }

  .add {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-1);
  }

  .mono {
    font-family: var(--font-mono);
    font-size: var(--text-xs);
  }

  .code {
    height: auto;
    padding: var(--space-2);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    resize: vertical;
  }

  .json {
    width: 100%;
  }

  .samples {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-1);
  }

  .sample-tab {
    display: inline-flex;
    align-items: center;
    border: 1px solid var(--border);
    border-radius: 999px;
  }

  .sample-tab.active {
    border-color: var(--accent);
    background: var(--accent-soft);
  }

  .sample-tab button {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    padding: 2px var(--space-2);
    border: 0;
    background: none;
    color: var(--text);
    font: inherit;
    font-size: var(--text-sm);
    cursor: pointer;
  }

  .sample-tab .close {
    padding: 2px 6px 2px 0;
    color: var(--text-muted);
  }

  .sample-tab .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--border-strong);
  }

  .sample-tab .dot.ok {
    background: var(--ok);
  }

  .sample-tab .dot.filtered {
    background: var(--warn);
  }

  .sample-tab .dot.bad {
    background: var(--danger);
  }

  .test {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: var(--space-3);
  }

  .test-col {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    min-width: 0;
  }

  .test-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    min-height: 32px;
  }

  .test-head h3 {
    margin: 0;
  }

  .out {
    flex: 1;
    min-height: 160px;
    max-height: 260px;
    margin: 0;
    padding: var(--space-2);
    overflow: auto;
    border-radius: var(--radius);
    background: var(--surface-2);
    font-size: var(--text-xs);
    white-space: pre-wrap;
  }

  .hint {
    color: var(--text-muted);
    font-size: var(--text-xs);
  }

  .error {
    margin: 0;
    color: var(--danger);
    font-size: var(--text-xs);
  }

  .small {
    font-size: var(--text-sm);
  }

  .grow {
    flex: 1;
  }

  ul {
    margin: 0;
    padding-left: var(--space-4);
  }

  @media (max-width: 900px) {
    .editor,
    .test {
      grid-template-columns: 1fr;
    }
  }
</style>
