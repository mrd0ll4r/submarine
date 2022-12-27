use crate::button_expander::device::{ButtonExpanderData, ButtonExpanderError};
use crate::device::{EventStream, HardwareDevice, InputHardwareDevice};
use crate::device_core::{DeviceReadCore, SynchronizedDeviceReadCore};
use crate::prom;
use crate::{poll, Result};
use alloy::config::{InputValue, InputValueType};
use alloy::event::{ButtonEvent, Event, EventKind};
use embedded_hal as hal;
use failure::*;
use serde::{Deserialize, Serialize};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) struct ButtonExpanderBoard {
    inner: SynchronizedDeviceReadCore,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ButtonExpanderBoardConfig {
    pub i2c_bus: String,
    polling_interval_millis: u64,
}

mod device {
    use embedded_hal as hal;

    const READOUT_PENDING_BIT: u8 = 0x01;
    const WATCHDOG_BIT: u8 = 0x02;

    const BOARD_ADDRESS: u8 = 0x3E;

    /// Errors that may occur when reading the expander.
    #[derive(Fail, Debug, Clone)]
    pub enum ButtonExpanderError {
        /// Occurs for a failed I2C bus operation.
        #[fail(display = "I2C error: {:?}", _0)]
        Bus(String),
    }

    impl ButtonExpanderError {
        fn i2c_error<E: Sync + Send + std::fmt::Debug + 'static>(err: E) -> ButtonExpanderError {
            ButtonExpanderError::Bus(format!("I2C operation failed: {:?}", err))
        }
    }

    pub struct ButtonExpander<I2C> {
        i2c: I2C,
    }

    #[derive(Clone, Debug)]
    pub enum ButtonExpanderData {
        ChecksumError,
        Ok { pin_a: u8, pin_b: u8 },
    }

    #[derive(Clone, Debug)]
    pub struct ButtonExpanderState {
        pub readout_pending: bool,
        pub watchdog_reset: bool,
        pub data: ButtonExpanderData,
    }

    impl<I2C, E> ButtonExpander<I2C>
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        pub(crate) fn new(i2c: I2C) -> ButtonExpander<I2C> {
            ButtonExpander { i2c }
        }

        pub(crate) fn reset_watchdog(&mut self) -> Result<(), ButtonExpanderError> {
            debug!("resetting watchdog on device {:x}", BOARD_ADDRESS);

            let reset_watchdog_bytes: [u8; 2] = [0x00, WATCHDOG_BIT];
            self.i2c
                .write(BOARD_ADDRESS, &reset_watchdog_bytes)
                .map_err(ButtonExpanderError::i2c_error)
        }

        fn read_device(
            &mut self,
            device_address: u8,
            buf: &mut [u8],
        ) -> Result<(), ButtonExpanderError> {
            debug!(
                "reading {} bytes of state for device {:x}",
                buf.len(),
                device_address
            );

            let address: [u8; 1] = [0x00];
            self.i2c
                .write_read(device_address, &address, buf)
                .map_err(ButtonExpanderError::i2c_error)
        }

        fn reorder_pin_positions(mut v1: u8, mut v2: u8) -> (u8, u8) {
            /*
            Pins in the registers:
            0x01:   P09 P08 P03 P02 P01 P15 P14 P13
            0x02:   P12 P11 P10 P07 P06 P05 P04 P00

            P12 is inverted
            P13 is inverted
             */

            // First, re-invert P12 and P13
            v1 = v1 ^ 0b0000_0001;
            v2 = v2 ^ 0b1000_0000;

            // Then extract the bits
            let pin_a =
                // P00
                ((v2 & 0b0000_0001) << 7) |
                    // P01
                    ((v1 & 0b0000_1000) << 3) |
                    // P02
                    ((v1 & 0b0001_0000) << 1) |
                    // P03
                    ((v1 & 0b0010_0000) >> 1) |
                    // P04
                    ((v2 & 0b0000_0010) << 2) |
                    // P05
                    (v2 & 0b0000_0100) |
                    // P06
                    ((v2 & 0b0000_1000) >> 2) |
                    // P07
                    ((v2 & 0b0001_0000) >> 4);

            let pin_b =
                // P08
                ((v1 & 0b0100_0000) << 1) |
                    // P09
                    ((v1 & 0b1000_0000) >> 1) |
                    // P10
                    (v2 & 0b0010_0000) |
                    // P11
                    ((v2 & 0b0100_0000) >> 2) |
                    // P12
                    ((v2 & 0b1000_0000) >> 4) |
                    // P13
                    ((v1 & 0b0000_0001) << 2) |
                    // P14
                    (v1 & 0b0000_0010) |
                    // P15
                    ((v1 & 0b0000_0100) >> 2);

            // Everything is flipped, and the bytes are in the wrong order.
            (!pin_b, !pin_a)
        }

        fn compute_checksum(b1: u8, b2: u8) -> u8 {
            let b2 = b2.rotate_left(3);
            let s = b1 + b2;
            s.rotate_left(3)
        }

        fn decode_single_transmission(transmission: &[u8], shift: u32) -> (u8, u8, bool) {
            assert_eq!(transmission.len(), 3);

            let b1 = transmission[0].rotate_right(shift);
            let b2 = transmission[1].rotate_right(shift);
            let cs = transmission[2].rotate_right(shift);
            let cs_computed = Self::compute_checksum(b1, b2);
            let cs_match = cs == cs_computed;

            trace!("transmission with shift {}: decoded {:?} to {:08b} {:08b} {:08b}, computed checksum {:08b}, match? {}",
                shift,transmission,b1,b2,cs,cs_computed,cs_match);

            let (pin_a, pin_b) = Self::reorder_pin_positions(b1, b2);

            (pin_a, pin_b, cs_match)
        }

        fn try_decode_transmissions(values: &[u8]) -> ButtonExpanderData {
            assert_eq!(values.len(), 9);

            let (t1_a, t1_b, t1_match) = Self::decode_single_transmission(&values[0..3], 0);
            let (t2_a, t2_b, t2_match) = Self::decode_single_transmission(&values[3..6], 3);
            let (t3_a, t3_b, t3_match) = Self::decode_single_transmission(&values[6..9], 6);

            match (t1_match, t2_match, t3_match) {
                (true, _, _) => ButtonExpanderData::Ok {
                    pin_a: t1_a,
                    pin_b: t1_b,
                },
                (_, true, _) => ButtonExpanderData::Ok {
                    pin_a: t2_a,
                    pin_b: t2_b,
                },
                (_, _, true) => ButtonExpanderData::Ok {
                    pin_a: t3_a,
                    pin_b: t3_b,
                },
                (false, false, false) => ButtonExpanderData::ChecksumError,
            }
        }

        pub(crate) fn read_expander_state(
            &mut self,
        ) -> Result<ButtonExpanderState, ButtonExpanderError> {
            let mut buf: [u8; 10] = [0_u8; 10];
            self.read_device(BOARD_ADDRESS, &mut buf)?;

            let readout_pending = (buf[0] & READOUT_PENDING_BIT) != 0;
            let watchdog_reset = (buf[0] & WATCHDOG_BIT) == 0;
            let data = Self::try_decode_transmissions(&buf[1..10]);

            Ok(ButtonExpanderState {
                readout_pending,
                watchdog_reset,
                data,
            })
        }
    }
}

