use crate::config::ValueScaling;
use crate::device::{
    EventStream, HardwareDevice, InputHardwareDevice, OutputHardwareDevice, OutputPort,
};
use crate::Result;
use alloy::config::{InputValue, InputValueType};
use alloy::event::{Event, EventKind};
use alloy::OutputValue;
use anyhow::{anyhow, ensure};
use futures::{Stream, StreamExt};
use log::trace;
use std::collections::VecDeque;
use std::iter;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

/// The core of most hardware devices.
///
/// This core is read-only, i.e. for sensors, but is also used as the base of the read/write core.
/// It handles device values (the values last read from/written to a device), and events.
#[derive(Debug)]
pub(crate) struct DeviceReadCore {
    pub alias: String,
    pub device_values: Vec<Option<Result<InputValue>>>,
    value_types: Vec<InputValueType>,
    pub events: Vec<Option<VecDeque<Event>>>,
    pub wakers: Vec<Option<Waker>>,
}

impl DeviceReadCore {
    pub(crate) fn new(alias: String, value_types: &[InputValueType]) -> DeviceReadCore {
        DeviceReadCore {
            alias,
            device_values: iter::repeat_with(|| None).take(value_types.len()).collect(),
            events: iter::repeat(None).take(value_types.len()).collect(),
            wakers: iter::repeat(None).take(value_types.len()).collect(),
            value_types: value_types.to_vec(),
        }
    }

    pub fn set_up_events(&mut self, index: usize) -> Result<()> {
        ensure!(index < self.device_values.len(), "invalid index: {}", index);
        ensure!(
            self.events[index].is_none(),
            "events already set up for index {}",
            index
        );

        self.events[index] = Some(VecDeque::new());

        Ok(())
    }

    pub(crate) fn poll_events(
        &mut self,
        cx: &mut Context<'_>,
        index: usize,
    ) -> Poll<Option<Event>> {
        // check whether events are actually set up (this should hopefully always work)
        let event = self.events[index]
            .as_mut()
            .expect("events for this port not set up")
            .pop_front();
        match event {
            None => {
                self.wakers[index] = Some(cx.waker().clone());
                Poll::Pending
            }
            Some(event) => Poll::Ready(Some(event)),
        }
    }

    pub(crate) fn update_value_and_generate_events(
        &mut self,
        ts: chrono::DateTime<chrono::Utc>,
        index: usize,
        value: Result<InputValue>,
        force_event: bool,
        suppress_update_event: bool,
    ) {
        assert!(index < self.events.len(), "device core index out of bounds");
        if let Some(events) = &mut self.events[index] {
            let generate_event = force_event
                || self.device_values[index]
                    .as_ref()
                    .map_or(true, |v| match v {
                        Ok(val) => {
                            if let Ok(val2) = &value {
                                !suppress_update_event && (val != val2)
                            } else {
                                true
                            }
                        }
                        Err(e) => {
                            if let Err(e2) = &value {
                                // TODO find a better way for this...
                                let s1 = format!("{}", e);
                                let s2 = format!("{}", e2);
                                s1 != s2
                            } else {
                                true
                            }
                        }
                    });
            match value {
                Ok(value) => {
                    // Only generate an event if something changed
                    if generate_event {
                        events.push_back(Event {
                            timestamp: ts,
                            inner: Ok(EventKind::Update { new_value: value }),
                        });
                    }
                    self.device_values[index] = Some(Ok(value));
                }
                Err(e) => {
                    let msg = format!("{:?}", e);
                    events.push_back(Event {
                        timestamp: ts,
                        inner: Err(msg),
                    });
                    self.device_values[index] = Some(Err(e));
                }
            }

            trace!("have {} events waiting for index {}", events.len(), index);
            if let Some(waker) = self.wakers[index].take() {
                waker.wake()
            }
        }
    }

    pub(crate) fn set_error_on_all_ports(
        &mut self,
        ts: chrono::DateTime<chrono::Utc>,
        error_message: String,
    ) {
        for i in 0..self.value_types.len() {
            self.update_value_and_generate_events(
                ts,
                i,
                Err(anyhow!(error_message.clone())),
                true,
                false,
            )
        }
    }

    fn port_alias(&self, port: u8) -> Result<String> {
        ensure!(
            (port as usize) < self.value_types.len(),
            "invalid port number"
        );
        Ok(format!("port-{}", port))
    }
}

/// A thread-safe version of a read-only device core.
///
/// It provides functionality to derive a read-only virtual device from it, i.e. it implements the
/// `HardwareDevice` trait.
#[derive(Clone, Debug)]
pub(crate) struct SynchronizedDeviceReadCore {
    pub(crate) core: Arc<Mutex<DeviceReadCore>>,
}

impl SynchronizedDeviceReadCore {
    pub(crate) fn new_from_core(core: DeviceReadCore) -> SynchronizedDeviceReadCore {
        SynchronizedDeviceReadCore {
            core: Arc::new(Mutex::new(core)),
        }
    }
}

impl HardwareDevice for SynchronizedDeviceReadCore {
    fn port_alias(&self, port: u8) -> Result<String> {
        let core = self.core.lock().unwrap();
        core.port_alias(port)
    }
}

impl InputHardwareDevice for SynchronizedDeviceReadCore {
    fn get_input_port(&self, port: u8) -> Result<(InputValueType, EventStream)> {
        let mut core = self.core.lock().unwrap();
        ensure!(
            (port as usize) < core.device_values.len(),
            "invalid port: {}",
            port
        );

        core.set_up_events(port as usize)
            .expect("unable to set up events");

        let dev = VirtualDeviceReadCore {
            index: port as usize,
            core: self.core.clone(),
        };

        Ok((*core.value_types.get(port as usize).unwrap(), dev.boxed()))
    }
}

