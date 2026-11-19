# Idea: longer history in the web UI, from the telemetry and log files

Status: idea, not scheduled. Noted while building the web UI's overview page.

## Today

The overview page's charts come from the live status stream only: the browser keeps the samples it receives (up to an hour, windows 1m–1h, zoom down to 30 s) and starts empty on each visit. A node's logs are an in-memory ring of its last 500 records. Nothing older is shown, though the server can already write both somewhere durable:

- metrics: `[[telemetry.file]]` sinks (SQLite, OTLP JSON Lines, InfluxDB line protocol, OpenMetrics), partitioned by time with retention;
- logs: the server log's `FILE` sink (rotating files), which also carries plugins' logs and output.

## The idea

Let the UI read those files back, so its charts and log views reach further than the page's own session:

- A query API over the telemetry files (most naturally the SQLite format: one database per partition, a fixed schema with label columns): series by name and labels over a time range, downsampled to the chart's width. The overview's longer windows (6h, 24h, 7d…) and the scene and node stats would use it; the live stream fills in the latest samples on top.
- Per-node log history: search the server's log files (or a structured per-node log file, if one is added) by node, time range and level, for the node modal's Logs tab beyond the in-memory ring.

## Questions

- Which telemetry format to query (SQLite is queryable as is; the text formats would need an index or a scan), and what happens when no file sink is configured (the UI offers only the live windows, as now).
- Downsampling on the server (min / max / mean per bucket) so long ranges stay small.
- Logs: a structured (JSON Lines) per-node file would make filtering simple; the plain server log would need parsing.
- Limits: query ranges and result sizes, and access (the same API auth).
