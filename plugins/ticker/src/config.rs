//! The command line (bootstrap args + the node's launch args) and its checks.

use crate::schedule::{MIN_INTERVAL, Schedule, parse_cron, parse_duration};
use crate::ticker::{Missed, Settings};
use clap::{Parser, ValueEnum};
use flwrs_plugin::cli::Bootstrap;
use jiff::Timestamp;
use jiff::tz::TimeZone;

/// flwrs source plugin that emits a tick at an interval or on a cron schedule
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub(crate) struct Args {
    #[command(flatten)]
    pub bootstrap: Bootstrap,

    /// Tick every this long, e.g. 500ms, 10s, 5m, 1h30m, 1d (at least 10ms).
    /// The default schedule when there's no --cron: 1s.
    #[arg(long, conflicts_with = "cron")]
    pub interval: Option<String>,

    /// Tick on a cron pattern: 5 fields (minute hour day-of-month month
    /// day-of-week), or 6 with seconds first; nicknames like @hourly too
    #[arg(long)]
    pub cron: Option<String>,

    /// Time zone for --cron, e.g. Europe/Berlin (IANA name)
    #[arg(long, requires = "cron")]
    pub timezone: Option<String>,

    /// Put interval ticks on wall-clock multiples of the interval since
    /// 1970-01-01 UTC (every 1m at :00), instead of counting from the start
    #[arg(long, conflicts_with = "cron")]
    pub align: bool,

    /// Also tick at the start (when the plugin starts, or at --start-time)
    #[arg(long)]
    pub fire_on_start: bool,

    /// Delay each tick by a random amount below this (tick_time stays the scheduled time)
    #[arg(long)]
    pub jitter: Option<String>,

    /// No ticks before this time (RFC 3339, e.g. 2026-01-01T00:00:00Z). An
    /// interval without --align counts from it.
    #[arg(long)]
    pub start_time: Option<String>,

    /// No ticks after this time (RFC 3339); then the source stops emitting
    #[arg(long)]
    pub end_time: Option<String>,

    /// What happens to ticks that couldn't go out on time (no credit from the
    /// hub, disconnected, suspended machine, clock change)
    #[arg(long, value_enum, default_value = "skip")]
    pub missed: MissedArg,

    /// With --missed catch_up: at most this many missed ticks are sent
    #[arg(long, default_value = "100", value_parser = clap::value_parser!(u32).range(1..=10_000))]
    pub max_catch_up: u32,

    /// With --missed skip: a tick that would go out later than this after its time is dropped
    #[arg(long, default_value = "1s")]
    pub max_late: String,

    /// Copied into every tick, to tell several tickers apart downstream
    #[arg(long, default_value = "")]
    pub label: String,

    /// Wait for the hub to take each tick before the next (emit_confirmed)
    #[arg(long)]
    pub confirmed: bool,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MissedArg {
    #[value(name = "skip")]
    Skip,
    #[value(name = "fire_once")]
    FireOnce,
    #[value(name = "catch_up")]
    CatchUp,
}

const DEFAULT_INTERVAL: &str = "1s";

impl Args {
    /// The ticker's settings; `now` anchors an unaligned interval without a
    /// `--start-time`. Every problem, not just the first.
    pub(crate) fn settings(&self, now: Timestamp) -> Result<Settings, Vec<String>> {
        let mut problems = vec![];
        let mut problem = |p: String| problems.push(p);
        let time = |flag: &str, value: &Option<String>| -> Result<Option<Timestamp>, String> {
            value
                .as_deref()
                .map(|v| {
                    v.parse::<Timestamp>()
                        .map_err(|e| format!("--{flag} {v:?}: not an RFC 3339 time ({e})"))
                })
                .transpose()
        };
        let start_time = time("start-time", &self.start_time).unwrap_or_else(|e| {
            problem(e);
            None
        });
        let end_time = time("end-time", &self.end_time).unwrap_or_else(|e| {
            problem(e);
            None
        });
        if let (Some(start), Some(end)) = (start_time, end_time)
            && end <= start
        {
            problem("--end-time must be after --start-time".into());
        }
        let duration = |flag: &str, value: &str, problem: &mut dyn FnMut(String)| {
            parse_duration(value)
                .map_err(|e| problem(format!("--{flag}: {e}")))
                .ok()
        };
        let jitter = match &self.jitter {
            None => Some(std::time::Duration::ZERO),
            Some(j) => duration("jitter", j, &mut problem),
        };
        let max_late = duration("max-late", &self.max_late, &mut problem);

        let (schedule, monotonic) = match &self.cron {
            Some(pattern) => {
                let cron = parse_cron(pattern)
                    .map_err(|e| problem(format!("--{e}")))
                    .ok();
                let tz = match self.timezone.as_deref() {
                    None | Some("UTC") => Some(TimeZone::UTC),
                    Some(name) => TimeZone::get(name)
                        .map_err(|e| problem(format!("--timezone {name:?}: {e}")))
                        .ok(),
                };
                let schedule = cron.zip(tz).map(|(cron, tz)| Schedule::Cron {
                    cron: Box::new(cron),
                    tz,
                });
                (schedule, false)
            }
            None => {
                let text = self.interval.as_deref().unwrap_or(DEFAULT_INTERVAL);
                let period = duration("interval", text, &mut problem);
                if let Some(period) = period {
                    if period < MIN_INTERVAL {
                        problem(format!("--interval must be at least {MIN_INTERVAL:?}"));
                    }
                    if let Some(jitter) = jitter
                        && jitter >= period
                    {
                        problem("--jitter must be shorter than --interval".into());
                    }
                }
                let anchor = if self.align {
                    Timestamp::UNIX_EPOCH
                } else {
                    start_time.unwrap_or(now)
                };
                let schedule = period
                    .filter(|p| *p >= MIN_INTERVAL)
                    .map(|period| Schedule::Every { period, anchor });
                (schedule, !self.align)
            }
        };
        match (schedule, jitter, max_late) {
            (Some(schedule), Some(jitter), Some(max_late)) if problems.is_empty() => Ok(Settings {
                schedule,
                start_time,
                end_time,
                fire_on_start: self.fire_on_start,
                missed: match self.missed {
                    MissedArg::Skip => Missed::Skip,
                    MissedArg::FireOnce => Missed::FireOnce,
                    MissedArg::CatchUp => Missed::CatchUp,
                },
                max_catch_up: self.max_catch_up,
                max_late,
                jitter,
                monotonic,
            }),
            _ => Err(problems),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(extra: &[&str]) -> Result<Args, clap::Error> {
        let mut argv = vec!["ticker", "--id", "n", "--port", "1"];
        argv.extend_from_slice(extra);
        Args::try_parse_from(argv)
    }

    fn settings(extra: &[&str]) -> Result<Settings, Vec<String>> {
        args(extra)
            .unwrap()
            .settings("2026-01-01T00:00:00Z".parse().unwrap())
    }

    #[test]
    fn defaults_to_every_second() {
        let s = settings(&[]).unwrap();
        assert!(matches!(s.schedule, Schedule::Every { period, .. } if period.as_secs() == 1));
        assert!(s.monotonic);
        assert_eq!(s.missed, Missed::Skip);
    }

    #[test]
    fn aligned_and_cron_use_the_wall_clock() {
        let s = settings(&["--interval", "1m", "--align"]).unwrap();
        assert!(
            matches!(s.schedule, Schedule::Every { anchor, .. } if anchor == Timestamp::UNIX_EPOCH)
        );
        assert!(!s.monotonic);
        let s = settings(&["--cron", "0 9 * * *", "--timezone", "Europe/Berlin"]).unwrap();
        assert!(matches!(s.schedule, Schedule::Cron { .. }));
        assert!(!s.monotonic);
    }

    #[test]
    fn rejects_bad_combinations() {
        assert!(args(&["--interval", "1s", "--cron", "* * * * *"]).is_err());
        assert!(args(&["--timezone", "UTC"]).is_err());
        assert!(args(&["--cron", "* * * * *", "--align"]).is_err());
        assert!(args(&["--missed", "sometimes"]).is_err());
        assert!(args(&["--max-catch-up", "0"]).is_err());
    }

    #[test]
    fn reports_every_problem() {
        let problems = settings(&[
            "--interval",
            "5ms",
            "--start-time",
            "yesterday",
            "--max-late",
            "soon",
        ])
        .unwrap_err();
        assert_eq!(problems.len(), 3, "{problems:?}");
        let problems = settings(&["--interval", "1s", "--jitter", "2s"]).unwrap_err();
        assert!(problems[0].contains("--jitter"), "{problems:?}");
        let problems = settings(&[
            "--start-time",
            "2026-01-02T00:00:00Z",
            "--end-time",
            "2026-01-01T00:00:00Z",
        ])
        .unwrap_err();
        assert!(problems[0].contains("--end-time"), "{problems:?}");
        assert!(settings(&["--cron", "61 * * * *"]).is_err());
        assert!(settings(&["--cron", "* * * * *", "--timezone", "Mars/Olympus"]).is_err());
    }

    #[test]
    fn start_time_anchors_an_interval() {
        let s = settings(&["--interval", "10s", "--start-time", "2026-01-05T00:00:03Z"]).unwrap();
        assert!(matches!(s.schedule, Schedule::Every { anchor, .. }
            if anchor == "2026-01-05T00:00:03Z".parse::<Timestamp>().unwrap()));
    }
}
