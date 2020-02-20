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
use crate::event::{consume_events, EventSubscriptions};
use alloy::Address;
use failure::{err_msg, Error, ResultExt};
use futures::channel::mpsc::{ channel, Sender};
use futures::StreamExt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
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
mod pca9685;
mod pca9685_sync;
mod poll;
mod prom;
mod server;

type Result<T> = std::result::Result<T, Error>;

struct State {
    inner: Arc<Mutex<DeviceState>>,
    event_subscriptions: Arc<EventSubscriptions>,
    spawner: Sender<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    logging::set_up_logging().context("unable to set up logging")?;

    info!("reading main config file...");
    let raw_main_config = config::ConfigFile::read_from_file("config.yaml")?;
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

    // Assume no subscriptions exist yet...
    let event_subscriptions = Arc::new(event::empty_subscriptions(
        &device_state.virtual_device_configs,
    ));

    info!("starting event stream consumers...");
    for (address, stream) in event_streams.into_iter() {
        task::spawn(consume_events(
            event_subscriptions.clone(),
            address as Address,
            stream,
        ));
    }

    let (tx, mut rx) = channel(0);
    task::spawn(async move {
        while let Some(t) = rx.next().await {
            task::spawn(t);
        }
    });

    let state = State {
        inner: Arc::new(Mutex::new(device_state)),
        event_subscriptions,
        spawner: tx,
    };

    info!("starting API...");
    let app = server::new(state);
    let server = app.listen(cfg.program.api_listen_address.clone());

    info!("listening on http://{}", cfg.program.api_listen_address);

    server.await?;

    Ok(())
}
