//! The tick loop: waits for each tick, emits it when the hub has credit, and
//! applies the missed-tick policy when it can't.
//!
//! - Ticks never queue up without a bound: at most one waits (`skip`,
//!   `fire_once`) or `max_catch_up` (`catch_up`).
//! - Waiting for credit is raced against the next tick (`LocalSink::ready`
//!   sends nothing, so it's safe to cancel); `skip` also drops a tick that
//!   would go out more than `max_late` after its time.
//! - The state lives across `run` calls, so after a reconnect the ticks
//!   missed meanwhile are counted (and caught up) by the same rules.

use crate::schedule::Schedule;
use jiff::Timestamp;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// A wall-clock schedule (cron, aligned intervals) is re-checked at least this
/// often, so a clock change or a suspended machine delays a tick by at most this.
const WALL_RECHECK: Duration = Duration::from_secs(5);

/// A gap longer than this many cron ticks isn't walked tick by tick: the
/// rest are skipped and the `missed` count stops there.
const SCAN_LIMIT: u64 = 100_000;

/// At most one missed-ticks warning per this interval.
const WARN_EVERY: Duration = Duration::from_secs(60);

/// What happens to ticks that couldn't go out on time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Missed {
    /// Dropped; the next tick goes out on schedule and counts them.
    Skip,
    /// One catch-up tick (the latest missed one) as soon as possible.
    FireOnce,
    /// Each missed tick, oldest first, up to `max_catch_up` (older ones are dropped).
    CatchUp,
}

#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub schedule: Schedule,
    /// No tick before this (default: when the plugin starts).
    pub start_time: Option<Timestamp>,
    /// No tick after this; the source finishes.
    pub end_time: Option<Timestamp>,
    /// Also a tick at the start (`start_time`, or when the plugin starts).
    pub fire_on_start: bool,
    pub missed: Missed,
    pub max_catch_up: u32,
    /// `skip`: a tick later than this is dropped.
    pub max_late: Duration,
    /// Each tick goes out a random `[0, jitter)` after its time.
    pub jitter: Duration,
    /// Schedule by the monotonic clock (plain intervals), not the wall clock.
    pub monotonic: bool,
}

/// One emitted tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tick {
    /// When it was scheduled.
    pub tick_time: Timestamp,
    /// When it went out.
    pub emit_time: Timestamp,
    /// 1 for the first tick since the plugin started.
    pub seq: u64,
    /// Ticks dropped since the previous one.
    pub missed: u32,
}

/// Where ticks go: the hub (`LocalSink`) or a test recorder.
pub(crate) trait Output {
    /// Resolves when `emit` would go out at once. Must be cancel-safe.
    async fn ready(&self) -> Result<(), String>;
    async fn emit(&self, tick: &Tick) -> Result<(), String>;
}

type WallFn = Arc<dyn Fn() -> Timestamp + Send + Sync>;

/// The ticker's sense of time: the wall clock, and for monotonic schedules
/// the wall clock at start plus the monotonic time since.
#[derive(Clone)]
pub(crate) struct Clock {
    wall: WallFn,
    base_wall: Timestamp,
    base: Instant,
    monotonic: bool,
}

impl Clock {
    pub(crate) fn system(monotonic: bool) -> Self {
        Self::with_wall(Arc::new(Timestamp::now), monotonic)
    }

    pub(crate) fn with_wall(wall: WallFn, monotonic: bool) -> Self {
        Self {
            base_wall: wall(),
            wall,
            base: Instant::now(),
            monotonic,
        }
    }

    /// The time ticks are scheduled by.
    fn now(&self) -> Timestamp {
        if self.monotonic {
            add(self.base_wall, self.base.elapsed())
        } else {
            (self.wall)()
        }
    }

    async fn sleep_until(&self, t: Timestamp) {
        loop {
            let left = since(t, self.now());
            if left.is_zero() {
                return;
            }
            let step = if self.monotonic {
                left
            } else {
                left.min(WALL_RECHECK)
            };
            tokio::time::sleep(step).await;
        }
    }
}

