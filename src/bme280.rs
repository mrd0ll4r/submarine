use crate::device::{EventStream, HardwareDevice, InputHardwareDevice};
use crate::device_core::{DeviceReadCore, SynchronizedDeviceReadCore};
use crate::prom;
use crate::Result;
use alloy::config::{InputValue, InputValueType};
use anyhow::{anyhow, ensure};
use embedded_hal as hal;
use log::{debug, warn};
use prometheus::Histogram;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::thread;
use std::time::{Duration, Instant};

// Pressure threshold to trigger a device reset
// between two adjacent readouts, in pascal.
const DEVICE_RESET_PRESSURE_DIFF: f32 = 20_000_f32;

#[derive(Debug)]
enum ReadoutError<E: Sync + Send + std::fmt::Debug + 'static> {
    SuspiciousValue { pressure_diff: f32 },
    Readout(bme280::Error<E>),
}

pub(crate) struct BME280 {
    inner: SynchronizedDeviceReadCore,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BME280Config {
    pub i2c_bus: String,
    i2c_slave_address: u8,
    readout_interval_seconds: u8,
    reset_every_n_readouts: Option<u16>,
}

impl BME280 {
    pub fn new<I2C, E>(dev: I2C, config: &BME280Config, alias: String) -> Result<BME280>
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::Read<Error = E>
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        ensure!(
            config.i2c_slave_address == 0x76 || config.i2c_slave_address == 0x77,
            "BME280 has I2C addresses 0x76 and 0x77 only"
        );
        let mut sensor =
            bme280::BME280::new(dev, config.i2c_slave_address, linux_embedded_hal::Delay);

        // TODO by default this a long IIR filter.
        // TODO with our low readout frequency, we probably don't need any of the IIR filtering. But we can't turn it off...

        sensor
            .init()
            .map_err(|e| anyhow!("unable to init: {:?}", e))?;

        let inner = SynchronizedDeviceReadCore::new_from_core(DeviceReadCore::new(
            alias.clone(),
            &[
                InputValueType::Temperature,
                InputValueType::Humidity,
                InputValueType::Pressure,
            ],
        ));
        let thread_inner = inner.clone();

        let readout_interval = config.readout_interval_seconds;
        let reset_every_n_readouts = config.reset_every_n_readouts;
        thread::Builder::new()
            .name(format!("BME280 {}", alias))
            .spawn(move || {
                BME280::run(
                    thread_inner,
                    sensor,
                    readout_interval,
                    alias,
                    reset_every_n_readouts,
                )
            })?;

        Ok(BME280 { inner })
    }

    fn run<I2C, E>(
        core: SynchronizedDeviceReadCore,
        mut dev: bme280::BME280<I2C, linux_embedded_hal::Delay>,
        readout_interval_secs: u8,
        alias: String,
        reset_every_n_readouts: Option<u16>,
    ) where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::Read<Error = E>
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        let readout_interval = Duration::from_secs(readout_interval_secs as u64);
        let readout_duration_histogram = prom::BME280_READ_DURATION
            .get_metric_with_label_values(&[&alias])
            .unwrap();

        let ok_counter = prom::BME280_MEASUREMENTS.with_label_values(&[alias.as_str(), "ok"]);
        let compensation_error_counter =
            prom::BME280_MEASUREMENTS.with_label_values(&[alias.as_str(), "compensation_failed"]);
        let i2c_error_counter =
            prom::BME280_MEASUREMENTS.with_label_values(&[alias.as_str(), "i2c_error"]);
        let invalid_data_error_counter =
            prom::BME280_MEASUREMENTS.with_label_values(&[alias.as_str(), "invalid_data"]);
        let no_calibration_error_counter =
            prom::BME280_MEASUREMENTS.with_label_values(&[alias.as_str(), "no_calibration_data"]);
        let unsupported_chip_counter =
            prom::BME280_MEASUREMENTS.with_label_values(&[alias.as_str(), "unsupported_chip"]);

        // Keep track of the last pressure we read.
        // In case there is a massive jump, we reset the chip.
        let mut last_pressure = None;

        // De-sync in case we have multiple of these.
        let millis = rand::thread_rng().gen_range(0..1000);
        debug!("will sleep {}ms to de-sync", millis);
        thread::sleep(Duration::from_millis(millis));

