use crate::prom;
use crate::Result;
use alloy::api::SubscriptionRequest;
use alloy::config::VirtualDeviceConfig;
use alloy::event::{AddressedEvent, Event, EventFilter};
use alloy::Address;
use bytes::{Buf, Bytes};
use futures::channel::mpsc;
use futures::channel::mpsc::{Receiver, Sender};
use futures::stream::{SplitSink, SplitStream, Stream};
use futures::task::{Context, Poll};
use futures::{SinkExt, StreamExt};
use itertools::Itertools;
use std::collections::HashMap;
use std::mem;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::net::{TcpListener, TcpStream};
use tokio::task;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

pub type EventSubscriptions = HashMap<Address, Mutex<Vec<(EventFilter, Sender<AddressedEvent>)>>>;

/// Creates a map of all addresses to empty subscriptions.
pub(crate) fn empty_subscriptions(configs: &Vec<VirtualDeviceConfig>) -> EventSubscriptions {
    let mut event_subscriptions_inner: EventSubscriptions = HashMap::new();
    for virtual_device_config in configs {
        event_subscriptions_inner.insert(virtual_device_config.address, Default::default());
    }

    event_subscriptions_inner
}

pub async fn run_server(addr: SocketAddr, subscriptions: Arc<EventSubscriptions>) -> Result<()> {
    let mut listener = TcpListener::bind(addr).await?;

    loop {
        // Asynchronously wait for an inbound TcpStream.
        let (stream, addr) = listener.accept().await?;
        info!("new connection from {}", addr);

        let subs = subscriptions.clone();

        // Spawn our handler to be run asynchronously.
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, subs).await {
                error!("unable to handle client: {:?}", e);
            }
        });
    }
}

// Socket writing task.
// This gets bytes from the byte_receiver and sends them out via the write_socket.
async fn do_writer(
    remote: SocketAddr,
    mut write_socket: SplitSink<Framed<TcpStream, LengthDelimitedCodec>, Bytes>,
    mut byte_receiver: Receiver<Vec<u8>>,
) -> Result<()> {
    let events_pushed_duration = &*prom::EVENT_PUSH_DURATION;
    while let Some(b) = byte_receiver.next().await {
        debug!("writer for {}: got {} bytes to send", remote, b.len());

        let before = Instant::now();

        let res = write_socket.send(Bytes::from(b)).await;
        if let Err(e) = res {
            error!("writer for {}: unable to write: {:?}", remote, e);
            break;
        }

        events_pushed_duration.observe(before.elapsed().as_secs_f64());
    }

    debug!("writer for {}: shutting down", remote);
    //write_socket.shutdown().await?;
    // TODO shut down the reader, the encoder dies automatically

    Ok(())
}

