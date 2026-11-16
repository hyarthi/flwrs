# ticker (`flwrs-ticker`)

A source that emits a tick at an interval or on a cron schedule. Use it to start workflows on a timer (polls, reports, housekeeping) or as a steady test input for other nodes.

## Launch args

`launch_args_style: flags`. Every key is optional; the default is a tick every second.

| Key | Default | Meaning |
|---|---|---|
| `interval` | `"1s"` | Tick every this long: whole numbers with `ms`, `s`, `m`, `h`, `d` (`500ms`, `1h30m`). At least `10ms`. Not with `cron`. |
| `cron` | | Tick on a cron pattern: 5 fields (`minute hour day-of-month month day-of-week`), or 6 with seconds first, or a nickname (`@hourly`, `@daily`, …). Syntax: [croner](https://docs.rs/croner) (`L`, `#`, `W` and `+` are supported). |
| `timezone` | `"UTC"` | IANA time zone for `cron`, e.g. `Europe/Berlin`. Only with `cron`. |
| `align` | `false` | Put interval ticks on wall-clock multiples of the interval since 1970-01-01 UTC (`1m` ticks at :00, `1h` at the full hour, `1d` at midnight UTC). Without it, ticks count from the start. Not with `cron`. |
| `fire_on_start` | `false` | Also tick at the start: when the plugin starts, or at `start_time`. |
| `jitter` | none | Delay each tick by a random amount below this, e.g. so that many tickers don't hit a service at the same instant. Must be shorter than `interval`. `tick_time` stays the scheduled time. |
| `start_time` | | No ticks before this time (RFC 3339). An unaligned interval counts from it. |
| `end_time` | | No ticks after this time (RFC 3339). Then the source stops emitting (the plugin stays connected). |
| `missed` | `"skip"` | What happens to ticks that couldn't go out on time: see below. |
| `max_catch_up` | `100` | With `catch_up`: at most this many missed ticks are sent (1–10 000). |
| `max_late` | `"1s"` | With `skip`: a tick that would go out later than this after its time is dropped. |
| `label` | `""` | Copied into every tick, to tell several tickers apart downstream. |
| `confirmed` | `false` | Wait for the hub to take each tick (`emit_confirmed`) before sending the next. |

## Output

| Field | Type | |
|---|---|---|
| `tick_time` | DATETIME | When the tick was scheduled. |
| `emit_time` | DATETIME | When it was sent. |
| `seq` | U64 | Tick number since the plugin started, from 1. It goes on across reconnects and starts over when the plugin restarts. |
| `missed` | U32 | Ticks dropped since the previous tick. |
| `label` | STRING | The `label` launch arg. |

## Clocks

- **A plain interval** runs on the monotonic clock: setting the system clock doesn't move it. On Linux and macOS the monotonic clock stops while the machine is suspended, so ticks resume after a suspend without counting the time as missed.
- **Aligned intervals and cron** follow the wall clock. The plugin checks the wall clock at least every 5 s, so after the clock is changed or the machine wakes from suspend, a tick goes out at most 5 s late. If the clock jumps forward, the ticks in between are missed. If it jumps back, ticks wait until the wall clock reaches them again.
- **Daylight-saving changes** (cron with a `timezone`), following croner:
  - A fixed time that falls in a skipped hour (spring forward) runs at the end of the gap: `30 2 * * *` in Europe/Berlin runs at 03:00 on the day the clocks go from 02:00 to 03:00.
  - A fixed time in a repeated hour (fall back) runs once, the first time the clock shows it.
  - Interval patterns (`*/30 * * * *`, wildcards in the hour field) skip the times that don't exist, and run in both instances of the repeated hour.

## Missed ticks

A tick is missed when it can't go out on time: the hub hasn't granted credit (a downstream queue is full), the plugin is disconnected, or the wall clock jumped. Ticks never pile up in memory:

- **`skip`** (default): missed ticks are dropped. The next tick goes out on schedule, with `missed` counting them. A tick still waiting for credit is replaced by the next one when that is due. A tick more than `max_late` past its time is dropped instead of being sent.
- **`fire_once`**: like `skip`, but the latest missed tick is sent as soon as possible, however late. `tick_time` shows how late it is.
- **`catch_up`**: each missed tick is sent, oldest first, up to `max_catch_up` waiting. Older ones are dropped and counted in `missed`. A cron gap of more than 100 000 ticks isn't counted beyond that.

Missed ticks are logged as a warning, at most once a minute.

## Delivery

The ticker uses the `CREDITS` and `ACKS` capabilities. It only sends a tick when the hub has granted credit; it checks for credit without sending, so it can drop a stale tick rather than queue it. Without `confirmed`, a tick that was sent but lost with a broken connection isn't sent again. With `confirmed`, each tick waits for the hub's answer before the next one can go out.

## Footprint

Measured with `cargo trellis bench plugins/ticker` (`benches/ticker.bench.toml`, a 10 ms interval) on an Apple M-series Mac, release build:

| Binary | Package | RSS | CPU at 100 ticks/s |
|---|---|---|---|
| 4.4 MB | 1.8 MB | 3.2 MiB | ~1% |

The time zone database comes from the system on Unix (`/usr/share/zoneinfo`) and is bundled on Windows ([jiff](https://docs.rs/jiff)), so the binary carries no copy of it. A Unix host without a zoneinfo database (some minimal containers) can only use `UTC`.

## Tests

- `cargo test -p flwrs-ticker`:
  - Unit tests for the schedules, including daylight-saving changes.
  - Unit tests for the missed-tick rules, on a paused clock with a fake wall clock that can jump.
  - Checks that the CLI flags, `schema/launch_args.json` and `schema/out.json` agree.
  - `tests/trellis.rs`: the trellis audit, plus the behaviour tests in `tests/trellis/` (credit, skipped ticks, a reconnect, cron).
- `cargo trellis audit|test|bench plugins/ticker` runs the same from the command line (settings in `trellis.toml`).
