<script lang="ts">
  // One property of a launch-args schema as a form control; nested objects
  // render their properties with this component again. `onchange` gets the
  // new value, or `undefined` to leave the property out (its default then
  // applies). Secrets and uploads are the settings form's job (top level).
  // Text fields (documents, code, JSON, long text) also take a file: dropped
  // on them or picked with Import, its text replaces theirs; and the plugin's
  // template (`x-flwrs-template`), where it has one: its text, or for a
  // field that isn't text (an object, a list), the JSON value it holds.
  import Plus from '@lucide/svelte/icons/plus';
  import X from '@lucide/svelte/icons/x';
  import FileUp from '@lucide/svelte/icons/file-up';
  import FileText from '@lucide/svelte/icons/file-text';
  import SchemaField from './SchemaField.svelte';
  import CodeEditor from './CodeEditor.svelte';
  import ObjectList from './ObjectList.svelte';
  import type { Scope } from '../../lib/lua/types';
  import { constraintHint, kindOf, labelOf, resolve, sameJson, typeOf, type Json, type Schema } from '../../lib/schemaForm';

  let {
    root,
    schema,
    name,
    value,
    onchange,
    required = false,
    disabled = false,
    errors = [],
    label = labelOf(name, schema),
    codeScope,
    templateOf,
  }: {
    root: Schema;
    schema: Schema;
    name: string;
    value: unknown;
    onchange: (value: unknown) => void;
    required?: boolean;
    disabled?: boolean;
    errors?: string[];
    label?: string;
    /** The scope a code field sees (Lua completion). */
    codeScope?: (schema: Schema) => Scope | null;
    /** A field's template text (`x-flwrs-template`), once loaded. */
    templateOf?: (schema: Schema) => string | undefined;
  } = $props();

  const kind = $derived(kindOf(schema));
  const hint = $derived(constraintHint(schema));
  // What leaving the field empty means: the default's actual value, in the
  // placeholder (inputs), the label (checkboxes), the first option (lists of
  // choices) or a hint (code, JSON, lists).
  const hasDefault = $derived(schema.default !== undefined && schema.default !== null);
  const placeholder = $derived(
    hasDefault && typeof schema.default !== 'object' ? `Default (${schema.default === '' ? 'empty' : String(schema.default)})` : '',
  );
  const defaultHint = $derived.by(() => {
    if (!hasDefault || kind === 'enum' || kind === 'boolean' || kind === 'string' || kind === 'integer' || kind === 'number') return null;
    const text = typeof schema.default === 'string' ? schema.default : JSON.stringify(schema.default);
    return `Default: ${text.length > 80 ? `${text.slice(0, 77)}…` : text}`;
  });
  const id = `f-${Math.random().toString(36).slice(2)}`;

  // Numbers: empty = left out.
  function number(text: string) {
    if (text.trim() === '') return onchange(undefined);
    const n = Number(text);
    onchange(Number.isFinite(n) ? n : text);
  }

  function text(t: string) {
    onchange(t === '' && !required ? undefined : t);
  }

  // JSON text: kept as typed while it doesn't parse.
  let jsonText = $state<string | null>(null);
  let jsonError = $state<string | null>(null);
  const shownJson = $derived(jsonText ?? (value === undefined ? '' : JSON.stringify(value, null, 2)));
  function json(t: string) {
    jsonText = t;
    if (t.trim() === '') {
      jsonError = null;
      onchange(undefined);
      return;
    }
    try {
      onchange(JSON.parse(t) as Json);
      jsonError = null;
    } catch (e) {
      jsonError = `Not valid JSON: ${e instanceof Error ? e.message : e}`;
    }
  }

  const list = $derived(Array.isArray(value) ? (value as unknown[]) : []);
  const itemType = $derived(schema.items ? typeOf(resolve(root, schema.items)) : 'string');
  function setItem(i: number, t: string) {
    const next = [...list];
    next[i] = itemType === 'string' ? t : t.trim() === '' ? t : Number(t);
    onchange(next);
  }

  const object = $derived(value && typeof value === 'object' && !Array.isArray(value) ? (value as Record<string, unknown>) : {});
  function setChild(key: string, v: unknown) {
    const next = { ...object };
    if (v === undefined) delete next[key];
    else next[key] = v;
    onchange(Object.keys(next).length || required ? next : undefined);
  }
  /** An array of objects: rows (`ObjectList`) instead of JSON text. */
  const objectItems = $derived.by(() => {
    if (typeOf(schema) !== 'array' || !schema.items) return null;
    const items = resolve(root, schema.items);
    return typeOf(items) === 'object' && Object.keys(items.properties ?? {}).length ? items : null;
  });
  /** An object's properties in their declared order, where the schema gives it. */
  const childKeys = $derived((schema['x-order'] as string[] | undefined) ?? Object.keys(schema.properties ?? {}));
  const multiline = $derived(
    !!schema.contentMediaType ||
      (schema.maxLength ?? 0) > 200 ||
      schema.format === 'textarea' ||
      !!schema['x-flwrs-template'] ||
      (typeof value === 'string' && value.includes('\n')),
  );

  // ---- documents: an object or its text, edited as text ----

  const documentText = $derived(typeof value === 'string' ? value : value === undefined ? '' : JSON.stringify(value, null, 2));

  // ---- text from a file (drop or Import) or the template ----

  /** Fields whose whole value is text a file can hold. */
  const textual = $derived(kind === 'document' || kind === 'code' || kind === 'json' || (kind === 'string' && multiline));
  const template = $derived(templateOf?.(schema));
  /** The field takes text (else a template is a JSON value). */
  const takesText = $derived(kind === 'string' || kind === 'code' || kind === 'document');
  const templateValue = $derived.by(() => {
    if (template === undefined || takesText) return template;
    try {
      return JSON.parse(template) as unknown;
    } catch {
      return undefined;
    }
  });
  const currentText = $derived(kind === 'json' ? shownJson : kind === 'document' ? documentText : typeof value === 'string' ? value : '');
  /** Larger files would make the editor (and the node's settings) unwieldy. */
  const MAX_FILE_BYTES = 4 * 1024 * 1024;
  let fileError = $state<string | null>(null);
  let dropDepth = $state(0);
  let picker = $state<HTMLInputElement>();

  function setText(t: string) {
    if (kind === 'json') json(t);
    else text(t);
  }

  const atTemplate = $derived(
    takesText ? currentText === template : sameJson(value, templateValue),
  );
  function useTemplate() {
    if (takesText || kind === 'json') setText(template!);
    else onchange(structuredClone($state.snapshot(templateValue)));
  }

  async function importFile(file: File | undefined) {
    if (!file) return;
    fileError = null;
    if (file.size > MAX_FILE_BYTES) {
      fileError = `${file.name} is too large for a setting (over 4 MiB): upload it to the node instead.`;
      return;
    }
    try {
      setText(await file.text());
    } catch (e) {
      fileError = `Couldn’t read ${file.name}: ${e instanceof Error ? e.message : e}`;
    }
  }

  const hasFiles = (e: DragEvent) => e.dataTransfer?.types.includes('Files') ?? false;
  // Capture phase, so the code editor doesn't insert the file itself.
  function ondragenter(e: DragEvent) {
    if (!textual || disabled || !hasFiles(e)) return;
    e.preventDefault();
    dropDepth++;
  }
  function ondragover(e: DragEvent) {
    if (!textual || disabled || !hasFiles(e)) return;
    e.preventDefault();
    e.stopPropagation();
    if (e.dataTransfer) e.dataTransfer.dropEffect = 'copy';
  }
  function ondragleave(e: DragEvent) {
    if (hasFiles(e)) dropDepth = Math.max(0, dropDepth - 1);
  }
  function ondrop(e: DragEvent) {
    if (!textual || disabled || !hasFiles(e)) return;
    e.preventDefault();
    e.stopPropagation();
    dropDepth = 0;
    importFile(e.dataTransfer?.files[0]);
  }
