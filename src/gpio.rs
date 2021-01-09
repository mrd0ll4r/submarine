use crate::device::{EventStream, HardwareDevice, VirtualDevice};
use crate::device_core::{DeviceReadCore, SynchronizedDeviceReadCore};
use crate::Result;
use alloy::config::ValueScaling;
use alloy::{HIGH, LOW};
use failure::ResultExt;
use rand::Rng;
use rppal::gpio;
use rppal::gpio::{Gpio, Level};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Configuration for a GPIO pin.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct GPIOConfig {
    bcm_pin: u8,
    pull: PullUpDown,
    readout_interval_milliseconds: u64,
    invert: bool,
}

/// Pull up/down configuration for a GPIO pin.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) enum PullUpDown {
    #[serde(rename = "up")]
    Up,
    #[serde(rename = "down")]
    Down,
}

pub(crate) struct GPIO {
    core: SynchronizedDeviceReadCore,
}

impl GPIO {
    pub(crate) fn new(alias: String, cfg: &GPIOConfig) -> Result<GPIO> {
        ensure!(
            cfg.readout_interval_milliseconds > 0,
            "readout_interval_milliseconds must be > 0"
        );

        // Check if the pin is available
        // TODO figure out if this ever becomes un-available.
        // If yes: re-create it for each measurement...
        let pin = Gpio::new()
            .context("unable to get GPIOs - is this a Raspberry Pi?")?
            .get(cfg.bcm_pin)
            .context(format!("unable to get pin {} - maybe busy?", cfg.bcm_pin))?;
        let pin = match &cfg.pull {
            PullUpDown::Up => pin.into_input_pullup(),
            PullUpDown::Down => pin.into_input_pulldown(),
        };

        let core = Arc::new(Mutex::new(DeviceReadCore::new(alias.clone(), 1)));
        let thread_core = core.clone();
        let readout_interval = Duration::from_millis(cfg.readout_interval_milliseconds as u64);

        thread::Builder::new()
            .name(format!("GPIO {}", alias))
            .spawn(move || GPIO::update_async(alias, thread_core, pin, readout_interval))?;

        Ok(GPIO { core })
    }

    fn update_async(
        alias: String,
        core: SynchronizedDeviceReadCore,
        pin: gpio::InputPin,
        readout_interval: Duration,
    ) {
        // de-sync in case we have multiple of these
        let millis = rand::thread_rng().gen_range(0..1000);
        debug!("{}: will sleep {}ms to de-sync", alias, millis);
        thread::sleep(Duration::from_millis(millis));

        loop {
            thread::sleep(readout_interval);

            let ts = chrono::Utc::now();
            let value = pin.read();
            debug!("{}: got value from pin {}: {}", alias, pin.pin(), value);

            let v = if value == Level::High { HIGH } else { LOW };

            {
                let mut core = core.lock().unwrap();
                core.update_value_and_generate_events(ts, 0, v);
            }
        }
    }
}

impl HardwareDevice for GPIO {
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
        ensure!(port < 1, "GPIO has one port: the value (0)");
        self.core.get_virtual_device(port, _scaling)
    }
}
