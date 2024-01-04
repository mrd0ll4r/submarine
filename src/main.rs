//#![allow(dead_code)]
//#![allow(unused_imports)]

#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate prometheus;

use crate::device::UniverseState;
use alloy::amqp;
use anyhow::Context;
use log::{debug, info};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::task;

mod bme280;
mod button_expander;
mod config;
mod device;
mod device_core;
mod dht22;
mod dht22_expander;
mod dht22_lib;
mod ds18;
mod ds18b20_expander;
mod ds18b20_expander_lib;
mod fan_heater;
mod gpio;
mod http;
mod i2c_mock;
mod logging;
mod mcp23017;
mod mcp23017_input;
mod pca9685;
mod pca9685_sync;
mod poll;
mod prom;

type Result<T> = anyhow::Result<T>;

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

    info!("connecting to AMQP broker...");
    let amqp_inputs_client =
        amqp::ExchangeSubmarineInput::new(&cfg.program.amqp_server_address, &Vec::new())
            .await
            .context("unable to connect to AMQP broker")?;

    info!("creating state...");
    let device_state = UniverseState::from_config(&cfg, &amqp_inputs_client)
        .await
        .context("unable to create device state")?;

    let state = Arc::new(Mutex::new(device_state));

    info!("starting HTTP server...");
    let http_server_address = cfg.program.http_server_listen_address.parse()?;
    let http_server = task::spawn(http::run_server(http_server_address, state.clone()));
    info!("HTTP server is listening on http://{}", http_server_address);

    http_server.await??;

    Ok(())
}
