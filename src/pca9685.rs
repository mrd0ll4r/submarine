use crate::config::ValueScaling;
use crate::device::{EventStream, HardwareDevice, OutputHardwareDevice, OutputPort};
use crate::device_core::{DeviceRWCore, SynchronizedDeviceRWCore};
use crate::poll;
use crate::prom;
use crate::Result;
use alloy::{OutputValue, HIGH, LOW};
use anyhow::{anyhow, ensure, Context};
use embedded_hal as hal;
use itertools::Itertools;
use log::{debug, warn};
use pwm_pca9685 as pwmdev;
use pwm_pca9685::Pca9685;
use serde::{Deserialize, Serialize};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PCA9685Config {
    pub i2c_bus: String,
    i2c_slave_address: u8,
    use_external_clock: bool,
    inverted: bool,
    output_driver_totem_pole: bool,
    pub update_interval_millis: u64,
    pub prescale: u8,
    #[serde(default)]
    pub sleep_after_millis: Option<u64>,
}

pub struct Pca9685Device<I2C> {
    dev: Pca9685<I2C>,
    last_change: Instant,
    device_enabled: bool,
    sleep_after: Option<Duration>,
}

pub(crate) fn set_up_device<I2C, E>(i2c: I2C, config: &PCA9685Config) -> Result<Pca9685Device<I2C>>
where
    I2C: hal::blocking::i2c::Write<Error = E>
        + Send
        + hal::blocking::i2c::WriteRead<Error = E>
        + 'static,
    E: Sync + Send + std::fmt::Debug + 'static,
{
    let addr = config.slave_address()?;
    ensure!(
        config.update_interval_millis != 0,
        "update_interval_millis must be > 0"
    );
    let mut dev = pwmdev::Pca9685::new(i2c, addr)
        .map_err(PCA9685::do_map_err::<I2C, E>)
        .context("unable to create PCA9685")?;

    if config.inverted {
        dev.set_output_logic_state(pwmdev::OutputLogicState::Inverted)
            .map_err(PCA9685::do_map_err::<I2C, E>)
            .context("unable to set PCA9685 to inverted mode")?;
    } else {
        dev.set_output_logic_state(pwmdev::OutputLogicState::Direct)
            .map_err(PCA9685::do_map_err::<I2C, E>)
            .context("unable to set PCA9685 to non-inverted mode")?;
    }

    if config.output_driver_totem_pole {
        dev.set_output_driver(pwmdev::OutputDriver::TotemPole)
            .map_err(PCA9685::do_map_err::<I2C, E>)
            .context("unable to set PCA9685 to totem pole")?;
    } else {
        dev.set_output_driver(pwmdev::OutputDriver::OpenDrain)
            .map_err(PCA9685::do_map_err::<I2C, E>)
            .context("unable to set PCA9685 to open drain")?;
    }

    if config.use_external_clock {
        dev.use_external_clock()
            .map_err(PCA9685::do_map_err::<I2C, E>)
            .context("unable to set PCA9685 to external clock")?;
    }

    dev.enable()
        .map_err(PCA9685::do_map_err::<I2C, E>)
        .context("unable to enable PCA9685")?;

    // This needs to be set after the device is enabled, for some reason.
    dev.set_prescale(config.prescale)
        .map_err(PCA9685::do_map_err::<I2C, E>)
        .context("unable to set PCA9685 prescale")?;

    Ok(Pca9685Device {
        dev,
        last_change: Instant::now(),
        device_enabled: true,
        sleep_after: config.sleep_after_millis.map(Duration::from_millis),
    })
}

impl PCA9685Config {
    pub(crate) fn slave_address(&self) -> Result<pwmdev::Address> {
        let addr = self.i2c_slave_address;
        // PCA9685 has 0b1 fixed, and I2C addresses are 7 bits long,
        // so we check for a 0b01 prefix.
        ensure!((addr >> 6) ^ 0b01 == 0, "invalid address");

        Ok(pwmdev::Address::from(addr))
    }
}

pub struct PCA9685 {
    core: SynchronizedDeviceRWCore,
}

impl PCA9685 {
    pub fn new<I2C, E>(dev: I2C, config: &PCA9685Config, alias: String) -> Result<PCA9685>
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        let dev = set_up_device(dev, config)?;

        let inner = SynchronizedDeviceRWCore::new_from_core(DeviceRWCore::new_dirty(
            alias.clone(),
            16,
            ValueScaling::Logarithmic,
        ));
        let inner_thread = inner.clone();

        let update_interval_milliseconds = config.update_interval_millis;
        thread::Builder::new()
            .name(format!("PCA {}", alias))
            .spawn(move || {
                PCA9685::handle_update_async(inner_thread, dev, update_interval_milliseconds, alias)
            })?;

