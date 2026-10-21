//! Gate behaviour over a manual clock and scripted pressure readings. Every
//! test runs on a current-thread runtime with a timeout.

use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use devlanes_core::{
    clock::{Clock, ManualClock},
    gate::{
        HoldReason, MemoryPressure, PressureSample, PressureSource, Release, StartGate,
        StartRequest, StartSlot, StartTicket,
    },
};
use devlanes_start_gate::{
    AdaptiveConfig, AdaptiveGate, AllAtOnce, Controller, FixedWindow, GateOptions,
};

/// Upper bound for every test body.
const LIMIT: Duration = Duration::from_secs(10);

/// Runs a test body under [`LIMIT`].
async fn bounded<F: Future<Output = ()>>(body: F) {
    tokio::time::timeout(LIMIT, body)
        .await
        .expect("test timed out");
}

/// Lets spawned tasks (the gate driver) run after the clock moved.
async fn settle() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

/// Advances the clock in one-second steps, letting the driver run after each.
async fn tick(clock: &ManualClock, seconds: u64) {
    for _ in 0..seconds {
        clock.advance(Duration::from_secs(1));
        settle().await;
    }
}

fn request(lane: &str, unit: &str, order: usize) -> StartRequest {
    needing(lane, unit, order, 0)
}

fn needing(lane: &str, unit: &str, order: usize, memory_need: u64) -> StartRequest {
    StartRequest {
        lane: lane.into(),
        unit: unit.into(),
        order,
        memory_need,
    }
}

/// Polls a ticket once without waiting.
fn try_take(ticket: &mut StartTicket) -> Option<StartSlot> {
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    match std::pin::Pin::new(ticket).poll(&mut context) {
        std::task::Poll::Ready(slot) => Some(slot),
        std::task::Poll::Pending => None,
    }
}

/// Removes and returns the granted tickets' slots; a finished ticket must not be
/// polled again.
fn take_granted(tickets: &mut Vec<StartTicket>) -> Vec<StartSlot> {
    let mut slots = Vec::new();
    tickets.retain_mut(|ticket| match try_take(ticket) {
        Some(slot) => {
            slots.push(slot);
            false
        }
        None => true,
    });
    slots
}

fn options(slot_timeout: u64) -> GateOptions {
    GateOptions {
        slot_timeout: Duration::from_secs(slot_timeout),
        ..GateOptions::default()
    }
}

fn fixed(max: usize, slot_timeout: u64) -> (FixedWindow, ManualClock) {
    let clock = ManualClock::new();
    let gate = FixedWindow::new(max, options(slot_timeout), Arc::new(clock.clone()));
    (gate, clock)
}

#[tokio::test]
async fn window_is_shared_across_lanes() {
    bounded(async {
        let (gate, _clock) = fixed(2, 60);
        let mut tickets = [
            gate.enqueue(request("acme@1", "web", 0)),
            gate.enqueue(request("acme@1", "api", 1)),
            gate.enqueue(request("acme@2", "web", 0)),
            gate.enqueue(request("acme@2", "api", 1)),
        ];
        let slots: Vec<Option<StartSlot>> = tickets.iter_mut().map(try_take).collect();
        let granted: Vec<bool> = slots.iter().map(Option::is_some).collect();
        // acme@2's web shares order 0 with acme@1's web but arrived later.
        assert_eq!(granted, [true, true, false, false]);
        let snapshot = gate.snapshot();
        assert_eq!(snapshot.mode, "fixed");
        assert_eq!(snapshot.window, Some(2));
        assert_eq!(snapshot.in_flight, 2);
        assert_eq!(snapshot.hold, Some(HoldReason::WindowFull));
        let waiting: Vec<(usize, &str, &str)> = snapshot
            .queued
            .iter()
            .map(|q| (q.position, q.request.lane.as_str(), q.request.unit.as_str()))
            .collect();
        assert_eq!(waiting, [(1, "acme@2", "web"), (2, "acme@2", "api")]);
        assert_eq!(snapshot.find("acme@2", "api").map(|q| q.position), Some(2));
    })
    .await;
}