/// A virtual device derived from a read-only core.
#[derive(Clone, Debug)]
struct VirtualDeviceReadCore {
    index: usize,
    core: Arc<Mutex<DeviceReadCore>>,
}

impl Stream for VirtualDeviceReadCore {
    type Item = Event;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut core = self.core.lock().unwrap();

        core.poll_events(cx, self.index)
    }
}

/// A read-write version of the device core.
///
/// This handles setting values to a buffer, keeping track of the dirtiness of that buffer, and of
/// possible errors from earlier updates.
#[derive(Debug)]
pub(crate) struct DeviceRWCore {
    pub read_core: DeviceReadCore,
    pub buffered_values: Vec<OutputValue>,
    pub scalings: Vec<ValueScaling>,
    pub dirty: bool,
}

impl DeviceRWCore {
    pub(crate) fn new(alias: String, count: usize, default_scaling: ValueScaling) -> DeviceRWCore {
        DeviceRWCore {
            read_core: DeviceReadCore::new(
                alias,
                &iter::repeat(InputValueType::Continuous)
                    .take(count)
                    .collect::<Vec<_>>(),
            ),
            buffered_values: iter::repeat(0).take(count).collect(),
            scalings: iter::repeat(default_scaling).take(count).collect(),
            dirty: false,
        }
    }

    pub(crate) fn new_dirty(
        alias: String,
        count: usize,
        default_scaling: ValueScaling,
    ) -> DeviceRWCore {
        let mut core = Self::new(alias, count, default_scaling);
        core.dirty = true;
        core
    }

    /// Sets a value in the value buffer and sets the dirty bit if the new values is different than
    /// the buffered value.
    ///
    /// Panics on invalid index.
    pub(crate) fn set(&mut self, index: usize, value: OutputValue) {
        let old_val = self.buffered_values[index];
        self.buffered_values[index] = value;
        if value != old_val {
            self.dirty = true;
        }
    }

    /// Transfers new values to the device values for future reads, generates change events, and
    /// places a possible error to be retrieved on future updates.
    pub(crate) fn finish_update(
        &mut self,
        new_values: Result<Vec<OutputValue>>,
        ts: chrono::DateTime<chrono::Utc>,
        force_events: bool,
        suppress_update_events: bool,
    ) {
        match new_values {
            Ok(new_values) => {
                self.update_and_generate_events(
                    new_values
                        .into_iter()
                        .map(|v| Ok(InputValue::Continuous(v))),
                    ts,
                    force_events,
                    suppress_update_events,
                );
            }
            Err(err) => {
                // set dirty to make sure we try again next time
                self.dirty = true;
                let msg = format!("{:?}", err);
                self.update_and_generate_events(
                    iter::repeat_with(|| msg.clone())
                        .take(self.buffered_values.len())
                        .map(|v| Err(anyhow!(v))),
                    ts,
                    true,
                    false,
                );
            }
        }
    }

    fn update_and_generate_events<T: Iterator<Item = Result<InputValue>>>(
        &mut self,
        new_values: T,
        ts: chrono::DateTime<chrono::Utc>,
        force_events: bool,
        suppress_update_events: bool,
    ) {
        // Generate events
        for (i, value) in new_values.into_iter().enumerate() {
            self.read_core.update_value_and_generate_events(
                ts,
                i,
                value,
                force_events,
                suppress_update_events,
            )
        }
    }
}

/// A thread-safe version of a R/W device core.
///
/// Implements the `HardwareDevice` trait.
#[derive(Debug, Clone)]
pub(crate) struct SynchronizedDeviceRWCore {
    pub(crate) core: Arc<Mutex<DeviceRWCore>>,
}

impl SynchronizedDeviceRWCore {
    pub(crate) fn new_from_core(core: DeviceRWCore) -> SynchronizedDeviceRWCore {
        SynchronizedDeviceRWCore {
            core: Arc::new(Mutex::new(core)),
        }
    }
}

impl HardwareDevice for SynchronizedDeviceRWCore {
    fn port_alias(&self, port: u8) -> Result<String> {
        let core = self.core.lock().unwrap();
        core.read_core.port_alias(port)
    }
}

impl OutputHardwareDevice for SynchronizedDeviceRWCore {
    fn update(&self) -> Result<()> {
        Ok(()) // I guess?
    }

    fn get_output_port(
        &self,
        port: u8,
        scaling: Option<ValueScaling>,
    ) -> Result<(Box<dyn OutputPort>, EventStream)> {
        let mut core = self.core.lock().unwrap();
        ensure!(
            (port as usize) < core.read_core.device_values.len(),
            "invalid port: {}",
            port
        );
        if let Some(scaling) = scaling {
            core.scalings[port as usize] = scaling
        }

        core.read_core
            .set_up_events(port as usize)
            .expect("unable to set up events");

        let dev = VirtualRWDeviceCore {
            index: port as usize,
            core: self.core.clone(),
        };

        Ok((Box::new(dev.clone()), dev.boxed()))
    }
}

/// The virtual device derived from a R/W device core.
#[derive(Clone, Debug)]
struct VirtualRWDeviceCore {
    index: usize,
    core: Arc<Mutex<DeviceRWCore>>,
}

impl OutputPort for VirtualRWDeviceCore {
    fn set(&self, value: OutputValue) -> Result<()> {
        let mut core = self.core.lock().unwrap();
        assert!(self.index < core.buffered_values.len());
        core.set(self.index, value);

        Ok(())
    }
}

impl Stream for VirtualRWDeviceCore {
    type Item = Event;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut core = self.core.lock().unwrap();

        core.read_core.poll_events(cx, self.index)
    }
}
