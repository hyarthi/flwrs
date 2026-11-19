<script lang="ts">
  // A remote plugin definition: a plugin that runs elsewhere, which this
  // server only describes so its nodes can be placed, wired, mapped and
  // checked. Steps: identity → payloads → node settings → appearance →
  // review; registered with `POST /api/plugins/remote` (or, with `id`, the
  // definition replaced: `PUT …/definition`). Nothing is packaged or deployed.
  import ArrowLeft from '@lucide/svelte/icons/arrow-left';
  import ArrowRight from '@lucide/svelte/icons/arrow-right';
  import Check from '@lucide/svelte/icons/check';
  import ImageUp from '@lucide/svelte/icons/image-up';
  import X from '@lucide/svelte/icons/x';
  import Page from '../components/Page.svelte';
  import Tabs from '../components/Tabs.svelte';
  import TokenPrompt from '../components/plugins/TokenPrompt.svelte';
  import FieldRows from '../components/plugins/FieldRows.svelte';
  import { ApiError } from '../lib/api';
  import { navigate } from '../lib/router.svelte';
  import { getDefinition, registerDefinition, replaceDefinition } from '../lib/plugins';
  import {
    ID_PATTERN,
    emptyState,
    fromDefinition,
    iconFile,
    iconPreview,
    parseSettings,
    rowsToSchema,
    schemaToRows,
    sidesOf,
    toDefinition,
    topSettings,
    type Side,
    type WizardState,
  } from '../lib/remotePlugin';

  let { id }: { id?: string } = $props();
  const editing = $derived(Boolean(id));

  let form = $state<WizardState>(emptyState());
  let loadError = $state<string | null>(null);
  let loaded = $state(false);

  $effect(() => {
    if (!id) {
      loaded = true;
      return;
    }
    getDefinition(id)
      .then((def) => {
        form = fromDefinition(def);
        loaded = true;
      })
      .catch((e) => (loadError = e instanceof Error ? e.message : String(e)));
  });

  const STEPS = [
    { id: 'identity', label: '1 · Identity' },
    { id: 'payloads', label: '2 · Payloads' },
    { id: 'settings', label: '3 · Node settings' },
    { id: 'appearance', label: '4 · Appearance' },
    { id: 'review', label: '5 · Review' },
  ];
  let step = $state('identity');
  const index = $derived(STEPS.findIndex((s) => s.id === step));

  const built = $derived(toDefinition(form));
  const settings = $derived(parseSettings(form.settings));
  const settingKeys = $derived(topSettings(settings.schema));

  // ---- payload sides: rows ↔ JSON ----

  function setMode(side: Side, mode: 'fields' | 'json') {
    if (mode === side.mode) return;
    if (mode === 'json') {
      side.json = side.fields.length ? JSON.stringify(rowsToSchema(side.fields), null, 2) : '';
    } else {
      let parsed: unknown = null;
      try {
        parsed = side.json.trim() ? JSON.parse(side.json) : { fields: [] };
      } catch {
        sideError = 'That JSON doesn’t parse: fix it first, or clear it.';
        return;
      }
      const rows = schemaToRows(parsed);
      if (!rows) {
        sideError = 'Not a SchemaDefinition ({"fields": […]}): fix it first, or clear it.';
        return;
      }
      side.fields = rows;
    }
    sideError = null;
    side.mode = mode;
  }
  let sideError = $state<string | null>(null);

  // ---- appearance ----

  let iconError = $state<string | null>(null);
  async function pickIcon(file: File | undefined) {
    if (!file) return;
    try {
      form.icon = await iconFile(file);
      iconError = null;
    } catch (e) {
      iconError = (e as Error).message;
    }
  }

  // ---- submit ----

  let saving = $state(false);
  let failure = $state<{ title: string; details: string[] } | null>(null);
  async function submit() {
    if (built.problems.length) return;
    saving = true;
    failure = null;
    try {
      const plugin = id ? await replaceDefinition(id, built.definition) : await registerDefinition(built.definition);
      navigate(`/plugins/by-id/${encodeURIComponent(plugin.id)}`);
    } catch (e) {
      const error = e instanceof ApiError ? e : new ApiError(0, String(e));
      failure = { title: error.message, details: error.details };
    } finally {
      saving = false;
    }
  }

  const sideLabel = (name: 'in' | 'out') => (name === 'in' ? 'Input: what its nodes receive' : 'Output: what its nodes emit');
</script>

<TokenPrompt />