#[tokio::test]
async fn failure_and_ready_release_in_order() {
    bounded(async {
        let (gate, _clock) = fixed(1, 60);
        let first = gate.acquire(request("acme@1", "a", 0)).await;
        let mut second = gate.enqueue(request("acme@1", "b", 1));
        let mut third = gate.enqueue(request("acme@1", "c", 2));
        assert!(try_take(&mut second).is_none());
        first.release(Release::Failed);
        assert_eq!(first.released(), Some(Release::Failed));
        let second = try_take(&mut second).expect("freed by the failure");
        assert!(try_take(&mut third).is_none());
        second.release(Release::Ready);
        second.release(Release::Failed);
        assert_eq!(second.released(), Some(Release::Ready));
        assert!(try_take(&mut third).is_some());
    })
    .await;
}

#[tokio::test]
async fn slot_timeout_frees_the_window() {
    bounded(async {
        let (gate, clock) = fixed(1, 20);
        let blocked = gate.acquire(request("acme@1", "db-wait", 0)).await;
        let mut next = gate.enqueue(request("acme@1", "db", 1));
        settle().await;
        tick(&clock, 19).await;
        assert!(try_take(&mut next).is_none());
        assert_eq!(blocked.released(), None);
        tick(&clock, 1).await;
        assert_eq!(blocked.released(), Some(Release::Timeout));
        let next = try_take(&mut next).expect("granted after the timeout");
        // Releasing after the timeout changes nothing.
        blocked.release(Release::Ready);
        assert_eq!(blocked.released(), Some(Release::Timeout));
        assert_eq!(gate.snapshot().in_flight, 1);
        drop(next);
        assert_eq!(gate.snapshot().in_flight, 0);
    })
    .await;
}

#[tokio::test]
async fn requests_are_fifo_and_declaration_order_holds_within_one() {
    bounded(async {
        let (gate, _clock) = fixed(1, 60);
        let holder = gate.acquire(request("acme@1", "first", 0)).await;
        let batch = |lane: &str, units: &[(&str, usize)]| -> Vec<(String, StartTicket)> {
            let requests = units
                .iter()
                .map(|(unit, order)| request(lane, unit, *order))
                .collect();
            units
                .iter()
                .map(|(unit, _)| (*unit).to_owned())
                .zip(gate.enqueue_batch(requests))
                .collect()
        };
        let mut tickets = batch("acme@1", &[("late", 3), ("early", 1)]);
        // A later request with lower declaration indices waits its turn.
        tickets.extend(batch("other@1", &[("first-of-other", 0)]));
        tickets.extend(batch("acme@2", &[("x", 5), ("y", 2)]));
        // A restart of a low-index unit cannot starve the others either.
        tickets.extend(batch("acme@1", &[("restarted", 0)]));
        let mut started = Vec::new();
        let mut current = holder;
        while !tickets.is_empty() {
            current.release(Release::Ready);
            let (index, slot) = tickets
                .iter_mut()
                .enumerate()
                .find_map(|(index, (_, ticket))| try_take(ticket).map(|slot| (index, slot)))
                .expect("one ticket granted per release");
            started.push(tickets.remove(index).0);
            current = slot;
        }
        assert_eq!(
            started,
            ["early", "late", "first-of-other", "y", "x", "restarted"]
        );
    })
    .await;
}

#[tokio::test]
async fn dropped_ticket_leaves_the_queue() {
    bounded(async {
        let (gate, _clock) = fixed(1, 60);
        let holder = gate.acquire(request("acme@1", "a", 0)).await;
        let abandoned = gate.enqueue(request("acme@1", "b", 1));
        let mut kept = gate.enqueue(request("acme@1", "c", 2));
        assert_eq!(gate.snapshot().queued.len(), 2);
        drop(abandoned);
        assert_eq!(gate.snapshot().queued.len(), 1);
        drop(holder);
        assert!(try_take(&mut kept).is_some());
    })
    .await;
}