</script>

{#snippet templateButton()}
  {#if templateValue !== undefined && !atTemplate && !disabled}
    <button class="btn ghost tool" type="button" title="Replace the value with the plugin’s template" onclick={useTemplate}>
      <FileText size={14} strokeWidth={1.75} />Use template
    </button>
  {/if}
{/snippet}

{#if objectItems}
  <ObjectList {root} items={objectItems} {value} {label} description={schema.description} {required} {disabled} {onchange} tools={templateButton} />
{:else if kind === 'object'}
  <fieldset class="group" {disabled}>
    <legend>{label}{required ? ' *' : ''} {@render templateButton()}</legend>
    {#if schema.description}<p class="hint">{schema.description}</p>{/if}
    {#each childKeys as key (key)}
      <SchemaField
        {root}
        schema={resolve(root, (schema.properties ?? {})[key] ?? {})}
        name={key}
        value={object[key]}
        required={schema.required?.includes(key)}
        {disabled}
        errors={errors.filter((e) => e.includes(`/${key}`))}
        {codeScope}
        {templateOf}
        onchange={(v) => setChild(key, v)}
      />
    {/each}
    {#each errors.filter((e) => !Object.keys(schema.properties ?? {}).some((k) => e.includes(`/${k}`))) as e, i (i)}
      <p class="error">{e}</p>
    {/each}
  </fieldset>
{:else if kind === 'boolean'}
  <div class="field">
    <label class="check">
      <input type="checkbox" checked={value === true || (value === undefined && schema.default === true)} {disabled} onchange={(e) => onchange(e.currentTarget.checked)} />
      {label}
      {#if typeof schema.default === 'boolean'}<span class="default">(default: {schema.default ? 'on' : 'off'})</span>{/if}
    </label>
    {#if schema.description}<span class="hint">{schema.description}</span>{/if}
    {#each errors as e, i (i)}<span class="error">{e}</span>{/each}
  </div>
{:else}
  <div
    class="field"
    class:dropping={dropDepth > 0}
    role="group"
    aria-label={label}
    ondragentercapture={ondragenter}
    ondragovercapture={ondragover}
    ondragleavecapture={ondragleave}
    ondropcapture={ondrop}
  >
    <div class="label-row">
      <label for={id}>{label}{required ? ' *' : ''}</label>
      {#if textual && !disabled}
        <span class="tools">
          {@render templateButton()}
          <button class="btn ghost tool" type="button" title="Replace the text with a file’s (or drop a file on the field)" onclick={() => picker?.click()}>
            <FileUp size={14} strokeWidth={1.75} />Import…
          </button>
          <input bind:this={picker} type="file" hidden onchange={(e) => { importFile(e.currentTarget.files?.[0]); e.currentTarget.value = ''; }} />
        </span>
      {/if}
    </div>
    {#if kind === 'enum'}
      <select {id} class="input" {disabled} value={value === undefined ? '' : String(value)} onchange={(e) => {
        const raw = e.currentTarget.value;
        const match = schema.enum!.find((v) => String(v) === raw);
        onchange(raw === '' ? undefined : match);
      }}>
        {#if !required || value === undefined}
          <option value="">{schema.default !== undefined ? `Default (${schema.default})` : '—'}</option>
        {/if}
        {#each schema.enum ?? [] as option (String(option))}
          <option value={String(option)}>{option}</option>
        {/each}
      </select>
    {:else if kind === 'integer' || kind === 'number'}
      <input
        {id}
        class="input"
        type="number"
        step={kind === 'integer' ? 1 : 'any'}
        min={schema.minimum}
        max={schema.maximum}
        {placeholder}
        {disabled}
        value={typeof value === 'number' ? value : ''}
        oninput={(e) => number(e.currentTarget.value)}
      />
    {:else if kind === 'string'}
      {#if multiline}
        <textarea {id} class="input mono-input" rows="6" {placeholder} {disabled} value={typeof value === 'string' ? value : ''} oninput={(e) => text(e.currentTarget.value)}></textarea>
      {:else}
        <input {id} class="input" {placeholder} {disabled} value={typeof value === 'string' ? value : ''} oninput={(e) => text(e.currentTarget.value)} />
      {/if}
    {:else if kind === 'document'}
      <CodeEditor value={documentText} language="yaml" {disabled} onchange={(v) => text(v)} />
    {:else if kind === 'code'}
      <CodeEditor
        value={typeof value === 'string' ? value : ''}
        language={schema['x-flwrs-code']?.language ?? 'lua'}
        scope={codeScope?.(schema) ?? null}
        params={schema['x-flwrs-code']?.params ?? null}
        {disabled}
        onchange={(v) => text(v)}
      />
      {#if schema['x-flwrs-code']}
        <span class="hint mono">
          function({(schema['x-flwrs-code'].params ?? []).join(', ')}){schema['x-flwrs-code'].signature ? ` · ${schema['x-flwrs-code'].signature}` : ''}
        </span>
      {/if}
    {:else if kind === 'list'}
      <div class="list">
        {#each list as item, i (i)}
          <div class="list-row">
            <input class="input" type={itemType === 'string' ? 'text' : 'number'} {disabled} value={String(item ?? '')} oninput={(e) => setItem(i, e.currentTarget.value)} />
            <button class="btn ghost icon" type="button" {disabled} aria-label="Remove" onclick={() => {
              const next = list.filter((_, j) => j !== i);
              onchange(next.length || required ? next : undefined);
            }}><X size={14} strokeWidth={1.75} /></button>
          </div>
        {/each}
        <button class="btn ghost add" type="button" {disabled} onclick={() => onchange([...list, itemType === 'string' ? '' : 0])}>
          <Plus size={14} strokeWidth={1.75} />Add
        </button>
      </div>
    {:else}
      <textarea {id} class="input mono-input" rows="8" spellcheck="false" {disabled} value={shownJson} oninput={(e) => json(e.currentTarget.value)} placeholder="JSON"></textarea>
      {#if jsonError}<span class="error">{jsonError}</span>{/if}
    {/if}
    {#if dropDepth > 0}<span class="hint">Drop the file to use its text</span>{/if}
    {#if fileError}<span class="error">{fileError}</span>{/if}
    {#if schema.description}<span class="hint">{schema.description}</span>{/if}
    {#if hint}<span class="hint">{hint}</span>{/if}
    {#if defaultHint}<span class="hint">{defaultHint}</span>{/if}
    {#each errors as e, i (i)}<span class="error">{e}</span>{/each}
  </div>
{/if}

<style>
  .group {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    margin: 0;
    padding: var(--space-3);
    border: 1px solid var(--border);
    border-radius: var(--radius);
  }

  legend {
    padding: 0 var(--space-1);
    font-size: var(--text-sm);
    font-weight: 500;
  }

  .hint {
    color: var(--text-muted);
    font-size: var(--text-xs);
    font-weight: 400;
  }

  .error {
    color: var(--danger);
    font-size: var(--text-xs);
    font-weight: 400;
  }

  p {
    margin: 0;
  }

  .mono-input {
    height: auto;
    padding: var(--space-2);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    resize: vertical;
  }

  .list {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .list-row {
    display: flex;
    gap: var(--space-1);
  }

  .add {
    align-self: flex-start;
  }

  .default {
    color: var(--text-muted);
    font-weight: 400;
  }

  .label-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-2);
  }

  .tools {
    display: flex;
    gap: var(--space-1);
  }

  .btn.tool {
    height: 24px;
    padding: 0 var(--space-2);
    gap: var(--space-1);
    color: var(--text-muted);
    font-size: var(--text-xs);
  }

  .btn.tool:hover {
    color: var(--text);
  }

  .dropping {
    border-radius: var(--radius);
    outline: 2px dashed var(--accent);
    outline-offset: var(--space-1);
  }
</style>