<Page>
  <header>
    <a class="btn ghost icon" href={id ? `/plugins/by-id/${encodeURIComponent(id)}` : '/plugins'} aria-label="Back" title="Back"><ArrowLeft size={16} strokeWidth={1.75} /></a>
    <div>
      <h1>{editing ? `Edit ${form.name || id}` : 'New remote plugin'}</h1>
      <p class="muted">
        Describes a plugin that runs elsewhere and connects to this server’s hub, so its nodes can be placed, wired, mapped and checked. Nothing runs here and
        nothing is deployed; its nodes are always remote.
      </p>
    </div>
  </header>

  {#if loadError}
    <div class="notice danger" role="alert">{loadError}</div>
  {:else if !loaded}
    <div class="card muted">Loading…</div>
  {:else}
    <Tabs tabs={STEPS} active={step} onselect={(s) => (step = s)} />

    <div class="card body">
      {#if step === 'identity'}
        <div class="grid">
          <label class="field">
            Id
            <input class="input mono" bind:value={form.id} disabled={editing} placeholder="edge-sensor" aria-invalid={form.id !== '' && !ID_PATTERN.test(form.id)} />
            <span class="hint">Unique on this server; lowercase letters, digits, - and _. It can’t change later.</span>
          </label>
          <label class="field">
            Name
            <input class="input" bind:value={form.name} placeholder="Edge sensor" />
          </label>
          <label class="field">
            Version
            <input class="input mono" bind:value={form.version} placeholder="1.0.0" />
            <span class="hint">The version the plugin reports when it connects: the hub checks it.</span>
          </label>
          <label class="field">
            Type
            <select class="input" bind:value={form.type} disabled={editing}>
              <option value="SOURCE">Source: produces events</option>
              <option value="TRANSFORM">Transform: changes events</option>
              <option value="SINK">Sink: consumes events</option>
            </select>
          </label>
        </div>
        <label class="field">
          Description
          <textarea class="input" rows="3" bind:value={form.description}></textarea>
        </label>
      {:else if step === 'payloads'}
        <p class="muted">The payloads the plugin declares when it connects: the hub refuses it if they differ. Mappings on its edges are checked against them.</p>
        {#if sideError}<div class="notice warn">{sideError}</div>{/if}
        {#each sidesOf(form.type) as name (name)}
          {@const side = form[name]}
          <section>
            <div class="section-head">
              <h3>{sideLabel(name)}</h3>
              <div class="seg" role="group" aria-label="Edit as">
                <button type="button" class="btn ghost" class:active={side.mode === 'fields'} onclick={() => setMode(side, 'fields')}>Fields</button>
                <button type="button" class="btn ghost" class:active={side.mode === 'json'} onclick={() => setMode(side, 'json')}>JSON</button>
              </div>
            </div>
            <label class="check">
              <input type="checkbox" bind:checked={side.perNode} />
              Each node sets its own, in its settings
            </label>
            {#if side.perNode}
              <label class="field inline">
                Settings key
                <input class="input mono" value={side.pointer.replace(/^\//, '')} oninput={(e) => (side.pointer = `/${e.currentTarget.value}`)} />
                <span class="hint">A node’s schema goes there (added to the node settings if they don’t have it). The fields below are the default, if any.</span>
              </label>
            {/if}
            {#if side.mode === 'fields'}
              <FieldRows bind:rows={side.fields} />
            {:else}
              <textarea class="input mono" rows="12" bind:value={side.json} placeholder={'{"fields": [{"key": "temperature", "type": "F64"}]}'}></textarea>
              <span class="hint">A <code>SchemaDefinition</code> in proto3 JSON.</span>
            {/if}
          </section>
        {/each}
      {:else if step === 'settings'}
        <p class="muted">
          Optional: a JSON Schema of a node’s settings, shown as the node’s form. A remote node’s settings never reach the plugin; they’re for its own payload schemas
          and for a configurator endpoint on the remote side (which sets some of them from the others). See the plugin specification, section 3.
        </p>
        <label class="field">
          Node settings (JSON Schema)
          <textarea class="input mono" rows="14" bind:value={form.settings} placeholder={'{"type": "object", "properties": {…}}'}></textarea>
          {#if settings.error}<span class="hint danger">{settings.error}</span>{/if}
        </label>
        {#if settingKeys.length}
          <fieldset>
            <legend>Set by the remote side’s configurator endpoint</legend>
            {#each settingKeys as key (key)}
              <label class="check">
                <input
                  type="checkbox"
                  checked={form.configured.includes(key)}
                  onchange={(e) => (form.configured = e.currentTarget.checked ? [...form.configured, key] : form.configured.filter((k) => k !== key))}
                />
                <code>{key}</code>
              </label>
            {/each}
            <span class="hint">A node that names an endpoint (its settings, “Configurator endpoint”) gets these from it.</span>
          </fieldset>
        {/if}
      {:else if step === 'appearance'}
        <div class="grid">
          <div class="field">
            Icon
            <div class="icon-row">
              <span class="icon-preview" style:--plugin-color={form.color || undefined}>
                {#if iconPreview(form.icon)}<img src={iconPreview(form.icon)} alt="" width="28" height="28" />{/if}
              </span>
              <label class="btn">
                <ImageUp size={16} strokeWidth={1.75} />Choose…
                <input type="file" accept=".svg,.png,image/svg+xml,image/png" hidden onchange={(e) => pickIcon(e.currentTarget.files?.[0])} />
              </label>
              {#if form.icon}<button class="btn ghost" type="button" onclick={() => (form.icon = null)}><X size={16} strokeWidth={1.75} />Remove</button>{/if}
            </div>
            {#if iconError}<span class="hint danger">{iconError}</span>{/if}
          </div>
          <label class="field">
            Colour
            <div class="icon-row">
              <input type="color" value={form.color || '#4f7cff'} oninput={(e) => (form.color = e.currentTarget.value)} aria-label="Colour" />
              <span class="mono muted">{form.color || 'none'}</span>
              {#if form.color}<button class="btn ghost" type="button" onclick={() => (form.color = '')}>Clear</button>{/if}
            </div>
          </label>
          <label class="field">
            Categories
            <input class="input" bind:value={form.categories} placeholder="Sensors, Edge" />
            <span class="hint">Comma-separated; the palette groups by the first.</span>
          </label>
        </div>
        <label class="field">
          Docs (Markdown)
          <textarea class="input mono" rows="12" bind:value={form.docs} placeholder="# Edge sensor&#10;&#10;What it does, where it runs, how to start it…"></textarea>
        </label>
      {:else}
        {#if built.problems.length}
          <div class="notice warn" role="alert">
            Before it can be {editing ? 'saved' : 'registered'}:
            <ul>{#each built.problems as p, i (i)}<li>{p}</li>{/each}</ul>
          </div>
        {/if}
        {#if failure}
          <div class="notice danger" role="alert">
            {failure.title}
            {#if failure.details.length}<ul>{#each failure.details as d, i (i)}<li>{d}</li>{/each}</ul>{/if}
          </div>
        {/if}
        <h3>manifest.json</h3>
        <pre class="mono">{JSON.stringify(built.definition.manifest, null, 2)}</pre>
        <h3>Files</h3>
        <ul class="files">
          {#each Object.keys(built.definition.files) as path (path)}<li class="mono">{path}</li>{:else}<li class="muted">none</li>{/each}
        </ul>
      {/if}
    </div>

    <div class="nav">
      <button class="btn" type="button" disabled={index === 0} onclick={() => (step = STEPS[index - 1].id)}><ArrowLeft size={16} strokeWidth={1.75} />Back</button>
      {#if index < STEPS.length - 1}
        <button class="btn primary" type="button" onclick={() => (step = STEPS[index + 1].id)}>Next<ArrowRight size={16} strokeWidth={1.75} /></button>
      {:else}
        <button class="btn primary" type="button" disabled={saving || built.problems.length > 0} onclick={submit}>
          <Check size={16} strokeWidth={1.75} />{saving ? 'Saving…' : editing ? 'Save' : 'Register'}
        </button>
      {/if}
    </div>
  {/if}
</Page>

<style>
  header {
    display: flex;
    gap: var(--space-3);
    align-items: flex-start;
  }

  h1 {
    margin: 0;
    font-size: var(--text-xl);
  }

  header p {
    margin: var(--space-1) 0 0;
    max-width: 72ch;
  }

  .body {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
  }

  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
    gap: var(--space-3);
  }

  section {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }

  .section-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }

  h3 {
    margin: 0;
    font-size: var(--text-md);
  }

  .seg {
    display: flex;
    gap: var(--space-1);
  }

  .seg .active {
    background: var(--surface-2);
    color: var(--text);
  }

  .inline {
    max-width: 480px;
  }

  textarea.mono {
    font-size: var(--text-sm);
  }

  fieldset {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    padding: var(--space-3);
  }

  .icon-row {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }

  .icon-preview {
    --plugin-color: var(--accent);
    display: grid;
    place-items: center;
    width: 40px;
    height: 40px;
    border-radius: var(--radius-sm);
    background: var(--surface-2);
    border-bottom: 3px solid var(--plugin-color);
  }

  .hint.danger {
    color: var(--danger);
  }

  pre {
    margin: 0;
    padding: var(--space-3);
    overflow: auto;
    max-height: 420px;
    border-radius: var(--radius-sm);
    background: var(--surface-2);
    font-size: var(--text-sm);
  }

  .files {
    margin: 0;
    padding-left: var(--space-4);
  }

  .nav {
    display: flex;
    justify-content: space-between;
  }

  ul {
    margin: var(--space-1) 0 0;
    padding-left: var(--space-4);
  }
</style>
