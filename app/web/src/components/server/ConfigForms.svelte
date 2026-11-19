<script lang="ts">
  // The config as forms: the sections on the left, the chosen one's
  // settings on the right (from its schema: descriptions, defaults as
  // placeholders). Empty = left out of the file, so the default applies.
  // Secrets: kept as they are unless replaced or cleared. Locked settings
  // are shown, not editable.
  import { untrack } from 'svelte';
  import Lock from '@lucide/svelte/icons/lock';
  import SchemaField from '../form/SchemaField.svelte';
  import { resolve, type Schema } from '../../lib/schemaForm';
  import type { Problem, SectionView } from '../../lib/server';

  let {
    sections,
    values,
    problems,
    secret,
    editable,
    onchange,
  }: {
    sections: SectionView[];
    /** Each section's values as the draft has them. */
    values: Record<string, Record<string, unknown>>;
    problems: Problem[];
    /** The placeholder that keeps a secret. */
    secret: string;
    editable: boolean;
    onchange: (section: string, values: Record<string, unknown>) => void;
  } = $props();

  let selected = $state(untrack(() => sections[0]?.key ?? ''));
  const section = $derived(sections.find((s) => s.key === selected) ?? sections[0]);
  const current = $derived(values[section?.key] ?? {});
  const sectionProblems = (key: string) => problems.filter((p) => p.section === key);
  const locked = (key: string) => section.locked.includes('') || section.locked.includes(key);

  function set(key: string, value: unknown) {
    const next = { ...current };
    if (value === undefined) delete next[key];
    else next[key] = value;
    onchange(section.key, next);
  }

  /** Where a setting's value comes from, as the draft has it. */
  function source(key: string): string {
    if (key in current) return 'file';
    return section.sources[key] === 'env' ? 'env' : 'default';
  }

  // A secret being typed (not yet in the draft until it's non-empty).
  let replacing = $state<Record<string, string>>({});
</script>

<div class="forms">
  <nav aria-label="Sections">
    {#each sections as s (s.key)}
      {@const n = sectionProblems(s.key).length}
      <button type="button" class:active={s.key === section?.key} onclick={() => (selected = s.key)}>
        <span>{s.title}</span>
        <span class="mono muted key">[{s.key}]</span>
        {#if n}<span class="badge danger">{n}</span>{/if}
      </button>
    {/each}
  </nav>

  {#if section}
    <section class="card">
      <header>
        <h2>{section.title} <span class="mono muted">[{section.key}]</span></h2>
        <p class="muted">{section.summary}</p>
        {#if section.locked.includes('')}
          <p class="notice warn"><Lock size={14} strokeWidth={1.75} />Locked: this section can only be changed in the file itself, by someone with access to the machine.</p>
        {/if}
        {#each sectionProblems(section.key) as p, i (i)}<p class="notice danger">{p.message}</p>{/each}
      </header>

      {#each (section.schema['x-order'] as string[] | undefined) ?? Object.keys(section.schema.properties ?? {}) as key (key)}
        {@const schema = resolve(section.schema, (section.schema.properties ?? {})[key] as Schema)}
        {@const isSecret = section.secrets.includes(key)}
        <div class="setting">
          <div class="meta">
            <span class="badge" class:accent={source(key) === 'file'} title={source(key) === 'file' ? 'Set in the file' : source(key) === 'env' ? 'From the environment' : 'The default'}>{source(key)}</span>
            {#if locked(key)}<span class="badge" title="Only in the file itself"><Lock size={11} strokeWidth={2} /> locked</span>{/if}
          </div>
          {#if isSecret}
            <label class="field">
              <span class="mono">{key}</span>
              {#if current[key] === secret && replacing[key] === undefined}
                <span class="secret">
                  <span class="muted">Set (hidden)</span>
                  {#if editable && !locked(key)}
                    <button type="button" class="btn ghost small" onclick={() => (replacing[key] = '')}>Replace</button>
                    <button type="button" class="btn ghost small danger" onclick={() => set(key, undefined)}>Clear</button>
                  {/if}
                </span>
              {:else}
                <input
                  class="input"
                  type="password"
                  autocomplete="off"
                  placeholder={current[key] === undefined ? 'not set' : ''}
                  disabled={!editable || locked(key)}
                  value={replacing[key] ?? ''}
                  oninput={(e) => {
                    replacing[key] = e.currentTarget.value;
                    set(key, e.currentTarget.value === '' ? undefined : e.currentTarget.value);
                  }}
                />
              {/if}
              {#if schema.description}<span class="hint">{schema.description}</span>{/if}
            </label>
          {:else}
            <SchemaField
              root={section.schema}
              {schema}
              name={key}
              label={key}
              value={current[key]}
              disabled={!editable || locked(key)}
              onchange={(v) => set(key, v)}
            />
          {/if}
        </div>
      {/each}
    </section>
  {/if}
</div>

<style>
  .forms {
    display: grid;
    grid-template-columns: 220px minmax(0, 1fr);
    gap: var(--space-4);
    align-items: start;
  }

  nav {
    display: flex;
    flex-direction: column;
    gap: 2px;
    position: sticky;
    top: var(--space-4);
  }

  nav button {
    display: grid;
    grid-template-columns: 1fr auto;
    align-items: center;
    gap: 0 var(--space-2);
    padding: var(--space-2) var(--space-3);
    border: 0;
    border-radius: var(--radius-sm);
    background: none;
    color: var(--text);
    text-align: left;
    cursor: pointer;
  }

  nav button:hover {
    background: var(--surface-hover);
  }

  nav button.active {
    background: var(--accent-soft);
    color: var(--accent);
  }

  nav .key {
    grid-column: 1;
    font-size: var(--text-xs);
  }

  nav .badge {
    grid-row: 1 / span 2;
    grid-column: 2;
  }

  .card {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
    padding: var(--space-4);
  }

  h2 {
    margin: 0;
    font-size: var(--text-lg);
  }

  header p {
    margin: var(--space-1) 0 0;
  }

  .notice {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }

  .setting {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .meta {
    display: flex;
    gap: var(--space-1);
  }

  .meta .badge {
    font-size: var(--text-xs);
  }

  .secret {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }

  .small {
    font-size: var(--text-sm);
  }

  @media (max-width: 760px) {
    .forms {
      grid-template-columns: 1fr;
    }

    nav {
      position: static;
    }
  }
</style>
