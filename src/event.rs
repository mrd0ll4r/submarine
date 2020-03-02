use crate::prom;
use alloy::config::VirtualDeviceConfig;
use alloy::event::{AddressedEvent, Event};
use alloy::Address;
use futures::{Stream, StreamExt};
use itertools::Itertools;
use std::collections::HashMap;
use std::pin::Pin;
use tokio::sync::broadcast;

// Broadcast channels for every address.
pub type EventPublishers = HashMap<Address, broadcast::Sender<Vec<AddressedEvent>>>;

/// Creates a map of all addresses to a separate broadcast channel.
pub(crate) fn new_publishers(configs: &Vec<VirtualDeviceConfig>) -> EventPublishers {
    let mut producers = HashMap::new();

    for virtual_device_config in configs {
        // We don't actually need the receiving end of this, we derive those from the Sender half
        // later.
        let (tx, _) = broadcast::channel(100);
        producers.insert(virtual_device_config.address, tx);
    }

    producers
}

pub(crate) async fn consume_events(
    broadcast_channel: broadcast::Sender<Vec<AddressedEvent>>,
    address: Address,
    mut stream: Pin<Box<dyn Stream<Item = Vec<Event>> + Send>>,
) {
    debug!("started event consumer for address {}", address);
    let events_generated = &*prom::EVENTS_GENERATED;

    while let Some(events) = stream.next().await {
        events_generated.inc_by(events.len() as i64);
        debug!(
            "got events for address {}: {}",
            address,
            events.iter().map(|e| format!("{:?}", e)).join(", ")
        );

        // Give them addresses...
        let addressed = events
            .into_iter()
            .map(|e| AddressedEvent { address, event: e })
            .collect::<Vec<AddressedEvent>>();

        // Broadcast to any possible subscribers
        let _ = broadcast_channel.send(addressed);
        // Ignore send error, that just means there are not subscribers at the moment.
    }

    debug!("event consumer for address {} shutting down", address);
}
