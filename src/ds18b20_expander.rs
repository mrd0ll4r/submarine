use crate::device::{EventStream, HardwareDevice, InputHardwareDevice};
use crate::device_core::{DeviceReadCore, SynchronizedDeviceReadCore};
use crate::ds18b20_expander_lib::{Ds18b20Expander, ExpanderError, SensorError};
use crate::{poll, prom, Result};
use alloy::config::{InputValue, InputValueType};
use anyhow::{anyhow, bail, ensure, Context};
use embedded_hal as hal;
use log::{debug, error, trace, warn};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::thread;
use std::time::{Duration, Instant};

/// Configuration for the I2C-to-DHT expander.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct DS18B20ExpanderConfig {
    pub i2c_bus: String,
    readout_interval_seconds: u8,
    i2c_slave_address: u8,
    sensor_ids: Vec<SensorToPortMapping>,
    readout_retries: u8,
}

impl DS18B20ExpanderConfig {
    pub(crate) fn slave_address(&self) -> Result<u8> {
        ensure!(self.i2c_slave_address == 0x3f, "invalid address");

        Ok(self.i2c_slave_address)
    }

    fn ensure_valid_sensor_mappings(&self) -> Result<()> {
        let mut ports = HashSet::new();
        let mut ids = HashSet::new();

        for mapping in self.sensor_ids.iter() {
            ensure!(mapping.port < 50, "invalid port");
            ensure!(
                !ids.contains(&mapping.sensor_id),
                "duplicate sensor ID {}",
                mapping.sensor_id
            );
            ensure!(
                !ports.contains(&mapping.port),
                "duplicate port {}",
                mapping.port
            );

            ids.insert(mapping.sensor_id);
            ports.insert(mapping.port);
        }

        Ok(())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct SensorToPortMapping {
    sensor_id: u64,
    port: u8,
}

pub(crate) struct DS18B20Expander {
    inner: SynchronizedDeviceReadCore,
    port_mappings: Vec<SensorToPortMapping>,
}

impl DS18B20Expander {
    pub(crate) fn new<I2C, E>(
        dev: I2C,
        alias: String,
        cfg: &DS18B20ExpanderConfig,
    ) -> Result<DS18B20Expander>
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
        ensure!(cfg.readout_retries > 0, "readout_retries must be > 0");

        let addr = cfg.slave_address()?;

        cfg.ensure_valid_sensor_mappings()
            .context("invalid sensor<->port mappings")?;

        // Initialize device, scan buses
        let mut expander = Ds18b20Expander::new(dev, addr);

        debug!("scanning for devices...");
        let sensor_ids = expander
            .scan_for_devices(cfg.readout_retries as usize, &mut linux_embedded_hal::Delay)
            .context("unable to scan for devices")?;
        debug!(
            "got device IDs {:?}",
            sensor_ids
                .iter()
                .map(|v| format!("{:016x}", v))
                .collect::<Vec<_>>()
        );

        let mut port_indices = Vec::with_capacity(cfg.sensor_ids.len());
        for mapping in cfg.sensor_ids.iter() {
            let index = sensor_ids.iter().position(|v| *v == mapping.sensor_id);
            match index {
                None => {
                    bail!(
                        "did not find sensor with ID {:016x} on the bus",
                        mapping.sensor_id
                    );
                }
                Some(i) => {
                    port_indices.push(i);
                }
            }
        }

        for sensor_id in sensor_ids.iter() {
            let found = cfg.sensor_ids.iter().any(|v| v.sensor_id == *sensor_id);
            if !found {
                warn!("found sensor with ID {:016x} on the bus, but don't have any mapping for this sensor configured",sensor_id);
            }
        }

        let inner = SynchronizedDeviceReadCore::new_from_core(DeviceReadCore::new(
            alias.clone(),
            &std::iter::repeat(InputValueType::Temperature)
                .take(50)
                .collect::<Vec<_>>(),
        ));

        // Launch a thread to do the work.
        let thread_core = inner.clone();
        let readout_interval = cfg.readout_interval_seconds;
        let readout_retries = cfg.readout_retries as usize;
        let port_mappings = cfg.sensor_ids.clone();
        thread::Builder::new()
            .name(format!("DS18B20Expander {}", alias))
            .spawn(move || {
                DS18B20Expander::run(
                    thread_core,
                    expander,
                    readout_interval,
                    port_indices,
                    port_mappings,
                    sensor_ids,
                    readout_retries,
                    alias,
                )
            })?;

        Ok(DS18B20Expander {
            inner,
            port_mappings: cfg.sensor_ids.clone(),
        })
    }

    fn run<I2C, E>(
        mut inner: SynchronizedDeviceReadCore,
        mut dev: Ds18b20Expander<I2C>,
        polling_interval_seconds: u8,
        port_indices: Vec<usize>,
        port_mappings: Vec<SensorToPortMapping>,
        sensor_ids: Vec<u64>,
        readout_retries: usize,
        alias: String,
    ) where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        let readout_ok_counter =
            prom::DS18_EXPANDER_MEASUREMENTS.with_label_values(&[alias.as_str(), "ok"]);
        let readout_i2c_error_counter =
            prom::DS18_EXPANDER_MEASUREMENTS.with_label_values(&[alias.as_str(), "i2c_error"]);
        let readout_bad_expander_error_counter =
            prom::DS18_EXPANDER_MEASUREMENTS.with_label_values(&[alias.as_str(), "bad_expander"]);
        let readout_crc_mismatch_counter =
            prom::DS18_EXPANDER_MEASUREMENTS.with_label_values(&[alias.as_str(), "crc_mismatch"]);

        // De-sync in case we have multiple of these.
        let millis = rand::thread_rng().gen_range(0..1000);
        debug!("will sleep {}ms to de-sync", millis);
        thread::sleep(Duration::from_millis(millis));

        let polling_interval = Duration::from_secs(polling_interval_seconds as u64);
        let mut last_wakeup = Instant::now();

        loop {
            // Calculate next sleep.
            let (s, lagging) = poll::calculate_sleep_duration(last_wakeup, polling_interval);
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

            // Read values
            let readout_res =
                dev.read_temperatures(&mut linux_embedded_hal::Delay, readout_retries);
            match readout_res {
                Err(err) => {
                    error!("{}: unable to read data: {:?}", alias, err);
                    {
                        let mut core = inner.core.lock().unwrap();
                        core.set_error_on_all_ports(chrono::Utc::now(), format!("{:?}", err))
                    }

                    match err {
                        ExpanderError::Bus(_) => readout_i2c_error_counter.inc(),
                        ExpanderError::CrcMismatch => readout_crc_mismatch_counter.inc(),
                        ExpanderError::BadExpander(_) => {
                            readout_bad_expander_error_counter.inc();
                        }
                        ExpanderError::MaxRetriesExceeded(err) => match *err {
                            ExpanderError::Bus(_) => readout_i2c_error_counter.inc(),
                            ExpanderError::CrcMismatch => readout_crc_mismatch_counter.inc(),
                            ExpanderError::BadExpander(_) => {
                                readout_bad_expander_error_counter.inc();
                            }
                            ExpanderError::MaxRetriesExceeded(_) => {
                                unreachable!()
                            }
                        },
                    }
                }
                Ok(values) => {
                    readout_ok_counter.inc();

                    // Handle results.
                    Self::handle_results(
                        &mut inner,
                        values,
                        &port_indices,
                        &port_mappings,
                        &sensor_ids,
                        &alias,
                    );
                }
            }
        }
    }

