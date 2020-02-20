use crate::prom;
use alloy::config::VirtualDeviceConfig;
use alloy::event::{AddressedEvent, Event, EventFilter};
use alloy::Address;
use futures::channel::mpsc;
use futures::channel::mpsc::{Receiver, Sender};
use futures::stream::Stream;
use futures::task::{Context, Poll};
use futures::StreamExt;
use futures::FutureExt;
use hyper_timeout::TimeoutConnector;
use std::collections::HashMap;
use std::mem;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::future::Future;
use itertools::Itertools;

pub type EventSubscriptions = HashMap<Address, Mutex<Vec<(EventFilter, Sender<AddressedEvent>)>>>;
pub type EventSubscriberRegistry = Arc<Mutex<HashMap<String, Sender<AddressedEvent>>>>;

lazy_static! {
    pub static ref REGISTRY: EventSubscriberRegistry = Arc::new(Mutex::new(HashMap::new()));
}

/// Creates a map of all addresses to empty subscriptions.
pub(crate) fn empty_subscriptions(configs: &Vec<VirtualDeviceConfig>) -> EventSubscriptions {
    let mut event_subscriptions_inner: EventSubscriptions = HashMap::new();
    for virtual_device_config in configs {
        event_subscriptions_inner.insert(virtual_device_config.address, Default::default());
    }

    event_subscriptions_inner
}

struct GreedyBuffer {
    inner: Receiver<AddressedEvent>,
    buf: Vec<AddressedEvent>,
    fused: bool,
    chunk_size: usize,
}

impl GreedyBuffer {
    fn close(mut self) {
        self.inner.close()
    }
}

impl Stream for GreedyBuffer {
    type Item = Vec<AddressedEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.fused {
            return Poll::Ready(None);
        }

        while let Poll::Ready(o) = self.inner.poll_next_unpin(cx) {
            debug!("got Ready({:?}",o);
            match o {
                Some(e) => {
                    self.buf.push(e);
                    if self.buf.len() >= self.chunk_size {
                        return Poll::Ready(Some(mem::replace(&mut self.buf, Default::default())));
                    }
                }
                None => {
                    // Underlying stream is closed, send out remaining elements if we have any.
                    self.fused = true;
                    return if !self.buf.is_empty() {
                        Poll::Ready(Some(mem::replace(&mut self.buf, Default::default())))
                    } else {
                        Poll::Ready(None)
                    };
                }
            }
        }

        // Nothing available  right now, see if we have anything buffered, if yes send that out.
        debug!("got Pending");
        if !self.buf.is_empty() {
            Poll::Ready(Some(mem::replace(&mut self.buf, Default::default())))
        } else {
            Poll::Pending
        }
    }
}

pub fn create_sender(endpoint: String) -> (Sender<AddressedEvent>,Pin<Box<dyn Future<Output=()> + Send>>) {
    let (tx, rx) = mpsc::channel(100);

    let task= async move {
        let events_pushed_duration_ok = prom::EVENT_PUSH_DURATION.with_label_values(&["ok"]);
        let events_pushed_duration_error = prom::EVENT_PUSH_DURATION.with_label_values(&["error"]);
        let mut buf = GreedyBuffer {
            inner: rx,
            buf: Vec::new(),
            fused: false,
            chunk_size: 50,
        };

        let https = hyper_rustls::HttpsConnector::new();
        let mut connector = TimeoutConnector::new(https);
        connector.set_connect_timeout(Some(Duration::from_secs(1)));
        connector.set_read_timeout(Some(Duration::from_secs(1)));
        connector.set_write_timeout(Some(Duration::from_secs(1)));
        let client = hyper::Client::builder().build::<_, hyper::Body>(connector);

        'outer: while let Some(addressed) = buf.next().await {
            debug!("got events [{}] for endpoint {}",addressed.iter().map(|e| format!("{:?}",e)).join(", "),endpoint);
            let serialized = serde_json::to_vec(&addressed).expect("unable to serialize");
            let mut failures:i32 = 0;

            loop {
                if failures > 2 {
                    warn!("endpoint {} generated too many failures",endpoint);
                    break 'outer;
                }
                let req = hyper::Request::builder()
                    .method("POST")
                    .uri(endpoint.clone())
                    .body(hyper::Body::from(serialized.clone()))
                    .expect("unable to build request");

                let before = Instant::now();
                let resp = client.request(req).await;

                match resp {
                    Err(e) => {
                        error!("unable to send events: {:?}", e);
                        events_pushed_duration_error.observe(before.elapsed().as_secs_f64());
                        failures += 1;
                    }
                    Ok(resp) => {
                        debug!("got {} response: {:?}", resp.status(), resp.body());
                        events_pushed_duration_ok.observe(before.elapsed().as_secs_f64());
                        // Don't forget to get out of the loop...
                        break;
                    }
                }
            }
        }

        // TODO this might be racy
        info!("closing receiver for {}",endpoint);
        buf.close();
        REGISTRY.lock().unwrap().remove(&endpoint);
    };

    (tx,task.boxed())
}

pub(crate) async fn consume_events(
    event_subscriptions: Arc<EventSubscriptions>,
    address: Address,
    mut stream: Pin<Box<dyn Stream<Item = Vec<Event>> + Send>>,
) {
    debug!("started event consumer for address {}", address);
    let events_generated = &*prom::EVENTS_GENERATED;
    let events_matched = &*prom::EVENTS_MATCHED;
    let events_dropped = &*prom::EVENTS_DROPPED;

    while let Some(next) = stream.next().await {
        debug!("got events for address {}: {:?}", address, next);
        events_generated.inc_by(next.len() as i64);

        // TODO chunk in smaller sizes?
        let consumers = event_subscriptions.get(&address);
        match consumers {
            None => {
                debug!("have no consumers for address {}", address);
                continue;
            }
            Some(consumers_m) => {
                let consumers = {
                    let mut c = consumers_m.lock().unwrap();
                    // remove dead consumers
                    c.retain(|c| !c.1.is_closed());
                    c.clone()
                };

                debug!("have {} consumers for address {}", consumers.len(), address);
                for event in next.into_iter() {
                    let mut senders: Vec<Sender<AddressedEvent>> = Vec::new();
                    for (filter, sender) in &consumers {
                        if filter.matches(&event) {
                            debug!("filter {:?} matched for event {:?}", filter, event);
                            senders.push(sender.clone());
                        }
                    }

                    if senders.is_empty() {
                        debug!("no matches for event {:?}", event);
                        continue;
                    }
                    events_matched.inc();

                    let addressed_event = AddressedEvent { address, event };

                    for  sender in senders.iter_mut() {
                        debug!("sending event {:?} to sender {:?}", addressed_event, sender);
                        let res = sender.try_send(addressed_event.clone());
                        match res {
                            Ok(_) => {}
                            Err(e) => {
                                if e.is_full() {
                                    events_dropped.inc();
                                }
                                if e.is_disconnected() {
                                    // TODO maybe figure this out
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
