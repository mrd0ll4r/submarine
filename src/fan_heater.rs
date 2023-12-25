use crate::config::ValueScaling;
use crate::device::{
    EventStream, HardwareDevice, InputHardwareDevice, OutputHardwareDevice, OutputPort,
};
use crate::device_core::{
    DeviceRWCore, DeviceReadCore, SynchronizedDeviceRWCore, SynchronizedDeviceReadCore,
};
use crate::fan_heater::device::{
    FanBoardState, FanHeaterError, HeaterBoardState, FAN_BOARD_MAX_AIR_IN_LEVEL,
    FAN_BOARD_MAX_AIR_OUT_LEVEL, HEATER_BOARD_MAX_HEATING_LEVEL,
};
use crate::{poll, prom, Result};
use alloy::config::{InputValue, InputValueType};
use alloy::{OutputValue, HIGH, LOW};
use anyhow::{anyhow, bail, ensure};
use embedded_hal as hal;
use log::{debug, error, trace, warn};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::thread;
use std::time::{Duration, Instant};

/// Configuration for the custom heater and fan control boards.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct FanHeaterConfig {
    pub i2c_bus: String,
    update_interval_seconds: u8,
}

mod device {
    use embedded_hal as hal;
    use log::debug;

    const WATCHDOG_BIT: u8 = 0x02;

    const FAN_BOARD_ADDRESS: u8 = 0x22;
    const FAN_BOARD_I2C_DISABLED_BIT: u8 = 0x01;
    pub const FAN_BOARD_MAX_AIR_IN_LEVEL: u8 = 6;
    pub const FAN_BOARD_MAX_AIR_OUT_LEVEL: u8 = 4;

    const HEATER_BOARD_ADDRESS: u8 = 0x23;
    const HEATER_BOARD_HEATING_DISABLED_BIT: u8 = 0x01;
    const HEATER_BOARD_HEATER_MASK: u8 = 0b0011_1111;
    const HEATER_BOARD_RELAY_1_MASK: u8 = 0b0100_0000;
    const HEATER_BOARD_RELAY_2_MASK: u8 = 0b1000_0000;
    pub const HEATER_BOARD_MAX_HEATING_LEVEL: u8 = 30;

    /// Errors that may occur when reading the expander.
    #[derive(Debug, Clone)]
    pub enum FanHeaterError {
        /// Occurs for a failed I2C bus operation.
        Bus(String),

        /// Occurs if the fan board misbehaves.
        BadFanBoard(String),

        /// Occurs if the heater board misbehaves.
        BadHeaterBoard(String),
    }