struct State {
    /// The start tick (`fire_on_start`), off the schedule.
    extra: Option<Timestamp>,
    /// The next scheduled tick not yet collected.
    cursor: Option<Timestamp>,
    /// Due, not yet emitted, oldest first.
    pending: VecDeque<Timestamp>,
    /// Dropped since the last emitted tick.
    missed: u64,
    /// Emitted so far.
    seq: u64,
    /// The jitter rolled for a tick: (tick, offset).
    offset: Option<(Timestamp, Duration)>,
    last_warning: Option<Instant>,
}

pub(crate) struct Ticker {
    settings: Settings,
    clock: Clock,
    state: tokio::sync::Mutex<State>,
}

impl Ticker {
    pub(crate) fn new(settings: Settings, clock: Clock) -> Self {
        let now = clock.now();
        let start = settings.start_time.map_or(now, |s| s.max(now));
        let within = |t: &Timestamp| settings.end_time.is_none_or(|end| *t <= end);
        let state = State {
            extra: settings.fire_on_start.then_some(start).filter(within),
            cursor: settings.schedule.next(start, false).filter(within),
            pending: VecDeque::new(),
            missed: 0,
            seq: 0,
            offset: None,
            last_warning: None,
        };
        Self {
            settings,
            clock,
            state: tokio::sync::Mutex::new(state),
        }
    }

