<script lang="ts">
  // A node's settings, saved to the scene's draft: its name, how it's
  // launched, its launch args as a form generated from the plugin's
  // launch-args schema (secrets set / replaced / cleared, uploads picked or
  // uploaded, code in an editor, configured fields read-only), its run-time
  // settings, and for remote nodes their identity and authentication.
  // Problems (a refused save's, else the draft check's) show next to their
  // fields.
  import { untrack } from 'svelte';
  import ChevronRight from '@lucide/svelte/icons/chevron-right';
  import Copy from '@lucide/svelte/icons/copy';
  import SchemaField from '../form/SchemaField.svelte';
  import { ApiError } from '../../lib/api';
  import {
    clone,
    getPath,
    labelOf,
    newToken,
    setPath,
    sortErrors,
    topFields,
    type FieldErrors,
    type Schema,
  } from '../../lib/schemaForm';
  import {
    getDefinitions,
    getNodeSchemas,
    putUpload,
    updateDraftNode,
    type GraphNode,
    type Plugin,
    type SchemaField as SchemaFieldTree,
    type Upload,
  } from '../../lib/sceneView';
  import { codeFieldScope } from '../../lib/lua/scope';
  import type { Scope } from '../../lib/lua/types';
  import { safeUploadName } from './uploads';

  let {
    sceneId,
    node,
    plugin,
    schema,
    uploads,
    problems,
    savedToken = null,
    justSaved = false,
    onsaved,
  }: {
    sceneId: string;
    node: GraphNode;
    plugin: Plugin | null;
    schema: Schema | null;
    uploads: Upload[];
    /** The draft check's (or live check's) lines about this node. */
    problems: string[];
    /** A token generated before the last save, still shown to copy. */
    savedToken?: string | null;
    /** The form was just re-created after a save. */
    justSaved?: boolean;
    onsaved: (saved: GraphNode, token: string | null) => void;
  } = $props();

  // ---- the form's state, from the node as it was when the form opened ----

  type Config = Record<string, unknown>;
  const original: Config = untrack(() => clone(node.config) ?? {});
  let name = $state(untrack(() => node.name ?? ''));
  let launch = $state<'local' | 'remote'>(
    (original.launch as 'local' | 'remote') ?? (untrack(() => plugin?.remote_only) ? 'remote' : 'local'),
  );
  let args = $state<Record<string, unknown>>(clone((original.launch_args as Record<string, unknown>) ?? {}));
  let runtime = $state<Record<string, unknown>>(clone((original.runtime as Record<string, unknown>) ?? {}));
  let remote = $state<Config>({
    remote_id: original.remote_id,
    auth: original.auth,
    require_tls: original.require_tls,
    tls: original.tls,
    configurator: original.configurator,
  });
  /** Secrets to replace (a value) or clear (`null`); others are kept. */
  let secretEdits = $state<Record<string, string | null>>({});
  /** A token generated in this form, shown until the form closes. */
  let token = $state<string | null>(null);
  let dirty = $state(false);

  const secrets = $derived(untrack(() => node.launch_args_secrets));
  const configured = $derived(launch === 'local' ? (plugin?.configured_fields ?? []) : []);
  const groups = $derived(schema ? topFields(schema, configured) : []);

  // ---- errors ----

  let saveErrors = $state<FieldErrors | null>(null);
  let saveMessage = $state<string | null>(untrack(() => (justSaved ? 'Saved to the draft.' : null)));
  const errors = $derived(saveErrors ?? sortErrors(problems, node.id));

  function touch() {
    dirty = true;
    saveMessage = null;
  }

  function setArg(key: string, value: unknown) {
    args = setPath(args, [key], value);
    touch();
  }

  function setRuntime(path: string[], value: unknown) {
    runtime = setPath(runtime, path, value);
    touch();
  }

  function setRemote(path: string[], value: unknown) {
    remote = setPath(remote, path, value);
    touch();
  }

  // ---- uploads for `"source": "upload"` fields ----

  let uploading = $state<string | null>(null);
  async function uploadFor(key: string, file: File | undefined) {
    if (!file) return;
    uploading = key;
    try {
      const saved = await putUpload(sceneId, node.id, safeUploadName(file.name), file);
      setArg(key, saved.name);
    } catch (e) {
      const error = e instanceof ApiError ? e : new ApiError(0, String(e));
      saveErrors = { ...errors, args: new Map(errors.args).set(key, [error.message, ...error.details]) };
    } finally {
      uploading = null;
    }
  }

  // ---- what code fields see (Lua completion) ----

  /** The plugin's definitions files, by path, once loaded. */
  let definitions = $state(new Map<string, string>());
  $effect(() => {
    if (!plugin || !schema) return;
    const paths = new Set<string>();
    const walk = (s: unknown) => {
      if (!s || typeof s !== 'object') return;
      const code = (s as Schema)['x-flwrs-code'];
      if (code?.definitions) paths.add(code.definitions);
      Object.values(s).forEach(walk);
    };
    walk(schema);
    const p = plugin;
    for (const path of paths) {
      getDefinitions(p.id, p.version, path).then((text) => {
        if (text !== null) definitions = new Map(untrack(() => definitions)).set(path, text);
      });
    }
  });

  /** The node's payload schemas for the settings as edited (`In` / `Out` in code). */
  let nodeSchemas = $state<{ in: SchemaFieldTree[] | null; out: SchemaFieldTree[] | null } | null>(null);
  let schemasTimer: ReturnType<typeof setTimeout> | undefined;
  $effect(() => {
    if (!plugin || !groups.some((g) => g.fields.some((f) => f.schema['x-flwrs-code']))) return;
    const snapshot = JSON.stringify(args);
    const id = plugin.id;
    clearTimeout(schemasTimer);
    schemasTimer = setTimeout(
      () => {
        getNodeSchemas(id, JSON.parse(snapshot))
          .then((s) => (nodeSchemas = s))
          .catch(() => {});
      },
      untrack(() => nodeSchemas) ? 800 : 0,
    );
    return () => clearTimeout(schemasTimer);
  });

  function codeScope(s: Schema): Scope | null {
    const code = s['x-flwrs-code'];
    if (!code || code.language !== 'lua') return null;
    return codeFieldScope(code, code.definitions ? definitions.get(code.definitions) : undefined, nodeSchemas);
  }

  // ---- save ----

  function buildConfig(): Config {
    let config: Config = { ...original, launch };
    const launchArgs: Record<string, unknown> = { ...args };
    for (const [key, value] of Object.entries(secretEdits)) launchArgs[key] = value;
    config = setPath(config, ['launch_args'], Object.keys(launchArgs).length ? launchArgs : undefined);
    config = setPath(config, ['runtime'], Object.keys(runtime).length ? runtime : undefined);
    for (const key of ['remote_id', 'auth', 'require_tls', 'tls', 'configurator', 'token']) delete config[key];
    if (launch === 'remote') {
      for (const [key, value] of Object.entries(remote)) if (value !== undefined && value !== '') config[key] = value;
      if (token) config.token = token;
    }
    return config;
  }

  let saving = $state(false);
  async function save(event: SubmitEvent) {
    event.preventDefault();
    saving = true;
    saveMessage = null;
    try {
      const saved = await updateDraftNode(sceneId, node.id, { name: name.trim() || null, plugin_id: node.plugin_id, config: buildConfig() });
      saveErrors = null;
      secretEdits = {};
      dirty = false;
      onsaved(saved, token);
    } catch (e) {
      const error = e instanceof ApiError ? e : new ApiError(0, String(e));
      const sorted = sortErrors(error.details);
      if (!error.details.length) sorted.other.push(error.message);
      else sorted.other.unshift(error.message);
      saveErrors = sorted;
    } finally {
      saving = false;
    }
  }

  // ---- run-time settings shown in the form (others are kept as they are) ----

  const RUNTIME: { path: string[]; label: string; hint?: string; type: 'number' | 'when_full' | 'tri' }[] = [
    { path: ['queue', 'capacity'], label: 'Queue capacity (events)', type: 'number' },
    { path: ['queue', 'max_bytes'], label: 'Queue size (bytes)', type: 'number' },
    { path: ['queue', 'when_full'], label: 'When the queue is full', type: 'when_full' },
    { path: ['queue', 'block_timeout_ms'], label: 'Block timeout (ms)', type: 'number' },
    { path: ['queue', 'durable'], label: 'Durable queue (survives a restart)', type: 'tri' },
    { path: ['queue', 'spill', 'enabled'], label: 'Spill to disk when memory is short', type: 'tri' },
    { path: ['delivery', 'max_attempts'], label: 'Delivery attempts', hint: 'then the event is dead-lettered', type: 'number' },
    { path: ['delivery', 'retry_delay_ms'], label: 'Retry delay (ms)', type: 'number' },
    { path: ['init_timeout_ms'], label: 'Start timeout (ms)', type: 'number' },
    { path: ['restart_delay_ms'], label: 'Restart delay (ms)', type: 'number' },
    { path: ['max_restarts'], label: 'Failed launches before giving up', type: 'number' },
    { path: ['reconnect_timeout_ms'], label: 'Reconnect timeout (ms)', type: 'number' },
    { path: ['shutdown_grace_ms'], label: 'Shutdown grace (ms)', type: 'number' },
    { path: ['kill_grace_ms'], label: 'Kill grace (ms)', type: 'number' },
  ];
  const runtimeFields = $derived(
    plugin?.takes_threads && launch === 'local'
      ? [{ path: ['threads'], label: 'Cores the plugin may use', hint: 'empty = the server’s default', type: 'number' as const }, ...RUNTIME]
      : RUNTIME,
  );

  const preview = (value: unknown) => (typeof value === 'string' ? value : JSON.stringify(value, null, 2));
  const fieldErrors = (key: string) => errors.args.get(key) ?? [];
  const copy = (text: string) => navigator.clipboard?.writeText(text).catch(() => {});