        Ok(PCA9685 { core: inner })
    }

    fn handle_update_async<I2C, E>(
        core: SynchronizedDeviceRWCore,
        mut dev: Pca9685Device<I2C>,
        update_interval_milliseconds: u64,
        alias: String,
    ) -> Result<()>
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        let update_interval = Duration::from_millis(update_interval_milliseconds);
        let mut sleep_duration = update_interval;
        let mut last_wakeup = Instant::now();
        let hist = prom::PCA9685_WRITE_DURATION
            .get_metric_with_label_values(&[&alias])
            .unwrap();

        loop {
            let (wake_up, values, scalings, dirty) =
                poll::poll_loop_begin(module_path!(), sleep_duration, &mut last_wakeup, &core);

            // Do the actual update
            let res =
                Self::handle_update_async_inner_outer(&values, scalings, dirty, &mut dev, &hist);
            if let Err(ref e) = res {
                warn!("{}: unable to update PCA: {:?}", alias, e)
            }

            sleep_duration = poll::poll_loop_end(
                module_path!(),
                res,
                &core,
                values,
                wake_up,
                update_interval,
                false,
            );
        }
    }

    pub(crate) fn handle_update_async_inner_outer<I2C, E>(
        values: &[OutputValue],
        scalings: Vec<ValueScaling>,
        dirty: bool,
        dev: &mut Pca9685Device<I2C>,
        metric: &prometheus::Histogram,
    ) -> Result<()>
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        assert_eq!(values.len(), 16);

        if !dirty {
            if let Some(sleep_after) = dev.sleep_after {
                if dev.device_enabled && dev.last_change.elapsed() > sleep_after {
                    debug!("putting PCA to sleep...");
                    dev.dev
                        .enable_restart_and_disable()
                        .map_err(Self::do_map_err::<I2C, E>)
                        .context("unable to put PCA to sleep")?;
                    dev.device_enabled = false
                }
            }
            return Ok(());
        }

        if !dev.device_enabled {
            debug!("restarting PCA...");
            dev.dev
                .restart(&mut linux_embedded_hal::Delay {})
                .map_err(Self::do_map_err::<I2C, E>)
                .context("unable to restart PCA")?;
            dev.device_enabled = true;
        }

        // Debug print
        debug!("values: [{}]", values.iter().join(", "));

        Self::handle_update_async_inner(values, scalings, &mut dev.dev, metric)
            .context("unable to update PCA")?;
        dev.last_change = Instant::now();

        Ok(())
    }

    fn handle_update_async_inner<I2C, E>(
        values: &[OutputValue],
        scalings: Vec<ValueScaling>,
        dev: &mut Pca9685<I2C>,
        metric: &prometheus::Histogram,
    ) -> Result<()>
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        // Calculate ON/OFF values
        let start_stop_values = Self::calculate_on_off_values(values, scalings);
        assert_eq!(start_stop_values.len(), 32);
        debug!(
            "start_stop values: [{}]",
            start_stop_values.iter().join(", ")
        );

        // Split into two arrays...
        let (a, b): (Vec<_>, Vec<_>) = start_stop_values.iter().tuples::<(_, _)>().unzip();

        let mut start_values: [u16; 16] = [0; 16];
        let mut stop_values: [u16; 16] = [0; 16];

        start_values.copy_from_slice(&a[..]);
        stop_values.copy_from_slice(&b[..]);

        // Send to device
        let before = Instant::now();
        dev.set_all_on_off(&start_values, &stop_values)
            .map_err(Self::do_map_err::<I2C, E>)?;
        let after = Instant::now();
        metric.observe(after.duration_since(before).as_micros() as f64);
        debug!(
            "PCA9685 set_all_on_off took {}µs => {}KiBit/s",
            after.duration_since(before).as_micros(),
            ((64 * 8) as f64 / after.duration_since(before).as_secs_f64()) / 1024_f64
        );

        Ok(())
    }

    fn do_map_err<I2C, E>(e: pwmdev::Error<E>) -> anyhow::Error
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        match e {
            pwmdev::Error::InvalidInputData => panic!("invalid input data"),
            pwmdev::Error::I2C(err) => anyhow!("{:?}", err),
        }
    }

    fn calculate_on_off_values(values: &[OutputValue], scalings: Vec<ValueScaling>) -> Vec<u16> {
        // TODO figure this out - use offsets to balance stuff?
        /*let offsets: Vec<u16> = (0..16_usize)
        .map(|n| Self::OFFSET[n % 4] + Self::OFFSET[n / 4])
        .collect();*/
        /*
        let offsets: Vec<u16> = (0..16_usize).map(|_| 0).collect();
        offsets
            .iter()
            .zip(values.iter())
            .flat_map(|c| [c.0 % 4096, ((c.1 >> 4) + c.0) % 4096_u16].to_vec())
            .collect()
            */
        values
            .iter()
            .zip(scalings.into_iter())
            .map(|(v, scaling)| {
                match scaling {
                    ValueScaling::Linear { from, to } => {
                        if from == LOW && to == HIGH {
                            return *v >> 4;
                        }
                        let scaled = alloy::map_range(
                            (LOW as f64, HIGH as f64),
                            (from as f64, to as f64),
                            *v as f64,
                        )
                        .floor() as u16;
                        scaled >> 4
                    }
                    ValueScaling::Logarithmic => {
                        // Apply logarithmic scaling according to https://www.mikrocontroller.net/articles/LED-Fading
                        // First, special-case MIN and MAX because they are probably common...
                        if *v == u16::MIN {
                            u16::MIN
                        } else if *v == u16::MAX {
                            4095
                        } else {
                            // pow(2, log2(b-1) * (x+1) / a)
                            2_f64
                                .powf((4096_f64 - 1_f64).log2() * (*v as f64 + 1_f64) / 65536_f64)
                                .floor() as u16
                        }
                    }
                }
            })
            .flat_map(|v| vec![0, v])
            .collect()
    }

    // Offsets to balance inputs
    //const OFFSET: [u16; 4] = [0, 1024, 2048, 3072];
}

impl OutputHardwareDevice for PCA9685 {
    fn update(&self) -> Result<()> {
        // NOP, because we update regularly anyway
        Ok(())
    }

    fn get_output_port(
        &self,
        port: u8,
        scaling: Option<ValueScaling>,
    ) -> Result<(Box<dyn OutputPort>, EventStream)> {
        ensure!(port < 16, "PCA9685 has 16 ports only");
        self.core.get_output_port(port, scaling)
    }
}

impl HardwareDevice for PCA9685 {
    fn port_alias(&self, port: u8) -> Result<String> {
        ensure!(port < 16, "PCA9685 has 16 ports only");
        Ok(format!("port-{}", port))
    }
}
