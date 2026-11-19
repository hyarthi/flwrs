<script lang="ts">
  // The server: its build and addresses, its config file (forms or TOML,
  // checked as you go, reviewed as a diff, saved with the key again), and
  // restarts with confirm-or-revert for a changed config.
  import RotateCw from '@lucide/svelte/icons/rotate-cw';
  import Save from '@lucide/svelte/icons/save';
  import Undo from '@lucide/svelte/icons/undo-2';
  import Lock from '@lucide/svelte/icons/lock';
  import Page from '../components/Page.svelte';
  import Tabs from '../components/Tabs.svelte';
  import Modal from '../components/Modal.svelte';
  import CodeEditor from '../components/form/CodeEditor.svelte';
  import ConfigForms from '../components/server/ConfigForms.svelte';
  import { ApiError } from '../lib/api';
  import { diffLines, hunks } from '../lib/diff';
  import {
    applyForm,
    checkConfig,
    confirmConfig,
    dismissRevert,
    getConfig,
    getInfo,
    problemText,
    restartServer,
    revertConfig,
    saveConfig,
    type CheckResponse,
    type ConfigView,
    type ServerInfo,
  } from '../lib/server';

  let info = $state<ServerInfo | null>(null);
  let view = $state<ConfigView | null>(null);
  let loadError = $state<string | null>(null);
  let tab = $state('overview');
  let mode = $state('forms');

  /** The text being edited (null: as saved). */
  let draft = $state<string | null>(null);
  let checked = $state<CheckResponse | null>(null);
  /** Form values per section, as the draft has them. */
  let formValues = $state<Record<string, Record<string, unknown>>>({});
  let notice = $state<{ kind: 'ok' | 'warn' | 'danger'; text: string; details?: string[] } | null>(null);

  const message = (e: unknown) => (e instanceof ApiError ? e : new ApiError(0, String(e)));

  async function load() {
    try {
      [info, view] = await Promise.all([getInfo(), getConfig()]);
      formValues = Object.fromEntries(view.sections.map((s) => [s.key, s.values]));
      draft = null;
      checked = null;
      loadError = null;
    } catch (e) {
      loadError = message(e).message;
    }
  }
  load();

  const text = $derived(draft ?? view?.text ?? '');
  const dirty = $derived(draft !== null && draft !== view?.text);
  const editable = $derived(view?.editable ?? false);
  const problems = $derived(checked?.problems ?? view?.problems ?? []);

  // ---- checking as you go ----

  let checkTimer: ReturnType<typeof setTimeout> | undefined;
  let checkError = $state<string | null>(null);
  function scheduleCheck(syncForms = false) {
    clearTimeout(checkTimer);
    checkTimer = setTimeout(async () => {
      const t = text;
      try {
        const result = await checkConfig(t);
        if (t !== text) return; // a newer edit is on its way
        checked = result;
        checkError = null;
        if (syncForms) formValues = result.values;
      } catch (e) {
        checkError = message(e).message;
      }
    }, 400);
  }

  function editText(t: string) {
    draft = t;
    formsStale = true;
    scheduleCheck();
  }

  let formsStale = false;
  $effect(() => {
    if (mode === 'forms' && formsStale) {
      formsStale = false;
      scheduleCheck(true);
    }
  });

  let applying: Promise<unknown> = Promise.resolve();
  function editForm(section: string, values: Record<string, unknown>) {
    formValues[section] = values;
    // In order: each edit applies to the text the previous one produced.
    applying = applying.then(async () => {
      try {
        const result = await applyForm(text, section, values);
        draft = result.text;
        scheduleCheck();
      } catch (e) {
        notice = { kind: 'danger', text: message(e).message };
      }
    });
  }

  function discard() {
    draft = null;
    checked = null;
    if (view) formValues = Object.fromEntries(view.sections.map((s) => [s.key, s.values]));
  }

  // ---- review and save ----

  let reviewing = $state(false);
  let reviewCheck = $state<CheckResponse | null>(null);
  let key = $state('');
  let saving = $state(false);
  let saveError = $state<{ text: string; details: string[] } | null>(null);
  const diff = $derived(reviewing && view?.text != null ? hunks(diffLines(view.text, text)) : []);

  async function review() {
    reviewing = true;
    saveError = null;
    reviewCheck = null;
    try {
      reviewCheck = await checkConfig(text);
    } catch (e) {
      const err = message(e);
      saveError = { text: err.message, details: err.details };
    }
  }

  async function save() {
    saving = true;
    saveError = null;
    try {
      const result = await saveConfig(text, info?.step_up ? key : null);
      reviewing = false;
      key = '';
      await load();
      notice = result.restart_needed
        ? { kind: 'warn', text: 'Saved. Most settings are read at start: restart the server to apply them.' }
        : { kind: 'ok', text: 'Saved.' };
    } catch (e) {
      const err = message(e);
      saveError = { text: err.message, details: err.details };
    } finally {
      saving = false;
    }
  }

  // ---- restart, then confirm or revert ----

  let restarting = $state<'ask' | 'waiting' | null>(null);
  let restartKey = $state('');
  let restartError = $state<{ text: string; details: string[] } | null>(null);
  let waitedS = $state(0);

  async function restart() {
    restartError = null;
    try {
      const before = info?.start_time ?? 0;
      await restartServer(info?.step_up ? restartKey : null);
      restartKey = '';
      restarting = 'waiting';
      waitedS = 0;
      const started = Date.now();
      // Wait for the new process to answer (a new start time).
      while (restarting === 'waiting') {
        await new Promise((r) => setTimeout(r, 1000));
        waitedS = Math.round((Date.now() - started) / 1000);
        try {
          const next = await getInfo();
          if (next.start_time !== before || next.pid !== info?.pid) {
            restarting = null;
            await load();
            notice = { kind: 'ok', text: 'The server restarted.' };
            return;
          }
        } catch {
          // still down
        }
      }
    } catch (e) {
      const err = message(e);
      restartError = { text: err.message, details: err.details };
    }
  }

  let now = $state(Date.now());
  $effect(() => {
    const timer = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(timer);
  });
  const confirmLeft = $derived(info?.confirm_deadline ? Math.max(0, info.confirm_deadline - Math.floor(now / 1000)) : null);
  let revertKey = $state('');
  let confirming = $state(false);

  async function keep() {
    confirming = true;
    try {
      await confirmConfig();
      await load();
      notice = { kind: 'ok', text: 'The new config stays.' };
    } catch (e) {
      notice = { kind: 'danger', text: message(e).message };
    } finally {
      confirming = false;
    }
  }

  async function revert() {
    confirming = true;
    try {
      const before = info?.start_time ?? 0;
      await revertConfig(info?.step_up ? revertKey : null);
      revertKey = '';
      restarting = 'waiting';
      const started = Date.now();
      while (restarting === 'waiting') {
        await new Promise((r) => setTimeout(r, 1000));
        waitedS = Math.round((Date.now() - started) / 1000);
        try {
          const next = await getInfo();
          if (next.start_time !== before) {
            restarting = null;
            await load();
            notice = { kind: 'ok', text: 'The previous config is back.' };
          }
        } catch {
          // still down
        }
      }
    } catch (e) {
      notice = { kind: 'danger', text: message(e).message };
    } finally {
      confirming = false;
    }
  }

  const time = (s: number) => new Date(s * 1000).toLocaleString();
  const TABS = [
    { id: 'overview', label: 'Overview' },
    { id: 'config', label: 'Configuration' },
  ];
