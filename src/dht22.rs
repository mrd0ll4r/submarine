use crate::device::{EventStream, HardwareDevice, VirtualDevice};
use crate::device_core::{DeviceReadCore, SynchronizedDeviceReadCore};
use crate::dht22_lib::ReadingError;
use crate::{dht22_lib, prom, Result};
use alloy::config::ValueScaling;
use failure::ResultExt;
use rand::Rng;
use rppal::gpio;
use rppal::gpio::{Gpio, Mode};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Configuration for a DHT22 (actually AM2302) temperature/humidity sensor.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct DHT22Config {
    bcm_pin: u8,
    adjust_priority: bool,
    use_experimental_implementation: bool,
    readout_interval_seconds: u8,
}

pub(crate) struct DHT22 {
    core: SynchronizedDeviceReadCore,
}

impl DHT22 {
    pub(crate) fn new(alias: String, cfg: &DHT22Config) -> Result<DHT22> {
        ensure!(
            cfg.readout_interval_seconds >= 2,
            "readout_interval_seconds must be at least 2"
        );

        // Check if the pin is available
        // TODO figure out if this ever becomes un-available.
        // If yes: re-create it for each measurement...
        let pin = Gpio::new()
            .context("unable to get GPIOs - is this a Raspberry Pi?")?
            .get(cfg.bcm_pin)
            .context(format!("unable to get pin {} - maybe busy?", cfg.bcm_pin))?
            .into_io(Mode::Output);

        let core = Arc::new(Mutex::new(DeviceReadCore::new(alias.clone(), 2)));
        let thread_core = core.clone();
        let adjust_priority = cfg.adjust_priority;
        let use_experimental_implementation = cfg.use_experimental_implementation;
        let readout_interval = Duration::from_secs(cfg.readout_interval_seconds as u64);

        thread::Builder::new()
            .name(format!("DHT22 {}", alias))
            .spawn(move || {
                DHT22::update_async(
                    alias,
                    thread_core,
                    pin,
                    adjust_priority,
                    readout_interval,
                    use_experimental_implementation,
                )
            })?;

        Ok(DHT22 { core })
    }

    fn update_async(
        alias: String,
        core: SynchronizedDeviceReadCore,
        mut pin: gpio::IoPin,
        adjust_priority: bool,
        readout_interval: Duration,
        use_experimental_implementation: bool,
    ) {
        let temp_gauge = prom::TEMPERATURE.with_label_values(&[alias.as_str()]);
        let humidity_gauge = prom::HUMIDITY.with_label_values(&[alias.as_str()]);
        let ok_counter = prom::DHT22_MEASUREMENTS.with_label_values(&[alias.as_str(), "ok"]);
        let checksum_error_counter =
            prom::DHT22_MEASUREMENTS.with_label_values(&[alias.as_str(), "checksum_error"]);
        let timeout_error_counter =
            prom::DHT22_MEASUREMENTS.with_label_values(&[alias.as_str(), "timeout_error"]);
        let gpio_error_counter =
            prom::DHT22_MEASUREMENTS.with_label_values(&[alias.as_str(), "gpio_error"]);
        let suspicous_value_counter =
            prom::DHT22_MEASUREMENTS.with_label_values(&[alias.as_str(), "suspicious_value"]);

        // de-sync in case we have multiple of these
        let millis = rand::thread_rng().gen_range(0..1000);
        debug!("will sleep {}ms to de-sync", millis);
        thread::sleep(Duration::from_millis(millis));

        loop {
            thread::sleep(readout_interval);

            let readings = if use_experimental_implementation {
                dht22_lib::read_pin_2(&mut pin, adjust_priority)
            } else {
                dht22_lib::read_pin(&mut pin, adjust_priority)
            };
            // Take this timestamp after the reading, because that takes a few milliseconds.
            let ts = chrono::Utc::now();

            match readings {
                Ok(readings) => {
                    debug!("got readings from pin {}: {:?}", pin.pin(), readings);

                    // We occasionally get these readings with correct checksums, so we might as
                    // well guard against them...
                    if readings.humidity > 100_f32 {
                        warn!(
                            "got suspicious reading from pin {}: {:?}",
                            pin.pin(),
                            readings
                        );
                        suspicous_value_counter.inc();
                        continue;
                    }

                    let temp = alloy::map_to_value((-40_f64, 80_f64), readings.temperature as f64);
                    let humidity = alloy::map_to_value((0_f64, 100_f64), readings.humidity as f64);

                    {
                        let mut core = core.lock().unwrap();

                        core.update_value_and_generate_events(ts, 0, temp);
                        core.update_value_and_generate_events(ts, 1, humidity);
                    }

                    temp_gauge.set(readings.temperature as f64);
                    humidity_gauge.set(readings.humidity as f64);
                    ok_counter.inc();
                }
                Err(err) => {
                    warn!("unable to read from pin {}: {:?}", pin.pin(), err);
                    match err {
                        ReadingError::Checksum => checksum_error_counter.inc(),
                        ReadingError::Timeout => timeout_error_counter.inc(),
                        ReadingError::Gpio(_) => gpio_error_counter.inc(),
                    }
                }
            }
        }
    }
}

impl HardwareDevice for DHT22 {
    fn alias(&self) -> String {
        self.core.alias()
    }

    fn update(&self) -> Result<()> {
        Ok(())
    }

    fn get_virtual_device(
        &self,
        port: u8,
        _scaling: Option<ValueScaling>,
    ) -> Result<(Box<dyn VirtualDevice>, EventStream)> {
        ensure!(port < 2, "DHT22 has two ports: 0=>temperature, 1=>humidity");
        self.core.get_virtual_device(port, _scaling)
    }
}
