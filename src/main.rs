//#![allow(dead_code)]
//#![allow(unused_imports)]

#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate prometheus;
#[macro_use]
extern crate failure;
#[macro_use]
extern crate log;

use crate::device::UniverseState;
use failure::{Error, ResultExt};
use futures::lock::Mutex;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::task;

mod bme280;
mod config;
mod device;
mod device_core;
mod dht22;
mod dht22_expander;
mod dht22_lib;
mod ds18;
mod fan_heater;
mod gpio;
mod i2c_mock;
mod logging;
mod mcp23017;
mod mcp23017_input;
mod pca9685;
mod pca9685_sync;
mod poll;
mod prom;
mod tcp;

type Result<T> = std::result::Result<T, Error>;

#[tokio::main]
async fn main() -> Result<()> {
    logging::set_up_logging().expect("unable to set up logging");
    info!("set up logging");

    info!("reading config...");
    let cfg = config::AggregatedConfig::read_recursively_from_file("config.yaml")?;
    debug!("loaded config: {:?}", cfg);

    info!("starting prometheus...");
    let prometheus_addr = cfg
        .program
        .prometheus_listen_address
        .parse::<SocketAddr>()
        .context("unable to parse prometheus_listen_address")?;
    prom::start_prometheus(prometheus_addr)?;

    info!("creating state...");
    let device_state = UniverseState::from_config(&cfg)
        .await
        .context("unable to create device state")?;

    let state = Arc::new(Mutex::new(device_state));

    info!("starting TCP server...");
    let tcp_server_address = cfg.program.tcp_server_listen_address.parse()?;
    let tcp_server = task::spawn(tcp::run_server(tcp_server_address, state.clone()));

    info!(
        "TCP server is listening on tcp://{}",
        cfg.program.tcp_server_listen_address
    );

    tcp_server.await??;

    Ok(())
}
