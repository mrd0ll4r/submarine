use crate::device::{EventStream, HardwareDevice, InputHardwareDevice};
use crate::device_core::{DeviceReadCore, SynchronizedDeviceReadCore};
use crate::Result;
use alloy::config::{InputValue, InputValueType};
use failure::ResultExt;
use rand::Rng;
use rppal::gpio;
use rppal::gpio::Gpio as RGpio;
use rppal::gpio::Level;
use serde::{Deserialize, Serialize};
use std::thread;
use std::time::Duration;

/// Configuration for a GPIO pin.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct GpioConfig {
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

pub(crate) struct Gpio {
    core: SynchronizedDeviceReadCore,
}

impl Gpio {
    pub(crate) fn new(alias: String, cfg: &GpioConfig) -> Result<Gpio> {
        ensure!(
            cfg.readout_interval_milliseconds > 0,
            "readout_interval_milliseconds must be > 0"
        );

        // Check if the pin is available
        // TODO figure out if this ever becomes un-available.
        // If yes: re-create it for each measurement...
        let pin = RGpio::new()
            .context("unable to get GPIOs - is this a Raspberry Pi?")?
            .get(cfg.bcm_pin)
            .context(format!("unable to get pin {} - maybe busy?", cfg.bcm_pin))?;
        let pin = match &cfg.pull {
            PullUpDown::Up => pin.into_input_pullup(),
            PullUpDown::Down => pin.into_input_pulldown(),
        };

        let core = SynchronizedDeviceReadCore::new_from_core(DeviceReadCore::new(
            alias.clone(),
            &[InputValueType::Binary],
        ));
        let thread_core = core.clone();
        let readout_interval = Duration::from_millis(cfg.readout_interval_milliseconds as u64);

        thread::Builder::new()
            .name(format!("GPIO {}", alias))
            .spawn(move || Gpio::update_async(alias, thread_core, pin, readout_interval))?;

        Ok(Gpio { core })
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

            {
                let mut core = core.core.lock().unwrap();
                core.update_value_and_generate_events(
                    ts,
                    0,
                    Ok(if value == Level::High {
                        InputValue::Binary(true)
                    } else {
                        InputValue::Binary(false)
                    }),
                );
            }
        }
    }
}

impl InputHardwareDevice for Gpio {
    fn get_input_port(&self, port: u8) -> Result<(InputValueType, EventStream)> {
        ensure!(port < 1, "GPIO has one port only");
        self.core.get_input_port(port)
    }
}

impl HardwareDevice for Gpio {
    fn port_alias(&self, port: u8) -> Result<String> {
        ensure!(port < 1, "GPIO has one port only");
        Ok("pin".to_string())
    }
}
