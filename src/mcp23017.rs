use crate::device::{HardwareDevice, VirtualDevice};
use crate::device_core::{DeviceRWCore, SynchronizedDeviceRWCore};
use crate::{prom, Result};
use alloy::event::Event;
use alloy::Value;
use embedded_hal as hal;
use futures::Stream;
use mcp23017 as mcpdev;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Instant, SystemTime};

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
            .map_err(|e| failure::err_msg(format!("{:?}", e)))?;

        let inner = Arc::new(Mutex::new(DeviceRWCore::new_dirty(16)));
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
                let mut core = core.lock().unwrap();
                while !core.dirty {
                    core = cond_var.wait(core).unwrap();
                }

                core.dirty = false;
                core.buffered_values.clone()
            };

            // Debug print
            for (i, value) in values.iter().enumerate() {
                trace!("values[{}] = {}", i, *value,);
            }

            // Do the actual update
            let res = Self::handle_update_async_inner(&values, &mut dev, &hist);
            debug!("updated: {:?}", res);

            // Update device values, generate events, populate error in case something went wrong.
            {
                let ts = SystemTime::now();
                let mut core = core.lock().unwrap();
                core.finish_update(values, ts, res.err());
            }
        }
    }

    fn handle_update_async_inner<I2C, E>(
        values: &[Value],
        dev: &mut mcpdev::MCP23017<I2C>,
        hist: &prometheus::Histogram,
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
        dev.write_gpioab(reg_full)
            .map_err(|e| failure::err_msg(format!("{:?}", e)))?;
        let after = Instant::now();
        hist.observe(after.duration_since(before).as_micros() as f64);
        debug!(
            "write_gpioab took {}µs => {}KiBit/s",
            after.duration_since(before).as_micros(),
            ((2 * 8) as f64 / after.duration_since(before).as_secs_f64()) / 1024_f64
        );

        Ok(())
    }

    fn compute_registers(values: &[Value]) -> (u8, u8) {
        let mut reg = 0_u16;
        for (i, value) in values.iter().enumerate() {
            // inverted
            if *value == 0 {
                reg |= 1 << i as u16
            }
        }

        ((reg >> 8) as u8, reg as u8)
    }

    pub(crate) fn do_map_err<I2C, E>(e: mcp23017::Error<E>) -> failure::Error
    where
        I2C: hal::blocking::i2c::Write<Error = E>
            + Send
            + hal::blocking::i2c::WriteRead<Error = E>
            + 'static,
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        match e {
            mcp23017::Error::BusError(ref err) => failure::err_msg(format!("{:?}", err)),
            mcp23017::Error::InterruptPinError => panic!("interrupt pin not found?"),
        }
    }
}

impl HardwareDevice for MCP23017 {
    fn update(&self) -> Result<()> {
        let mut core = self.core.lock().unwrap();

        if core.dirty {
            self.cond_var.notify_one();
        }

        match core.err.take() {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn get_virtual_device(&self, port: u8) -> Result<Box<dyn VirtualDevice + Send>> {
        ensure!(port < 16, "MCP23017 has 16 ports only");
        self.core.get_virtual_device(port)
    }

    fn get_event_stream(&self, port: u8) -> Result<Pin<Box<dyn Stream<Item = Vec<Event>> + Send>>> {
        ensure!(port < 16, "MCP23017 has 16 ports only");
        self.core.get_event_stream(port)
    }
}