    /// Emits ticks until the schedule ends (`Ok`) or the output fails.
    /// Cancelling it loses nothing: the next call carries on.
    pub(crate) async fn run(&self, out: &impl Output) -> Result<(), String> {
        let mut st = self.state.lock().await;
        loop {
            let now = self.clock.now();
            self.collect(&mut st, now);
            self.drop_late(&mut st, now);
            let next = match (st.extra, st.cursor) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let wake = next.map(|t| add(t, self.offset(&mut st, t)));
            if st.pending.is_empty() {
                let Some(wake) = wake else {
                    return Ok(()); // the schedule has ended
                };
                self.clock.sleep_until(wake).await;
                continue;
            }
            let next_tick = async {
                match wake {
                    Some(wake) => self.clock.sleep_until(wake).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                ready = out.ready() => {
                    ready?;
                    let now = self.clock.now();
                    if self.drop_late(&mut st, now) {
                        continue;
                    }
                    let tick_time = *st.pending.front().expect("checked above");
                    let missed = st.missed;
                    let tick = Tick {
                        tick_time,
                        emit_time: (self.clock.wall)(),
                        seq: st.seq + 1,
                        missed: u32::try_from(missed).unwrap_or(u32::MAX),
                    };
                    // Taken off only once it's out: a cancelled emit is tried again.
                    out.emit(&tick).await?;
                    st.pending.pop_front();
                    st.seq += 1;
                    st.missed = 0;
                    if missed > 0 {
                        log::info!("tick {} went out after {missed} missed tick(s)", tick.seq);
                    }
                }
                _ = next_tick => {}
            }
        }
    }

    /// Moves the ticks due by `now` into `pending`, keeping at most what the
    /// policy allows and counting the rest as missed.
    fn collect(&self, st: &mut State, now: Timestamp) {
        let s = &self.settings;
        if let Some(t) = st.extra.filter(|t| *t <= now) {
            st.extra = None;
            st.pending.push_back(t);
        }
        if let Some(first) = st.cursor.filter(|t| *t <= now) {
            let upto = s.end_time.map_or(now, |end| end.min(now));
            let limit = match s.schedule {
                Schedule::Every { .. } => u64::MAX,
                Schedule::Cron { .. } => SCAN_LIMIT,
            };
            let (n, last) = s.schedule.count(first, upto, limit);
            let last = last.unwrap_or(first);
            let keep = n.min(self.capacity() as u64);
            st.missed += n - keep;
            st.pending.extend(self.last_ticks(first, n, last, keep));
            let mut cursor = s.schedule.next(last, false);
            if n == SCAN_LIMIT && cursor.is_some_and(|c| c <= upto) {
                // Too far behind to count: skip to the present.
                cursor = s.schedule.next(upto, false);
            }
            st.cursor = cursor.filter(|t| s.end_time.is_none_or(|end| *t <= end));
        }
        while st.pending.len() > self.capacity() {
            st.pending.pop_front();
            st.missed += 1;
        }
        if st.missed > 0 && st.last_warning.is_none_or(|w| w.elapsed() >= WARN_EVERY) {
            st.last_warning = Some(Instant::now());
            log::warn!(
                "{} tick(s) missed so far (no credit from the hub, disconnected, or the clock jumped); missed = {:?}",
                st.missed,
                s.missed
            );
        }
    }

    /// `skip`: drops the waiting ticks that are already too late. True if any.
    fn drop_late(&self, st: &mut State, now: Timestamp) -> bool {
        if self.settings.missed != Missed::Skip {
            return false;
        }
        let allowed = self.settings.max_late + self.settings.jitter;
        let mut dropped = false;
        while st.pending.front().is_some_and(|t| since(now, *t) > allowed) {
            st.pending.pop_front();
            st.missed += 1;
            dropped = true;
        }
        dropped
    }

    fn capacity(&self) -> usize {
        match self.settings.missed {
            Missed::CatchUp => self.settings.max_catch_up.max(1) as usize,
            Missed::Skip | Missed::FireOnce => 1,
        }
    }

    /// The last `keep` of the `n` ticks from `first` to `last`.
    fn last_ticks(&self, first: Timestamp, n: u64, last: Timestamp, keep: u64) -> Vec<Timestamp> {
        match &self.settings.schedule {
            Schedule::Every { period, .. } => (0..keep)
                .rev()
                .map(|i| sub(last, *period * i as u32))
                .collect(),
            schedule @ Schedule::Cron { .. } => {
                let mut ticks = VecDeque::with_capacity(keep as usize);
                let mut t = Some(first);
                for _ in 0..n {
                    let Some(tick) = t else { break };
                    if ticks.len() == keep as usize {
                        ticks.pop_front();
                    }
                    ticks.push_back(tick);
                    t = schedule.next(tick, false);
                }
                ticks.into()
            }
        }
    }

    /// The jitter for tick `t`, rolled once per tick.
    fn offset(&self, st: &mut State, t: Timestamp) -> Duration {
        let jitter = self.settings.jitter;
        if jitter.is_zero() {
            return Duration::ZERO;
        }
        match st.offset {
            Some((tick, offset)) if tick == t => offset,
            _ => {
                let offset = Duration::from_nanos(random() % jitter.as_nanos().max(1) as u64);
                st.offset = Some((t, offset));
                offset
            }
        }
    }
}

fn random() -> u64 {
    getrandom::u64().unwrap_or_else(|_| {
        // No randomness source: the time's low bits are enough for jitter.
        Timestamp::now().subsec_nanosecond() as u64
    })
}

fn add(t: Timestamp, d: Duration) -> Timestamp {
    Timestamp::from_nanosecond(t.as_nanosecond() + d.as_nanos() as i128).unwrap_or(Timestamp::MAX)
}

fn sub(t: Timestamp, d: Duration) -> Timestamp {
    Timestamp::from_nanosecond(t.as_nanosecond() - d.as_nanos() as i128).unwrap_or(Timestamp::MIN)
}

/// `a - b`, or zero if `b` is later.
fn since(a: Timestamp, b: Timestamp) -> Duration {
    let nanos = a.as_nanosecond() - b.as_nanosecond();
    Duration::from_nanos(u64::try_from(nanos.max(0)).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::{parse_cron, parse_duration};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicI64, Ordering};
    use tokio::sync::watch;

    const START: &str = "2026-01-01T00:00:00Z";

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn at(secs: f64) -> Timestamp {
        add(ts(START), Duration::from_secs_f64(secs))
    }

    /// A wall clock that follows tokio's (paused) clock, plus a jump.
    struct FakeWall {
        jump_ms: Arc<AtomicI64>,
    }

    impl FakeWall {
        fn clock(monotonic: bool) -> (Clock, Self) {
            let jump_ms = Arc::new(AtomicI64::new(0));
            let base = Instant::now();
            let jump = jump_ms.clone();
            let wall: WallFn = Arc::new(move || {
                let t = add(ts(START), base.elapsed());
                let jumped = t.as_nanosecond() + jump.load(Ordering::SeqCst) as i128 * 1_000_000;
                Timestamp::from_nanosecond(jumped).unwrap()
            });
            (Clock::with_wall(wall, monotonic), Self { jump_ms })
        }

        fn jump(&self, d: Duration) {
            self.jump_ms
                .fetch_add(d.as_millis() as i64, Ordering::SeqCst);
        }
    }

    /// Records ticks; `credit` closed = `ready` waits.
    struct Recorder {
        credit: watch::Sender<bool>,
        ticks: Mutex<Vec<Tick>>,
    }

    impl Recorder {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                credit: watch::Sender::new(true),
                ticks: Mutex::new(vec![]),
            })
        }

        fn ticks(&self) -> Vec<(Timestamp, u64, u32)> {
            self.ticks
                .lock()
                .unwrap()
                .iter()
                .map(|t| (t.tick_time, t.seq, t.missed))
                .collect()
        }
    }

    impl Output for Arc<Recorder> {
        async fn ready(&self) -> Result<(), String> {
            let mut rx = self.credit.subscribe();
            rx.wait_for(|open| *open).await.map_err(|e| e.to_string())?;
            Ok(())
        }
        async fn emit(&self, tick: &Tick) -> Result<(), String> {
            self.ticks.lock().unwrap().push(tick.clone());
            Ok(())
        }
    }

    fn every(period: &str) -> Settings {
        Settings {
            schedule: Schedule::Every {
                period: parse_duration(period).unwrap(),
                anchor: ts(START),
            },
            start_time: None,
            end_time: None,
            fire_on_start: false,
            missed: Missed::Skip,
            max_catch_up: 100,
            max_late: Duration::from_millis(100),
            jitter: Duration::ZERO,
            monotonic: true,
        }
    }

    /// Runs the ticker in the background on the paused clock.
    fn start(settings: Settings, monotonic: bool) -> (Arc<Recorder>, FakeWall) {
        let (clock, wall) = FakeWall::clock(monotonic);
        let ticker = Arc::new(Ticker::new(settings, clock));
        let out = Recorder::new();
        let o = out.clone();
        tokio::spawn(async move { ticker.run(&o).await });
        (out, wall)
    }

    async fn advance(secs: f64) {
        tokio::time::sleep(Duration::from_secs_f64(secs)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn ticks_on_schedule() {
        let (out, _) = start(every("1s"), true);
        advance(3.5).await;
        assert_eq!(
            out.ticks(),
            vec![(at(1.0), 1, 0), (at(2.0), 2, 0), (at(3.0), 3, 0)]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn fire_on_start_and_end_time() {
        let mut settings = every("1s");
        settings.fire_on_start = true;
        settings.end_time = Some(at(2.0));
        let (clock, _wall) = FakeWall::clock(true);
        let ticker = Ticker::new(settings, clock);
        let out = Recorder::new();
        // The schedule ends, so `run` returns.
        tokio::time::timeout(Duration::from_secs(10), ticker.run(&out))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            out.ticks(),
            vec![(at(0.0), 1, 0), (at(1.0), 2, 0), (at(2.0), 3, 0)]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn start_time_in_the_future() {
        let mut settings = every("1s");
        settings.schedule = Schedule::Every {
            period: Duration::from_secs(1),
            anchor: at(10.0),
        };
        settings.start_time = Some(at(10.0));
        settings.fire_on_start = true;
        let (out, _) = start(settings, true);
        advance(11.5).await;
        assert_eq!(out.ticks(), vec![(at(10.0), 1, 0), (at(11.0), 2, 0)]);
    }

    /// No credit from 0 s to 3.5 s; ticks every second.
    async fn blocked(missed: Missed, max_catch_up: u32) -> Vec<(Timestamp, u64, u32)> {
        let mut settings = every("1s");
        settings.missed = missed;
        settings.max_catch_up = max_catch_up;
        let (out, _) = start(settings, true);
        out.credit.send_replace(false);
        advance(3.5).await;
        out.credit.send_replace(true);
        advance(1.0).await; // up to 4.5 s
        out.ticks()
    }

    #[tokio::test(start_paused = true)]
    async fn skip_drops_late_ticks() {
        // 1, 2 and 3 are dropped (3 is 0.5 s late > max_late); 4 counts them.
        assert_eq!(blocked(Missed::Skip, 100).await, vec![(at(4.0), 1, 3)]);
    }

    #[tokio::test(start_paused = true)]
    async fn fire_once_sends_the_latest() {
        assert_eq!(
            blocked(Missed::FireOnce, 100).await,
            vec![(at(3.0), 1, 2), (at(4.0), 2, 0)]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn catch_up_sends_each_up_to_the_limit() {
        assert_eq!(
            blocked(Missed::CatchUp, 100).await,
            vec![
                (at(1.0), 1, 0),
                (at(2.0), 2, 0),
                (at(3.0), 3, 0),
                (at(4.0), 4, 0)
            ]
        );
        assert_eq!(
            blocked(Missed::CatchUp, 2).await,
            vec![(at(2.0), 1, 1), (at(3.0), 2, 0), (at(4.0), 3, 0)]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_restarted_run_counts_what_it_missed() {
        let (clock, _wall) = FakeWall::clock(true);
        let ticker = Arc::new(Ticker::new(every("1s"), clock));
        let out = Recorder::new();
        let _ = tokio::time::timeout(Duration::from_secs_f64(1.5), ticker.run(&out)).await;
        advance(2.0).await; // "disconnected": 2 and 3 pass without a run
        let o = out.clone();
        let t = ticker.clone();
        tokio::spawn(async move { t.run(&o).await });
        advance(1.0).await; // up to 4.5 s
        assert_eq!(out.ticks(), vec![(at(1.0), 1, 0), (at(4.0), 2, 2)]);
    }

    #[tokio::test(start_paused = true)]
    async fn aligned_ticks_follow_the_wall_clock() {
        let mut settings = every("1s");
        settings.monotonic = false;
        let (out, wall) = start(settings, false);
        advance(1.5).await;
        // The wall clock jumps 10 s ahead: the ticks in between are missed.
        wall.jump(Duration::from_secs(10));
        advance(1.0).await; // wall: 12.5 s
        assert_eq!(out.ticks(), vec![(at(1.0), 1, 0), (at(12.0), 2, 10)]);
    }

    #[tokio::test(start_paused = true)]
    async fn wall_clock_ticks_follow_a_small_jump() {
        let mut settings = every("10s");
        settings.monotonic = false;
        let (out, wall) = start(settings, false);
        advance(10.5).await;
        wall.jump(Duration::from_secs(5)); // wall 15.5 s
        advance(5.0).await; // wall 20.5 s
        assert_eq!(out.ticks(), vec![(at(10.0), 1, 0), (at(20.0), 2, 0)]);
    }

    #[tokio::test(start_paused = true)]
    async fn cron_ticks() {
        let mut settings = every("1s");
        settings.schedule = Schedule::Cron {
            cron: Box::new(parse_cron("*/2 * * * * *").unwrap()),
            tz: jiff::tz::TimeZone::UTC,
        };
        settings.monotonic = false;
        let (out, _) = start(settings, false);
        advance(5.5).await;
        assert_eq!(out.ticks(), vec![(at(2.0), 1, 0), (at(4.0), 2, 0)]);
    }

    #[tokio::test(start_paused = true)]
    async fn jitter_delays_within_its_bound() {
        let mut settings = every("1s");
        settings.jitter = Duration::from_millis(300);
        let (out, _) = start(settings, true);
        advance(3.5).await;
        let ticks = out.ticks.lock().unwrap().clone();
        assert_eq!(ticks.len(), 3);
        for t in ticks {
            let delay = since(t.emit_time, t.tick_time);
            assert!(delay < Duration::from_millis(300), "{delay:?}");
            assert_eq!(t.missed, 0);
        }
    }
}
