# Idea: per-channel authorization in the push sinks

Status: idea, not scheduled (phase 8, Nov 2026: auth mirrors the other plugins for now: who may connect, not what they may read).

## Problem

Any authenticated subscriber can read any allowed channel. A tenant per channel (`tenants/acme/...`) needs subscribers limited to their own channels.

## Sketch

- A `channel_access` setting: principal (the token's label or the certificate's name) → channel patterns, e.g. `{"acme": ["tenants/acme/**"], "ops": ["**"]}`; refused subscriptions get 403.
- Patterns could use the principal itself: `tenants/${principal}/**`.
- Later: a JWT bearer mode whose claims carry the channels.
