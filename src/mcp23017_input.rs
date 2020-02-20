use crate::device::{HardwareDevice, VirtualDevice};
use crate::device_core::{DeviceReadCore, SynchronizedDeviceReadCore};
use crate::mcp23017::{MCP23017Config, MCP23017};
use crate::prom;
use crate::{poll, Result};
use alloy::event::{ButtonEvent, Event, EventKind};
use alloy::{HIGH, LOW};
use embedded_hal as hal;
use failure::*;
use futures::Stream;
use mcp23017 as mcpdev;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

pub(crate) struct MCP23017Input {
    inner: SynchronizedDeviceReadCore,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct MCP23017InputConfig {
    pub i2c_bus: String,
    i2c_slave_address: u8,
    polling_interval_millis: u64,
}

impl MCP23017Input {
    pub fn new<I2C, E>(
        dev: I2C,
        config: &MCP23017InputConfig,
        alias: String,
    ) -> Result<MCP23017Input>
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        let addr = MCP23017Config::slave_address(config.i2c_slave_address)?;
        ensure!(
            config.polling_interval_millis != 0,
            "polling interval must be greater than zero"
        );
        let mut mcp = mcpdev::MCP23017::new(dev, addr).map_err(MCP23017::do_map_err::<I2C, E>)?;
        mcp.write_gpioab(0)
            .map_err(|e| failure::err_msg(format!("{:?}", e)))?;
        mcp.all_pin_mode(mcpdev::PinMode::INPUT)
            .map_err(|e| failure::err_msg(format!("{:?}", e)))?;

        let inner = Arc::new(Mutex::new(DeviceReadCore::new(16)));
        let down = [
            Instant::now(),
            Instant::now(),
            Instant::now(),
            Instant::now(),
            Instant::now(),
            Instant::now(),
            Instant::now(),
            Instant::now(),
            Instant::now(),
            Instant::now(),
            Instant::now(),
            Instant::now(),
            Instant::now(),
            Instant::now(),
            Instant::now(),
            Instant::now(),
        ];
        let down_secs: [u64; 16] = Default::default();
        let thread_inner = inner.clone();

        let polling_interval = config.polling_interval_millis;
        thread::Builder::new()
            .name(format!("MCP-i {}", alias))
            .spawn(move || {
                MCP23017Input::run(thread_inner, mcp, polling_interval, alias, down_secs, down)
            })?;

        Ok(MCP23017Input { inner })
    }

    fn run<I2C, E>(
        inner: SynchronizedDeviceReadCore,
        mut dev: mcpdev::MCP23017<I2C>,
        polling_interval_millis: u64,
        alias: String,
        mut down_secs: [u64; 16],
        mut down: [Instant; 16],
    ) where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        let polling_interval = Duration::from_millis(polling_interval_millis);
        let mut sleep_duration = polling_interval;
        let mut last_wakeup = Instant::now();
        let hist = prom::MCP23017_READ_DURATION
            .get_metric_with_label_values(&[&alias])
            .unwrap();

        loop {
            thread::sleep(sleep_duration);
            let wake_up = Instant::now();
            let ts = SystemTime::now();
            trace!("{} µs since last loop", (wake_up - last_wakeup).as_micros());
            last_wakeup = wake_up;

            let mut state = inner.lock().unwrap();

            // Get new values, generate events
            let res = MCP23017Input::update_inner(
                &mut state,
                wake_up,
                ts,
                &mut dev,
                &hist,
                &mut down,
                &mut down_secs,
            );

            if let Err(e) = res {
                warn!("unable to read values: {:?}", e);
            }

            let (s, lagging) = poll::calculate_sleep_duration(wake_up, polling_interval);
            if lagging > Duration::from_secs(0) {
                warn!("took too long, lagging {}µs behind", lagging.as_micros());
            }
            sleep_duration = s;
        }
    }

    fn update_inner<I2C, E>(
        state: &mut DeviceReadCore,
        mono_ts: Instant,
        real_ts: SystemTime,
        dev: &mut mcpdev::MCP23017<I2C>,
        hist: &prometheus::Histogram,
        down: &mut [Instant; 16],
        down_secs: &mut [u64; 16],
    ) -> Result<()>
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        let before = Instant::now();
        let vals = dev
            .read_gpioab()
            .map_err(|err| failure::err_msg(format!("{:?}", err)))?;
        let after = Instant::now();
        hist.observe(after.duration_since(before).as_micros() as f64);
        debug!(
            "read_gpioab took {}µs => {}KiBit/s",
            after.duration_since(before).as_micros(),
            ((2 * 8) as f64 / after.duration_since(before).as_secs_f64()) / 1024_f64
        );

        let (high, low) = (((vals >> 8) & 0xFF) as u8, (vals & 0xFF) as u8);
        let vals = (low as u16) << 8 | high as u16;
        trace!("read values: {:#018b}", vals);

        let mut new_values = [false; 16];
        for (i, val) in new_values.iter_mut().enumerate() {
            *val = (vals & (0b1 << i)) != 0
        }

        for (i, new_val) in new_values.iter().enumerate() {
            let previous_val = state.device_values[i] == HIGH;
            let down_for = mono_ts.duration_since(down[i]);

            if *new_val && !previous_val {
                // Down
                if let Some(events) = &mut state.events[i] {
                    events.push(Event {
                        timestamp: real_ts,
                        inner: EventKind::Button(ButtonEvent::Down),
                    });
                }
                down[i] = mono_ts;
                down_secs[i] = 0;
            } else if !*new_val && previous_val {
                // Up
                if let Some(events) = &mut state.events[i] {
                    events.push(Event {
                        timestamp: real_ts,
                        inner: EventKind::Button(ButtonEvent::Up),
                    });
                    events.push(Event {
                        timestamp: real_ts,
                        inner: EventKind::Button(ButtonEvent::Clicked { duration: down_for }),
                    });
                }
            } else if *new_val {
                // No change (because other arms didn't match)
                // And was down, so is still down
                // So process this as a long-press
                if down_for.as_secs() > down_secs[i] {
                    down_secs[i] = down_for.as_secs();
                    if let Some(events) = &mut state.events[i] {
                        events.push(Event {
                            timestamp: real_ts,
                            inner: EventKind::Button(ButtonEvent::LongPress {
                                seconds: down_secs[i],
                            }),
                        });
                    }
                }
            }

            state.update_value_and_generate_events(real_ts, i, if *new_val { HIGH } else { LOW });
        }

        Ok(())
    }
}

impl HardwareDevice for MCP23017Input {
    fn update(&self) -> Result<()> {
        // TODO ?
        Ok(())
    }

    fn get_virtual_device(&self, port: u8) -> Result<Box<dyn VirtualDevice + Send>> {
        ensure!(port < 16, "MCP23017 has 16 ports only");
        self.inner.get_virtual_device(port)
    }

    fn get_event_stream(&self, port: u8) -> Result<Pin<Box<dyn Stream<Item = Vec<Event>> + Send>>> {
        ensure!(port < 16, "MCP23017 has 16 ports only");
        self.inner.get_event_stream(port)
    }
}
