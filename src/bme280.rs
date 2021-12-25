use crate::device::{EventStream, HardwareDevice, InputHardwareDevice};
use crate::device_core::{DeviceReadCore, SynchronizedDeviceReadCore};
use crate::prom;
use crate::Result;
use alloy::config::{InputValue, InputValueType};
use bme280;
use embedded_hal as hal;
use failure::*;
use linux_embedded_hal;
use prometheus::core::{AtomicI64, GenericCounter};
use prometheus::Histogram;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) struct BME280 {
    inner: SynchronizedDeviceReadCore,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BME280Config {
    pub i2c_bus: String,
    i2c_slave_address: u8,
    readout_interval_seconds: u8,
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

        sensor
            .init()
            .map_err(|e| failure::err_msg(format!("unable to init: {:?}", e)))?;

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
        thread::Builder::new()
            .name(format!("BME280 {}", alias))
            .spawn(move || BME280::run(thread_inner, sensor, readout_interval, alias))?;

        Ok(BME280 { inner })
    }

    fn run<I2C, E>(
        core: SynchronizedDeviceReadCore,
        mut dev: bme280::BME280<I2C, linux_embedded_hal::Delay>,
        readout_interval_secs: u8,
        alias: String,
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

        // De-sync in case we have multiple of these.
        let millis = rand::thread_rng().gen_range(0..1000);
        debug!("will sleep {}ms to de-sync", millis);
        thread::sleep(Duration::from_millis(millis));

        loop {
            // Read sensor.
            BME280::read_sensor(
                &alias,
                &mut dev,
                &core,
                &ok_counter,
                &compensation_error_counter,
                &i2c_error_counter,
                &invalid_data_error_counter,
                &no_calibration_error_counter,
                &unsupported_chip_counter,
                &readout_duration_histogram,
            );

            // Sleep afterwards (so we get a fresh reading when the program starts).
            thread::sleep(readout_interval);
        }
    }

    fn read_sensor<I2C, E>(
        alias: &str,
        dev: &mut bme280::BME280<I2C, linux_embedded_hal::Delay>,
        core: &SynchronizedDeviceReadCore,
        ok_counter: &GenericCounter<AtomicI64>,
        compensation_error_counter: &GenericCounter<AtomicI64>,
        i2c_error_counter: &GenericCounter<AtomicI64>,
        invalid_data_error_counter: &GenericCounter<AtomicI64>,
        no_calibration_error_counter: &GenericCounter<AtomicI64>,
        unsupported_chip_counter: &GenericCounter<AtomicI64>,
        readout_duration_histogram: &Histogram,
    ) where
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
                    );
                    core.update_value_and_generate_events(
                        ts,
                        1,
                        Ok(InputValue::Humidity(readings.humidity as f64)),
                    );
                    core.update_value_and_generate_events(
                        ts,
                        2,
                        Ok(InputValue::Pressure(readings.pressure as f64)),
                    );
                }

                ok_counter.inc();
            }
            Err(err) => {
                warn!("unable to read BME280 {}: {:?}", alias, err);

                let msg = format!("{:?}", err);
                {
                    let mut core = core.core.lock().unwrap();
                    core.set_error_on_all_ports(ts, msg)
                }

                match err {
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
                }
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
            _ => Err(err_msg(
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
