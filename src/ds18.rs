use crate::device::{EventStream, HardwareDevice, InputHardwareDevice};
use crate::device_core::{DeviceReadCore, SynchronizedDeviceReadCore};
use crate::prom;
use crate::Result;
use alloy::config::{InputValue, InputValueType};
use anyhow::{ensure, Context};
use log::{debug, warn};
use prometheus::core::{AtomicU64, GenericCounter};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

/// Configuration for a DS18(B20) 1wire temperature sensor.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct DS18Config {
    id: String,
    readout_interval_seconds: u8,
}

pub(crate) struct DS18 {
    core: SynchronizedDeviceReadCore,
}

impl DS18 {
    pub(crate) fn new(alias: String, cfg: &DS18Config) -> Result<DS18> {
        ensure!(
            cfg.readout_interval_seconds >= 2,
            "readout_interval_seconds must be at least 2"
        );

        // Check if 1wire is available
        std::fs::metadata("/sys/bus/w1/devices")
            .context("unable to open 1-wire devices -- is 1-wire enabled?")?;

        // Check if the device is available and a support DS18 device
        let mut p = std::path::PathBuf::from("/sys/bus/w1/devices");
        p.push(cfg.id.clone());
        p.push("temperature");
        debug!("calculated path {} for sensor {}", p.display(), &alias);
        std::fs::metadata(&p).context(format!(
            "unable to open sensor {} with ID {}, maybe wrong ID or missing w1_therm module?",
            &alias, &cfg.id
        ))?;

        let core = SynchronizedDeviceReadCore::new_from_core(DeviceReadCore::new(
            alias.clone(),
            &[InputValueType::Temperature],
        ));
        let thread_core = core.clone();
        let readout_interval = Duration::from_secs(cfg.readout_interval_seconds as u64);
        let id = cfg.id.clone();

        thread::Builder::new()
            .name(format!("DS18 {}", alias))
            .spawn(move || DS18::update_async(alias, id, p, thread_core, readout_interval))?;

        Ok(DS18 { core })
    }

    fn update_async(
        alias: String,
        id: String,
        p: PathBuf,
        core: SynchronizedDeviceReadCore,
        readout_interval: Duration,
    ) {
        let ok_counter = prom::DS18_MEASUREMENTS.with_label_values(&[alias.as_str(), "ok"]);
        let error_counter = prom::DS18_MEASUREMENTS.with_label_values(&[alias.as_str(), "error"]);
        let conversion_error_counter =
            prom::DS18_MEASUREMENTS.with_label_values(&[alias.as_str(), "conversion_error"]);

        // De-sync in case we have multiple of these.
        let millis = rand::thread_rng().gen_range(0..1000);
        debug!("will sleep {}ms to de-sync", millis);
        thread::sleep(Duration::from_millis(millis));

        loop {
            // Read sensor.
            DS18::do_read_sensor(
                &alias,
                &id,
                &p,
                &core,
                &ok_counter,
                &error_counter,
                &conversion_error_counter,
            );

            // Sleep afterwards (so we get a fresh reading when the program starts).
            thread::sleep(readout_interval);
        }
    }

    fn do_read_sensor(
        alias: &str,
        id: &str,
        p: &PathBuf,
        core: &SynchronizedDeviceReadCore,
        ok_counter: &GenericCounter<AtomicU64>,
        error_counter: &GenericCounter<AtomicU64>,
        conversion_error_counter: &GenericCounter<AtomicU64>,
    ) {
        let res = std::fs::read(&p);

        // Take this timestamp after the reading, because that takes some time.
        let ts = chrono::Utc::now();

        match res {
            Ok(readings) => {
                debug!(
                    "got readings from device {} with ID {}: {:?}",
                    alias, id, readings
                );
                match String::from_utf8(readings) {
                    Ok(temp_str) => {
                        let temp_str = temp_str.trim();
                        debug!(
                            "parsed temperature string from device {} with ID {}: {}",
                            alias, id, temp_str
                        );
                        let temp_milidegrees: std::result::Result<i64, _> = temp_str.parse();
                        match temp_milidegrees {
                            Ok(temp) => {
                                let t = temp as f64 / 1000.0;
                                debug!(
                                    "parsed integer temperature from device {} with ID {}: {} => {}°C",
                                    alias, id, temp, t
                                );

                                {
                                    let mut core = core.core.lock().unwrap();
                                    core.update_value_and_generate_events(
                                        ts,
                                        0,
                                        Ok(InputValue::Temperature(t)),
                                        true,
                                        false,
                                    );
                                }

                                ok_counter.inc();
                            }
                            Err(err) => {
                                warn!("unable to parse temperature for 1-wire device {} with ID {}: {:?}", alias, id, err);

                                let msg = format!("{:?}", err);
                                {
                                    let mut core = core.core.lock().unwrap();
                                    core.set_error_on_all_ports(
                                        ts,
                                        format!("unable to parse: {}", msg),
                                    )
                                }

                                conversion_error_counter.inc();
                            }
                        }
                    }
                    Err(err) => {
                        warn!(
                            "unable to parse string for 1-wire device {} with ID {}: {:?}",
                            alias, id, err
                        );

                        let msg = format!("{:?}", err);
                        {
                            let mut core = core.core.lock().unwrap();
                            core.set_error_on_all_ports(ts, format!("unable to parse: {}", msg))
                        }
                        conversion_error_counter.inc();
                    }
                }
            }
            Err(err) => {
                warn!(
                    "unable to read 1-wire device {} with ID {}: {:?}",
                    alias, id, err
                );

                let msg = format!("{:?}", err);
                {
                    let mut core = core.core.lock().unwrap();
                    core.set_error_on_all_ports(ts, format!("unable to read: {}", msg))
                }

                error_counter.inc();
            }
        }
    }
}

impl InputHardwareDevice for DS18 {
    fn get_input_port(&self, port: u8) -> Result<(InputValueType, EventStream)> {
        ensure!(port < 1, "DS18 has only one port");
        self.core.get_input_port(port)
    }
}

impl HardwareDevice for DS18 {
    fn port_alias(&self, port: u8) -> Result<String> {
        ensure!(port < 1, "DS18 has only one port");
        Ok("temperature".to_string())
    }
}
