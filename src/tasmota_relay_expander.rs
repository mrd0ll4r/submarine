use crate::config::ValueScaling;
use crate::device::{EventStream, HardwareDevice, OutputHardwareDevice, OutputPort};
use crate::device_core::{DeviceRWCore, SynchronizedDeviceRWCore};
use crate::{prom, Result};
use alloy::{HIGH, LOW};
use anyhow::{bail, ensure, Context};
use itertools::Itertools;
use log::{debug, error, info, trace};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::iter::repeat;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub(crate) struct TasmotaRelayExpander {
    update_channel: tokio::sync::mpsc::Sender<Vec<(usize, bool)>>,
    core: SynchronizedDeviceRWCore,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TasmotaRelayExpanderConfig {
    device_host: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
#[allow(unused)]
struct StatusResponseBody {
    pub module: i64,
    pub device_name: String,
    pub friendly_name: Vec<String>,
    pub topic: String,
    pub button_topic: String,
    pub power: String,
    pub power_lock: String,
    pub power_on_state: i64,
    pub led_state: i64,
    pub led_mask: String,
    pub save_data: i64,
    pub save_state: i64,
    pub switch_topic: String,
    pub switch_mode: Vec<i64>,
    pub button_retain: i64,
    pub switch_retain: i64,
    pub sensor_retain: i64,
    pub power_retain: i64,
    pub info_retain: i64,
    pub state_retain: i64,
    pub status_retain: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct StatusResponse {
    pub status: StatusResponseBody,
}

impl TasmotaRelayExpander {
    fn build_request(
        client: &reqwest::Client,
        cmd_base_uri: &Url,
        command: String,
    ) -> Result<reqwest::Request> {
        let mut url = cmd_base_uri.clone();
        url.set_query(Some(format!("cmnd={}", command).as_str()));
        let res = client.get(url).build()?;
        Ok(res)
    }
    pub async fn new(
        config: &TasmotaRelayExpanderConfig,
        alias: String,
    ) -> Result<TasmotaRelayExpander> {
        ensure!(!config.device_host.is_empty(), "device_host must be set");

        let client = reqwest::Client::new();
        let base_uri = Url::parse(format!("http://{}/cm", config.device_host).as_str())
            .context("unable to build device base URL")?;

        /*
        let status_req = Self::build_request(&client, &base_uri, String::from("status"))
            .context("unable to build status request")?;
        let status_resp = client
            .execute(status_req)
            .await
            .context("unable to execute status request")?
            .bytes()
            .await
            .context("unable to read status response")?
            .to_vec();
        let status_resp_text =
            String::from_utf8(status_resp).context("unable to parse status response")?;
        debug!("{}: status_resp_text: {}", alias, status_resp_text);

        let status_resp = serde_json::from_str::<StatusResponse>(&status_resp_text)
            .context("unable to parse status response")?;
        info!(
            "{} ({}): successfully queried device with name {}",
            alias, config.device_host, status_resp.status.device_name
        );
         */

        let inner = SynchronizedDeviceRWCore::new_from_core(DeviceRWCore::new_dirty(
            alias.clone(),
            13,
            ValueScaling::default(),
        ));
        let inner2 = inner.clone();
        let (update_tx, update_rx) = tokio::sync::mpsc::channel(1);

        tokio::spawn(TasmotaRelayExpander::handle_update_async(
            inner2, update_rx, alias, client, base_uri,
        ));

        Ok(TasmotaRelayExpander {
            update_channel: update_tx,
            core: inner,
        })
    }

    async fn handle_update_async(
        core: SynchronizedDeviceRWCore,
        mut update_channel: tokio::sync::mpsc::Receiver<Vec<(usize, bool)>>,
        alias: String,
        client: reqwest::Client,
        base_uri: Url,
    ) -> Result<()> {
        let hist = prom::TASMOTA_RELAY_EXPANDER_WRITE_DURATION
            .get_metric_with_label_values(&[&alias])
            .unwrap();

        while let Some(new_values) = update_channel.recv().await {
            // Debug print
            for (i, value) in new_values.iter().enumerate() {
                trace!("{}: values[{}] = {:?}", alias, i, *value,);
            }

            // Do the actual update
            let before = Instant::now();
            let res = Self::handle_update_async_inner(&new_values, &client, &base_uri).await;
            hist.observe(before.elapsed().as_micros() as f64);
            debug!("{}: updated: {:?}", alias, res);
            if let Err(ref e) = res {
                error!("{}: update failed: {:?}", alias, e);
            }

            // Update device values, generate events, populate error in case something went wrong.
            {
                let ts = chrono::Utc::now();
                let mut core = core.core.lock().unwrap();
                let mut new_device_values = repeat(false)
                    .take(core.read_core.device_values.len())
                    .collect::<Vec<_>>();
                for (i, v) in new_values.into_iter() {
                    new_device_values[i] = v;
                }
                core.finish_update(
                    res.map(|_| {
                        new_device_values
                            .into_iter()
                            .map(|v| if v { HIGH } else { LOW })
                            .collect()
                    }),
                    ts,
                    false,
                    false,
                );
            }
        }

        Ok(()) // I guess?
    }

    async fn handle_update_async_inner(
        values: &[(usize, bool)],
        client: &reqwest::Client,
        base_url: &Url,
    ) -> Result<()> {
        // Send to device
        let commands = values
            .iter()
            .map(|(i, v)| format!("power{} {}", i + 1, if *v { "on" } else { "off" }))
            .join("; ");
        let command = format!("backlog0; {}", commands);
        debug!("built command string {}", command);

        let req = Self::build_request(client, base_url, command)
            .context("unable to build power request")?;

        let resp = client
            .execute(req)
            .await
            .context("unable to execute backlog0 power request")?;
        if !resp.status().is_success() {
            bail!("power request did not succeed: {:?}", resp);
        }

        Ok(())
    }
}

impl OutputHardwareDevice for TasmotaRelayExpander {
    fn update(&self) -> Result<()> {
        let mut core = self.core.core.lock().unwrap();

        if core.dirty {
            let ports_in_use = core
                .read_core
                .events
                .iter()
                .enumerate()
                .filter(|(_, e)| e.is_some())
                .map(|(i, _)| i)
                .collect::<Vec<_>>();

            core.dirty = false;
            let new_values = core
                .buffered_values
                .iter()
                .map(|v| *v != 0)
                .enumerate()
                .filter(|(i, _)| ports_in_use.contains(i))
                .collect::<Vec<_>>();
            drop(core);
            let chan = self.update_channel.clone();
            tokio::task::spawn_blocking(move || {
                chan.blocking_send(new_values)
                    .expect("unable to send updated values")
            });
        }

        Ok(())
    }

    fn get_output_port(
        &self,
        port: u8,
        scaling: Option<ValueScaling>,
    ) -> Result<(Box<dyn OutputPort>, EventStream)> {
        ensure!(port < 13, "TasmotaRelayExpander has 13 ports only");
        self.core.get_output_port(port, scaling)
    }
}

impl HardwareDevice for TasmotaRelayExpander {
    fn port_alias(&self, port: u8) -> Result<String> {
        ensure!(port < 13, "TasmotaRelayExpander has 13 ports only");
        Ok(format!("{}", port))
    }
}
