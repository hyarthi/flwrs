<script lang="ts">
  // An on/off switch (a checkbox underneath, so keyboard and forms work).
  let {
    checked,
    label,
    disabled = false,
    busy = false,
    onchange,
  }: {
    checked: boolean;
    /** For screen readers (and the tooltip). */
    label: string;
    disabled?: boolean;
    /** A change is under way: shown, and further clicks ignored. */
    busy?: boolean;
    onchange: (checked: boolean) => void;
  } = $props();
</script>

<label class="switch" class:busy title={label}>
  <input
    type="checkbox"
    role="switch"
    aria-label={label}
    {checked}
    disabled={disabled || busy}
    onchange={(e) => onchange(e.currentTarget.checked)}
  />
  <span class="track" aria-hidden="true"><span class="thumb"></span></span>
</label>

<style>
  .switch {
    position: relative;
    display: inline-flex;
    cursor: pointer;
  }

  input {
    position: absolute;
    inset: 0;
    margin: 0;
    opacity: 0;
    cursor: inherit;
  }

  .track {
    display: inline-flex;
    width: 32px;
    height: 18px;
    padding: 2px;
    border-radius: 999px;
    background: var(--border-strong);
    transition: background var(--dur) var(--ease);
  }

  .thumb {
    width: 14px;
    height: 14px;
    border-radius: 50%;
    background: var(--surface);
    box-shadow: var(--shadow-sm);
    transition: transform var(--dur) var(--ease);
  }

  input:checked + .track {
    background: var(--accent);
  }

  input:checked + .track .thumb {
    transform: translateX(14px);
  }

  input:focus-visible + .track {
    outline: 2px solid var(--focus-ring);
    outline-offset: 2px;
  }

  input:disabled {
    cursor: not-allowed;
  }

  input:disabled + .track {
    opacity: 0.5;
  }

  .busy .track {
    opacity: 0.7;
  }
</style>
