<script lang="ts">
  import logo from '../assets/logo.svg';
  import { ApiError } from '../lib/api';
  import { session } from '../lib/session.svelte';

  let key = $state('');
  let busy = $state(false);
  let error = $state<string | null>(null);

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    if (!key || busy) return;
    busy = true;
    error = null;
    try {
      // On success App.svelte takes the user on to `?next=`.
      await session.login(key);
      key = '';
    } catch (e) {
      error =
        e instanceof ApiError && e.status === 401
          ? 'That key is not right.'
          : e instanceof Error
            ? e.message
            : String(e);
    } finally {
      busy = false;
    }
  }
</script>

<div class="login">
  <form class="card panel" onsubmit={submit}>
    <img src={logo} alt="" width="48" height="48" />
    <h1>Log in to flwrs</h1>
    <label class="field">
      <span>Secret key</span>
      <!-- svelte-ignore a11y_autofocus -->
      <input
        class="input"
        type="password"
        name="key"
        autocomplete="current-password"
        autofocus
        required
        bind:value={key}
        aria-invalid={error ? 'true' : undefined}
        aria-describedby={error ? 'login-error' : undefined}
      />
    </label>
    {#if error}
      <p class="error" id="login-error" role="alert">{error}</p>
    {/if}
    <button class="btn primary" type="submit" disabled={busy || !key}>
      {busy ? 'Logging in…' : 'Log in'}
    </button>
    <p class="hint muted">The key is set in the server's config: <code>[http.auth] secret_key</code>.</p>
  </form>
</div>

<style>
  .login {
    display: grid;
    place-items: center;
    height: 100%;
    padding: var(--space-5);
  }

  .panel {
    display: flex;
    flex-direction: column;
    align-items: stretch;
    gap: var(--space-4);
    width: min(360px, 100%);
    padding: var(--space-6) var(--space-5);
  }

  img {
    align-self: center;
    border-radius: 10px;
  }

  h1 {
    text-align: center;
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    font-size: var(--text-sm);
    font-weight: 500;
  }

  .btn {
    justify-content: center;
  }

  .error {
    color: var(--danger);
    font-size: var(--text-sm);
  }

  .hint {
    font-size: var(--text-xs);
    text-align: center;
  }
</style>