        let mut readout_count = 0_u32;
        loop {
            // Read sensor.
            let res = BME280::read_sensor(
                &alias,
                &mut dev,
                &mut last_pressure,
                &core,
                &readout_duration_histogram,
            );

            match res {
                Ok(_) => {
                    readout_count += 1;
                    ok_counter.inc();

                    if let Some(reset_every_n_readouts) = &reset_every_n_readouts {
                        let reset_counter = *reset_every_n_readouts as u32;

                        if readout_count >= reset_counter {
                            debug!("attempting to reset BME {}", alias);
                            match dev.init() {
                                Ok(_) => {
                                    debug!("successfully reset BME {}", alias);
                                    readout_count = 0;
                                }
                                Err(e) => {
                                    warn!(
                                        "unable to reset BME {}, retrying next time: {:?}",
                                        alias, e
                                    );
                                    // Do not reset readout_count, retry next time.
                                }
                            }
                        }
                    }
                }
                Err(err) => {
                    match err {
                        ReadoutError::SuspiciousValue { pressure_diff } => {
                            warn!("{}: pressure difference since last measurement is {}, exceeds threshold of {}, resetting device",alias,pressure_diff,DEVICE_RESET_PRESSURE_DIFF);
                            loop {
                                match dev.init() {
                                    Ok(_) => {
                                        debug!("successfully reset BME {}", alias);
                                        break;
                                    }
                                    Err(e) => {
                                        warn!(
                                            "unable to reset BME {}, trying again: {:?}",
                                            alias, e
                                        );
                                    }
                                }
                            }

                            // Retry reading the sensor.
                            continue;
                        }
                        ReadoutError::Readout(err) => match err {
                            bme280::Error::CompensationFailed => {
                                compensation_error_counter.inc();
                            }
                            bme280::Error::I2c(_) => {
                                i2c_error_counter.inc();
                            }
                            bme280::Error::InvalidData => {
                                invalid_data_error_counter.inc();
                            }
                            bme280::Error::NoCalibrationData => {
                                no_calibration_error_counter.inc();
                            }
                            bme280::Error::UnsupportedChip => {
                                unsupported_chip_counter.inc();
                            }
                        },
                    }
                }
            }

            // Sleep afterwards (so we get a fresh reading when the program starts).
            thread::sleep(readout_interval);
        }
    }

    fn read_sensor<I2C, E>(
        alias: &str,
        dev: &mut bme280::BME280<I2C, linux_embedded_hal::Delay>,
        last_pressure: &mut Option<f32>,
        core: &SynchronizedDeviceReadCore,
        readout_duration_histogram: &Histogram,
    ) -> std::result::Result<(), ReadoutError<E>>
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::Read<Error = E>
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        let before = Instant::now();
        let res = dev.measure();
        let after = Instant::now();
        readout_duration_histogram.observe(after.duration_since(before).as_micros() as f64);
        debug!("{}: got measurements: {:?}", alias, res);

        // Take this timestamp after the reading, because that takes some time.
        let ts = chrono::Utc::now();

        match res {
            Ok(readings) => {
                debug!("got measurements from device {}: {:?}", alias, readings);
                if let Some(last_pressure) = last_pressure.take() {
                    let diff = (readings.pressure - last_pressure).abs();
                    debug!(
                        "{}: last pressure was {}, absolute diff is {}",
                        alias, last_pressure, diff
                    );
                    if diff >= DEVICE_RESET_PRESSURE_DIFF {
                        // last_pressure is None, since we called take above.
                        return Err(ReadoutError::SuspiciousValue {
                            pressure_diff: diff,
                        });
                    }
                } else {
                    debug!("{}: no last pressure value", alias);
                }
                *last_pressure = Some(readings.pressure);

                debug!(
                    "got valid measurements from device {}: {:?}",
                    alias, readings
                );

                {
                    let mut core = core.core.lock().unwrap();

                    core.update_value_and_generate_events(
                        ts,
                        0,
                        Ok(InputValue::Temperature(readings.temperature as f64)),
                        true,
                    );
                    core.update_value_and_generate_events(
                        ts,
                        1,
                        Ok(InputValue::Humidity(readings.humidity as f64)),
                        true,
                    );
                    core.update_value_and_generate_events(
                        ts,
                        2,
                        Ok(InputValue::Pressure(readings.pressure as f64)),
                        true,
                    );
                }

                Ok(())
            }
            Err(err) => {
                warn!("unable to read BME280 {}: {:?}", alias, err);

                let msg = format!("{:?}", err);
                {
                    let mut core = core.core.lock().unwrap();
                    core.set_error_on_all_ports(ts, msg)
                }

                Err(ReadoutError::Readout(err))
            }
        }
    }
}

impl HardwareDevice for BME280 {
    fn port_alias(&self, port: u8) -> Result<String> {
        match port {
            0 => Ok("temperature".to_string()),
            1 => Ok("humidity".to_string()),
            2 => Ok("pressure".to_string()),
            _ => Err(anyhow!(
                "BME280 has three ports: temperature, humidity, and pressure",
            )),
        }
    }
}

impl InputHardwareDevice for BME280 {
    fn get_input_port(&self, port: u8) -> Result<(InputValueType, EventStream)> {
        ensure!(
            port < 3,
            "BME280 has three ports: temperature, humidity, and pressure"
        );
        self.inner.get_input_port(port)
    }
}
