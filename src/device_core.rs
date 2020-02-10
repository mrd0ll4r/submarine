use crate::device::{EventStream, HardwareDevice, VirtualDevice};
use crate::Result;
use failure::{err_msg, Error};
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::SystemTime;
use std::{iter, mem};
use alloy::Value;
use alloy::event::{Event, EventKind};

/// The core of most hardware devices.
///
/// This core is read-only, i.e. for sensors, but is also used as the base of the read/write core.
/// It handles device values (the values last read from/written to a device), and events.
pub(crate) struct DeviceReadCore {
    pub device_values: Vec<Value>,
    pub events: Vec<Option<Vec<Event>>>,
    pub wakers: Vec<Option<Waker>>,
}

impl DeviceReadCore {
    pub(crate) fn new(count: usize) -> DeviceReadCore {
        DeviceReadCore {
            device_values: iter::repeat(0).take(count).collect(),
            events: iter::repeat(None).take(count).collect(),
            wakers: iter::repeat(None).take(count).collect(),
        }
    }

    pub fn set_up_events(&mut self, index: usize) -> Result<()> {
        ensure!(index < self.device_values.len(), "invalid index: {}", index);
        ensure!(
            self.events[index].is_none(),
            "events already set up for index {}",
            index
        );

        self.events[index] = Some(Vec::new());

        Ok(())
    }

    pub(crate) fn poll_events(
        &mut self,
        cx: &mut Context<'_>,
        index: usize,
    ) -> Poll<Option<Vec<Event>>> {
        // check whether events are actually set up (this should hopefully always work)
        let state = self;
        let event_queue: &mut Vec<Event> = state.events[index]
            .as_mut()
            .expect("events for this index not set up");

        // check for events
        if event_queue.is_empty() {
            // no events -> put our waker somewhere, in case events arrive at some point
            state.wakers[index] = Some(cx.waker().clone());
            return Poll::Pending;
        }

        // we have events -> get and return them
        let events = mem::replace(event_queue, Vec::new());
        Poll::Ready(Some(events))
    }

    pub(crate) fn update_value_and_generate_events(
        &mut self,
        ts: SystemTime,
        index: usize,
        value: Value,
    ) {
        if let Some(events) = &mut self.events[index] {
            if value != self.device_values[index] {
                events.push(Event {
                    timestamp: ts,
                    inner: EventKind::Change { new_value: value },
                });
                self.device_values[index] = value;
            }

            trace!("have {} events waiting for index {}", events.len(), index);
            if let Some(waker) = self.wakers[index].take() {
                waker.wake()
            }
        }
    }
}

/// A thread-safe version of a read-only device core.
///
/// It provides functionality to derive a read-only virtual device from it, i.e. it implements the
/// `HardwareDevice` trait.
pub(crate) type SynchronizedDeviceReadCore = Arc<Mutex<DeviceReadCore>>;

impl HardwareDevice for SynchronizedDeviceReadCore {
    fn update(&self) -> Result<()> {
        Ok(())
    }

    fn get_virtual_device(&self, port: u8) -> Result<Box<dyn VirtualDevice + Send>> {
        let core = self.lock().unwrap();
        ensure!(
            (port as usize) < core.device_values.len(),
            "invalid port: {}",
            port
        );

        Ok(Box::new(VirtualDeviceReadCore {
            index: port as usize,
            core: self.clone(),
        }))
    }

    fn get_event_stream(&self, port: u8) -> Result<EventStream> {
        let mut core = self.lock().unwrap();

        core.set_up_events(port as usize)?;

        Ok(VirtualDeviceReadCore {
            index: port as usize,
            core: self.clone(),
        }
        .boxed())
    }
}

/// A virtual device derived from a read-only core.
struct VirtualDeviceReadCore {
    index: usize,
    core: SynchronizedDeviceReadCore,
}

impl VirtualDevice for VirtualDeviceReadCore {
    fn set(&self, _value: Value) -> Result<()> {
        Err(err_msg("read-only"))
    }