</script>

<form class="settings" onsubmit={save}>
  <div class="row">
    <label class="field grow">
      Name
      <input class="input" bind:value={name} maxlength="200" placeholder={plugin?.name ?? node.plugin_id} oninput={touch} />
    </label>
    <label class="field">
      Runs
      <select
        class="input"
        bind:value={launch}
        onchange={touch}
        title={plugin?.remote_only ? 'A remote plugin definition: nothing of it runs on this server' : undefined}
      >
        <option value="local" disabled={plugin?.remote_only}>Here (local)</option>
        <option value="remote">Elsewhere (remote)</option>
      </select>
    </label>
  </div>

  {#if errors.other.length}
    <div class="notice danger" role="alert">
      <ul>{#each errors.other as e, i (i)}<li>{e}</li>{/each}</ul>
    </div>
  {/if}

  {#if !plugin}
    <div class="notice warn">Plugin <code>{node.plugin_id}</code> isn’t installed on this server: its settings can’t be shown as a form.</div>
  {/if}

  {#if launch === 'remote'}
    <section>
      <h3>Remote plugin</h3>
      <p class="hint">Someone else starts this plugin, possibly on another machine; it connects to this server’s hub.</p>
      <label class="field">
        Remote id
        <input class="input" value={(remote.remote_id as string) ?? ''} placeholder={node.id} oninput={(e) => setRemote(['remote_id'], e.currentTarget.value || undefined)} />
        <span class="hint">What the plugin is started with (<code>--id</code>); defaults to the node id. Survives export and import.</span>
      </label>
      <label class="field">
        Authenticates with
        <select class="input" value={(remote.auth as string) ?? ''} onchange={(e) => setRemote(['auth'], e.currentTarget.value || undefined)}>
          <option value="">The scene’s default</option>
          <option value="token">A pre-shared token</option>
          <option value="mtls">A TLS client certificate (mTLS)</option>
        </select>
      </label>
      {#if remote.auth !== 'mtls'}
        <div class="field">
          Token
          {#if token ?? savedToken}
            {@const shown = (token ?? savedToken)!}
            <div class="token">
              <code>{shown}</code>
              <button class="btn ghost icon" type="button" title="Copy" aria-label="Copy the token" onclick={() => copy(shown)}><Copy size={14} strokeWidth={1.75} /></button>
            </div>
            <span class="hint">
              {token ? 'Saved with the node: copy it now, it isn’t shown again once this closes.' : 'Saved. Copy it now: it isn’t shown again once this closes.'}
              Start the plugin with it (<code>FLWRS_TOKEN</code> or <code>--token</code>).
            </span>
          {:else}
            <div class="row">
              <span class="badge {node.has_token ? 'ok' : 'warn'}">{node.has_token ? 'Set' : 'Not set'}</span>
              <button class="btn" type="button" onclick={() => { token = newToken(); touch(); }}>
                {node.has_token ? 'Generate a new token' : 'Generate a token'}
              </button>
            </div>
          {/if}
        </div>
        <label class="field">
          Must use TLS
          <select class="input" value={remote.require_tls === undefined ? '' : String(remote.require_tls)} onchange={(e) => setRemote(['require_tls'], e.currentTarget.value === '' ? undefined : e.currentTarget.value === 'true')}>
            <option value="">The scene’s default</option>
            <option value="true">Yes</option>
            <option value="false">No (plain TCP)</option>
          </select>
        </label>
      {:else}
        <label class="field">
          Certificate fingerprint (SHA-256)
          <input class="input mono" value={(getPath(remote, ['tls', 'client_cert_sha256']) as string) ?? ''} placeholder="Any certificate from the hub’s client CA naming the remote id" oninput={(e) => setRemote(['tls', 'client_cert_sha256'], e.currentTarget.value || undefined)} />
        </label>
      {/if}
      {#if (plugin?.configured_fields.length ?? 0) > 0}
        {@const endpoint = remote.configurator as Record<string, unknown> | undefined}
        {@const auth = endpoint?.auth as Record<string, unknown> | undefined}
        <fieldset class="endpoint">
          <legend>Configurator endpoint</legend>
          <label class="field">
            URL
            <input class="input" value={(endpoint?.url as string) ?? ''} placeholder="https://…/configure" oninput={(e) => {
              const url = e.currentTarget.value.trim();
              // No URL: no endpoint at all.
              if (url) setRemote(['configurator', 'url'], url);
              else setRemote(['configurator'], undefined);
            }} />
            <span class="hint">Where the remote side runs the plugin’s configurator (must be allowed by <code>[plugins] remote_configurator_urls</code>). Without one, set {plugin?.configured_fields.join(', ')} by hand.</span>
          </label>
          {#if endpoint?.url}
            <label class="field">
              The server authenticates with
              <select class="input" value={(auth?.type as string) ?? ''} onchange={(e) => {
                const type = e.currentTarget.value;
                setRemote(['configurator', 'auth'], type ? { type, ...(type === 'basic' ? { username: '' } : type === 'mtls' ? { cert: '', key: '' } : {}) } : undefined);
              }}>
                <option value="">Nothing</option>
                <option value="basic">A user name and password (basic)</option>
                <option value="bearer">A bearer token</option>
                <option value="mtls">A client certificate (mTLS; https only)</option>
              </select>
            </label>
            {#if auth?.type === 'basic'}
              <div class="row">
                <label class="field grow">User name<input class="input" value={(auth.username as string) ?? ''} oninput={(e) => setRemote(['configurator', 'auth', 'username'], e.currentTarget.value)} /></label>
                <label class="field grow">Password<input class="input" type="password" autocomplete="new-password" placeholder="Empty: keep the stored one" oninput={(e) => setRemote(['configurator', 'auth', 'password'], e.currentTarget.value || undefined)} /></label>
              </div>
            {:else if auth?.type === 'bearer'}
              <label class="field">Token<input class="input" type="password" autocomplete="new-password" placeholder="Empty: keep the stored one" oninput={(e) => setRemote(['configurator', 'auth', 'token'], e.currentTarget.value || undefined)} /></label>
            {:else if auth?.type === 'mtls'}
              <div class="row">
                {#each [['cert', 'Certificate (PEM)'], ['key', 'Key (PEM)']] as [part, label] (part)}
                  <label class="field grow">
                    {label}
                    <select class="input" value={(auth[part] as string) ?? ''} onchange={(e) => setRemote(['configurator', 'auth', part], e.currentTarget.value)}>
                      <option value="">Pick an uploaded file</option>
                      {#each uploads as u (u.name)}<option value={u.name}>{u.name}</option>{/each}
                    </select>
                  </label>
                {/each}
              </div>
            {/if}
            <label class="field">
              The endpoint’s CA
              <select class="input" value={(endpoint.ca as string) ?? ''} onchange={(e) => setRemote(['configurator', 'ca'], e.currentTarget.value || undefined)}>
                <option value="">The bundled public roots</option>
                {#each uploads as u (u.name)}<option value={u.name}>{u.name}</option>{/each}
              </select>
              <span class="hint">Files come from the node’s uploads (the Uploads tab).</span>
            </label>
            <label class="check">
              <input type="checkbox" checked={endpoint.send_secrets === true} onchange={(e) => setRemote(['configurator', 'send_secrets'], e.currentTarget.checked || undefined)} />
              Also send the node’s secret settings to the endpoint
            </label>
          {/if}
        </fieldset>
      {/if}
      {#each errors.remote as e, i (i)}<p class="error">{e}</p>{/each}
    </section>
  {/if}

  {#if schema}
    {#each groups as group (`${group.advanced}:${group.group}`)}
      {@const body = group.fields}
      <details class="section" open={!group.advanced || body.some((f) => fieldErrors(f.key).length)}>
        <summary><ChevronRight size={14} strokeWidth={2} />{group.group ?? (group.advanced ? 'Advanced' : 'Settings')}</summary>
        <div class="fields">
          {#each body as field (field.key)}
            {#if field.secret}
              {@const isSet = secrets[`/${field.key}`]}
              {@const edit = secretEdits[field.key]}
              <div class="field">
                <span>{labelOf(field.key, field.schema)}{field.required ? ' *' : ''} <span class="badge">secret</span></span>
                {#if edit !== undefined && edit !== null}
                  <div class="row">
                    <input class="input grow" type="password" autocomplete="new-password" value={edit} oninput={(e) => { secretEdits = { ...secretEdits, [field.key]: e.currentTarget.value }; touch(); }} />
                    <button class="btn ghost" type="button" onclick={() => { const { [field.key]: _, ...rest } = secretEdits; secretEdits = rest; }}>Keep</button>
                  </div>
                {:else}
                  <div class="row">
                    <span class="badge {edit === null ? 'warn' : isSet ? 'ok' : ''}">{edit === null ? 'Will be cleared' : isSet ? 'Set' : 'Not set'}</span>
                    <button class="btn" type="button" onclick={() => { secretEdits = { ...secretEdits, [field.key]: '' }; touch(); }}>{isSet ? 'Replace' : 'Set'}</button>
                    {#if isSet && edit !== null}
                      <button class="btn ghost" type="button" onclick={() => { secretEdits = { ...secretEdits, [field.key]: null }; touch(); }}>Clear</button>
                    {/if}
                  </div>
                {/if}
                {#if field.schema.description}<span class="hint">{field.schema.description}</span>{/if}
                {#each fieldErrors(field.key) as e, i (i)}<span class="error">{e}</span>{/each}
              </div>
            {:else if field.configured}
              <div class="field">
                <span>{labelOf(field.key, field.schema)} <span class="badge accent">configured</span></span>
                {#if args[field.key] !== undefined}
                  <pre class="preview">{preview(args[field.key])}</pre>
                {:else}
                  <span class="muted small">Not set yet: the plugin’s configurator sets it when the node is saved.</span>
                {/if}
                <span class="hint">Set by the plugin’s configurator from the other settings.</span>
                {#each fieldErrors(field.key) as e, i (i)}<span class="error">{e}</span>{/each}
              </div>
            {:else if field.upload}
              <div class="field">
                <label for={`up-${field.key}`}>{labelOf(field.key, field.schema)}{field.required ? ' *' : ''} <span class="badge">file</span></label>
                <div class="row">
                  <select id={`up-${field.key}`} class="input grow" value={(args[field.key] as string) ?? ''} onchange={(e) => setArg(field.key, e.currentTarget.value || undefined)}>
                    <option value="">{field.required ? 'Pick an uploaded file' : 'None'}</option>
                    {#each uploads as upload (upload.name)}
                      <option value={upload.name}>{upload.name}</option>
                    {/each}
                    {#if typeof args[field.key] === 'string' && !uploads.some((u) => u.name === args[field.key])}
                      <option value={args[field.key] as string}>{args[field.key]} (missing)</option>
                    {/if}
                  </select>
                  <label class="btn">
                    {uploading === field.key ? 'Uploading…' : 'Upload…'}
                    <input type="file" hidden disabled={uploading !== null} onchange={(e) => uploadFor(field.key, e.currentTarget.files?.[0])} />
                  </label>
                </div>
                {#if field.schema.description}<span class="hint">{field.schema.description}</span>{/if}
                {#each fieldErrors(field.key) as e, i (i)}<span class="error">{e}</span>{/each}
              </div>
            {:else}
              <SchemaField
                root={schema}
                schema={field.schema}
                name={field.key}
                value={args[field.key]}
                required={field.required}
                errors={fieldErrors(field.key)}
                {codeScope}
                onchange={(v) => setArg(field.key, v)}
              />
            {/if}
          {/each}
        </div>
      </details>
    {/each}
    {#each [...errors.args.entries()].filter(([key]) => !schema?.properties?.[key]) as [key, list] (key)}
      {#each list as e, i (i)}<p class="error">{e}</p>{/each}
    {/each}
  {:else if plugin && !plugin.has_launch_args_schema}
    <p class="muted small">This plugin takes no settings.</p>
  {/if}

  <details class="section" open={errors.runtime.length > 0}>
    <summary><ChevronRight size={14} strokeWidth={2} />Run-time (queue, delivery, restarts)</summary>
    <div class="fields grid">
      {#each runtimeFields as f (f.path.join('.'))}
        {@const value = getPath(runtime, f.path)}
        <label class="field">
          {f.label}
          {#if f.type === 'number'}
            <input class="input" type="number" min="0" placeholder="Default" value={typeof value === 'number' ? value : ''} oninput={(e) => setRuntime(f.path, e.currentTarget.value === '' ? undefined : Number(e.currentTarget.value))} />
          {:else if f.type === 'when_full'}
            <select class="input" value={(value as string) ?? ''} onchange={(e) => setRuntime(f.path, e.currentTarget.value || undefined)}>
              <option value="">Default (block; drop newest on a cycle)</option>
              <option value="block">Block the producer</option>
              <option value="drop_newest">Drop the newest</option>
              <option value="drop_oldest">Drop the oldest</option>
            </select>
          {:else}
            <select class="input" value={value === undefined ? '' : String(value)} onchange={(e) => setRuntime(f.path, e.currentTarget.value === '' ? undefined : e.currentTarget.value === 'true')}>
              <option value="">Default</option>
              <option value="true">On</option>
              <option value="false">Off</option>
            </select>
          {/if}
          {#if f.hint}<span class="hint">{f.hint}</span>{/if}
        </label>
      {/each}
    </div>
    {#each errors.runtime as e, i (i)}<p class="error">{e}</p>{/each}
  </details>

  <div class="actions">
    {#if saveMessage}<span class="muted small">{saveMessage}</span>{/if}
    {#if dirty}<span class="muted small">Unsaved changes</span>{/if}
    <button class="btn primary" type="submit" disabled={saving || !dirty}>{saving ? 'Saving…' : 'Save to draft'}</button>
  </div>
</form>

<style>
  .settings {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  .row {
    display: flex;
    align-items: flex-end;
    gap: var(--space-2);
  }

  .grow {
    flex: 1;
    min-width: 0;
  }

  section {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    padding: var(--space-3);
    border: 1px solid var(--border);
    border-radius: var(--radius);
  }

  h3 {
    margin: 0;
    font-size: var(--text-sm);
  }

  .section {
    border: 1px solid var(--border);
    border-radius: var(--radius);
  }

  summary {
    display: flex;
    align-items: center;
    gap: var(--space-1);
    padding: var(--space-2) var(--space-3);
    font-size: var(--text-sm);
    font-weight: 600;
    cursor: pointer;
    list-style: none;
  }

  summary::-webkit-details-marker {
    display: none;
  }

  summary :global(svg) {
    transition: transform var(--dur-fast) var(--ease);
  }

  details[open] > summary :global(svg) {
    transform: rotate(90deg);
  }

  .fields {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    padding: 0 var(--space-3) var(--space-3);
  }

  .fields.grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(200px, 1fr));
  }

  .hint {
    color: var(--text-muted);
    font-size: var(--text-xs);
    font-weight: 400;
  }

  .error {
    margin: 0;
    color: var(--danger);
    font-size: var(--text-xs);
    font-weight: 400;
  }

  .small {
    font-size: var(--text-sm);
  }

  .preview {
    max-height: 160px;
    margin: 0;
    padding: var(--space-2);
    overflow: auto;
    border-radius: var(--radius-sm);
    background: var(--surface-2);
    font-size: var(--text-xs);
  }

  .endpoint {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    margin: 0;
    padding: var(--space-3);
    border: 1px solid var(--border);
    border-radius: var(--radius);
  }

  .endpoint legend {
    padding: 0 var(--space-1);
    font-size: var(--text-sm);
    font-weight: 500;
  }

  .token {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-1) var(--space-2);
    border-radius: var(--radius-sm);
    background: var(--warn-soft);
  }

  .token code {
    flex: 1;
    overflow-wrap: anywhere;
  }

  .actions {
    position: sticky;
    bottom: calc(-1 * var(--space-5));
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: var(--space-3);
    padding: var(--space-3) 0;
    background: var(--surface);
  }

  ul {
    margin: 0;
    padding-left: var(--space-4);
  }
</style>
