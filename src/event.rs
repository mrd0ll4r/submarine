use crate::device::{Address, Value};
use crate::prom;
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use crate::config::VirtualDeviceConfig;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum ButtonEvent {
    Down,
    Up,
    Clicked { duration: Duration },
    LongPress { seconds: u64 },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AddressedEvent {
    pub address: u16,
    pub event: Event,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Event {
    pub timestamp: SystemTime,
    pub inner: EventKind,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum EventKind {
    Button(ButtonEvent),
    Change { new_value: Value },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EventFilter {
    pub strategy: EventFilterStrategy,
    pub entries: Vec<EventFilterEntry>,
}

impl EventFilter {
    fn match_inner(entry: &EventFilterEntry, e: &Event) -> bool {
        match entry {
            EventFilterEntry::Any => true,
            EventFilterEntry::Kind { kind } => match kind {
                EventFilterKind::Change => match e.inner {
                    EventKind::Change { .. } => true,
                    _ => false,
                },
                EventFilterKind::Button { filter } => match &e.inner {
                    EventKind::Button(e) => match filter {
                        ButtonEventFilter::Down => match e {
                            ButtonEvent::Down => true,
                            _ => false,
                        },
                        ButtonEventFilter::Up => match e {
                            ButtonEvent::Up => true,
                            _ => false,
                        },
                        ButtonEventFilter::Clicked => match e {
                            ButtonEvent::Clicked { .. } => true,
                            _ => false,
                        },
                        ButtonEventFilter::LongPress => match e {
                            ButtonEvent::LongPress { .. } => true,
                            _ => false,
                        },
                    },
                    _ => false,
                },
            },
        }
    }

    fn matches(&self, e: &Event) -> bool {
        match &self.strategy {
            EventFilterStrategy::Any => {
                for entry in &self.entries {
                    if Self::match_inner(entry, e) {
                        return true;
                    }
                }
                false
            }
            EventFilterStrategy::All => {
                for entry in &self.entries {
                    if !Self::match_inner(entry, e) {
                        return false;
                    }
                }
                true
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum EventFilterStrategy {
    Any,
    All,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum EventFilterEntry {
    Any,
    Kind { kind: EventFilterKind },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum EventFilterKind {
    Change,
    Button { filter: ButtonEventFilter },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum ButtonEventFilter {
    Down,
    Up,
    Clicked,
    LongPress,
}

pub(crate) type EventSubscriptions = HashMap<Address, Mutex<Vec<(EventFilter, String)>>>;

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
