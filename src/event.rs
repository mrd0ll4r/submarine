use crate::device::EventStream;
use crate::prom;
use alloy::config::VirtualDeviceConfig;
use alloy::event::AddressedEvent;
use alloy::Address;
use futures::StreamExt;
use std::collections::HashMap;
use tokio::sync::broadcast;

// Broadcast channels for every address.
pub type EventPublishers = HashMap<Address, broadcast::Sender<AddressedEvent>>;

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
    broadcast_channel: broadcast::Sender<AddressedEvent>,
    address: Address,
    mut stream: EventStream,
) {
    debug!("started event consumer for address {}", address);
    let events_generated = &*prom::EVENTS_GENERATED;

    while let Some(event) = stream.next().await {
        events_generated.inc();
        debug!("got event for address {}: {:?}", address, event);

        // Give it an address...
        let addressed = AddressedEvent { address, event };

        // Broadcast to any possible subscribers
        let _ = broadcast_channel.send(addressed);
        // Ignore send error, that just means there are no subscribers at the moment.
    }

    debug!("event consumer for address {} shutting down", address);
}
