use crate::device::UniverseState;
use anyhow::{Context, Result};
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use warp::Filter;

/// Wrapper to pretty-print optional values.
struct OptFmt<T>(Option<T>);

impl<T: fmt::Display> fmt::Display for OptFmt<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(ref t) = self.0 {
            fmt::Display::fmt(t, f)
        } else {
            f.write_str("-")
        }
    }
}

pub(crate) async fn run_server(addr: SocketAddr, state: Arc<Mutex<UniverseState>>) -> Result<()> {
    let api = filters::api(state);

    let routes = api.with(warp::log::custom(move |info: warp::log::Info<'_>| {
        // This is the exact same as warp::log::log("api"), but logging at DEBUG instead of INFO.
        log::debug!(
            target: "api",
            "{} \"{} {} {:?}\" {} \"{}\" \"{}\" {:?}",
            OptFmt(info.remote_addr()),
            info.method(),
            info.path(),
            info.version(),
            info.status().as_u16(),
            OptFmt(info.referer()),
            OptFmt(info.user_agent()),
            info.elapsed(),
        );
    }));

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
    use anyhow::{anyhow, bail, Context};
    use itertools::Itertools;
    use log::{debug, error, warn};
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
        let mut failures = Vec::new();
        for req in set_requests {
            match req.target {
                SetRequestTarget::Address(addr) => {
                    if let Err(err) = state
                        .set_address(addr, req.value)
                        .await
                        .context("unable to set address")
                    {
                        warn!("unable to set address: {:?}", &err);
                        failures.push(err);
                    }
                }
                SetRequestTarget::Alias(_) => {
                    bail!("alias targets not supported at the moment")
                }
            }
        }

        if let Err(err) = state.update().await.context("unable to update hardware") {
            error!("unable to update hardware: {:?}", &err);
            failures.push(err);
        }

        if !failures.is_empty() {
            return Err(anyhow!(
                "unable to set one or more outputs: {:?}",
                failures.iter().map(|e| format!("{:?}", e)).join("; ")
            ));
        }

        Ok(())
    }
}