    fn handle_results(
        inner: &mut SynchronizedDeviceReadCore,
        readings: Vec<std::result::Result<f64, SensorError>>,
        port_indices: &[usize],
        port_mappings: &[SensorToPortMapping],
        sensor_ids: &[u64],
        alias: &str,
    ) {
        let ts = chrono::Utc::now();
        let mut core = inner.core.lock().unwrap();

        for (i, reading) in IntoIterator::into_iter(readings).enumerate() {
            let sensor_id = sensor_ids[i];
            let mapping_index = port_indices.iter().position(|v| *v == i);
            let port_index = mapping_index.map(|v| &port_mappings[v]);
            if let Some(port_index) = port_index.as_ref() {
                assert_eq!(port_index.sensor_id, sensor_id);
            }

            debug!(
                "{}: got reading {:?} for sensor with ID {:016x}",
                alias, reading, sensor_id
            );

            match reading {
                Err(e) => {
                    warn!(
                        "{}: failed to read sensor with ID {:016x}: {:?}",
                        alias, sensor_id, e
                    );

                    if let Some(port_index) = port_index.as_ref() {
                        let msg = format!("{:?}", e);
                        core.update_value_and_generate_events(
                            ts,
                            port_index.port as usize,
                            Err(anyhow!(msg)),
                            true,
                        );
                    }
                }
                Ok(temperature) => {
                    debug!(
                        "{}: got valid temperature {} for sensor with ID {:016x}",
                        alias, temperature, sensor_id
                    );
                    if let Some(port_index) = port_index.as_ref() {
                        core.update_value_and_generate_events(
                            ts,
                            port_index.port as usize,
                            Ok(InputValue::Temperature(temperature)),
                            true,
                        );
                    }
                }
            }
        }
    }
}

impl HardwareDevice for DS18B20Expander {
    fn port_alias(&self, port: u8) -> Result<String> {
        match port {
            0..=49 => Ok(format!("ds18b20-{}-temperature", port)),
            _ => Err(anyhow!("DS18B20 expander has 50 ports",)),
        }
    }
}

impl InputHardwareDevice for DS18B20Expander {
    fn get_input_port(&self, port: u8) -> Result<(InputValueType, EventStream)> {
        ensure!(port < 50, "DS18B20 expander has 50 ports");
        ensure!(
            self.port_mappings.iter().any(|pm| pm.port == port),
            "port {} not configured via sensor<->port mappings",
            port
        );
        self.inner.get_input_port(port)
    }
}
