use crate::device::UniverseState;
use crate::prom;
use crate::Result;
use alloy::api::{APIRequest, APIResult, Message, SetRequest, SubscriptionRequest};
use alloy::event::{AddressedEvent, EventFilter};
use alloy::tcp::Connection;
use alloy::Address;
use failure::err_msg;
use failure::ResultExt;
use futures::lock::Mutex;
use futures::{Stream, StreamExt};
use itertools::Itertools;
use std::collections::HashMap;
use std::mem;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::task;

pub(crate) async fn run_server(addr: SocketAddr, state: Arc<Mutex<UniverseState>>) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;

    loop {
        // Asynchronously wait for an inbound TcpStream.
        let (stream, addr) = listener.accept().await?;
        info!("new connection from {}", addr);
        stream.set_nodelay(true)?;

        // Set up all the stuff we need for a client.
        let state = state.clone();

        // Spawn our handler to be run asynchronously.
        tokio::spawn(async move {
            if let Err(e) = Client::new(state, stream).await {
                error!("unable to handle client: {:?}", e);
            }
        });
    }
}

pub(crate) struct Client {
    remote: SocketAddr,
}

impl Client {
    async fn handle_set_request(state: &mut UniverseState, reqs: Vec<SetRequest>) -> Result<()> {
        for req in reqs {
            state
                .set_address(req.address, req.value)
                .await
                .context("unable to set address")?;
        }

        state.update().await.context("unable to update hardware")?;

        Ok(())
    }

    async fn handle_subscription_request(
        remote: &SocketAddr,
        subscriptions: Arc<HashMap<Address, Mutex<Option<EventFilter>>>>,
        req: SubscriptionRequest,
        state: &Arc<Mutex<UniverseState>>,
        event_sink: Sender<AddressedEvent>,
    ) -> Result<()> {
        let filters = subscriptions.get(&req.address);
        if filters.is_none() {
            return Err(err_msg(format!("invalid address: {}", req.address)));
        }

        let mut filter = filters.unwrap().lock().await;
        let was_empty = filter.is_none();

        *filter = Some(EventFilter {
            strategy: req.strategy,
            entries: req.filters,
        });

        if was_empty {
            let mut event_stream = state
                .lock()
                .await
                .get_event_stream(req.address)
                .context("invalid address")?;
            let address = req.address;
            let remote = *remote;
            let subscriptions = subscriptions.clone();
            task::spawn(async move {
                debug!("event_filter {}/{}: started", remote, address);
                let events_matched = &*prom::EVENTS_MATCHED;
                let events_dropped = &*prom::EVENTS_DROPPED;

                loop {
                    // TODO introduce a per-client shutdown broadcast, triggered by whatever, and
                    // select here
                    let val = event_stream.recv().await;
                    let event = match val {
                        Err(e) => match e {
                            broadcast::error::RecvError::Closed => {
                                break;
                            }
                            broadcast::error::RecvError::Lagged(count) => {
                                debug!(
                                    "event_filter {}/{}: lagged {} events",
                                    remote, address, count
                                );
                                events_dropped.inc_by(count);
                                continue;
                            }
                        },
                        Ok(e) => e,
                    };

                    let f = subscriptions.get(&address).unwrap_or_else(|| {
                        panic!("missing subscriptions entry for address {}", address)
                    });
                    let filters = {
                        let filters = f.lock().await;

                        if filters.is_none() {
                            // No more filters there, so we should quit
                            break;
                        }
                        filters.clone().unwrap()
                    };

                    if !filters.matches(&event.event) {
                        continue;
                    }
                    debug!(
                        "event_filter {}/{}: filter {:?} matched for event {:?}",
                        remote, address, filters, event
                    );

                    let res = event_sink.send(event).await;
                    if res.is_err() {
                        // The only possible error is a SendError, if the remote is closed.
                        debug!("event_filter {}/{}: unable to send", remote, address);
                        break;
                    }

                    // only count this if we actually sent it out
                    events_matched.inc();
                }

                debug!("event_filter {}/{}: shutting down", remote, address);
            });
        }

        Ok(())
    }

    async fn handle_request(
        remote: SocketAddr,
        state: Arc<Mutex<UniverseState>>,
        req: APIRequest,
        event_sink: Sender<AddressedEvent>,
        subscriptions: Arc<HashMap<Address, Mutex<Option<EventFilter>>>>,
    ) -> Result<APIResult> {
        match req {
            APIRequest::SystemTime => Ok(APIResult::SystemTime(chrono::Utc::now())),
            APIRequest::Ping => Ok(APIResult::Ping),
            APIRequest::Set(set_requests) => {
                let mut state = state.lock().await;
                Self::handle_set_request(&mut state, set_requests).await?;
                Ok(APIResult::Set)
            }
            APIRequest::Subscribe(req) => {
                Self::handle_subscription_request(&remote, subscriptions, req, &state, event_sink)
                    .await?;
                Ok(APIResult::Subscribe)
            }
        }
    }

