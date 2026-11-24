<script lang="ts">
  // How to work with the scene editor: the info button in the canvas
  // controls (or `?`) opens it.
  import Modal from '../Modal.svelte';
  import Cog from '@lucide/svelte/icons/cog';
  import Funnel from '@lucide/svelte/icons/funnel';
  import ArrowRightLeft from '@lucide/svelte/icons/arrow-right-left';
  import MemoryStick from '@lucide/svelte/icons/memory-stick';
  import HardDriveDownload from '@lucide/svelte/icons/hard-drive-download';
  import Database from '@lucide/svelte/icons/database';
  import Maximize from '@lucide/svelte/icons/maximize';

  let { onclose }: { onclose: () => void } = $props();

  const mac = typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);
  const mod = mac ? '⌘' : 'Ctrl';
</script>

<Modal title="Working with the scene editor" {onclose} size="md">
  <div class="help">
    <section>
      <h3>Nodes</h3>
      <dl>
        <dt>Add</dt>
        <dd>Drag a plugin from the palette onto the canvas, or click it to place it in the middle.</dd>
        <dt>Move</dt>
        <dd>Drag it (it snaps to the grid).</dd>
        <dt>Settings, uploads, stats, logs</dt>
        <dd>Double-click the node.</dd>
        <dt>Select several</dt>
        <dd><kbd>Shift</kbd> + drag a box on the canvas, or <kbd>{mac ? '⌘' : 'Ctrl'}</kbd> + click each.</dd>
        <dt>Remove</dt>
        <dd>Select it and press <kbd>Backspace</kbd> or <kbd>Delete</kbd>. It stays as a shadow with a restore button until the draft is submitted.</dd>
        <dt>Copy, paste, duplicate</dt>
        <dd><kbd>{mod} C</kbd>, <kbd>{mod} V</kbd>, <kbd>{mod} D</kbd>: the selected nodes, with the edges between them.</dd>
      </dl>
    </section>

    <section>
      <h3>Edges</h3>
      <dl>
        <dt>Connect</dt>
        <dd>Drag from a node’s right-hand dot (what it sends) to another’s left-hand dot (what it takes). Sources take nothing and sinks send nothing; there’s at most one edge each way between two nodes.</dd>
        <dt>The box in the middle</dt>
        <dd>
          <span class="icon"><Cog size={13} strokeWidth={2} /></span> opens the edge’s mapping: rules or a Lua script, a filter, and a test with sample events.
          <span class="icon"><Funnel size={12} strokeWidth={2} /></span> means it filters events,
          <span class="icon"><ArrowRightLeft size={12} strokeWidth={2} /></span> that it maps them.
        </dd>
        <dt>Bend it</dt>
        <dd>Drag the box: the edge passes through it. Double-click the box to put it back where it’s placed automatically.</dd>
        <dt>Under the box</dt>
        <dd>
          The receiving node’s queue:
          <span class="icon"><MemoryStick size={12} strokeWidth={2} /></span> in memory,
          <span class="icon"><HardDriveDownload size={12} strokeWidth={2} /></span> spilling to disk when memory is short,
          <span class="icon"><Database size={12} strokeWidth={2} /></span> durable (kept on disk until delivered). It’s set in that node’s run-time settings.
        </dd>
        <dt>Remove</dt>
        <dd>Click the line to select it, then <kbd>Backspace</kbd> or <kbd>Delete</kbd>.</dd>
      </dl>
    </section>

    <section>
      <h3>Groups</h3>
      <dl>
        <dt>Create and edit</dt>
        <dd>In the scene’s Settings. Chips on a node name its groups; hover one, or a legend entry, to pick out the members.</dd>
        <dt>Join</dt>
        <dd>Drop a node on a frame, or tick the group in the node’s settings.</dd>
        <dt>Leave</dt>
        <dd>Right-click the node, or <kbd>{mac ? '⌥' : 'Alt'}</kbd> + drag it out of the frame, or untick it in its settings. A plain drag only stretches the outline.</dd>
      </dl>
    </section>

    <section>
      <h3>The canvas</h3>
      <dl>
        <dt>Pan and zoom</dt>
        <dd>Drag the empty canvas to pan; scroll or pinch to zoom (or the + and − buttons). <span class="icon"><Maximize size={12} strokeWidth={2} /></span> fits everything in view; the map in the corner shows where you are.</dd>
      </dl>
    </section>

    <section>
      <h3>Drafts</h3>
      <dl>
        <dt>Editing</dt>
        <dd>Every change goes to the scene’s draft: what runs (live) stays as it is until you <strong>Submit</strong>. <strong>Review</strong> shows the changes and the checks, <strong>Discard</strong> drops them.</dd>
        <dt>Undo, redo</dt>
        <dd><kbd>{mod} Z</kbd> undoes; <kbd>{mac ? '⇧⌘ Z' : 'Ctrl Shift Z'}</kbd> or <kbd>{mod} Y</kbd> redoes (or the arrows in the bar).</dd>
        <dt>Markers</dt>
        <dd>Nodes and edges show what the draft adds (<span class="badge accent">new</span>), changes or removes (a shadow with a restore button). A conflict with live is marked in the warning colour: <strong>Rebase</strong> in the bar brings live’s changes in, and Review resolves conflicts.</dd>
      </dl>
    </section>

    <section>
      <h3>While the scene runs</h3>
      <dl>
        <dt>Nodes</dt>
        <dd>A green glow means it’s connected, yellow that it’s starting or reconnecting, red that it’s down.</dd>
        <dt>Edges</dt>
        <dd>Dots travel along an edge as events flow: green when the receiving queue keeps up, yellow when it spills to disk, orange when it blocks or drops. A dot leaving the box upwards was filtered out, downwards was dropped.</dd>
      </dl>
    </section>

    <p class="muted small">Press <kbd>?</kbd> on the canvas to open this again.</p>
  </div>
</Modal>

<style>
  .help {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
    font-size: var(--text-sm);
  }

  h3 {
    margin: 0 0 var(--space-2);
    font-size: var(--text-sm);
    font-weight: 600;
  }

  dl {
    display: grid;
    grid-template-columns: minmax(120px, 11rem) 1fr;
    gap: var(--space-2) var(--space-3);
    margin: 0;
  }

  dt {
    color: var(--text-muted);
    font-weight: 500;
  }

  dd {
    margin: 0;
  }

  kbd {
    padding: 1px 5px;
    border: 1px solid var(--border);
    border-bottom-width: 2px;
    border-radius: var(--radius-sm);
    background: var(--surface-2);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    white-space: nowrap;
  }

  .icon {
    display: inline-flex;
    vertical-align: -2px;
    color: var(--text-muted);
  }

  .small {
    font-size: var(--text-xs);
  }

  p {
    margin: 0;
  }

  @media (max-width: 560px) {
    dl {
      grid-template-columns: 1fr;
      gap: var(--space-1);
    }

    dd {
      margin-bottom: var(--space-2);
    }
  }
</style>
