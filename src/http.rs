use crate::device::UniverseState;
use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use warp::Filter;

pub(crate) async fn run_server(addr: SocketAddr, state: Arc<Mutex<UniverseState>>) -> Result<()> {
    let api = filters::api(state);

    let routes = api.with(warp::log("api"));

    // Start up the server...
    let (_, fut) = warp::serve(routes)
        .try_bind_ephemeral(addr)
        .context("unable to bind")?;
    fut.await;

    Ok(())
}

mod filters {
    use super::handlers;
    use crate::device::UniverseState;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use warp::{body, Filter};

    pub(crate) fn api(
        state: Arc<Mutex<UniverseState>>,
    ) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        warp::path!("api" / "v1" / ..).and(universe(state))
    }

    pub(crate) fn universe(
        state: Arc<Mutex<UniverseState>>,
    ) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        warp::path!("universe" / ..).and(
            universe_config(state.clone())
                .or(universe_last_values(state.clone()))
                .or(universe_set(state.clone())),
        )
    }

    pub(crate) fn universe_config(
        state: Arc<Mutex<UniverseState>>,
    ) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        warp::path!("config")
            .and(warp::get())
            .and(with_state(state))
            .and_then(handlers::get_universe_config)
    }

    pub(crate) fn universe_last_values(
        state: Arc<Mutex<UniverseState>>,
    ) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        warp::path!("last_values")
            .and(warp::get())
            .and(with_state(state))
            .and_then(handlers::get_universe_last_values)
    }

    pub(crate) fn universe_set(
        state: Arc<Mutex<UniverseState>>,
    ) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        warp::path!("set")
            .and(warp::post())
            .and(with_state(state))
            .and(body::json())
            .and_then(handlers::post_universe_set)
    }

    fn with_state(
        state: Arc<Mutex<UniverseState>>,
    ) -> impl Filter<Extract = (Arc<Mutex<UniverseState>>,), Error = std::convert::Infallible> + Clone
    {
        warp::any().map(move || state.clone())
    }
}

mod handlers {
    use crate::device::UniverseState;
    use alloy::api::{SetRequest, SetRequestTarget};
    use anyhow::{bail, Context};
    use log::debug;
    use std::convert::Infallible;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use warp::http;

    pub(crate) async fn get_universe_config(
        state: Arc<Mutex<UniverseState>>,
    ) -> Result<impl warp::Reply, Infallible> {
        let cfg = state.lock().await.to_alloy_universe_config().await;

        Ok(warp::reply::json(&cfg))
    }

    pub(crate) async fn get_universe_last_values(
        state: Arc<Mutex<UniverseState>>,
    ) -> Result<impl warp::Reply, Infallible> {
        let cfg = state.lock().await.get_all_last_values().await;

        Ok(warp::reply::json(&cfg))
    }

    pub(crate) async fn post_universe_set(
        state: Arc<Mutex<UniverseState>>,
        set_requests: Vec<SetRequest>,
    ) -> Result<impl warp::Reply, Infallible> {
        let mut state = state.lock().await;

        let res = handle_set_requests(&mut state, set_requests).await;
        debug!("handle_set_requests returned {:?}", res);

        // TODO figure out proper errors
        match res {
            Ok(_) => Ok(http::StatusCode::OK),
            Err(_) => Ok(http::StatusCode::BAD_REQUEST),
        }
    }

    async fn handle_set_requests(
        state: &mut UniverseState,
        set_requests: Vec<SetRequest>,
    ) -> Result<(), anyhow::Error> {
        for req in set_requests {
            match req.target {
                SetRequestTarget::Address(addr) => {
                    state
                        .set_address(addr, req.value)
                        .await
                        .context("unable to set address")?;
                }
                SetRequestTarget::Alias(_) => {
                    bail!("alias targets not supported at the moment")
                }
            }
        }

        state.update().await.context("unable to update hardware")?;

        Ok(())
    }
}