impl ButtonExpanderBoard {
    pub fn new<I2C, E>(
        dev: I2C,
        config: &ButtonExpanderBoardConfig,
        alias: String,
    ) -> Result<ButtonExpanderBoard>
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        ensure!(
            config.polling_interval_millis != 0,
            "polling interval must be greater than zero"
        );
        let mut dev = device::ButtonExpander::new(dev);

        // Try reading it to see if it's actually there.
        let mut state = dev
            .read_expander_state()
            .context("unable to read expander state")?;
        let mut count = 0;
        while state.watchdog_reset {
            count += 1;
            if count > 2 {
                bail!("board has sticky watchdog reset?");
            }
            warn!("board had a watchdog reset, attempting to clear...");
            dev.reset_watchdog()?;
            state = dev
                .read_expander_state()
                .context("unable to read expander state")?;
        }

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
            .name(format!("ButtonExpander {}", alias))
            .spawn(move || {
                Self::run(thread_inner, dev, polling_interval, alias, down_secs, down)
            })?;

        Ok(ButtonExpanderBoard { inner })
    }

    fn run<I2C, E>(
        inner: SynchronizedDeviceReadCore,
        mut dev: device::ButtonExpander<I2C>,
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
        let hist = prom::BUTTON_EXPANDER_READ_DURATION
            .get_metric_with_label_values(&[alias.as_str()])
            .unwrap();
        let reads_counter_ok = prom::BUTTON_EXPANDER_READS
            .get_metric_with_label_values(&[alias.as_str(), "ok"])
            .unwrap();
        let reads_counter_i2c_err = prom::BUTTON_EXPANDER_READS
            .get_metric_with_label_values(&[alias.as_str(), "i2c_error"])
            .unwrap();
        let reads_counter_checksum_err = prom::BUTTON_EXPANDER_READS
            .get_metric_with_label_values(&[alias.as_str(), "checksum_error"])
            .unwrap();
        let reads_counter_watchdog_reset = prom::BUTTON_EXPANDER_READS
            .get_metric_with_label_values(&[alias.as_str(), "watchdog_reset"])
            .unwrap();

        let mut consecutive_i2c_errors = 0;

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

            let before = Instant::now();
            let state = dev.read_expander_state();
            let after = Instant::now();
            hist.observe(after.duration_since(before).as_micros() as f64);
            trace!("{}: read state {:?}", alias, state);

            match state {
                Ok(state) => {
                    consecutive_i2c_errors = 0;
                    if state.watchdog_reset {
                        reads_counter_watchdog_reset.inc();
                        let mut state = state;
                        while state.watchdog_reset {
                            warn!("board had a watchdog reset, attempting to clear...");
                            let res = dev.reset_watchdog();
                            if let Err(e) = res {
                                warn!("unable to clear watchdog reset: {:?}", e);
                                continue;
                            }

                            let state_res = dev.read_expander_state();
                            match state_res {
                                Ok(state_ok) => state = state_ok,
                                Err(err) => {
                                    warn!("unable to read state: {:?}", err);
                                }
                            }
                        }
                    } else {
                        match state.data {
                            ButtonExpanderData::Ok { pin_a, pin_b } => {
                                reads_counter_ok.inc();

                                let vals = (pin_a as u16) << 8 | pin_b as u16;
                                trace!("{}: read values: {:#018b}", alias, vals);

                                let mut core = inner.core.lock().unwrap();

                                // Get new values, generate events
                                ButtonExpanderBoard::update_inner(
                                    &mut core,
                                    wake_up,
                                    ts,
                                    vals,
                                    &mut down,
                                    &mut down_secs,
                                );
                            }
                            ButtonExpanderData::ChecksumError => {
                                reads_counter_checksum_err.inc();
                                warn!("{}: got checksum mismatch, ignoring readout", alias)
                                // We do not set an error on all ports, because that clears prior values.
                                // Instead, we just leave everything as it is, and try again next readout.
                                // This way, no (human-caused) events should get lost.
                                // On the downside, we might miscalculate some button_pressed duration, but
                                // only by a few milliseconds..
                            }
                        }
                    }
                }
                Err(err) => match err {
                    ButtonExpanderError::Bus(e) => {
                        reads_counter_i2c_err.inc();
                        warn!("{}: unable to read values: {:?}", alias, e);
                        // We ignore this for now until maybe some day we find what's actually wrong.
                        // TODO fixme
                        consecutive_i2c_errors += 1;
                        if consecutive_i2c_errors >= 10000 {
                            // It's probably dead.
                            let mut core = inner.core.lock().unwrap();
                            core.set_error_on_all_ports(ts, format!("{:?}", e))
                        }
                    }
                },
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

    fn update_inner(
        core: &mut DeviceReadCore,
        mono_ts: Instant,
        real_ts: chrono::DateTime<chrono::Utc>,
        vals: u16,
        down: &mut [Instant; 16],
        down_secs: &mut [u64; 16],
    ) {
        let mut new_values = [false; 16];
        for (i, val) in new_values.iter_mut().enumerate() {
            *val = (vals & (0b1 << i)) != 0
        }

        for (i, new_val) in new_values.iter().enumerate() {
            if let Some(Ok(previous_val)) = core.device_values[i] {
                let previous_val = previous_val == InputValue::Binary(true);
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

            core.update_value_and_generate_events(real_ts, i, Ok(InputValue::Binary(*new_val)));
        }
    }
}

impl InputHardwareDevice for ButtonExpanderBoard {
    fn get_input_port(&self, port: u8) -> Result<(InputValueType, EventStream)> {
        ensure!(port < 16, "button expander has 16 ports only");
        self.inner.get_input_port(port)
    }
}

impl HardwareDevice for ButtonExpanderBoard {
    fn port_alias(&self, port: u8) -> Result<String> {
        ensure!(port < 16, "button expander has 16 ports only");
        Ok(format!("port-{}", port))
    }
}
