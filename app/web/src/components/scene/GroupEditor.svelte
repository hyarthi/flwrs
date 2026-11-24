<script lang="ts">
  // The scene's groups in the settings dialog: create, rename, set limits,
  // delete, with nesting shown. Families a scene doesn't accept (an imported
  // file can hold them) are read-only, with the reason; they can be deleted.
  import Plus from '@lucide/svelte/icons/plus';
  import Trash from '@lucide/svelte/icons/trash-2';
  import {
    MAX_GROUP_ID,
    SCENE_FAMILIES,
    familyLabel,
    groupColors,
    groupMessages,
    isEditable,
    nesting,
    policySummary,
    rowProblems,
    rowToGroup,
    type GroupRow,
  } from '../../lib/groups';

  let {
    rows = $bindable(),
    members,
    problems,
  }: {
    rows: GroupRow[];
    /** Members by the group's id when the dialog opened. */
    members: Map<string, Set<string>>;
    problems: string[];
  } = $props();

  const invalid = $derived(rowProblems(rows));
  const memberCount = (r: GroupRow) => (r.original ? (members.get(r.original)?.size ?? 0) : 0);

  // Nesting by the rows' current names.
  const renamed = $derived(new Map(rows.filter((r) => r.original).map((r) => [r.id, members.get(r.original!) ?? new Set<string>()])));
  const levels = $derived(nesting(rows.map(rowToGroup), renamed));
  const colors = $derived(groupColors(rows.map((r) => r.id)));
  /** Parents before their children. */
  const ordered = $derived.by(() => {
    const out: { row: GroupRow; index: number }[] = [];
    const entries = rows.map((row, index) => ({ row, index }));
    const visit = (parent: string | null) => {
      for (const e of entries) {
        if ((levels.get(e.row.id)?.parent ?? null) !== parent || out.includes(e)) continue;
        out.push(e);
        visit(e.row.id);
      }
    };
    visit(null);
    for (const e of entries) if (!out.includes(e)) out.push(e);
    return out;
  });

  function add() {
    const used = new Set(rows.map((r) => r.id));
    let n = rows.length + 1;
    while (used.has(`group-${n}`)) n++;
    rows = [...rows, { original: null, id: `group-${n}`, family: SCENE_FAMILIES[0], policy: { limits: {} }, cpu: '' }];
  }

  const remove = (index: number) => (rows = rows.filter((_, i) => i !== index));
</script>

<fieldset>
  <legend>Groups</legend>
  {#if rows.length === 0}
    <span class="hint muted">No groups yet. A resources group shares a core budget among its nodes.</span>
  {/if}
  {#each ordered as { row, index } (row.original ?? index)}
    {@const depth = levels.get(row.id)?.depth ?? 0}
    {@const messages = row.original ? groupMessages(problems, row.original) : []}
    {@const editable = isEditable(row)}
    <div class="row" style:--depth={depth} style:--chip-color={`var(--group-${colors.get(row.id) ?? 1})`}>
      <div class="line">
        <span class="swatch" aria-hidden="true"></span>
        {#if editable}
          <input class="input id" aria-label="Group name" maxlength={MAX_GROUP_ID} bind:value={rows[index].id} />
          <label class="cores">
            Cores
            <input class="input" type="number" min="1" step="1" placeholder="No limit" bind:value={() => rows[index].cpu, (v) => (rows[index].cpu = v === null || v === undefined ? '' : String(v))} />
          </label>
        {:else}
          <span class="mono fixed-id">{row.id}</span>
          <span class="badge">{familyLabel(row.family)}</span>
          <span class="muted policy">{policySummary(rowToGroup(row))}</span>
        {/if}
        <span class="muted count" title="Nodes in this group">{memberCount(row)} {memberCount(row) === 1 ? 'node' : 'nodes'}</span>
        <button class="btn ghost icon" type="button" aria-label={`Delete group ${row.id}`} title="Delete the group (its nodes stay, without it)" onclick={() => remove(index)}>
          <Trash size={14} strokeWidth={1.75} />
        </button>
      </div>
      {#if invalid.has(index)}<span class="error">{invalid.get(index)}</span>{/if}
      {#each messages as m, i (i)}<span class="error">{m}</span>{/each}
    </div>
  {/each}
  <button class="btn add" type="button" onclick={add}><Plus size={14} strokeWidth={1.75} />Add a resources group</button>
  <span class="hint muted">Add nodes by dropping them on a frame, or in a node’s settings. Within a family, groups nest or share no nodes.</span>
</fieldset>

<style>
  fieldset {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
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

  .row {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    padding-left: calc(var(--depth) * var(--space-4));
  }

  .line {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-width: 0;
  }

  .swatch {
    flex: none;
    width: 12px;
    height: 12px;
    border-radius: 3px;
    background: var(--chip-color);
  }

  .id {
    flex: 1;
    min-width: 0;
    font-family: var(--font-mono);
  }

  .fixed-id {
    flex: none;
    max-width: 40%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .cores {
    display: flex;
    align-items: center;
    gap: var(--space-1);
    flex: none;
    font-size: var(--text-xs);
  }

  .cores input {
    width: 88px;
  }

  .policy,
  .count,
  .hint,
  .error {
    font-size: var(--text-xs);
  }

  .policy {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .count {
    flex: none;
  }

  .error {
    color: var(--danger);
  }

  .add {
    align-self: flex-start;
  }
</style>
