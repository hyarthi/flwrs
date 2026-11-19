# Idea: HTTP/3 and WebTransport for the HTTP plugins

Status: deferred. Bi-directional streaming is covered today by the WS source and the WS push sink (and WebSockets over HTTP/2).

## Sketch

- A cargo feature `http3` on the kit's server core (`quinn` + `h3` + `h3-quinn`): a UDP listener next to TCP, `Alt-Svc` advertised on HTTP/1.1 and HTTP/2 answers, TLS 1.3 with ALPN `h3` from the same certificate uploads.
- The HTTP source and the HTTP push sink get it unchanged (SSE works over HTTP/3 as is).
- WebTransport later, on top (`h3-webtransport`), once browsers and the crates settle: a transport for the push sinks and a bi-directional source.

## Costs and risks

- `h3` is pre-1.0 and marked experimental; `h3-webtransport` more so.
- A bigger binary and more memory per connection: the likeliest reason for a first edge / data-centre split of a plugin.
- Firewalls and load balancers often block or don't proxy UDP 443.