#[tokio::test]
async fn cpu_settle_releases_started_units() {
    bounded(async {
        let (gate, clock) = fixed(1, 60);
        let slot = gate.acquire(request("acme@1", "watcher", 0)).await;
        let mut next = gate.enqueue(request("acme@1", "web", 1));
        slot.report_cpu(180.0);
        tick(&clock, 1).await;
        slot.report_cpu(4.0);
        tick(&clock, 1).await;
        slot.report_cpu(30.0);
        tick(&clock, 1).await;
        slot.report_cpu(5.0);
        tick(&clock, 1).await;
        slot.report_cpu(6.0);
        assert_eq!(slot.released(), None, "only 1 s below the threshold");
        tick(&clock, 1).await;
        slot.report_cpu(2.0);
        assert_eq!(slot.released(), Some(Release::Settled));
        assert!(try_take(&mut next).is_some());
    })
    .await;
}

#[tokio::test]
async fn all_at_once_grants_everything_and_counts_holders() {
    bounded(async {
        let clock = ManualClock::new();
        let gate = AllAtOnce::new(options(60), Arc::new(clock));
        let mut slots = Vec::new();
        for index in 0..25 {
            slots.push(
                gate.acquire(request("acme@1", &format!("s{index}"), index))
                    .await,
            );
        }
        let snapshot = gate.snapshot();
        assert_eq!(snapshot.window, None);
        assert_eq!(snapshot.in_flight, 25);
        assert!(snapshot.queued.is_empty());
        slots.clear();
        assert_eq!(gate.snapshot().in_flight, 0);
    })
    .await;
}

/// Pressure that follows a script over the manual clock's time.
struct Trace {
    clock: ManualClock,
    script: fn(u64) -> PressureSample,
    /// Available bytes by second; `None` for an unknown amount.
    memory: fn(u64) -> Option<u64>,
    readings: Mutex<Vec<u64>>,
}