    impl FanHeaterError {
        fn i2c_error<E: Sync + Send + std::fmt::Debug + 'static>(err: E) -> FanHeaterError {
            FanHeaterError::Bus(format!("I2C operation failed: {:?}", err))
        }
    }

    pub struct FanHeaterBoard<I2C> {
        i2c: I2C,
    }

    #[derive(Clone, Debug)]
    pub struct FanBoardState {
        pub i2c_disabled: bool,
        pub watchdog_reset: bool,
        pub air_in: u8,
        pub air_out: u8,
    }

    #[derive(Clone, Debug)]
    pub struct HeaterBoardState {
        pub heater_disabled: bool,
        pub watchdog_reset: bool,
        pub heater_level: u8,
        pub relay_1: bool,
        pub relay_2: bool,
    }

    impl<I2C, E> FanHeaterBoard<I2C>
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        pub(crate) fn new(i2c: I2C) -> FanHeaterBoard<I2C> {
            FanHeaterBoard { i2c }
        }

        fn reset_watchdog(&mut self, device_address: u8) -> Result<(), FanHeaterError> {
            debug!("resetting watchdog on device {:x}", device_address);

            let reset_watchdog_bytes: [u8; 2] = [0x00, WATCHDOG_BIT];
            self.i2c
                .write(device_address, &reset_watchdog_bytes)
                .map_err(FanHeaterError::i2c_error)
        }

        pub(crate) fn reset_fan_board_watchdog(&mut self) -> Result<(), FanHeaterError> {
            self.reset_watchdog(FAN_BOARD_ADDRESS)
        }

        pub(crate) fn reset_heater_board_watchdog(&mut self) -> Result<(), FanHeaterError> {
            self.reset_watchdog(HEATER_BOARD_ADDRESS)
        }

        fn read_device(
            &mut self,
            device_address: u8,
            buf: &mut [u8],
        ) -> Result<(), FanHeaterError> {
            debug!(
                "reading {} bytes of state for device {:x}",
                buf.len(),
                device_address
            );

            let address: [u8; 1] = [0x00];
            self.i2c
                .write_read(device_address, &address, buf)
                .map_err(FanHeaterError::i2c_error)
        }

        pub(crate) fn read_fan_state(&mut self) -> Result<FanBoardState, FanHeaterError> {
            let mut buf: [u8; 3] = [0_u8; 3];
            self.read_device(FAN_BOARD_ADDRESS, &mut buf)?;

            let i2c_disabled = (buf[0] & FAN_BOARD_I2C_DISABLED_BIT) != 0;
            let watchdog_reset = (buf[0] & WATCHDOG_BIT) == 0;
            let air_in = buf[1];
            let air_out = buf[2];

            if air_in > FAN_BOARD_MAX_AIR_IN_LEVEL {
                return Err(FanHeaterError::BadFanBoard(format!(
                    "bad air in level: {}",
                    air_in
                )));
            }
            if air_out > FAN_BOARD_MAX_AIR_OUT_LEVEL {
                return Err(FanHeaterError::BadFanBoard(format!(
                    "bad air out level: {}",
                    air_out
                )));
            }

            Ok(FanBoardState {
                i2c_disabled,
                watchdog_reset,
                air_in,
                air_out,
            })
        }

        pub(crate) fn read_heater_state(&mut self) -> Result<HeaterBoardState, FanHeaterError> {
            let mut buf: [u8; 2] = [0_u8; 2];
            self.read_device(HEATER_BOARD_ADDRESS, &mut buf)?;

            let heater_disabled = (buf[0] & HEATER_BOARD_HEATING_DISABLED_BIT) != 0;
            let watchdog_reset = (buf[0] & WATCHDOG_BIT) == 0;
            let heater_level = buf[1] & HEATER_BOARD_HEATER_MASK;
            let relay_1 = (buf[1] & HEATER_BOARD_RELAY_1_MASK) != 0;
            let relay_2 = (buf[1] & HEATER_BOARD_RELAY_2_MASK) != 0;

            if heater_level > HEATER_BOARD_MAX_HEATING_LEVEL {
                return Err(FanHeaterError::BadHeaterBoard(format!(
                    "bad heater level: {}",
                    heater_level
                )));
            }

            Ok(HeaterBoardState {
                heater_disabled,
                watchdog_reset,
                heater_level,
                relay_1,
                relay_2,
            })
        }

        fn write_state(&mut self, device_address: u8, buf: &[u8]) -> Result<(), FanHeaterError> {
            debug!("writing state {:?} for device {:x}", buf, device_address);

            let mut addressed_state = Vec::with_capacity(buf.len() + 1);
            addressed_state.push(1_u8);
            addressed_state.extend_from_slice(buf);
            assert_eq!(addressed_state.len(), buf.len() + 1);

            self.i2c
                .write(device_address, &addressed_state)
                .map_err(FanHeaterError::i2c_error)
        }

        pub(crate) fn write_fan_state(
            &mut self,
            air_in: u8,
            air_out: u8,
        ) -> Result<(), FanHeaterError> {
            debug!("writing fan state in={}, out={}", air_in, air_out);
            assert!(air_in <= FAN_BOARD_MAX_AIR_IN_LEVEL);
            assert!(air_out <= FAN_BOARD_MAX_AIR_OUT_LEVEL);

            let state: [u8; 2] = [air_in, air_out];
            self.write_state(FAN_BOARD_ADDRESS, &state)
        }

        pub(crate) fn write_heater_state(
            &mut self,
            heater_level: u8,
            relay_1: bool,
            relay_2: bool,
        ) -> Result<(), FanHeaterError> {
            debug!(
                "writing heater state heater={}, relay_1={}, relay_2={}",
                heater_level, relay_1, relay_2
            );
            assert!(heater_level <= HEATER_BOARD_MAX_HEATING_LEVEL);

            let mut state = heater_level & HEATER_BOARD_HEATER_MASK;
            if relay_1 {
                state |= HEATER_BOARD_RELAY_1_MASK
            };
            if relay_2 {
                state |= HEATER_BOARD_RELAY_2_MASK
            };
            let state: [u8; 1] = [state];
            self.write_state(HEATER_BOARD_ADDRESS, &state)
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct FanHeater {
    inner: SynchronizedDeviceRWCore,
    inner_read: SynchronizedDeviceReadCore,
}

impl FanHeater {
    pub(crate) fn new<I2C, E>(dev: I2C, alias: String, cfg: &FanHeaterConfig) -> Result<FanHeater>
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        ensure!(
            cfg.update_interval_seconds >= 1,
            "update_interval_seconds must be at least 1"
        );

        let inner = SynchronizedDeviceRWCore::new_from_core(DeviceRWCore::new(
            alias.clone(),
            // In/out ventilation + heating + 2 relays
            2 + 1 + 2,
            ValueScaling::default(),
        ));
        let inner_read = SynchronizedDeviceReadCore::new_from_core(DeviceReadCore::new(
            alias.clone(),
            &std::iter::repeat(InputValueType::Binary)
                .take(2)
                .collect::<Vec<_>>(),
        ));

        // Create device and read both boards to see if they're actually there.
        let mut dev = device::FanHeaterBoard::new(dev);
        let mut fan_board_state = dev.read_fan_state().map_err(|err| anyhow!("{:?}", err))?;
        let mut count = 0;
        while fan_board_state.watchdog_reset {
            count += 1;
            if count > 2 {
                bail!("fan board has sticky watchdog reset?");
            }
            warn!("fan board had a watchdog reset, attempting to clear...");
            dev.reset_fan_board_watchdog()
                .map_err(|err| anyhow!("{:?}", err))?;
            fan_board_state = dev.read_fan_state().map_err(|err| anyhow!("{:?}", err))?;
        }

        let mut heater_board_state = dev
            .read_heater_state()
            .map_err(|err| anyhow!("{:?}", err))?;
        count = 0;
        while heater_board_state.watchdog_reset {
            count += 1;
            if count > 2 {
                bail!("heater board has sticky watchdog reset?");
            }
            warn!("heater board had a watchdog reset, attempting to clear...");
            dev.reset_heater_board_watchdog()
                .map_err(|err| anyhow!("{:?}", err))?;
            heater_board_state = dev
                .read_heater_state()
                .map_err(|err| anyhow!("{:?}", err))?;
        }

        // Launch a thread to do the work.
        let thread_core = inner.clone();
        let thread_read_core = inner_read.clone();
        let update_interval = cfg.update_interval_seconds;
        thread::Builder::new()
            .name("FanHeater".to_string())
            .spawn(move || Self::run(thread_core, thread_read_core, dev, update_interval, alias))?;

        Ok(FanHeater { inner, inner_read })
    }

    fn run<I2C, E>(
        inner: SynchronizedDeviceRWCore,
        inner_read: SynchronizedDeviceReadCore,
        mut dev: device::FanHeaterBoard<I2C>,
        polling_interval_seconds: u8,
        alias: String,
    ) where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        let ok_counter = prom::FAN_HEATER_WRITES.with_label_values(&[alias.as_str(), "ok"]);
        let readout_i2c_error_counter =
            prom::FAN_HEATER_WRITES.with_label_values(&[alias.as_str(), "i2c_error"]);
        let readout_bad_fan_board_error_counter =
            prom::FAN_HEATER_WRITES.with_label_values(&[alias.as_str(), "bad_fan_board"]);
        let readout_bad_heater_board_error_counter =
            prom::FAN_HEATER_WRITES.with_label_values(&[alias.as_str(), "bad_heater_board"]);
        let readout_fan_board_watchdog_reset_error_counter = prom::FAN_HEATER_WRITES
            .with_label_values(&[alias.as_str(), "fan_board_watchdog_reset"]);
        let readout_heater_board_watchdog_reset_error_counter = prom::FAN_HEATER_WRITES
            .with_label_values(&[alias.as_str(), "heater_board_watchdog_reset"]);

        // De-sync in case we have multiple of these.
        let millis = rand::thread_rng().gen_range(0..1000);
        debug!("will sleep {}ms to de-sync", millis);
        thread::sleep(Duration::from_millis(millis));

        let polling_interval = Duration::from_secs(polling_interval_seconds as u64);
        let mut last_wakeup = Instant::now();
        let mut next_bonus_sleep = None;

        loop {
            // Calculate next sleep.
            let bonus_sleep = next_bonus_sleep.take().unwrap_or(Duration::from_secs(0));
            let (s, lagging) =
                poll::calculate_sleep_duration(last_wakeup, polling_interval + bonus_sleep);
            if lagging > Duration::from_secs(0) {
                warn!(
                    "{}: took too long, lagging {}µs behind",
                    alias,
                    lagging.as_micros()
                );
            }

            // Sleep
            thread::sleep(s);
            let wake_up = Instant::now();
            trace!(
                "{}: {} µs since last loop",
                alias,
                (wake_up - last_wakeup).as_micros()
            );
            last_wakeup = wake_up;

            // Get values to write
            let values = {
                let core = inner.core.lock().unwrap();
                core.buffered_values.clone()
            };
            debug!("{}: got buffered output values {:?}", alias, values);
            let (air_in, air_out, heater_level, relay_1, relay_2) =
                Self::states_from_output_values(values);
            debug!("{}: calculated output values to write: air_in={}, air_out={}, heater_level={}, relay_1={}, relay_2={}",alias,air_in,air_out,heater_level,relay_1,relay_2);

            // Attempt write, then read current states
            let res = match Self::do_stuff(
                &mut dev,
                &alias,
                air_in,
                air_out,
                heater_level,
                relay_1,
                relay_2,
            ) {
                Ok((fan_state, heater_state)) => {
                    debug!(
                        "{}: wrote values, got readouts: {:?}, {:?}",
                        alias, fan_state, heater_state
                    );

                    // If watchdog reset: Reset watchdog, mark error
                    // Else: generate events for device values
                    if fan_state.watchdog_reset {
                        readout_fan_board_watchdog_reset_error_counter.inc();
                        if let Err(err) = dev.reset_fan_board_watchdog() {
                            error!("{}: unable to reset fan board watchdog: {:?}", alias, err);
                        }
                        Err(anyhow!("fan board watchdog reset"))
                    } else if heater_state.watchdog_reset {
                        readout_heater_board_watchdog_reset_error_counter.inc();
                        if let Err(err) = dev.reset_heater_board_watchdog() {
                            error!(
                                "{}: unable to reset heater board watchdog: {:?}",
                                alias, err
                            );
                        }
                        Err(anyhow!("heater board watchdog reset"))
                    } else {
                        ok_counter.inc();
                        Ok((
                            vec![
                                fan_state.air_in as OutputValue,
                                fan_state.air_out as OutputValue,
                                heater_state.heater_level as OutputValue,
                                if heater_state.relay_1 { HIGH } else { LOW },
                                if heater_state.relay_2 { HIGH } else { LOW },
                            ],
                            (
                                InputValue::Binary(fan_state.i2c_disabled),
                                InputValue::Binary(heater_state.heater_disabled),
                            ),
                        ))
                    }
                }
                Err(err) => {
                    error!(
                        "{}: unable to read/write fan/heater combo: {:?}",
                        alias, err
                    );
                    match &err {
                        FanHeaterError::Bus(_) => readout_i2c_error_counter.inc(),
                        FanHeaterError::BadFanBoard(_) => {
                            readout_bad_fan_board_error_counter.inc();
                        }
                        FanHeaterError::BadHeaterBoard(_) => {
                            readout_bad_heater_board_error_counter.inc();
                        }
                    }
                    Err(anyhow!("unable to write/read values: {:?}", err))
                }
            };

            let (rw_res, r_res) = match res {
                Ok((rw_res, r_res)) => (Ok(rw_res), Ok(r_res)),
                Err(err) => (Err(anyhow!("{:?}", err)), Err(err)),
            };

            let ts = chrono::Utc::now();
            {
                let mut core = inner.core.lock().unwrap();
                core.finish_update(rw_res, ts, false);
            }
            {
                let mut read_core = inner_read.core.lock().unwrap();
                match r_res {
                    Ok((i2c_disabled, heater_disabled)) => {
                        read_core.update_value_and_generate_events(ts, 0, Ok(i2c_disabled), false);
                        read_core.update_value_and_generate_events(
                            ts,
                            1,
                            Ok(heater_disabled),
                            false,
                        );
                    }
                    Err(err) => {
                        let err = format!("{:?}", err);
                        read_core.set_error_on_all_ports(ts, err);
                    }
                }
            }
        }
    }

    fn do_stuff<I2C, E>(
        dev: &mut device::FanHeaterBoard<I2C>,
        alias: &str,
        air_in: u8,
        air_out: u8,
        heater_level: u8,
        relay_1: bool,
        relay_2: bool,
    ) -> std::result::Result<(FanBoardState, HeaterBoardState), FanHeaterError>
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        debug!("{}: writing fan state", alias);
        dev.write_fan_state(air_in, air_out)?;
        debug!("{}: wrote fan state, writing heater state", alias);
        dev.write_heater_state(heater_level, relay_1, relay_2)?;
        debug!("{}: wrote heater state", alias);

        debug!("{}: reading fan state", alias);
        let fan_board_state = dev.read_fan_state()?;
        debug!("{}: read fan state {:?}", alias, fan_board_state);

        debug!("{}: reading heater state", alias);
        let heater_board_state = dev.read_heater_state()?;
        debug!("{}: read heater state {:?}", alias, heater_board_state);

        Ok((fan_board_state, heater_board_state))
    }

    fn states_from_output_values(values: Vec<OutputValue>) -> (u8, u8, u8, bool, bool) {
        assert_eq!(values.len(), 5);

        let air_in = if values[0] > FAN_BOARD_MAX_AIR_IN_LEVEL as u16 {
            FAN_BOARD_MAX_AIR_IN_LEVEL
        } else {
            values[0] as u8
        };
        let air_out = if values[1] > FAN_BOARD_MAX_AIR_OUT_LEVEL as u16 {
            FAN_BOARD_MAX_AIR_OUT_LEVEL
        } else {
            values[1] as u8
        };
        let heater_level = if values[2] > HEATER_BOARD_MAX_HEATING_LEVEL as u16 {
            HEATER_BOARD_MAX_HEATING_LEVEL
        } else {
            values[2] as u8
        };
        let relay_1 = values[3] != 0;
        let relay_2 = values[4] != 0;

        (air_in, air_out, heater_level, relay_1, relay_2)
    }
}

