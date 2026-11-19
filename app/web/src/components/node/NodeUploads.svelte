<script lang="ts">
  // The files uploaded to a node (what its `"source": "upload"` settings
  // name): upload, replace, download, delete. Changes go to the draft.
  import Upload from '@lucide/svelte/icons/upload';
  import Download from '@lucide/svelte/icons/download';
  import Trash from '@lucide/svelte/icons/trash-2';
  import RefreshCw from '@lucide/svelte/icons/refresh-cw';
  import { ApiError } from '../../lib/api';
  import { deleteUpload, putUpload, uploadContentUrl, type Upload as UploadRow } from '../../lib/sceneView';
  import { formatBytes, safeUploadName } from './uploads';

  let {
    sceneId,
    nodeId,
    uploads,
    inDraft,
    usedBy,
    onchanged,
  }: {
    sceneId: string;
    nodeId: string;
    uploads: UploadRow[];
    inDraft: boolean;
    /** Upload name → the settings that name it. */
    usedBy: Map<string, string[]>;
    onchanged: () => void;
  } = $props();

  let error = $state<ApiError | null>(null);
  let busy = $state<string | null>(null);
  let picker: HTMLInputElement;
  let replacing: string | null = null;

  async function send(name: string, file: File) {
    busy = name;
    error = null;
    try {
      await putUpload(sceneId, nodeId, name, file);
      onchanged();
    } catch (e) {
      error = e instanceof ApiError ? e : new ApiError(0, String(e));
    } finally {
      busy = null;
    }
  }

  async function picked(files: FileList | null) {
    const list = [...(files ?? [])];
    const target = replacing;
    replacing = null;
    if (picker) picker.value = '';
    for (const file of list) await send(target ?? safeUploadName(file.name), file);
  }

  async function remove(name: string) {
    busy = name;
    error = null;
    try {
      await deleteUpload(sceneId, nodeId, name);
      onchanged();
    } catch (e) {
      error = e instanceof ApiError ? e : new ApiError(0, String(e));
    } finally {
      busy = null;
    }
  }
</script>

<div class="uploads">
  <div class="head">
    <p class="muted small">Files the node’s settings can name (they reach the plugin in its <code>config/</code> folder). Changes go to the draft.</p>
    <button class="btn" type="button" disabled={busy !== null} onclick={() => { replacing = null; picker.click(); }}>
      <Upload size={16} strokeWidth={1.75} />Upload
    </button>
  </div>
  <input bind:this={picker} type="file" multiple hidden onchange={(e) => picked(e.currentTarget.files)} />

  {#if error}
    <div class="notice danger" role="alert">
      <div>
        {error.message}
        {#if error.details.length}<ul>{#each error.details as d, i (i)}<li>{d}</li>{/each}</ul>{/if}
      </div>
    </div>
  {/if}

  {#if uploads.length === 0}
    <p class="muted small empty">No files yet.</p>
  {:else}
    <table class="table">
      <thead>
        <tr><th>Name</th><th>Size</th><th>Used by</th><th><span class="sr-only">Actions</span></th></tr>
      </thead>
      <tbody>
        {#each uploads as upload (upload.name)}
          <tr>
            <td>
              <span class="mono name">{upload.name}</span>
              <div class="mono muted" title={upload.sha256}>sha256 {upload.sha256.slice(0, 12)}…</div>
            </td>
            <td class="muted">{formatBytes(upload.size)}</td>
            <td class="muted">{usedBy.get(upload.name)?.join(', ') ?? '—'}</td>
            <td class="actions">
              <a class="btn ghost icon" href={uploadContentUrl(sceneId, nodeId, upload.name, inDraft)} download={upload.name} title="Download (refused for a secret setting’s file)" aria-label={`Download ${upload.name}`}>
                <Download size={16} strokeWidth={1.75} />
              </a>
              <button class="btn ghost icon" type="button" disabled={busy !== null} title="Replace" aria-label={`Replace ${upload.name}`} onclick={() => { replacing = upload.name; picker.click(); }}>
                <RefreshCw size={16} strokeWidth={1.75} />
              </button>
              <button class="btn ghost icon" type="button" disabled={busy !== null || usedBy.has(upload.name)} title={usedBy.has(upload.name) ? 'Still named by the settings' : 'Delete'} aria-label={`Delete ${upload.name}`} onclick={() => remove(upload.name)}>
                <Trash size={16} strokeWidth={1.75} />
              </button>
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</div>

<style>
  .uploads {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
  }

  .small {
    font-size: var(--text-sm);
  }

  .empty {
    padding: var(--space-4) 0;
    text-align: center;
  }

  .name {
    font-size: var(--text-sm);
  }

  .actions {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-1);
  }

  ul {
    margin: var(--space-1) 0 0;
    padding-left: var(--space-4);
  }
</style>
