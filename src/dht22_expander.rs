use crate::device::{EventStream, HardwareDevice, InputHardwareDevice};
use crate::device_core::{DeviceReadCore, SynchronizedDeviceReadCore};
use crate::{dht22_lib, poll, prom, Result};
use alloy::config::{InputValue, InputValueType};
use embedded_hal as hal;
use failure::err_msg;
use prometheus::core::{AtomicU64, GenericCounter};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::thread;
use std::time::{Duration, Instant};

/// Configuration for the I2C-to-DHT expander.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct DHT22ExpanderConfig {
    pub i2c_bus: String,
    readout_interval_seconds: u8,
    i2c_slave_address: u8,
    power_cycle_every_n_readouts: Option<u16>,
    power_cycle_power_off_seconds: Option<u8>,
}

impl DHT22ExpanderConfig {
    pub(crate) fn slave_address(&self) -> Result<u8> {
        ensure!(self.i2c_slave_address == 0x3d, "invalid address");

        Ok(self.i2c_slave_address)
    }
}

/// Errors that may occur when reading the expander.
#[derive(Debug, Clone)]
pub enum ExpanderError {
    /// Occurs for a failed I2C bus operation.
    Bus(String),

    /// Occurs when the expander indicates the readout is still in progress at the time the data
    /// is being read out.
    ExpanderReadoutNotFinished,

    /// Occurs when the expander indicates that it has been reset by the watchdog.
    ExpanderWatchdogReset,

    /// Occurs if the expander misbehaves.
    BadExpander(String),
}

impl ExpanderError {
    fn i2c_error<E: Sync + Send + std::fmt::Debug + 'static>(err: E) -> ExpanderError {
        ExpanderError::Bus(format!("I2C operation failed: {:?}", err))
    }
}

/// Errors that may occur when interpreting the data for one sensor attached to the expander.
#[derive(Debug, Clone, Copy)]
pub enum ExpanderSensorError {
    /// Occurs if a timeout occurred reading the pin.
    Timeout,

    /// Occurs if the checksum value from the DHT22 is incorrect.
    Checksum,

    /// Occurs if no start condition from the sensor was detected.
    /// This can also mean that no sensor is attached to that port.
    NoStartCondition,

    /// Occurs if a suspicious value is returned from the sensor.
    /// This is currently detected as humidity>100%.
    SuspiciousValue,
}

pub(crate) struct DHT22Expander {
    inner: SynchronizedDeviceReadCore,
}

impl DHT22Expander {
    pub(crate) fn new<I2C, E>(
        dev: I2C,
        alias: String,
        cfg: &DHT22ExpanderConfig,
    ) -> Result<DHT22Expander>
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        ensure!(
            cfg.readout_interval_seconds >= 2,
            "readout_interval_seconds must be at least 2"
        );
        let addr = cfg.slave_address()?;
        match cfg.power_cycle_every_n_readouts {
            None => {
                warn!(
                    "{}: power_cycle_every_n_readouts missing, power cycling disabled",
                    alias
                )
            }
            Some(power_cycle_interval) => {
                ensure!(power_cycle_interval > 0,
                "power_cycle_every_n_readouts must be greater zero, or not specified to disable power cycling");

                ensure!(
                    cfg.power_cycle_power_off_seconds.is_some(),
                    "power_cycle_power_off_seconds must be set if power cycling is enabled"
                )
            }
        }

        let inner = SynchronizedDeviceReadCore::new_from_core(DeviceReadCore::new(
            alias.clone(),
            &std::iter::repeat([InputValueType::Temperature, InputValueType::Humidity])
                .take(16)
                .flatten()
                .collect::<Vec<_>>(),
        ));

        // Launch a thread to do the work.
        let thread_core = inner.clone();
        let readout_interval = cfg.readout_interval_seconds;
        let power_cycle_interval = cfg.power_cycle_every_n_readouts;
        let power_cycle_power_down_seconds = cfg.power_cycle_power_off_seconds.unwrap_or(0);
        thread::Builder::new()
            .name(format!("DHT22Expander {}", alias))
            .spawn(move || {
                DHT22Expander::run(
                    thread_core,
                    dev,
                    addr,
                    readout_interval,
                    power_cycle_interval,
                    power_cycle_power_down_seconds,
                    alias,
                )
            })?;

