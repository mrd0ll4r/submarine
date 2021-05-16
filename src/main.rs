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

use crate::device::DeviceState;
use crate::event::consume_events;
use alloy::Address;
use failure::{err_msg, Error, ResultExt};
use futures::lock::Mutex;
use futures::FutureExt;
use std::sync::Arc;
use tokio::task;

mod config;
mod device;
mod event;
mod i2c_mock;
mod logging;
mod mcp23017;
mod mcp23017_input;
//mod mcp23s17;
mod device_core;
mod dht22;
mod dht22_lib;
mod ds18;
mod gpio;
mod pca9685;
mod pca9685_sync;
mod poll;
mod prom;
mod tcp;

type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug)]
struct State {
    inner: Arc<Mutex<DeviceState>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // This will probably not logged, but meh...
    info!("reading main config file...");
    let raw_main_config = config::ConfigFile::read_from_file("config.yaml")?;

    // TODO it would be nicer to have this as an env variable or command line argument...
    let program_config = raw_main_config
        .config
        .program
        .clone()
        .expect("missing program config block in main config file");

    logging::set_up_logging(program_config.log_to_file).context("unable to set up logging")?;
    info!(
        "set up logging, loging to file? {}",
        program_config.log_to_file
    );

    // Now we can actually start logging stuff
    debug!("read raw main config: {:?}", raw_main_config);

    let mut additional_configs = match raw_main_config.config.program {
        None => return Err(err_msg("missing program config")),
        Some(ref cfg) => match &cfg.config_files {
            Some(files) => {
                if !files.is_empty() {
                    info!("reading auxiliary config files...");
                    let mut configs = Vec::new();
                    for file in files {
                        info!("reading config file \"{}\"", file);
                        let raw_config = config::ConfigFile::read_from_file(file.as_str())?;
                        configs.push(raw_config.config);
                    }
                    configs
                } else {
                    Vec::new()
                }
            }
            None => Vec::new(),
        },
    };

    // We insert this at 0 to make sure it's the "main" config from which we take the program part.
    // This actually doesn't matter because multiple program parts will throw an error during
    // aggregation...
    additional_configs.insert(0, raw_main_config.config);

    info!("aggregating config files...");
    let cfg = config::AggregatedConfig::aggregate_from(additional_configs)?;
    debug!("aggregated config: {:?}", cfg);

    info!("starting prometheus...");
    let prometheus_addr = cfg.program.prometheus_listen_address.parse()?;
    prom::start_prometheus(prometheus_addr)?;

    info!("creating state...");
    let (device_state, event_streams) =
        DeviceState::from_config(&cfg).context("unable to create device state")?;

    // Set up event broadcasts
    let event_broadcasts = Arc::new(event::new_publishers(&device_state.virtual_device_configs));

    info!("starting event stream consumers...");
    for (address, stream) in event_streams.into_iter() {
        let broadcast_chan = event_broadcasts.get(&address).unwrap().clone();
        task::spawn(consume_events(broadcast_chan, address as Address, stream));
    }

    let state = State {
        inner: Arc::new(Mutex::new(device_state)),
    };

    info!("starting TCP server...");
    let tcp_server_address = cfg.program.tcp_server_listen_address.parse()?;
    let mut tcp_server = task::spawn(tcp::run_server(
        tcp_server_address,
        state.inner.clone(),
        event_broadcasts.clone(),
    ))
    .fuse();

    info!(
        "TCP server is listening on tcp://{}",
        cfg.program.tcp_server_listen_address
    );

    loop {
        futures::select! {
            res = tcp_server => {
                if let Err(e) = res {
                    return Err(e.into())
                }
            },
            /*res = http_server => {
                if let Err(e) = res {
                    return Err(e.into())
                }
            },*/
            complete=> break,
        }
    }

    Ok(())
}
