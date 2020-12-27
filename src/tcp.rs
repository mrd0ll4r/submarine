use crate::device::DeviceState;
use crate::event::EventPublishers;
use crate::prom;
use crate::Result;
use alloy::api::{
    APIRequest, APIResult, Message, SetRequest, SetRequestTarget, SubscriptionRequest,
};
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

pub(crate) async fn run_server(
    addr: SocketAddr,
    state: Arc<Mutex<DeviceState>>,
    event_publishers: Arc<EventPublishers>,
) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;

    loop {
        // Asynchronously wait for an inbound TcpStream.
        let (stream, addr) = listener.accept().await?;
        info!("new connection from {}", addr);
        stream.set_nodelay(true)?;

        // Set up all the stuff we need for a client.
        let state = state.clone();
        let publishers = event_publishers.clone();

        // Spawn our handler to be run asynchronously.
        tokio::spawn(async move {
            if let Err(e) = Client::new(state, publishers, stream).await {
                error!("unable to handle client: {:?}", e);
            }
        });
    }
}

pub(crate) struct Client {
    remote: SocketAddr,
}

impl Client {
    fn set_inner(state: &mut DeviceState, reqs: Vec<SetRequest>) -> Result<()> {
        for req in reqs {
            match req.target {
                SetRequestTarget::Address(address) => {
                    state
                        .set_address(address, req.value)
                        .context("unable to set address")?;
                }
                SetRequestTarget::Group(group) => {
                    state
                        .set_group(&group, req.value)
                        .context("unable to set group")?;
                }
            }
        }

        state.update().context("unable to update hardware")?;

        Ok(())
    }

    async fn handle_subscription_request(
        remote: &SocketAddr,
        subscriptions: Arc<HashMap<Address, Mutex<Option<EventFilter>>>>,
        req: SubscriptionRequest,
        event_producers: Arc<EventPublishers>,
        event_sink: Sender<AddressedEvent>,
    ) -> Result<()> {
        let filters = subscriptions.get(&req.address);
        if let None = filters {
            return Err(err_msg(format!("invalid address: {}", req.address)));
        }

        let mut filter = filters.unwrap().lock().await;
        let was_empty = filter.is_none();

        *filter = Some(EventFilter {
            strategy: req.strategy,
            entries: req.filters,
        });

        if was_empty {
            let mut event_stream = event_producers.get(&req.address).unwrap().subscribe();
            let address = req.address;
            let remote = remote.clone();
            let subscriptions = subscriptions.clone();
            task::spawn(async move {
                debug!("event_filter {}/{}: started", remote, address);
                let events_matched = &*prom::EVENTS_MATCHED;
                let events_dropped = &*prom::EVENTS_DROPPED;

                'outer: loop {
                    // TODO introduce a per-client shutdown broadcast, triggered by whatever, and
                    // select here
                    let val = event_stream.recv().await;
                    let events = match val {
                        Err(e) => match e {
                            broadcast::error::RecvError::Closed => {
                                break;
                            }
                            broadcast::error::RecvError::Lagged(count) => {
                                debug!(
                                    "event_filter {}/{}: lagged {} events",
                                    remote, address, count
                                );
                                events_dropped.inc_by(count as i64);
                                continue;
                            }
                        },
                        Ok(e) => e,
                    };

                    let f = subscriptions.get(&address).expect(
                        format!("missing subscriptions entry for address {}", address).as_str(),
                    );
                    let filters = {
                        let filters = f.lock().await;

                        if let None = *filters {
                            // No more filters there, so we should quit
                            break;
                        }
                        filters.clone().unwrap()
                    };

                    for event in events {
                        if !filters.matches(&event.event) {
                            continue;
                        }
                        debug!(
                            "event_filter {}/{}: filter {:?} matched for event {:?}",
                            remote, address, filters, event
                        );

                        let res = event_sink.send(event).await;
                        if let Err(_) = res {
                            debug!("event_filter {}/{}: unable to send", remote, address);
                            break 'outer;
                        }

                        // only count this if we actually sent it out
                        events_matched.inc();
                    }
                }

                debug!("event_filter {}/{}: shutting down", remote, address);
            });
        }

