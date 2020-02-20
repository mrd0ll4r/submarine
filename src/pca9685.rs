use crate::device::{EventStream, HardwareDevice, VirtualDevice};
use crate::device_core::{DeviceRWCore, SynchronizedDeviceRWCore};
use crate::poll;
use crate::prom;
use crate::Result;
use alloy::Value;
use embedded_hal as hal;
use itertools::Itertools;
use pwm_pca9685 as pwmdev;
use pwm_pca9685::Pca9685;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
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
}

pub(crate) fn set_up_device<I2C, E>(i2c: I2C, config: &PCA9685Config) -> Result<Pca9685<I2C>>
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
    let mut dev = pwmdev::Pca9685::new(i2c, addr);

    dev.set_prescale(4).map_err(PCA9685::do_map_err::<I2C, E>)?;

    if config.inverted {
        dev.set_output_logic_state(pwmdev::OutputLogicState::Inverted)
            .map_err(PCA9685::do_map_err::<I2C, E>)?;
    } else {
        dev.set_output_logic_state(pwmdev::OutputLogicState::Direct)
            .map_err(PCA9685::do_map_err::<I2C, E>)?;
    }

    if config.output_driver_totem_pole {
        dev.set_output_driver(pwmdev::OutputDriver::TotemPole)
            .map_err(PCA9685::do_map_err::<I2C, E>)?;
    } else {
        dev.set_output_driver(pwmdev::OutputDriver::OpenDrain)
            .map_err(PCA9685::do_map_err::<I2C, E>)?;
    }

    if config.use_external_clock {
        dev.use_external_clock()
            .map_err(PCA9685::do_map_err::<I2C, E>)?;
    }

    dev.enable().map_err(PCA9685::do_map_err::<I2C, E>)?;

    Ok(dev)
}

impl PCA9685Config {
    pub(crate) fn slave_address(&self) -> Result<pwmdev::SlaveAddr> {
        let addr = self.i2c_slave_address;
        // PCA9685 has 0b1 fixed, and I2C addresses are 7 bits long,
        // so we check for a 0b01 prefix.
        ensure!((addr >> 6) ^ 0b01 == 0, "invalid address");

        Ok(pwmdev::SlaveAddr::Alternative(
            addr & 0b0010_0000 != 0,
            addr & 0b0001_0000 != 0,
            addr & 0b0000_1000 != 0,
            addr & 0b0000_0100 != 0,
            addr & 0b0000_0010 != 0,
            addr & 0b0000_0001 != 0,
        ))
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

        let inner = Arc::new(Mutex::new(DeviceRWCore::new_dirty(16)));
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
        mut dev: Pca9685<I2C>,
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
            let (wake_up, values, dirty) =
                poll::poll_loop_begin(module_path!(), sleep_duration, &mut last_wakeup, &core);

            // Do the actual update
            let res = Self::handle_update_async_inner_outer(&values, dirty, &mut dev, &hist);

            sleep_duration =
                poll::poll_loop_end(module_path!(), res, &core, values, wake_up, update_interval);
        }
    }

    pub(crate) fn handle_update_async_inner_outer<I2C, E>(
        values: &[Value],
        dirty: bool,
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
        assert_eq!(values.len(), 16);

        if !dirty {
            return Ok(());
        }

        // Debug print
        for (i, value) in values.iter().enumerate() {
            trace!("values[{}] = {}", i, *value);
        }

        Self::handle_update_async_inner(values, dev, metric)
    }

    fn handle_update_async_inner<I2C, E>(
        values: &[Value],
        dev: &mut pwmdev::Pca9685<I2C>,
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
        let start_stop_values = Self::calculate_on_off_values(&values);
        assert_eq!(start_stop_values.len(), 32);

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

    fn do_map_err<I2C, E>(e: pwmdev::Error<E>) -> failure::Error
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        match e {
            pwmdev::Error::InvalidInputData => panic!(e),
            pwmdev::Error::I2C(err) => failure::err_msg(format!("{:?}", err)),
        }
    }

    fn calculate_on_off_values(values: &[Value]) -> Vec<u16> {
        // TODO figure this out
        /*let offsets: Vec<u16> = (0..16_usize)
        .map(|n| Self::OFFSET[n % 4] + Self::OFFSET[n / 4])
        .collect();*/
        let offsets: Vec<u16> = (0..16_usize).map(|_| 0).collect();
        offsets
            .iter()
            .zip(values.iter())
            .flat_map(|c| [c.0 % 4096, ((c.1 >> 4) + c.0) % 4096_u16].to_vec())
            .collect()
    }

    // Offsets to balance inputs
    //const OFFSET: [u16; 4] = [0, 1024, 2048, 3072];
}

impl HardwareDevice for PCA9685 {
    fn update(&self) -> Result<()> {
        self.core.update()
    }

    fn get_virtual_device(&self, port: u8) -> Result<Box<dyn VirtualDevice + Send>> {
        ensure!(port < 16, "PCA9685 has 16 ports only");
        self.core.get_virtual_device(port)
    }

    fn get_event_stream(&self, port: u8) -> Result<EventStream> {
        ensure!(port < 16, "PCA9685 has 16 ports only");
        self.core.get_event_stream(port)
    }
}
