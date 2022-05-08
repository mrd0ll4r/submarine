use crate::device::{EventStream, HardwareDevice, InputHardwareDevice};
use crate::device_core::{DeviceReadCore, SynchronizedDeviceReadCore};
use crate::mcp23017::{MCP23017Config, MCP23017};
use crate::prom;
use crate::{poll, Result};
use alloy::config::{InputValue, InputValueType};
use alloy::event::{ButtonEvent, Event, EventKind};
use embedded_hal as hal;
use failure::*;
use mcp23017 as mcpdev;
use serde::{Deserialize, Serialize};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) struct MCP23017Input {
    inner: SynchronizedDeviceReadCore,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MCP23017InputConfig {
    pub i2c_bus: String,
    i2c_slave_address: u8,
    polling_interval_millis: u64,
    enable_pullups: bool,
    invert: bool,
}

#[derive(Fail, Debug)]
enum MCPInputError<E>
where
    E: Sync + Send + std::fmt::Debug + 'static,
{
    #[fail(display = "Suspicious all-one readout")]
    SuspiciousValue,
    #[fail(display = "I2C error: {:?}", _0)]
    I2C(E),
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
        for i in 0..16 {
            mcp.pull_up(i, config.enable_pullups)
                .map_err(|e| failure::err_msg(format!("{:?}", e)))?;
        }
        for i in 0..16 {
            mcp.invert_input_polarity(i, config.invert)
                .map_err(|e| failure::err_msg(format!("{:?}", e)))?;
        }
        mcp.all_pin_mode(mcpdev::PinMode::INPUT)
            .map_err(|e| failure::err_msg(format!("{:?}", e)))?;

        let inner = SynchronizedDeviceReadCore::new_from_core(DeviceReadCore::new(
            alias.clone(),
            &std::iter::repeat(InputValueType::Binary)
                .take(16)
                .collect::<Vec<_>>(),
        ));
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
            .get_metric_with_label_values(&[alias.as_str()])
            .unwrap();
        let reads_counter_ok = prom::MCP23017_READS
            .get_metric_with_label_values(&[alias.as_str(), "ok"])
            .unwrap();
        let reads_counter_i2c_err = prom::MCP23017_READS
            .get_metric_with_label_values(&[alias.as_str(), "i2c_error"])
            .unwrap();
        let reads_counter_suspicious_value = prom::MCP23017_READS
            .get_metric_with_label_values(&[alias.as_str(), "suspicious_value"])
            .unwrap();

        loop {
            thread::sleep(sleep_duration);
            let wake_up = Instant::now();
            let ts = chrono::Utc::now();
            trace!(
                "{}: {} µs since last loop",
                alias,
                (wake_up - last_wakeup).as_micros()
            );
            last_wakeup = wake_up;

            let mut core = inner.core.lock().unwrap();

            // Get new values, generate events
            let res = MCP23017Input::update_inner(
                &mut core,
                wake_up,
                ts.clone(),
                &mut dev,
                &hist,
                &mut down,
                &mut down_secs,
                &alias,
            );

            if let Err(e) = res {
                match e {
                    MCPInputError::SuspiciousValue => {
                        reads_counter_suspicious_value.inc();
                        warn!("{}: got suspicious all-one reading. Ignoring.", alias)
                        // We do not set an error on all ports, because that clears prior values.
                        // Instead, we just leave everything as it is, and try again next readout.
                        // This way, no (human-caused) events should get lost.
                        // On the downside, we might miscalculate some button_pressed duration, but
                        // only by a few milliseconds..
                    }
                    MCPInputError::I2C(e) => {
                        reads_counter_i2c_err.inc();
                        warn!("{}: unable to read values: {:?}", alias, e);
                        core.set_error_on_all_ports(ts, format!("{:?}", e))
                    }
                }
            } else {
                reads_counter_ok.inc();
            }

            let (s, lagging) = poll::calculate_sleep_duration(wake_up, polling_interval);
            if lagging > Duration::from_secs(0) {
                warn!(
                    "{}: took too long, lagging {}µs behind",
                    alias,
                    lagging.as_micros()
                );
            }
            sleep_duration = s;
        }
    }

    fn update_inner<I2C, E>(
        core: &mut DeviceReadCore,
        mono_ts: Instant,
        real_ts: chrono::DateTime<chrono::Utc>,
        dev: &mut mcpdev::MCP23017<I2C>,
        hist: &prometheus::Histogram,
        down: &mut [Instant; 16],
        down_secs: &mut [u64; 16],
        alias: &str,
    ) -> std::result::Result<(), MCPInputError<E>>
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        let before = Instant::now();
        let vals = dev.read_gpioab().map_err(|err| MCPInputError::I2C(err))?;
        let after = Instant::now();
        hist.observe(after.duration_since(before).as_micros() as f64);
        debug!(
            "{}: read_gpioab took {}µs => {}KiBit/s",
            alias,
            after.duration_since(before).as_micros(),
            ((2 * 8) as f64 / after.duration_since(before).as_secs_f64()) / 1024_f64
        );

        if vals == 0b1111_1111_1111_1111 {
            return Err(MCPInputError::SuspiciousValue);
        }

        let (high, low) = (((vals >> 8) & 0xFF) as u8, (vals & 0xFF) as u8);
        let vals = (low as u16) << 8 | high as u16;
        trace!("{}: read values: {:#018b}", alias, vals);

        let mut new_values = [false; 16];
        for (i, val) in new_values.iter_mut().enumerate() {
            *val = (vals & (0b1 << i)) != 0
        }

        for (i, new_val) in new_values.iter().enumerate() {
            if let Some(ref previous_val) = core.device_values[i] {
                if let Ok(previous_val) = previous_val {
                    let previous_val = *previous_val == InputValue::Binary(true);
                    let down_for = mono_ts.duration_since(down[i]);

                    if *new_val && !previous_val {
                        // Down
                        if let Some(events) = &mut core.events[i] {
                            events.push_back(Event {
                                timestamp: real_ts,
                                inner: Ok(EventKind::Button(ButtonEvent::Down)),
                            });
                        }
                        down[i] = mono_ts;
                        down_secs[i] = 0;
                    } else if !*new_val && previous_val {
                        // Up
                        if let Some(events) = &mut core.events[i] {
                            events.push_back(Event {
                                timestamp: real_ts,
                                inner: Ok(EventKind::Button(ButtonEvent::Up)),
                            });
                            events.push_back(Event {
                                timestamp: real_ts,
                                inner: Ok(EventKind::Button(ButtonEvent::Clicked {
                                    duration: down_for,
                                })),
                            });
                        }
                    } else if *new_val {
                        // No change (because other arms didn't match)
                        // And was down, so is still down
                        // So process this as a long-press
                        if down_for.as_secs() > down_secs[i] {
                            down_secs[i] = down_for.as_secs();
                            if let Some(events) = &mut core.events[i] {
                                events.push_back(Event {
                                    timestamp: real_ts,
                                    inner: Ok(EventKind::Button(ButtonEvent::LongPress {
                                        seconds: down_secs[i],
                                    })),
                                });
                            }
                        }
                    }
                }
            }

            core.update_value_and_generate_events(real_ts, i, Ok(InputValue::Binary(*new_val)));
        }

        Ok(())
    }
}

impl InputHardwareDevice for MCP23017Input {
    fn get_input_port(&self, port: u8) -> Result<(InputValueType, EventStream)> {
        ensure!(port < 16, "MCP23017 has 16 ports only");
        self.inner.get_input_port(port)
    }
}

impl HardwareDevice for MCP23017Input {
    fn port_alias(&self, port: u8) -> Result<String> {
        MCP23017::compute_port_alias(port)
    }
}
