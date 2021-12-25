use crate::config::ValueScaling;
use crate::device_core::SynchronizedDeviceRWCore;
use crate::Result;
use alloy::OutputValue;
use std::time::{Duration, Instant};
use std::{thread, time};

pub(crate) fn get_values_and_dirty(
    core: &SynchronizedDeviceRWCore,
) -> (Vec<OutputValue>, Vec<ValueScaling>, bool) {
    let mut core = core.core.lock().unwrap();
    let (values, scalings, dirty) = (
        core.buffered_values.clone(),
        core.scalings.clone(),
        core.dirty,
    );
    core.dirty = false;
    (values, scalings, dirty)
}

pub(crate) fn calculate_sleep_duration(
    wake_up: time::Instant,
    polling_interval: time::Duration,
) -> (time::Duration, time::Duration) {
    let now = time::Instant::now();
    let next_poll = wake_up + polling_interval;
    if now >= next_poll {
        (time::Duration::from_secs(0), now.duration_since(next_poll))
    } else {
        (next_poll.duration_since(now), time::Duration::from_secs(0))
    }
}

pub(crate) fn poll_loop_begin_sleep(
    module_name: &'static str,
    sleep_duration: Duration,
    last_wakeup: &mut Instant,
) -> Instant {
    thread::sleep(sleep_duration);
    let wake_up = Instant::now();
    trace!(
        target: module_name,
        "{} µs since last loop",
        (wake_up - *last_wakeup).as_micros()
    );
    *last_wakeup = wake_up;

    wake_up
}

pub(crate) fn poll_loop_begin(
    module_name: &'static str,
    sleep_duration: Duration,
    last_wakeup: &mut Instant,
    core: &SynchronizedDeviceRWCore,
) -> (Instant, Vec<OutputValue>, Vec<ValueScaling>, bool) {
    let wake_up = poll_loop_begin_sleep(module_name, sleep_duration, last_wakeup);

    let (values, scalings, dirty) = get_values_and_dirty(core);
    (wake_up, values, scalings, dirty)
}

pub(crate) fn poll_loop_end(
    module_name: &'static str,
    update_result: Result<()>,
    core: &SynchronizedDeviceRWCore,
    values: Vec<OutputValue>,
    wake_up: Instant,
    update_interval: Duration,
) -> Duration {
    debug!(target: module_name, "updated: {:?}", update_result);

    let ts = chrono::Utc::now();

    // Update device values, generate events, populate error in case something went wrong.
    {
        let mut core = core.core.lock().unwrap();
        core.finish_update(update_result.map(|_| values), ts);
    }

    let (s, lagging) = calculate_sleep_duration(wake_up, update_interval);
    if lagging > Duration::from_secs(0) {
        warn!(
            target: module_name,
            "took too long, lagging {}µs behind",
            lagging.as_micros()
        );
    }
    s
}
