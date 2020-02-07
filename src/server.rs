use crate::config::VirtualDeviceConfig;
use crate::device::DeviceState;
use crate::event::{EventFilter, EventFilterEntry, EventFilterStrategy};
use crate::{logging, Result, State};
use failure::ResultExt;
use http::status::StatusCode;
use serde::{Deserialize, Serialize};
use std::convert::{From, Into};
use tide::{IntoResponse, Response};

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

#[derive(Serialize, Deserialize, Debug)]
struct SubscriptionRequest {
    address: u16,
    strategy: EventFilterStrategy,
    filters: Vec<EventFilterEntry>,
    callback: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct SetRequest {
    values: Vec<SetRequestInner>,
}

#[derive(Serialize, Deserialize, Debug)]
enum SetRequestInner {
    Address { address: u16, value: u16 },
    Alias { alias: String, value: u16 },
    Group { group: String, value: u16 },
}

#[derive(Serialize, Deserialize, Debug)]
struct APIResponse {
    ok: bool,
    status: u16,
    error: Option<String>,
    result: Option<APIResult>,
}

#[derive(Serialize, Deserialize, Debug)]
enum APIResult {
    Set,
    Get {
        value: u16,
    },
    Subscription,
    Device {
        virtual_devices: Vec<VirtualDeviceConfig>,
    },
}

impl APIResponse {
    fn from_result(result: APIResult) -> Self {
        APIResponse {
            ok: true,
            status: StatusCode::OK.as_u16(),
            error: None,
            result: Some(result),
        }
    }

    fn from_status(status: StatusCode) -> Self {
        APIResponse {
            ok: status.is_success(),
            status: status.as_u16(),
            error: if status.is_success() {
                None
            } else {
                Some(status.canonical_reason().unwrap().to_string())
            },
            result: None,
        }
    }

    fn from_error(e: failure::Error) -> Self {
        APIResponse {
            ok: false,
            status: StatusCode::BAD_REQUEST.as_u16(),
            error: Some(format!("{}", e)),
            result: None,
        }
    }

    fn as_response(&self) -> Response {
        Response::new(self.status).body_json(&self).unwrap()
    }
}

impl IntoResponse for APIResponse {
    fn into_response(self) -> Response {
        self.as_response()
    }
}

impl From<APIResponse> for tide::Error {
    fn from(resp: APIResponse) -> Self {
        tide::Error::from(resp.into_response())
    }
}

impl From<StatusCode> for APIResponse {
    fn from(status: StatusCode) -> Self {
        APIResponse::from_status(status)
    }
}

impl From<APIResult> for APIResponse {
    fn from(res: APIResult) -> Self {
        APIResponse::from_result(res)
    }
}

pub fn result_to_response<T: IntoResponse, E: IntoResponse>(
    r: std::result::Result<T, E>,
) -> Response {
    match r {
        Ok(r) => r.into_response(),
        Err(r) => {
            let res = r.into_response();
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

async fn set(mut req: tide::Request<State>) -> tide::Result<impl IntoResponse> {
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

async fn get(req: tide::Request<State>) -> tide::Result<impl IntoResponse> {
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

async fn devices(req: tide::Request<State>) -> tide::Result<impl IntoResponse> {
    let res = {
        let state = req.state().inner.lock().unwrap();
        state.virtual_device_configs.clone()
    };

    Ok(APIResponse::from(APIResult::Device {
        virtual_devices: res,
    }))
}

async fn subscribe(mut req: tide::Request<State>) -> tide::Result<impl IntoResponse> {
    let r: SubscriptionRequest = req
        .body_json()
        .await
        .map_err(|_| APIResponse::from(StatusCode::BAD_REQUEST))?;

    debug!("subscription request: {:?}", r);

    let state = &*req.state().event_subscriptions;
    let a = state
        .get(&r.address)
        .expect("missing address in event filters");
    {
        let mut filters = a.lock().unwrap();
        filters.push((
            EventFilter {
                strategy: r.strategy,
                entries: r.filters,
            },
            r.callback,
        ));
    }

    Ok(APIResponse::from(APIResult::Subscription))
}
