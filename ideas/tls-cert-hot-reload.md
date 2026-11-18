# Idea: reloading TLS certificates without a restart

Status: idea, not scheduled. Today the hub's `[hub.tls]` certificate and the kit server's `tls_cert` / `tls_key` are read once at start.

## Problem

Short-lived certificates (ACME / Let's Encrypt: 90 days, soon shorter; internal CAs with days) need a restart to rotate. For the hub that restarts the server; for a plugin it restarts the node (dropping its connections).

## Sketch

- rustls's `ResolvesServerCert`: a resolver holding an `ArcSwap` of the certified key; new handshakes use the new one, open connections keep theirs.
- **Hub:** watch the files (mtime polled every minute; no `notify` dependency needed), reload on change, keep the old one if the new files don't parse (ERROR, rate-limited).
- **Plugins:** a changed upload changes the node's settings, so the director restarts the node today. A plugin could instead watch its `config/` file and swap, if the director could tell "only an upload's content changed" (a manifest flag `reloads: ["tls_cert", "tls_key"]`).
- Client CA bundles (`client_ca`, `tls_client_ca`) the same way.