        Ok(())
    }

    async fn handle_request(
        remote: SocketAddr,
        state: Arc<Mutex<DeviceState>>,
        req: APIRequest,
        event_sink: Sender<AddressedEvent>,
        event_producers: Arc<EventPublishers>,
        subscriptions: Arc<HashMap<Address, Mutex<Option<EventFilter>>>>,
    ) -> Result<APIResult> {
        match req {
            APIRequest::SystemTime => Ok(APIResult::SystemTime(chrono::Utc::now())),
            APIRequest::Ping => Ok(APIResult::Ping),
            APIRequest::Get(address) => {
                let state = state.lock().await;
                let value = state.get_address(address)?;
                Ok(APIResult::Get(value))
            }
            APIRequest::Set(set_requests) => {
                let mut state = state.lock().await;
                Self::set_inner(&mut state, set_requests)?;
                Ok(APIResult::Set)
            }
            APIRequest::Subscribe(req) => {
                Self::handle_subscription_request(
                    &remote,
                    subscriptions,
                    req,
                    event_producers,
                    event_sink,
                )
                .await?;
                Ok(APIResult::Subscribe)
            }
            APIRequest::Devices => {
                let state = state.lock().await;
                let devices = state.virtual_device_configs.clone();
                Ok(APIResult::Devices(devices))
            }
        }
    }

    async fn handle_incoming_messages(
        state: Arc<Mutex<DeviceState>>,
        remote: SocketAddr,
        mut input: Receiver<Message>,
        output: Sender<Message>,
        event_sink: Sender<AddressedEvent>,
        event_producers: Arc<EventPublishers>,
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
                Message::Request { id, inner } => {
                    debug!(
                        "handler {}: received request with ID {}: {:?}",
                        remote, id, inner
                    );
                    let remote = remote.clone();
                    let state = state.clone();
                    let event_sink = event_sink.clone();
                    let subscriptions = subscriptions.clone();
                    let output = output.clone();
                    let producers = event_producers.clone();
                    task::spawn(async move {
                        let result = Self::handle_request(
                            remote,
                            state,
                            inner,
                            event_sink,
                            producers,
                            subscriptions,
                        )
                        .await
                        .map_err(|e| format!("{:?}", e));

                        let res = output.send(Message::Response { id, inner: result }).await;
                        if let Err(_) = res {
                            debug!("in-flight request for {}: shutting down", remote);
                        }
                    });
                }
            }
        }

        debug!("handler {}: shutting down", remote);
    }

    pub(crate) async fn new(
        state: Arc<Mutex<DeviceState>>,
        event_producers: Arc<EventPublishers>,
        conn: TcpStream,
    ) -> Result<Client> {
        let conn = Connection::new(conn).await?;
        let remote = conn.remote;
        let messages_out = conn.messages_out.clone();
        let messages_out2 = conn.messages_out.clone();
        let messages_in = conn.messages_in;

        let (event_sink, event_stream) = channel::<AddressedEvent>(100);

        let mut subscriptions = HashMap::new();
        let virtual_devices = state.lock().await.virtual_device_configs.clone();
        for cfg in virtual_devices {
            subscriptions.insert(cfg.address, Mutex::new(None));
        }

        task::spawn(Self::handle_incoming_messages(
            state,
            remote,
            messages_in,
            messages_out2,
            event_sink,
            event_producers,
            Arc::new(subscriptions),
        ));

        task::spawn(async move {
            let mut buf = GreedyBuffer {
                inner: event_stream,
                buf: Vec::new(),
                fused: false,
                chunk_size: 50,
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
        debug!("GreedyBuffer got Pending");
        if !self.buf.is_empty() {
            Poll::Ready(Some(mem::replace(&mut self.buf, Default::default())))
        } else {
            Poll::Pending
        }
    }
}
