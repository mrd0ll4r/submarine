use crate::prom;
use alloy::config::VirtualDeviceConfig;
use alloy::event::{AddressedEvent, Event, EventFilter};
use alloy::Address;
use futures::{Stream, StreamExt};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub type EventSubscriptions = HashMap<Address, Mutex<Vec<(EventFilter, String)>>>;

/// Creates a map of all addresses to empty subscriptions.
pub(crate) fn empty_subscriptions(configs: &Vec<VirtualDeviceConfig>) -> EventSubscriptions {
    let mut event_subscriptions_inner: EventSubscriptions = HashMap::new();
    for virtual_device_config in configs {
        event_subscriptions_inner.insert(virtual_device_config.address, Default::default());
    }

    event_subscriptions_inner
}
pub(crate) async fn consume_events(
    event_subscriptions: Arc<EventSubscriptions>,
    client: hyper::Client<
        hyper_rustls::HttpsConnector<hyper::client::connect::HttpConnector>,
        hyper::Body,
    >,
    address: Address,
    mut stream: Pin<Box<dyn Stream<Item = Vec<Event>> + Send>>,
) {
    debug!("started event consumer for address {}", address);
    let events_generated = &*prom::EVENTS_GENERATED;
    let events_matched = &*prom::EVENTS_MATCHED;
    let events_pushed_duration_ok = prom::EVENTS_PUSHED.with_label_values(&["ok"]);
    let events_pushed_duration_error = prom::EVENTS_PUSHED.with_label_values(&["error"]);

    while let Some(next) = stream.next().await {
        debug!("got events for address {}: {:?}", address, next);
        events_generated.inc_by(next.len() as i64);

        let consumers = event_subscriptions.get(&address);
        match consumers {
            None => {
                debug!("have no consumers for address {}", address);
                continue;
            }
            Some(consumers) => {
                let consumers = {
                    let c = consumers.lock().unwrap();
                    c.clone()
                };

                debug!("have {} consumers for address {}", consumers.len(), address);
                for event in next.into_iter() {
                    let mut endpoints: Vec<&String> = Vec::new();
                    for (filter, endpoint) in &consumers {
                        if filter.matches(&event) {
                            debug!(
                                "filter {:?} for endpoint {} matched for event {:?}",
                                filter, endpoint, event
                            );
                            endpoints.push(&endpoint);
                        }
                    }

                    if endpoints.is_empty() {
                        debug!("no matches for event {:?}", event);
                        continue;
                    }
                    events_matched.inc();

                    let e = {
                        let addressed_event = AddressedEvent {
                            address,
                            event: event.clone(),
                        };
                        serde_json::to_vec(&addressed_event).expect("unable to serialize event")
                    };

                    for ep in endpoints {
                        // TODO make this async? That risks losing ordering...
                        debug!("sending event {:?} to endpoint {}", event, ep);
                        let before = Instant::now();
                        let req = hyper::Request::builder()
                            .method("POST")
                            .uri(ep)
                            .body(hyper::Body::from(e.clone()))
                            .expect("unable to build request");
                        let resp = client.request(req).await;
                        match resp {
                            Err(e) => {
                                error!("unable to send event: {:?}", e);
                                events_pushed_duration_error
                                    .observe(before.elapsed().as_secs_f64());
                            }
                            Ok(resp) => {
                                debug!("got {} response: {:?}", resp.status(), resp.body());
                                events_pushed_duration_ok.observe(before.elapsed().as_secs_f64())
                            }
                        }
                    }
                }
            }
        }
    }
}