</script>

<Page>
  {#if loadError}
    <div class="notice danger" role="alert">{loadError}</div>
  {/if}

  {#if info?.confirm_deadline && confirmLeft !== null}
    <div class="notice warn confirm" role="alert">
      <div class="grow">
        <strong>The server restarted with a changed config.</strong> Keep it? Without an answer, the previous config comes back in
        <span class="mono">{confirmLeft} s</span> (and the server restarts again).
      </div>
      {#if info.step_up}<input class="input key" type="password" placeholder="Secret key (to revert)" bind:value={revertKey} autocomplete="off" />{/if}
      <button class="btn" type="button" disabled={confirming} onclick={revert}><Undo size={16} strokeWidth={1.75} />Revert now</button>
      <button class="btn primary" type="button" disabled={confirming} onclick={keep}>Keep it</button>
    </div>
  {/if}
  {#if info?.last_revert}
    <div class="notice warn">
      <span class="grow">The last config change was put back ({time(info.last_revert.time)}): {info.last_revert.reason}.</span>
      <button class="btn ghost small" type="button" onclick={async () => { await dismissRevert(); await load(); }}>Dismiss</button>
    </div>
  {/if}
  {#if notice}
    <div class="notice {notice.kind}" role="status">
      <span class="grow">{notice.text}</span>
      {#if notice.kind === 'warn' && info?.restart_needed}<button class="btn" type="button" onclick={() => (restarting = 'ask')}><RotateCw size={16} strokeWidth={1.75} />Restart…</button>{/if}
      <button class="btn ghost small" type="button" onclick={() => (notice = null)}>Dismiss</button>
    </div>
  {/if}

  <Tabs tabs={TABS} active={tab} onselect={(t) => (tab = t)} strong />

  {#if !info}
    <div class="card pad muted">Loading…</div>
  {:else if tab === 'overview'}
    <div class="grid">
      <dl class="card pad">
        <dt>Version</dt><dd class="mono">{info.version}</dd>
        <dt>Platform</dt><dd class="mono">{info.platform}</dd>
        <dt>Process</dt><dd>pid <span class="mono">{info.pid}</span>, started {time(info.start_time)}</dd>
        <dt>Features</dt>
        <dd class="badges">{#each info.features as f (f)}<span class="badge">{f}</span>{:else}<span class="muted">none (barebones)</span>{/each}</dd>
      </dl>
      <dl class="card pad">
        <dt>API and UI</dt><dd class="mono">{info.http_address}</dd>
        <dt>Hub</dt><dd class="mono">{info.hub_address}</dd>
        <dt>Hub (TLS)</dt><dd class="mono">{info.hub_tls_address ?? '—'}</dd>
      </dl>
      <dl class="card pad">
        <dt>Config file</dt><dd class="mono path">{info.config_path}</dd>
        <dt>Editing</dt>
        <dd>
          {#if info.config_edit && !info.read_only_reason}on{:else}<span class="muted">off: {info.read_only_reason ?? 'set [http] config_edit = true in the file'}</span>{/if}
        </dd>
        <dt>Restart</dt><dd><span class="mono">{info.restart_mode}</span>{info.restart_mode === 'exit' ? ' (a service manager starts it again)' : ''}</dd>
        <dt>Applied</dt>
        <dd>{info.restart_needed ? 'the file changed since the start: a restart applies it' : 'the file is what the server runs with'}</dd>
      </dl>
    </div>
    <div>
      <button class="btn" type="button" onclick={() => (restarting = 'ask')}><RotateCw size={16} strokeWidth={1.75} />Restart server…</button>
    </div>
  {:else if view}
    <div class="config-bar">
      <div class="seg" role="group" aria-label="Edit as">
        <button type="button" class="btn ghost small" class:active={mode === 'forms'} aria-pressed={mode === 'forms'} onclick={() => (mode = 'forms')}>Forms</button>
        <button type="button" class="btn ghost small" class:active={mode === 'toml'} aria-pressed={mode === 'toml'} onclick={() => (mode = 'toml')}>TOML</button>
      </div>
      <span class="mono muted path">{view.path}</span>
      <span class="grow"></span>
      {#if !editable}
        <span class="muted small"><Lock size={14} strokeWidth={1.75} /> Read-only: {view.read_only_reason}</span>
      {:else}
        {#if dirty}<span class="badge accent">unsaved changes</span>{/if}
        <button class="btn" type="button" disabled={!dirty} onclick={discard}>Discard</button>
        <button class="btn primary" type="button" disabled={!dirty} onclick={review}><Save size={16} strokeWidth={1.75} />Review and save…</button>
      {/if}
    </div>
    {#if checkError}<div class="notice danger">{checkError}</div>{/if}
    {#if problems.length && mode === 'toml'}
      <div class="notice danger"><ul>{#each problems as p, i (i)}<li>{problemText(p)}</li>{/each}</ul></div>
    {/if}
    {#if checked?.locked.length}
      <div class="notice warn">Locked settings changed (they can only change in the file itself): {checked.locked.join(', ')}</div>
    {/if}

    {#if mode === 'forms'}
      <ConfigForms sections={view.sections} values={formValues} {problems} secret={view.secret} {editable} onchange={editForm} />
    {:else if view.text === null}
      <div class="notice danger">The file doesn’t parse as TOML, so its secrets can’t be hidden: fix it on the server.</div>
    {:else}
      <p class="muted small">Secrets show as <code>{view.secret}</code>: leave that as it is to keep them, or type a new value.</p>
      <CodeEditor value={text} language="toml" tall disabled={!editable} onchange={editText} />
    {/if}
  {/if}
</Page>

{#if reviewing}
  <Modal title="Review and save" size="lg" onclose={() => (reviewing = false)}>
    <div class="review">
      {#if reviewCheck?.problems.length}
        <div class="notice danger">Fix these first:<ul>{#each reviewCheck.problems as p, i (i)}<li>{problemText(p)}</li>{/each}</ul></div>
      {/if}
      {#if reviewCheck?.locked.length}
        <div class="notice danger">Locked settings changed; they can only be changed in the file itself: {reviewCheck.locked.join(', ')}</div>
      {/if}
      <div class="diff mono" aria-label="Changes">
        {#each diff as hunk, h (h)}
          {#if h > 0}<div class="gap">⋯</div>{/if}
          {#each hunk as line, i (i)}
            <div class="line {line.kind}"><span class="no">{line.kind === 'add' ? '' : line.old}</span><span class="no">{line.kind === 'del' ? '' : line.new}</span><span class="sign">{line.kind === 'add' ? '+' : line.kind === 'del' ? '−' : ' '}</span><span class="t">{line.text}</span></div>
          {/each}
        {:else}
          <p class="muted">No changes.</p>
        {/each}
      </div>
      <p class="muted small">Saved to the file (the old one kept as <code>.bak</code>). Most settings are read at start: a restart applies them.</p>
      {#if info?.step_up}
        <label class="field">
          Secret key (asked again for this change)
          <input class="input" type="password" autocomplete="off" bind:value={key} />
        </label>
      {/if}
      {#if saveError}
        <div class="notice danger">{saveError.text}{#if saveError.details.length}<ul>{#each saveError.details as d, i (i)}<li>{d}</li>{/each}</ul>{/if}</div>
      {/if}
      <div class="dialog-actions">
        <button class="btn" type="button" onclick={() => (reviewing = false)}>Cancel</button>
        <button
          class="btn primary"
          type="button"
          disabled={saving || !reviewCheck || reviewCheck.problems.length > 0 || reviewCheck.locked.length > 0 || (info?.step_up && !key)}
          onclick={save}
        >
          {saving ? 'Saving…' : 'Save'}
        </button>
      </div>
    </div>
  </Modal>
{/if}

{#if restarting}
  <Modal title="Restart the server" size="md" onclose={() => restarting === 'ask' && (restarting = null)}>
    {#if restarting === 'ask'}
      <div class="review">
        <p>The server stops gracefully and starts again{info?.restart_mode === 'exit' ? ' (it exits with status 75 for its service manager to start it)' : ''}. While it does:</p>
        <ul>
          <li>running scenes stop and start again; plugins get a shutdown first;</li>
          <li>events in memory and spill queues are lost (durable queues keep theirs);</li>
          <li>remote plugins reconnect.</li>
        </ul>
        {#if info?.restart_needed}
          <p class="notice warn">The config file changed since the start. After the restart, confirm the new config here within the time <code>[server] confirm_timeout_s</code> gives, or the previous one comes back.</p>
        {/if}
        <p class="muted small">Checked first: the file’s settings, its folders, and ports that changed.</p>
        {#if info?.step_up}
          <label class="field">
            Secret key
            <input class="input" type="password" autocomplete="off" bind:value={restartKey} />
          </label>
        {/if}
        {#if restartError}
          <div class="notice danger">{restartError.text}{#if restartError.details.length}<ul>{#each restartError.details as d, i (i)}<li>{d}</li>{/each}</ul>{/if}</div>
        {/if}
        <div class="dialog-actions">
          <button class="btn" type="button" onclick={() => (restarting = null)}>Cancel</button>
          <button class="btn primary" type="button" disabled={info?.step_up && !restartKey} onclick={restart}><RotateCw size={16} strokeWidth={1.75} />Restart</button>
        </div>
      </div>
    {:else}
      <div class="review">
        <p>Restarting… <span class="muted mono">{waitedS} s</span></p>
        {#if waitedS > 45}
          <p class="notice warn">
            No answer yet. If <code>[http]</code>'s host or port changed, open the server at its new address and confirm the config there before it’s put back. A server that can’t start
            puts the previous config back on its next start.
          </p>
        {/if}
      </div>
    {/if}
  </Modal>
{/if}

<style>
  .grow {
    flex: 1;
  }

  .pad {
    padding: var(--space-4);
  }

  .small {
    font-size: var(--text-sm);
  }

  .confirm {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
  }

  .notice {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
  }

  .notice ul {
    width: 100%;
    margin: var(--space-1) 0 0;
    padding-left: var(--space-4);
  }

  .key {
    width: 200px;
  }

  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(280px, 1fr));
    gap: var(--space-3);
  }

  dl {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: var(--space-2) var(--space-4);
    margin: 0;
    align-content: start;
  }

  dt {
    color: var(--text-muted);
  }

  dd {
    margin: 0;
    min-width: 0;
  }

  dd.badges {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-1);
  }

  .path {
    overflow-wrap: anywhere;
  }

  .config-bar {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-3);
  }

  .seg {
    display: flex;
    gap: 2px;
    padding: 2px;
    border: 1px solid var(--border);
    border-radius: var(--radius);
    background: var(--surface);
  }

  .seg .active {
    background: var(--accent-soft);
    color: var(--accent);
  }

  .review {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  .review ul {
    margin: 0;
    padding-left: var(--space-4);
  }

  .diff {
    max-height: 50vh;
    overflow: auto;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    font-size: var(--text-xs);
  }

  .line {
    display: grid;
    grid-template-columns: 3.5em 3.5em 1.5em 1fr;
    white-space: pre-wrap;
  }

  .line .no {
    padding-right: var(--space-2);
    color: var(--text-faint);
    text-align: right;
  }

  .line.add {
    background: var(--ok-soft);
  }

  .line.del {
    background: var(--danger-soft);
  }

  .gap {
    padding: 0 var(--space-2);
    color: var(--text-faint);
  }
</style>