// Socket reading task.
// This modifies event subscriptions.
async fn do_read(
    remote: SocketAddr,
    mut read_socket: SplitStream<Framed<TcpStream, LengthDelimitedCodec>>,
    event_sender: Sender<AddressedEvent>,
    subscriptions: Arc<EventSubscriptions>,
) {
    while let Some(msg) = read_socket.next().await {
        if let Err(e) = msg {
            error!(
                "receiver for {}: unable to receive message: {:?}",
                remote, e
            );
            break;
        }

        let msg = serde_json::from_slice::<SubscriptionRequest>(msg.unwrap().bytes());
        if let Err(e) = msg {
            error!("receiver for {}: unable to decode message: {:?}", remote, e);
            break;
        }

        let msg = msg.unwrap();
        debug!("receiver for {}: got message {:?}", remote, msg);

        let filters = subscriptions.get(&msg.address);
        if let None = filters {
            error!(
                "receiver for {}: registration for invalid address: {:?}",
                remote, msg
            );
            break;
        }

        let mut filters = filters.unwrap().lock().unwrap();
        filters.push((
            EventFilter {
                strategy: msg.strategy,
                entries: msg.filters,
            },
            event_sender.clone(),
        ));
    }

    /*
    let mut buffer = BytesMut::with_capacity(4096);
    let mut next_msg_length = None;
    let mut msg_buf = [0_u8; 4096];

    while let Ok(n) = read_socket.read_buf(&mut buffer).await {
        if next_msg_length.is_none() && buffer.len() < 2 {
            // we need to read at least the length prefix...
            continue;
        }

        if let None = next_msg_length {
            next_msg_length = Some(buffer.get_u16());
        }

        if buffer.len() < next_msg_length.unwrap() as usize {
            // we need to read at least this many bytes...
            continue;
        }

        // we have enough bytes to decode a message!
        buffer.copy_to_slice(&mut msg_buf[..next_msg_length.unwrap() as usize]);
        let msg = serde_json::from_slice::<SubscriptionRequest>(
            &msg_buf[..next_msg_length.unwrap() as usize],
        );
        if let Err(e) = msg {
            error!("receiver for {}: unable to decode message: {:?}", remote, e);
            break;
        }

        let msg = msg.unwrap();
        debug!("receiver for {}: got message {:?}", remote, msg);

        let filters = subscriptions.get(&msg.address);
        if let None = filters {
            error!(
                "receiver for {}: registration for invalid address: {:?}",
                remote, msg
            );
            break;
        }

        let mut filters = filters.unwrap().lock().unwrap();
        filters.push((
            EventFilter {
                strategy: msg.strategy,
                entries: msg.filters,
            },
            event_sender.clone(),
        ));

        // reset this to be able to receive next message
        next_msg_length = None;
    }
    */

    // Socket shut down, or we received a malformed message, or something else went wrong...
    debug!("reader for {}: shutting down", remote);
}

// Event aggregation and encoding task.
// Runs until event_receiver or byte_sender is closed.
async fn do_encode(
    remote: SocketAddr,
    event_receiver: Receiver<AddressedEvent>,
    mut byte_sender: Sender<Vec<u8>>,
) {
    let mut buf = GreedyBuffer {
        inner: event_receiver,
        buf: Vec::new(),
        fused: false,
        chunk_size: 50,
    };

    while let Some(addressed) = buf.next().await {
        debug!(
            "encoding task for {}: got events [{}]",
            remote,
            addressed.iter().map(|e| format!("{:?}", e)).join(", ")
        );
        let serialized = serde_json::to_vec(&addressed).expect("unable to serialize");

        let res = byte_sender.send(serialized).await;
        if let Err(e) = res {
            debug!(
                "encoding task for {}: got {:?} on send, shutting down",
                remote, e
            );
            break;
        }
    }

    debug!("encoding task for {}: shutting down", remote);
}

async fn handle_connection(s: TcpStream, subscriptions: Arc<EventSubscriptions>) -> Result<()> {
    let remote = s.peer_addr()?;
    let framed = Framed::new(
        s,
        LengthDelimitedCodec::builder()
            .length_field_length(2)
            .new_codec(),
    );

    let (write_socket, read_socket) = framed.split();
    let (event_sender, event_receiver) = mpsc::channel::<AddressedEvent>(100);
    let (byte_sender, byte_receiver) = mpsc::channel::<Vec<u8>>(0);

    let encoder_handle = task::spawn(do_encode(remote.clone(), event_receiver, byte_sender));

    let (_, res2, res) = tokio::join!(
        do_read(remote.clone(), read_socket, event_sender, subscriptions),
        do_writer(remote.clone(), write_socket, byte_receiver),
        encoder_handle
    );

    res2?;
    res?;
    Ok(())
}

struct GreedyBuffer {
    inner: Receiver<AddressedEvent>,
    buf: Vec<AddressedEvent>,
    fused: bool,
    chunk_size: usize,
}

impl Stream for GreedyBuffer {
    type Item = Vec<AddressedEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.fused {
            return Poll::Ready(None);
        }

        while let Poll::Ready(o) = self.inner.poll_next_unpin(cx) {
            debug!("got Ready({:?}", o);
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

                    for sender in senders.iter_mut() {
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