impl Trace {
    /// A trace with unknown available memory.
    fn cpu(clock: &ManualClock, script: fn(u64) -> PressureSample) -> Arc<Self> {
        Arc::new(Self {
            clock: clock.clone(),
            script,
            memory: |_| None,
            readings: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl PressureSource for Trace {
    fn name(&self) -> &'static str {
        "trace"
    }

    async fn sample(&self) -> Result<PressureSample, devlanes_core::Error> {
        let second = self.clock.now().as_secs();
        if let Ok(mut readings) = self.readings.lock() {
            readings.push(second);
        }
        Ok((self.script)(second))
    }

    async fn available_memory(&self) -> Result<Option<u64>, devlanes_core::Error> {
        Ok((self.memory)(self.clock.now().as_secs()))
    }
}

fn reading(cpu: f64, memory: MemoryPressure) -> PressureSample {
    PressureSample {
        cpu_percent: Some(cpu),
        memory: Some(memory),
        cpu_stall_percent: None,
    }
}

/// 0-7 s idle CPU, 8-15 s overloaded, 16-19 s idle with memory warn, 20-22 s
/// critical, then idle again.
fn script(second: u64) -> PressureSample {
    match second {
        0..=7 => reading(20.0, MemoryPressure::Normal),
        8..=15 => reading(97.0, MemoryPressure::Normal),
        16..=19 => reading(20.0, MemoryPressure::Warn),
        20..=22 => reading(20.0, MemoryPressure::Critical),
        _ => reading(20.0, MemoryPressure::Normal),
    }
}

#[tokio::test]
async fn adaptive_window_follows_a_pressure_trace() {
    bounded(async {
        let clock = ManualClock::new();
        let trace = Trace::cpu(&clock, script);
        let gate = AdaptiveGate::new(
            AdaptiveConfig::new(2, 6, 75.0),
            options(1000),
            Arc::new(clock.clone()),
            trace.clone(),
        );
        // Plenty of demand, and slots are never released, so only the window
        // decides how many start.
        let mut tickets: Vec<StartTicket> = (0..40)
            .map(|i| gate.enqueue(request("acme@1", &format!("s{i}"), i)))
            .collect();
        let mut slots = Vec::new();
        let mut windows = Vec::new();
        settle().await;
        for second in 0..=26 {
            if second > 0 {
                tick(&clock, 1).await;
            }
            slots.extend(take_granted(&mut tickets));
            let snapshot = gate.snapshot();
            windows.push(snapshot.window.expect("bounded window"));
            if second == 21 {
                assert_eq!(snapshot.hold, Some(HoldReason::MemoryCritical));
                assert_eq!(
                    snapshot.pressure.and_then(|p| p.memory),
                    Some(MemoryPressure::Critical)
                );
            }
        }
        assert_eq!(
            windows,
            [
                // The first reading has no CPU (it would span the idle time),
                // then +1 every 2 s with headroom.
                2, 2, 2, 3, 3, 4, 4, 5, //
                5, 5, 5, 2, 2, 2, 1, 1, // halved after 3 s above 90%, again 3 s later
                1, 1, 1, 1, // warn: halving floors at 1
                1, 1, 1, // critical: paused
                1, 1, 2, 2, // recovers by +1 every 2 s
            ]
        );
        // Started units never exceed the largest window reached (5).
        assert_eq!(slots.len(), 5);
        assert_eq!(gate.snapshot().in_flight, 5);
        let readings = trace.readings.lock().map(|r| r.clone()).unwrap_or_default();
        assert_eq!(readings.first(), Some(&0));
        assert!(readings.windows(2).all(|pair| pair[1] == pair[0] + 1));
    })
    .await;
}

#[tokio::test]
async fn adaptive_gate_restarts_at_initial_after_idle() {
    bounded(async {
        let clock = ManualClock::new();
        let trace = Trace::cpu(&clock, |_| reading(10.0, MemoryPressure::Normal));
        let gate = AdaptiveGate::new(
            AdaptiveConfig::new(2, 8, 75.0),
            options(1000),
            Arc::new(clock.clone()),
            trace,
        );
        let mut tickets: Vec<StartTicket> = (0..10)
            .map(|i| gate.enqueue(request("acme@1", &format!("s{i}"), i)))
            .collect();
        settle().await;
        assert_eq!(
            gate.snapshot().pressure.and_then(|p| p.cpu_percent),
            None,
            "first reading of a busy period drops its CPU"
        );
        tick(&clock, 7).await;
        assert_eq!(gate.snapshot().window, Some(5));
        let slots = take_granted(&mut tickets);
        assert_eq!(slots.len(), 5);
        drop(tickets);
        for slot in &slots {
            slot.release(Release::Ready);
        }
        tick(&clock, 1).await;
        assert_eq!(gate.snapshot().window, Some(2));
        // The next busy period starts with a fresh CPU baseline again.
        let _again = gate.enqueue(request("acme@1", "again", 0));
        settle().await;
        assert_eq!(gate.snapshot().pressure.and_then(|p| p.cpu_percent), None);
        tick(&clock, 1).await;
        assert_eq!(
            gate.snapshot().pressure.and_then(|p| p.cpu_percent),
            Some(10.0)
        );
    })
    .await;
}

/// 1000 bytes available until second 4, then 5000.
fn memory_frees_at_four(second: u64) -> Option<u64> {
    Some(if second < 4 { 1000 } else { 5000 })
}

#[tokio::test]
async fn head_waits_for_memory_then_goes_ahead() {
    bounded(async {
        let clock = ManualClock::new();
        let trace = Arc::new(Trace {
            clock: clock.clone(),
            script: |_| reading(10.0, MemoryPressure::Normal),
            memory: memory_frees_at_four,
            readings: Mutex::new(Vec::new()),
        });
        let gate = FixedWindow::with_pressure(4, options(60), Arc::new(clock.clone()), trace);
        // The first start runs before any reading: unknown memory never blocks.
        let holder = gate.acquire(needing("acme@1", "holder", 0, 0)).await;
        settle().await;
        assert_eq!(gate.snapshot().available_memory, Some(1000));
        let mut tickets = gate.enqueue_batch(vec![
            needing("acme@1", "a", 0, 400),
            needing("acme@1", "b", 1, 400),
            needing("acme@1", "c", 2, 400),
        ]);
        let first = take_granted(&mut tickets);
        assert_eq!(first.len(), 2, "two fit in 1000 bytes; the third does not");
        let snapshot = gate.snapshot();
        assert_eq!(snapshot.hold, Some(HoldReason::Memory));
        assert_eq!(snapshot.queued[0].request.memory_need, 400);
        drop(holder);
        assert!(
            take_granted(&mut tickets).is_empty(),
            "window has room, memory has not"
        );
        first[0].release(Release::Ready);
        assert_eq!(
            take_granted(&mut tickets).len(),
            1,
            "a finished start frees its share"
        );
        assert!(tickets.is_empty());

        // A need larger than the machine waits while others start, and goes
        // ahead once memory is reported free.
        let mut big = vec![gate.enqueue(needing("acme@2", "big", 0, 4000))];
        assert_eq!(gate.snapshot().hold, Some(HoldReason::Memory));
        tick(&clock, 3).await;
        assert!(take_granted(&mut big).is_empty());
        tick(&clock, 1).await;
        assert_eq!(take_granted(&mut big).len(), 1, "memory freed at 4 s");
    })
    .await;
}

#[tokio::test]
async fn memory_hold_never_refuses() {
    bounded(async {
        let clock = ManualClock::new();
        let trace = Arc::new(Trace {
            clock: clock.clone(),
            script: |_| reading(10.0, MemoryPressure::Normal),
            memory: |_| Some(1000),
            readings: Mutex::new(Vec::new()),
        });
        let gate = FixedWindow::with_pressure(4, options(10), Arc::new(clock.clone()), trace);
        let primer = gate.acquire(needing("acme@1", "primer", 0, 0)).await;
        settle().await;
        // With nothing else starting, a need above the available memory goes ahead.
        drop(primer);
        let alone = gate.acquire(needing("acme@1", "alone", 0, 9000)).await;
        settle().await;
        tick(&clock, 5).await;
        // Something is starting now, so the next big need waits, but no longer
        // than `slot_timeout`: here `alone` times out at 10 s and frees it.
        let mut waiting = vec![gate.enqueue(needing("acme@1", "waits", 1, 9000))];
        assert_eq!(gate.snapshot().hold, Some(HoldReason::Memory));
        tick(&clock, 4).await;
        assert!(take_granted(&mut waiting).is_empty());
        tick(&clock, 1).await;
        assert_eq!(alone.released(), Some(Release::Timeout));
        assert_eq!(take_granted(&mut waiting).len(), 1);
    })
    .await;
}

#[test]
fn controller_trace_without_a_runtime() {
    let mut controller = Controller::new(AdaptiveConfig::new(4, 8, 75.0));
    let steps = [
        (0, 50.0, MemoryPressure::Normal, 4),
        (1, 50.0, MemoryPressure::Normal, 4),
        (2, 50.0, MemoryPressure::Normal, 5),
        (3, 80.0, MemoryPressure::Normal, 5), // between target and 90%: hold
        (4, 50.0, MemoryPressure::Normal, 5),
        (6, 50.0, MemoryPressure::Normal, 6),
        (7, 50.0, MemoryPressure::Warn, 3), // warn halves at once
        (8, 50.0, MemoryPressure::Warn, 3), // and again only after 3 s
        (10, 50.0, MemoryPressure::Warn, 1),
        (11, 95.0, MemoryPressure::Normal, 1),
    ];
    for (second, cpu, memory, window) in steps {
        controller.observe(Duration::from_secs(second), &reading(cpu, memory), true);
        assert_eq!(controller.window(), window, "at {second}s");
    }
    controller.observe(
        Duration::from_secs(12),
        &reading(10.0, MemoryPressure::Critical),
        true,
    );
    assert!(controller.paused());
    controller.reset();
    assert_eq!(controller.window(), 4);
    assert!(!controller.paused());
}
