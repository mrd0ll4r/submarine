use crate::bme280::BME280Config;
use crate::dht22::DHT22Config;
use crate::ds18::DS18Config;
use crate::gpio::GPIOConfig;
use crate::mcp23017::MCP23017Config;
use crate::mcp23017_input::MCP23017InputConfig;
use crate::pca9685::PCA9685Config;
use crate::Result;
use alloy::config::VirtualDeviceConfig;
use failure::ResultExt;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// The data structure parsed directly from any config file.
/// This includes a top-level `config` block.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct ConfigFile {
    pub(crate) config: RawConfig,
}

/// A raw, unprocessed, organic, locally-harvested config from one file.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct RawConfig {
    pub(crate) program: Option<ProgramConfig>,
    pub(crate) devices: Option<Vec<DeviceConfig>>,
    pub(crate) virtual_devices: Option<Vec<VirtualDeviceConfig>>,
}

/// The processed, filtered, aggregated config for the whole program and all devices.
/// This is aggregated from multiple config files.
/// Only one `program` block will be taken (the first, later ones will cause errors), and all
/// `devices` and `virtual_devices` blocks will be concatenated.
#[derive(Debug, Clone)]
pub(crate) struct AggregatedConfig {
    pub(crate) program: ProgramConfig,
    pub(crate) devices: Vec<DeviceConfig>,
    pub(crate) virtual_devices: Vec<VirtualDeviceConfig>,
}

impl AggregatedConfig {
    /// Aggregates multiple raw configs into one aggregated config.
    /// This will filter and check a few things, see the doc on `AggregatedConfig`.
    pub(crate) fn aggregate_from(raw: Vec<RawConfig>) -> Result<AggregatedConfig> {
        // TODO maybe move some of the sanitization from device.rs to here?
        let mut cfg = RawConfig {
            program: None,
            devices: Some(Vec::new()),
            virtual_devices: Some(Vec::new()),
        };

        for r in raw {
            if let Some(p) = r.program {
                match cfg.program {
                    Some(_) => {
                        return Err(failure::err_msg("duplicate program config"));
                    }
                    None => cfg.program = Some(p),
                }
            }
            if let Some(d) = r.devices {
                cfg.devices = match cfg.devices {
                    Some(mut devs) => {
                        devs.extend(d.into_iter());
                        Some(devs)
                    }
                    None => {
                        panic!("devices is none?");
                    }
                }
            }
            if let Some(d) = r.virtual_devices {
                cfg.virtual_devices = match cfg.virtual_devices {
                    Some(mut devs) => {
                        devs.extend(d.into_iter());
                        Some(devs)
                    }
                    None => {
                        panic!("virtual_devices is none?");
                    }
                }
            }
        }

        Ok(AggregatedConfig {
            program: cfg.program.unwrap_or_default(),
            devices: cfg.devices.unwrap(),
            virtual_devices: cfg.virtual_devices.unwrap(),
        })
    }
}

impl ConfigFile {
    /// Reads a file and parses its YAML content as a `ConfigFile`.
    pub(crate) fn read_from_file<P: AsRef<Path>>(path: P) -> Result<ConfigFile> {
        let contents = fs::read(path).context("unable to read file")?;

        let cfg: ConfigFile =
            serde_yaml::from_slice(contents.as_slice()).context("unable to parse config")?;

        Ok(cfg)
    }
}

/// The config for the program as a whole, independent of devices.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct ProgramConfig {
    pub(crate) http_server_listen_address: String,
    pub(crate) tcp_server_listen_address: String,
    pub(crate) prometheus_listen_address: String,
    pub(crate) config_files: Option<Vec<String>>,
    pub(crate) log_to_file: bool,
}

impl Default for ProgramConfig {
    fn default() -> Self {
        ProgramConfig {
            http_server_listen_address: "localhost:3000".to_string(),
            tcp_server_listen_address: "localhost:3030".to_string(),
            prometheus_listen_address: "0.0.0.0:6969".to_string(),
            config_files: None,
            log_to_file: true,
        }
    }
}

/// Configuration for specific hardware devices.
///
/// These generally have an alias and a device-specific configuration, which is needed by the driver
/// for that device.
/// The YAML representation is a tagged enum, with lowercase keys.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub(crate) enum DeviceConfig {
    #[serde(rename = "pca9685")]
    PCA9685 {
        alias: String,
        config: PCA9685Config,
    },
    #[serde(rename = "pca9685_synchronized")]
    PCA9685Synchronized {
        alias: String,
        config: PCA9685Config,
    },
    #[serde(rename = "ds18")]
    DS18 { alias: String, config: DS18Config },
    #[serde(rename = "mcp23017")]
    MCP23017 {
        alias: String,
        config: MCP23017Config,
    },
    #[serde(rename = "bme280")]
    BME280 { alias: String, config: BME280Config },
    #[serde(rename = "mcp23017_input")]
    MCP23017Input {
        alias: String,
        config: MCP23017InputConfig,
    },
    #[serde(rename = "dht22")]
    DHT22 { alias: String, config: DHT22Config },
    #[serde(rename = "gpio")]
    GPIO { alias: String, config: GPIOConfig },
}