    async fn handle_incoming_messages(
        state: Arc<Mutex<UniverseState>>,
        remote: SocketAddr,
        mut input: Receiver<Message>,
        output: Sender<Message>,
        event_sink: Sender<AddressedEvent>,
        subscriptions: Arc<HashMap<Address, Mutex<Option<EventFilter>>>>,
    ) {
        while let Some(msg) = input.recv().await {
            match msg {
                Message::Version(_) => {
                    error!("handler {}: received version packet", remote);
                    break;
                }
                Message::Events(_) => {
                    error!("handler {}: received events packet", remote);
                    break;
                }
                Message::Response { id: _, inner: _ } => {
                    error!("handler {}: received response packet", remote);
                    break;
                }
                Message::Config(_) => {
                    error!("handler {}: received config packet", remote);
                    break;
                }
                Message::Request { id, inner } => {
                    debug!(
                        "handler {}: received request with ID {}: {:?}",
                        remote, id, inner
                    );
                    let remote = remote;
                    let state = state.clone();
                    let event_sink = event_sink.clone();
                    let subscriptions = subscriptions.clone();
                    let output = output.clone();
                    task::spawn(async move {
                        let result =
                            Self::handle_request(remote, state, inner, event_sink, subscriptions)
                                .await
                                .map_err(|e| format!("{:?}", e));

                        let res = output.send(Message::Response { id, inner: result }).await;
                        if res.is_err() {
                            // The only possible error is a SendError, if the remote is closed.
                            debug!("in-flight request for {}: shutting down", remote);
                        }
                    });
                }
            }
        }

        debug!("handler {}: shutting down", remote);
    }

    pub(crate) async fn new(state: Arc<Mutex<UniverseState>>, conn: TcpStream) -> Result<Client> {
        let conn = Connection::new(conn).await?;
        let remote = conn.remote;
        let messages_out = conn.messages_out.clone();
        let messages_out2 = conn.messages_out.clone();
        let messages_in = conn.messages_in;

        let (event_sink, event_stream) = channel::<AddressedEvent>(100);

        // We have to push two things to the client:
        // - The current universe config
        // - Update events for every input, containing the last known value
        let (cfg, update_events) = {
            let state = state.lock().await;
            let cfg = state.to_alloy_universe_config().await;
            let update_events = state.get_update_events_for_current_values().await;
            (cfg, update_events)
        };

        let subscriptions = cfg
            .devices
            .iter()
            .flat_map(|dev| {
                dev.inputs
                    .iter()
                    .map(|input| input.address)
                    .chain(dev.outputs.iter().map(|output| output.address))
            })
            .map(|addr| (addr, Mutex::new(None)))
            .collect();

        task::spawn(Self::handle_incoming_messages(
            state,
            remote,
            messages_in,
            messages_out2,
            event_sink,
            Arc::new(subscriptions),
        ));

        task::spawn(async move {
            let mut buf = GreedyBuffer {
                inner: event_stream,
                buf: Vec::new(),
                fused: false,
                chunk_size: 10,
            };

            while let Some(addressed) = buf.next().await {
                debug!(
                    "event_buffer {}: got events [{}]",
                    remote,
                    addressed.iter().map(|e| format!("{:?}", e)).join(", ")
                );

                let res = messages_out.send(Message::Events(addressed)).await;
                if let Err(e) = res {
                    debug!("event_buffer {}: unable to send {:?}", remote, e);
                    break;
                }
            }

            debug!("event_buffer {}: shutting down", remote);
        });

        // Push the things
        conn.messages_out
            .send(Message::Config(cfg))
            .await
            .context("unable to push universe config to client")?;
        conn.messages_out
            .send(Message::Events(update_events))
            .await
            .context("unable to push initial values to client")?;

        Ok(Client { remote })
    }
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

        while let Poll::Ready(o) = self.inner.poll_recv(cx) {
            debug!("GreedyBuffer got Ready({:?})", o);
            match o {
                Some(e) => {
                    self.buf.push(e);
                    if self.buf.len() >= self.chunk_size {
                        return Poll::Ready(Some(mem::take(&mut self.buf)));
                    }
                }
                None => {
                    // Underlying stream is closed, send out remaining elements if we have any.
                    self.fused = true;
                    return if !self.buf.is_empty() {
                        Poll::Ready(Some(mem::take(&mut self.buf)))
                    } else {
                        Poll::Ready(None)
                    };
                }
            }
        }

        // Nothing available  right now, see if we have anything buffered, if yes send that out.
        debug!("GreedyBuffer got Pending");
        if !self.buf.is_empty() {
            Poll::Ready(Some(mem::take(&mut self.buf)))
        } else {
            Poll::Pending
        }
    }
}