        Ok(DHT22Expander { inner })
    }

    fn run<I2C, E>(
        mut inner: SynchronizedDeviceReadCore,
        mut dev: I2C,
        device_address: u8,
        polling_interval_seconds: u8,
        power_cycle_every_n_readouts: Option<u16>,
        power_cycle_power_down_seconds: u8,
        alias: String,
    ) where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        let ok_counter = prom::DHT22_MEASUREMENTS.with_label_values(&[alias.as_str(), "ok"]);
        let checksum_error_counter =
            prom::DHT22_MEASUREMENTS.with_label_values(&[alias.as_str(), "checksum_error"]);
        let timeout_error_counter =
            prom::DHT22_MEASUREMENTS.with_label_values(&[alias.as_str(), "timeout_error"]);
        let no_start_condition_error_counter =
            prom::DHT22_MEASUREMENTS.with_label_values(&[alias.as_str(), "no_start_condition"]);
        let suspicious_value_counter =
            prom::DHT22_MEASUREMENTS.with_label_values(&[alias.as_str(), "suspicious_value"]);
        let readout_ok_counter =
            prom::DHT22_EXPANDER_READOUTS.with_label_values(&[alias.as_str(), "ok"]);
        let readout_i2c_error_counter =
            prom::DHT22_EXPANDER_READOUTS.with_label_values(&[alias.as_str(), "i2c_error"]);
        let readout_bad_expander_error_counter =
            prom::DHT22_EXPANDER_READOUTS.with_label_values(&[alias.as_str(), "bad_expander"]);
        let readout_not_finished_error_counter = prom::DHT22_EXPANDER_READOUTS
            .with_label_values(&[alias.as_str(), "readout_not_finished"]);
        let readout_watchdog_reset_error_counter =
            prom::DHT22_EXPANDER_READOUTS.with_label_values(&[alias.as_str(), "watchdog_reset"]);

        // De-sync in case we have multiple of these.
        let millis = rand::thread_rng().gen_range(0..1000);
        debug!("will sleep {}ms to de-sync", millis);
        thread::sleep(Duration::from_millis(millis));

        let polling_interval = Duration::from_secs(polling_interval_seconds as u64);
        let mut last_wakeup = Instant::now();
        let mut next_bonus_sleep = None;
        let mut readout_counter = 0;

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

            readout_counter += 1;

            // Check if we need to power cycle.
            if let Some(n) = power_cycle_every_n_readouts {
                if readout_counter >= n {
                    debug!("{}: attempting to power cycle sensors", alias);
                    // Power cycle
                    match Self::power_cycle_sensors(
                        &mut dev,
                        device_address,
                        power_cycle_power_down_seconds,
                    ) {
                        Ok(_) => {
                            debug!("{}: successfully power cycled sensors", alias);
                            readout_counter = 0;
                            // Give it some bonus sleep time
                            next_bonus_sleep = Some(Duration::from_secs(
                                2 + (power_cycle_power_down_seconds as u64),
                            ));
                        }
                        Err(e) => {
                            warn!(
                                "{}: failed to power cycle sensors: {:?}, will try again",
                                alias, e
                            );
                        }
                    }
                    continue;
                }
            }

            // Init readout
            match Self::init_readout(&mut dev, device_address) {
                Ok(_) => {
                    debug!("{}: initialized readout", alias)
                }
                Err(e) => {
                    error!("{}: unable to initialize readout: {:?}", alias, e);
                    readout_i2c_error_counter.inc();
                    continue;
                }
            }

            // Wait some time for readout to finish
            thread::sleep(Duration::from_millis(500));

            // Readout
            let readings = match Self::readout_sensors(&mut dev, device_address, &alias) {
                Ok(r) => {
                    readout_ok_counter.inc();
                    r
                }
                Err(e) => {
                    error!("{}: unable to read data: {:?}", alias, e);
                    {
                        let mut core = inner.core.lock().unwrap();
                        core.set_error_on_all_ports(chrono::Utc::now(), format!("{:?}", e))
                    }

                    match e {
                        ExpanderError::Bus(_) => {
                            readout_i2c_error_counter.inc();
                        }
                        ExpanderError::ExpanderReadoutNotFinished => {
                            readout_not_finished_error_counter.inc();
                            // TODO wait a bit and try again? probably not, we'll try again anyway...
                        }
                        ExpanderError::ExpanderWatchdogReset => {
                            readout_watchdog_reset_error_counter.inc();

                            // Reset the watchdog flag, sleep, try again.
                            match Self::reset_watchdog(&mut dev, device_address) {
                                Ok(_) => {
                                    debug!("{}: reset watchdog timer", alias)
                                }
                                Err(e) => {
                                    error!("{}: failed to reset watchdog flag: {:?}", alias, e)
                                }
                            }
                        }
                        ExpanderError::BadExpander(_) => {
                            readout_bad_expander_error_counter.inc();
                        }
                    }

                    continue;
                }
            };

            // Handle results.
            Self::handle_results(
                &mut inner,
                readings,
                &ok_counter,
                &checksum_error_counter,
                &timeout_error_counter,
                &suspicious_value_counter,
                &no_start_condition_error_counter,
                &alias,
            );
        }
    }

    fn reset_watchdog<I2C, E>(
        dev: &mut I2C,
        device_address: u8,
    ) -> std::result::Result<(), ExpanderError>
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        let reset_watchdog_bytes: [u8; 2] = [0x00, 0x02];
        dev.write(device_address, &reset_watchdog_bytes)
            .map_err(ExpanderError::i2c_error)
    }

    fn handle_results(
        inner: &mut SynchronizedDeviceReadCore,
        readings: [std::result::Result<dht22_lib::Reading, ExpanderSensorError>; 16],
        ok_counter: &GenericCounter<AtomicU64>,
        checksum_error_counter: &GenericCounter<AtomicU64>,
        timeout_error_counter: &GenericCounter<AtomicU64>,
        suspicious_value_counter: &GenericCounter<AtomicU64>,
        no_start_condition_counter: &GenericCounter<AtomicU64>,
        alias: &str,
    ) {
        let ts = chrono::Utc::now();
        let mut core = inner.core.lock().unwrap();

        for (i, reading) in IntoIterator::into_iter(readings).enumerate() {
            match reading {
                Err(e) => {
                    warn!("{}: failed to read DHT {}: {:?}", alias, i, e);

                    let msg = format!("{:?}", e);
                    core.update_value_and_generate_events(ts, i * 2, Err(err_msg(msg.clone())));
                    core.update_value_and_generate_events(ts, i * 2 + 1, Err(err_msg(msg)));

                    // Set prometheus counters accordingly.
                    match e {
                        ExpanderSensorError::Timeout => timeout_error_counter.inc(),
                        ExpanderSensorError::Checksum => checksum_error_counter.inc(),
                        ExpanderSensorError::NoStartCondition => no_start_condition_counter.inc(),
                        ExpanderSensorError::SuspiciousValue => suspicious_value_counter.inc(),
                    }
                }
                Ok(readings) => {
                    core.update_value_and_generate_events(
                        ts,
                        i * 2,
                        Ok(InputValue::Temperature(readings.temperature as f64)),
                    );
                    core.update_value_and_generate_events(
                        ts,
                        i * 2 + 1,
                        Ok(InputValue::Humidity(readings.humidity as f64)),
                    );

                    ok_counter.inc();
                }
            }
        }
    }

    fn init_readout<I2C, E>(
        dev: &mut I2C,
        device_address: u8,
    ) -> std::result::Result<(), ExpanderError>
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        // We write 0x01 to initiate a readout.
        // Critically, we don't write 0x03, which would reset the watchdog-reset bit.
        let init_readout_bytes: [u8; 2] = [0x00, 0x01];
        dev.write(device_address, &init_readout_bytes)
            .map_err(ExpanderError::i2c_error)
    }

    fn power_cycle_sensors<I2C, E>(
        dev: &mut I2C,
        device_address: u8,
        power_down_seconds: u8,
    ) -> std::result::Result<(), ExpanderError>
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        // We write 0b0000_1100 = 0x0C, to toggle power on both banks off.
        // This does not trigger a readout and does not touch the watchdog-reset bit.
        let power_down_bytes: [u8; 2] = [0x00, 0x0C];
        dev.write(device_address, &power_down_bytes)
            .map_err(ExpanderError::i2c_error)?;

        // Wait for some time for the sensors to reset.
        thread::sleep(Duration::from_secs(power_down_seconds as u64));

        // We then write 0x00 to re-power the sensors, again without triggering a readout
        // and without touching the watchgo-reset bit.
        let power_on_bytes: [u8; 2] = [0x00, 0x00];
        dev.write(device_address, &power_on_bytes)
            .map_err(ExpanderError::i2c_error)
    }

    fn readout_sensors<I2C, E>(
        dev: &mut I2C,
        device_address: u8,
        alias: &str,
    ) -> std::result::Result<
        [std::result::Result<dht22_lib::Reading, ExpanderSensorError>; 16],
        ExpanderError,
    >
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        // We read 16*6 + 1 = 97 bytes of data.
        let mut buf: [u8; 97] = [0; 97];
        dev.write_read(device_address, &[0x00], &mut buf)
            .map_err(ExpanderError::i2c_error)?;

        // The first byte is the status byte for the expander.
        // Check whether the readout finished and whether there was a watchdog reset.
        if buf[0] & 0x01 != 0 {
            return Err(ExpanderError::ExpanderReadoutNotFinished);
        }
        if buf[0] & 0x02 == 0 {
            return Err(ExpanderError::ExpanderWatchdogReset);
        }

        // The follow 96 bytes are data for the 16 connected sensors.
        let dht_buf = &buf[1..];

        let mut results: [std::result::Result<dht22_lib::Reading, ExpanderSensorError>; 16] =
            [Err(ExpanderSensorError::NoStartCondition); 16];
        for i in 0..16 {
            // Examine the status byte, which is the last byte of each group of six.
            let status_byte = dht_buf[i * 6 + 5];
            debug!("{}: DHT {}: status byte is {:0x}", alias, i, status_byte);
            match status_byte {
                0x01 => {
                    // No start condition.
                    results[i] = Err(ExpanderSensorError::NoStartCondition)
                }
                0x02 => {
                    // Timeout.
                    results[i] = Err(ExpanderSensorError::Timeout)
                }
                0x00 => {
                    // Readout successful.
                    let data_bytes = &dht_buf[i * 6..i * 6 + 5];
                    let checksum_byte = data_bytes[4];

                    // Compute checksum.
                    let checksum = dht22_lib::compute_checksum(
                        data_bytes[0],
                        data_bytes[1],
                        data_bytes[2],
                        data_bytes[3],
                    );
                    debug!(
                        "{}: DHT {}: computed checksum {:#010b}, expected {:#010b}, match={}",
                        alias,
                        i,
                        checksum_byte,
                        checksum,
                        checksum_byte == checksum
                    );

                    // Compare the computed checksum against the checksum byte.
                    if checksum != checksum_byte {
                        results[i] = Err(ExpanderSensorError::Checksum);
                        continue;
                    }

                    // Decode data.
                    let reading = dht22_lib::decode_reading(
                        data_bytes[0],
                        data_bytes[1],
                        data_bytes[2],
                        data_bytes[3],
                    );
                    debug!("{}: DHT {}: decoded reading {:?}", alias, i, reading);

                    // Check for suspicious reading.
                    if reading.humidity > 100_f32 {
                        warn!("{}: DHT {}: got suspicious reading {:?}", alias, i, reading);
                        results[i] = Err(ExpanderSensorError::SuspiciousValue);
                        continue;
                    }

                    // Reading looks good, add to results.
                    results[i] = Ok(reading)
                }
                _ => {
                    error!("{}: DHT {}: status byte is {:0x}", alias, i, status_byte);
                    return Err(ExpanderError::BadExpander(
                        "invalid DHT status byte".to_string(),
                    ));
                }
            }
        }

        Ok(results)
    }
}

impl HardwareDevice for DHT22Expander {
    fn port_alias(&self, port: u8) -> Result<String> {
        match port {
            0..=31 => {
                if port % 2 == 0 {
                    Ok(format!("dht-{}-temperature", port / 2))
                } else {
                    Ok(format!("dht-{}-humidity", port / 2))
                }
            }
            _ => Err(err_msg(
                "DHT22Expander has 32 ports: 16 groups of temperature and humidity",
            )),
        }
    }
}

impl InputHardwareDevice for DHT22Expander {
    fn get_input_port(&self, port: u8) -> Result<(InputValueType, EventStream)> {
        ensure!(
            port < 32,
            "DHT22Expander has 32 ports: 16 groups of temperature and humidity"
        );
        self.inner.get_input_port(port)
    }
}
