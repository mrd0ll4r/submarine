use crate::config::ValueScaling;
use crate::device::{EventStream, HardwareDevice, OutputHardwareDevice, OutputPort};
use crate::device_core::{DeviceRWCore, SynchronizedDeviceRWCore};
use crate::{prom, Result};
use alloy::OutputValue;
use anyhow::{anyhow, ensure};
use embedded_hal as hal;
use log::{debug, error, trace};
use mcp23017 as mcpdev;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Condvar};
use std::thread;
use std::time::Instant;

pub(crate) struct MCP23017 {
    cond_var: Arc<Condvar>,
    core: SynchronizedDeviceRWCore,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MCP23017Config {
    pub i2c_bus: String,
    i2c_slave_address: u8,
}

impl MCP23017Config {
    pub(crate) fn slave_address(addr: u8) -> Result<u8> {
        // MCP23017 has 0b0100 fixed, and I2C addresses are 7 bits long,
        // so we check for a 0b0010_0 prefix
        ensure!((addr >> 3) ^ 0b0_0100 == 0, "invalid address");

        Ok(addr)
    }
}

impl MCP23017 {
    pub fn new<I2C, E>(dev: I2C, config: &MCP23017Config, alias: String) -> Result<MCP23017>
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        let addr = MCP23017Config::slave_address(config.i2c_slave_address)?;
        let mut mcp = mcpdev::MCP23017::new(dev, addr).map_err(Self::do_map_err::<I2C, E>)?;
        mcp.all_pin_mode(mcpdev::PinMode::OUTPUT)
            .map_err(|e| anyhow!("{:?}", e))?;

        let inner = SynchronizedDeviceRWCore::new_from_core(DeviceRWCore::new_dirty(
            alias.clone(),
            16,
            ValueScaling::default(),
        ));
        let inner2 = inner.clone();
        let cond_var = Arc::new(Condvar::new());
        let cond_var2 = cond_var.clone();

        thread::Builder::new()
            .name(format!("MCP-o {}", alias))
            .spawn(move || MCP23017::handle_update_async(inner2, cond_var2, mcp, alias))?;

        Ok(MCP23017 {
            cond_var,
            core: inner,
        })
    }

    fn handle_update_async<I2C, E>(
        core: SynchronizedDeviceRWCore,
        cond_var: Arc<Condvar>,
        mut dev: mcpdev::MCP23017<I2C>,
        alias: String,
    ) -> Result<()>
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        let hist = prom::MCP23017_WRITE_DURATION
            .get_metric_with_label_values(&[&alias])
            .unwrap();
        loop {
            // Get copy of the values and the device
            let values = {
                let mut core = core.core.lock().unwrap();
                while !core.dirty {
                    core = cond_var.wait(core).unwrap();
                }

                core.dirty = false;
                core.buffered_values.clone()
            };

            // Debug print
            for (i, value) in values.iter().enumerate() {
                trace!("{}: values[{}] = {}", alias, i, *value,);
            }

            // Do the actual update
            let res = Self::handle_update_async_inner(&values, &mut dev, &hist, &alias);
            debug!("{}: updated: {:?}", alias, res);
            if let Err(ref e) = res {
                error!("{}: update failed: {:?}", alias, e)
            }

            // Update device values, generate events, populate error in case something went wrong.
            {
                let ts = chrono::Utc::now();
                let mut core = core.core.lock().unwrap();
                core.finish_update(res.map(|_| values), ts, false, false);
            }
        }
    }

    fn handle_update_async_inner<I2C, E>(
        values: &[OutputValue],
        dev: &mut mcpdev::MCP23017<I2C>,
        hist: &prometheus::Histogram,
        alias: &str,
    ) -> Result<()>
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        assert_eq!(values.len(), 16);

        // Calculate registers
        let (reg_a, reg_b) = Self::compute_registers(values);
        let reg_full = ((reg_a as u16) << 8) | reg_b as u16;

        // Send to device
        let before = Instant::now();
        dev.write_gpioab(reg_full).map_err(|e| anyhow!("{:?}", e))?;
        let after = Instant::now();
        hist.observe(after.duration_since(before).as_micros() as f64);
        debug!(
            "{}: write_gpioab took {}µs => {}KiBit/s",
            alias,
            after.duration_since(before).as_micros(),
            ((2 * 8) as f64 / after.duration_since(before).as_secs_f64()) / 1024_f64
        );

        Ok(())
    }

    fn compute_registers(values: &[OutputValue]) -> (u8, u8) {
        let mut reg = 0_u16;
        for (i, value) in values.iter().enumerate() {
            // non-inverted
            if *value != 0 {
                reg |= 1 << i as u16
            }
        }

        ((reg >> 8) as u8, reg as u8)
    }

    pub(crate) fn do_map_err<I2C, E>(e: mcp23017::Error<E>) -> anyhow::Error
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        match e {
            mcp23017::Error::BusError(ref err) => anyhow!("{:?}", err),
            mcp23017::Error::InterruptPinError => panic!("interrupt pin not found?"),
        }
    }

    pub(crate) fn compute_port_alias(port: u8) -> Result<String> {
        ensure!(port < 16, "MCP23017 has 16 ports only");
        let mut s = String::new();
        if port < 8 {
            s.push_str("a-")
        } else {
            s.push_str("b-")
        }
        s.push_str(format!("{}", port % 8).as_str());

        Ok(s)
    }
}

impl OutputHardwareDevice for MCP23017 {
    fn update(&self) -> Result<()> {
        let core = self.core.core.lock().unwrap();

        if core.dirty {
            self.cond_var.notify_one();
        }

        Ok(())
    }

    fn get_output_port(
        &self,
        port: u8,
        scaling: Option<ValueScaling>,
    ) -> Result<(Box<dyn OutputPort>, EventStream)> {
        ensure!(port < 16, "MCP23017 has 16 ports only");
        self.core.get_output_port(port, scaling)
    }
}

impl HardwareDevice for MCP23017 {
    fn port_alias(&self, port: u8) -> Result<String> {
        MCP23017::compute_port_alias(port)
    }
}
