<script lang="ts">
  // A code field: CodeMirror 6, loaded only when a code field is shown (its
  // own chunk), with a textarea until then (and if loading fails). For Lua
  // with a `scope`: completion (Ctrl+Space, or as you type) and hover docs.
  // `toml` and `yaml`: highlighting only (the server config's text, documents).
  import { onDestroy } from 'svelte';
  import type { Scope } from '../../lib/lua/types';

  let {
    value,
    language,
    disabled = false,
    compact = false,
    tall = false,
    scope = null,
    params = null,
    onchange,
  }: {
    value: string;
    language: string;
    disabled?: boolean;
    compact?: boolean;
    /** A whole file: up to most of the window's height. */
    tall?: boolean;
    /** What the code sees (Lua): read at each completion, so it may change. */
    scope?: Scope | null;
    /** The code's function parameters (Lua): given, syntax errors are marked as you type. */
    params?: string[] | null;
    onchange: (value: string) => void;
  } = $props();

  let host: HTMLDivElement;
  let ready = $state(false);
  let view: { destroy(): void; state: { doc: { toString(): string } }; dispatch(t: unknown): void } | null = null;

  async function load() {
    const [{ EditorView, basicSetup }, { EditorState, StateField }, { StreamLanguage }, { lua }, { hoverTooltip, showTooltip }, { linter }, complete, { checkLua }] =
      await Promise.all([
        import('codemirror'),
        import('@codemirror/state'),
        import('@codemirror/language'),
        import('@codemirror/legacy-modes/mode/lua'),
        import('@codemirror/view'),
        import('@codemirror/lint'),
        import('../../lib/lua/complete'),
        import('../../lib/lua/check'),
      ]);
    const extensions = [
      basicSetup,
      EditorView.updateListener.of((update) => {
        if (update.docChanged) onchange(update.state.doc.toString());
      }),
      EditorState.readOnly.of(disabled),
      EditorView.theme({
        '&': { fontSize: 'var(--text-xs)', backgroundColor: 'var(--surface)', color: 'var(--text)' },
        '.cm-content': { fontFamily: 'var(--font-mono)', caretColor: 'var(--text)' },
        '.cm-gutters': { backgroundColor: 'var(--surface-2)', color: 'var(--text-faint)', border: 'none' },
        '.cm-activeLine, .cm-activeLineGutter': { backgroundColor: 'var(--surface-hover)' },
        '&.cm-focused': { outline: '2px solid var(--focus-ring)' },
        '.cm-scroller': { minHeight: compact ? '44px' : '160px', maxHeight: compact ? '200px' : tall ? '70vh' : '420px' },
        '.cm-tooltip': { backgroundColor: 'var(--surface)', color: 'var(--text)', border: '1px solid var(--border)', borderRadius: '6px' },
        '.cm-tooltip-autocomplete ul li[aria-selected]': { backgroundColor: 'var(--accent-soft)', color: 'var(--text)' },
        '.cm-completionDetail': { color: 'var(--text-muted)', fontStyle: 'normal', marginLeft: '0.75em' },
        '.cm-completionInfo': { maxWidth: '360px', whiteSpace: 'pre-wrap', fontSize: 'var(--text-xs)' },
        '.lua-hover': { maxWidth: '420px', padding: '6px 8px', fontSize: 'var(--text-xs)', whiteSpace: 'pre-wrap' },
        '.lua-hover code': { display: 'block', marginBottom: '4px', fontFamily: 'var(--font-mono)' },
        '.lua-signature strong': { color: 'var(--accent)' },
        '.cm-diagnostic-error': { borderLeftColor: 'var(--danger)' },
      }),
    ];
    if (language === 'toml') {
      const { toml } = await import('@codemirror/legacy-modes/mode/toml');
      extensions.push(StreamLanguage.define(toml));
    }
    if (language === 'yaml') {
      // YAML highlighting reads JSON well enough too (documents may be either).
      const { yaml } = await import('@codemirror/legacy-modes/mode/yaml');
      extensions.push(StreamLanguage.define(yaml));
    }
    if (language === 'lua') {
      extensions.push(StreamLanguage.define(lua));
      // basicSetup's autocompletion takes sources from the language data.
      extensions.push(EditorState.languageData.of(() => [{ autocomplete: complete.luaCompletions(() => scope) }]));
      // Signature help: the call the cursor is in, its current parameter in bold.
      const signature = (state: import('@codemirror/state').EditorState) => {
        const sel = state.selection.main;
        if (!scope || !sel.empty) return null;
        const hit = complete.signatureAt(scope, state.doc.toString(), sel.head);
        if (!hit) return null;
        return {
          pos: sel.head,
          above: true,
          strictSide: true,
          arrow: false,
          create() {
            const dom = document.createElement('div');
            dom.className = 'lua-hover lua-signature';
            const code = document.createElement('code');
            code.append(`${hit.name}(`);
            hit.sig.params.forEach((p, i) => {
              if (i) code.append(', ');
              const part = document.createElement(i === hit.active ? 'strong' : 'span');
              part.textContent = `${p.name}${p.optional ? '?' : ''}: ${p.type}`;
              code.append(part);
            });
            code.append(`)${hit.sig.returns.length ? `: ${hit.sig.returns.join(', ')}` : ''}`);
            dom.append(code);
            const doc = hit.sig.params[hit.active]?.doc ?? hit.doc;
            if (doc) {
              const body = document.createElement('div');
              body.textContent = doc;
              dom.append(body);
            }
            return { dom };
          },
        };
      };
      extensions.push(
        StateField.define({
          create: signature,
          update: (value, tr) => (tr.docChanged || tr.selection ? signature(tr.state) : value),
          provide: (f) => showTooltip.from(f),
        }),
      );
      if (params) {
        const names = params;
        extensions.push(linter((v) => checkLua(v.state.doc.toString(), names, v.state.doc), { delay: 700 }));
      }
      extensions.push(
        hoverTooltip((v, pos) => {
          if (!scope) return null;
          const hit = complete.hoverText(scope, v.state.doc.toString(), pos);
          if (!hit) return null;
          return {
            pos: hit.from,
            end: hit.to,
            above: true,
            create() {
              const dom = document.createElement('div');
              dom.className = 'lua-hover';
              const [head, ...rest] = hit.text.split('\n');
              const code = document.createElement('code');
              code.textContent = head;
              dom.append(code);
              if (rest.length) {
                const body = document.createElement('div');
                body.textContent = rest.join('\n');
                dom.append(body);
              }
              return { dom };
            },
          };
        }),
      );
    }
    if (!host) return;
    view = new EditorView({ parent: host, state: EditorState.create({ doc: value, extensions }) });
    ready = true;
  }

  load().catch((e) => console.error('code editor failed to load', e));

  // A value set from outside (e.g. reset) replaces the editor's text.
  $effect(() => {
    const next = value;
    if (view && view.state.doc.toString() !== next) {
      view.dispatch({ changes: { from: 0, to: view.state.doc.toString().length, insert: next } });
    }
  });

  onDestroy(() => view?.destroy());
</script>

<div class="editor" bind:this={host}>
  {#if !ready}
    <textarea class="input fallback" rows={compact ? 2 : 8} spellcheck="false" {disabled} {value} oninput={(e) => onchange(e.currentTarget.value)}></textarea>
  {/if}
</div>

<style>
  .editor {
    overflow: hidden;
    border: 1px solid var(--border);
    border-radius: var(--radius);
  }

  .fallback {
    width: 100%;
    height: auto;
    border: 0;
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    resize: vertical;
  }
</style>
