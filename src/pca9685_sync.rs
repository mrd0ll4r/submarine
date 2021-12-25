use crate::device_core::SynchronizedDeviceRWCore;
use crate::pca9685::{PCA9685Config, Pca9685Device, PCA9685};
use crate::{pca9685, poll, prom, Result};
use embedded_hal as hal;
use prometheus::Histogram;
use std::thread;
use std::time::{Duration, Instant};

pub(crate) struct PCA9685Synchronized<I2C> {
    cores: Vec<(
        SynchronizedDeviceRWCore,
        Pca9685Device<I2C>,
        Histogram,
        String,
    )>,
}

impl<I2C, E> PCA9685Synchronized<I2C>
where
    I2C: hal::blocking::i2c::Write<Error = E>
        + Send
        + hal::blocking::i2c::WriteRead<Error = E>
        + 'static,
    E: Sync + Send + std::fmt::Debug + 'static,
{
    pub(crate) fn new() -> PCA9685Synchronized<I2C> {
        PCA9685Synchronized { cores: Vec::new() }
    }

    pub(crate) fn add(
        &mut self,
        i2c: I2C,
        core: SynchronizedDeviceRWCore,
        cfg: &PCA9685Config,
        alias: String,
    ) -> Result<()> {
        let dev = pca9685::set_up_device(i2c, cfg)?;
        let hist = prom::PCA9685_WRITE_DURATION
            .get_metric_with_label_values(&[&alias])
            .unwrap();

        self.cores.push((core, dev, hist, alias));

        Ok(())
    }

    pub(crate) fn start(self, update_interval_millis: u64) -> Result<()> {
        thread::Builder::new()
            .name("PCA-sync".to_string())
            .spawn(move || Self::handle_update_async(self.cores, update_interval_millis))?;
        Ok(())
    }

    fn handle_update_async(
        mut cores: Vec<(
            SynchronizedDeviceRWCore,
            Pca9685Device<I2C>,
            Histogram,
            String,
        )>,
        update_interval_millis: u64,
    ) -> Result<()> {
        let update_interval = Duration::from_millis(update_interval_millis);
        let num_cores = cores.len();
        let mut sleep_duration = update_interval;
        let mut last_wakeup = Instant::now();
        loop {
            let wake_up =
                poll::poll_loop_begin_sleep(module_path!(), sleep_duration, &mut last_wakeup);
            let ts = chrono::Utc::now();

            for (i, tuple) in cores.iter_mut().enumerate() {
                let (ref mut core, ref mut dev, ref hist, ref alias) = tuple;
                let (values, scalings, dirty) = poll::get_values_and_dirty(core);

                // Do the actual update
                let res =
                    PCA9685::handle_update_async_inner_outer(&values, scalings, dirty, dev, hist);
                debug!(
                    "updated PCA9685 {} ({} of {}): {:?}",
                    alias,
                    i + 1,
                    num_cores,
                    res
                );

                // Update device values, generate events, populate error in case something went wrong.
                {
                    let mut core = core.core.lock().unwrap();
                    core.finish_update(res.map(|_| values), ts);
                }
            }

            trace!(
                "updated {} PCAs in {} µs",
                num_cores,
                wake_up.elapsed().as_micros()
            );

            let (s, lagging) = poll::calculate_sleep_duration(wake_up, update_interval);
            if lagging > Duration::from_secs(0) {
                warn!("took too long, lagging {}µs behind", lagging.as_micros());
            }
            sleep_duration = s;
        }
    }
}