    fn get(&self) -> Result<Value> {
        let core = self.core.lock().unwrap();
        assert!(self.index < core.device_values.len());

        Ok(core.device_values[self.index])
    }
}

impl Stream for VirtualDeviceReadCore {
    type Item = Vec<Event>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut core = self.core.lock().unwrap();

        core.poll_events(cx, self.index)
    }
}

/// A read-write version of the device core.
///
/// This handles setting values to a buffer, keeping track of the dirtiness of that buffer, and of
/// possible errors from earlier updates.
pub(crate) struct DeviceRWCore {
    pub read_core: DeviceReadCore,
    pub buffered_values: Vec<Value>,
    pub dirty: bool,
    pub err: Option<Error>,
}

impl DeviceRWCore {
    pub(crate) fn new(count: usize) -> DeviceRWCore {
        DeviceRWCore {
            read_core: DeviceReadCore::new(count),
            buffered_values: iter::repeat(0).take(count).collect(),
            dirty: false,
            err: None,
        }
    }

    pub(crate) fn new_dirty(count: usize) -> DeviceRWCore {
        let mut core = Self::new(count);
        core.dirty = true;
        core
    }

    /// Sets a value in the value buffer and sets the dirty bit.
    ///
    /// Panics on invalid index.
    pub(crate) fn set(&mut self, index: usize, value: Value) {
        self.buffered_values[index] = value;
        self.dirty = true;
    }

    /// Transfers new values to the device values for future reads, generates change events, and
    /// places a possible error to be retrieved on future updates.
    pub(crate) fn finish_update(
        &mut self,
        new_values: Vec<Value>,
        ts: SystemTime,
        err: Option<Error>,
    ) {
        match err {
            None => {
                self.update_and_generate_events(new_values, ts);
            }
            Some(err) => {
                // set dirty to make sure we try again next time
                self.dirty = true;
                self.err = Some(err)
            }
        }
    }

    fn update_and_generate_events(&mut self, new_values: Vec<Value>, ts: SystemTime) {
        assert_eq!(new_values.len(), self.buffered_values.len());

        // generate events
        for (i, value) in new_values.into_iter().enumerate() {
            self.read_core
                .update_value_and_generate_events(ts, i, value)
        }
    }
}

/// A thread-safe version of a R/W device core.
///
/// Implements the `HardwareDevice` trait.
pub(crate) type SynchronizedDeviceRWCore = Arc<Mutex<DeviceRWCore>>;

impl HardwareDevice for SynchronizedDeviceRWCore {
    fn update(&self) -> Result<()> {
        let err = {
            let mut core = self.lock().unwrap();
            core.err.take()
        };

        match err {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn get_virtual_device(&self, port: u8) -> Result<Box<dyn VirtualDevice + Send>> {
        let core = self.lock().unwrap();
        ensure!(
            (port as usize) < core.read_core.device_values.len(),
            "invalid port: {}",
            port
        );

        Ok(Box::new(VirtualRWDeviceCore {
            index: port as usize,
            core: self.clone(),
        }))
    }

    fn get_event_stream(&self, port: u8) -> Result<EventStream> {
        let mut core = self.lock().unwrap();

        core.read_core.set_up_events(port as usize)?;

        Ok(VirtualRWDeviceCore {
            index: port as usize,
            core: self.clone(),
        }
        .boxed())
    }
}

/// The virtual device derived from a R/W device core.
struct VirtualRWDeviceCore {
    index: usize,
    core: SynchronizedDeviceRWCore,
}

impl VirtualDevice for VirtualRWDeviceCore {
    fn set(&self, value: Value) -> Result<()> {
        let mut core = self.core.lock().unwrap();
        assert!(self.index < core.buffered_values.len());
        core.set(self.index, value);

        Ok(())
    }

    fn get(&self) -> Result<Value> {
        let core = self.core.lock().unwrap();
        assert!(self.index < core.read_core.device_values.len());

        Ok(core.read_core.device_values[self.index])
    }
}

impl Stream for VirtualRWDeviceCore {
    type Item = Vec<Event>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut core = self.core.lock().unwrap();

        core.read_core.poll_events(cx, self.index)
    }
}
