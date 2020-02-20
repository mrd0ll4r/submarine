use crate::device::DeviceState;
use crate::{logging, Result, State};
use alloy::api::*;
use alloy::event::EventFilter;
use failure::ResultExt;
use http::status::StatusCode;
use std::convert::{From, Into};
use tide::Response;

pub(crate) fn new(s: State) -> tide::Server<State> {
    let mut app = tide::with_state(s);
    app.middleware(logging::RequestLogger::new());
    app.at("/").get(home);
    app.at("/devices")
        .get(|req| async move { result_to_response(devices(req).await) });
    app.at("/set")
        .post(|req| async move { result_to_response(set(req).await) });
    app.at("/subscribe")
        .post(|req| async move { result_to_response(subscribe(req).await) });
    app.at("/get/:addr")
        .get(|req| async move { result_to_response(get(req).await) });
    app
}

async fn home(_req: tide::Request<State>) -> impl tide::IntoResponse {
    "Hello, world!"
}

fn api_response_to_tide_response(resp: APIResponse) -> Response {
    Response::new(resp.status).body_json(&resp).unwrap()
}

pub fn result_to_response(r: std::result::Result<APIResponse, APIResponse>) -> Response {
    match r {
        Ok(r) => api_response_to_tide_response(r),
        Err(r) => {
            let res = api_response_to_tide_response(r);
            if res.status().is_success() {
                panic!(
                    "Attempted to yield error response with success code {:?}",
                    res.status()
                )
            }
            res
        }
    }
}

fn set_inner(state: &mut DeviceState, req: SetRequest) -> Result<()> {
    for value in req.values {
        match value {
            SetRequestInner::Address { address, value } => {
                state
                    .set_address(address, value)
                    .context("unable to set address")?;
            }
            SetRequestInner::Alias { alias, value } => {
                state
                    .set_alias(&alias, value)
                    .context("unable to set alias")?;
            }
            SetRequestInner::Group { group, value } => {
                state
                    .set_group(&group, value)
                    .context("unable to set group")?;
            }
        }
    }

    state.update().context("unable to update hardware")?;

    Ok(())
}

async fn set(mut req: tide::Request<State>) -> std::result::Result<APIResponse, APIResponse> {
    let r: SetRequest = req
        .body_json()
        .await
        .map_err(|_| APIResponse::from(StatusCode::BAD_REQUEST))?;
    debug!("set request: {:?}", r);

    let res = {
        let mut state = req.state().inner.lock().unwrap();
        set_inner(&mut state, r)
    };

    res.map(|_| APIResult::Set)
        .map(APIResponse::from)
        .map_err(|e| {
            warn!("set failed: {:?}", e);
            APIResponse::from_error(e).into()
        })
}

async fn get(req: tide::Request<State>) -> std::result::Result<APIResponse, APIResponse> {
    let addr: u16 = req.param("addr").unwrap();
    debug!("get parsed addr: {}", addr);

    let res = {
        let state = req.state().inner.lock().unwrap();
        state.get_address(addr)
    };

    res.map(|res| APIResult::Get { value: res })
        .map(APIResponse::from)
        .map_err(|e| {
            warn!("get failed: {:?}", e);
            APIResponse::from_error(e).into()
        })
}

async fn devices(req: tide::Request<State>) -> std::result::Result<APIResponse, APIResponse> {
    let res = {
        let state = req.state().inner.lock().unwrap();
        state.virtual_device_configs.clone()
    };

    Ok(APIResponse::from(APIResult::Devices {
        virtual_devices: res,
    }))
}

async fn subscribe(mut req: tide::Request<State>) -> std::result::Result<APIResponse, APIResponse> {
    let r: SubscriptionRequest = req
        .body_json()
        .await
        .map_err(|_| APIResponse::from(StatusCode::BAD_REQUEST))?;

    debug!("subscription request: {:?}", r);

    let callback = r.callback.clone();

    let mut senders = crate::event::REGISTRY.lock().unwrap();
    if !senders.contains_key(&r.callback) {
        let (sender, t) = crate::event::create_sender(callback.clone());
        senders.insert(callback, sender);
        let res = req.state().spawner.clone().try_send(t);
        match res {
            Ok(()) => {}
            Err(e) => {
                error!("unable to spawn new sender: {:?}", e);
                return Err(APIResponse::from_status(StatusCode::INTERNAL_SERVER_ERROR));
            }
        }
    }
    let sender = senders.get(&r.callback).unwrap().clone();

    let subscriptions = &*req.state().event_subscriptions;
    let mut filters = subscriptions
        .get(&r.address)
        .expect("missing address in event filters")
        .lock()
        .unwrap();
    filters.push((
        EventFilter {
            strategy: r.strategy,
            entries: r.filters,
        },
        sender,
    ));

    Ok(APIResponse::from(APIResult::Subscription))
}