impl HardwareDevice for FanHeater {
    fn port_alias(&self, port: u8) -> Result<String> {
        match port {
            0 => Ok(format!("fan-in-level")),
            1 => Ok(format!("fan-out-level")),
            2 => Ok(format!("heater-level")),
            3 => Ok(format!("heater-relay-1")),
            4 => Ok(format!("heater-relay-2")),
            5 => Ok(format!("fan-i2c-disabled")),
            6 => Ok(format!("heater-disabled")),
            _ => Err(anyhow!("Fan/Heater boards have 7 ports")),
        }
    }
}

impl OutputHardwareDevice for FanHeater {
    fn update(&self) -> Result<()> {
        // nop
        Ok(())
    }

    fn get_output_port(
        &self,
        port: u8,
        scaling: Option<ValueScaling>,
    ) -> Result<(Box<dyn OutputPort>, EventStream)> {
        ensure!(port < 5, "Fan/Heater boards have 5 I/O ports");
        self.inner.get_output_port(port, scaling)
    }
}

impl InputHardwareDevice for FanHeater {
    fn get_input_port(&self, port: u8) -> Result<(InputValueType, EventStream)> {
        ensure!(
            port >= 5 && port < 7,
            "Fan/Heater boards have two readonly ports: 5=>fan-i2c-disabled, 6=>heater-disabled"
        );
        self.inner_read.get_input_port(port - 5)
    }
}
