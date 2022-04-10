use crate::bme280::BME280Config;
use crate::dht22::DHT22Config;
use crate::dht22_expander::DHT22ExpanderConfig;
use crate::ds18::DS18Config;
use crate::gpio::GPIOConfig;
use crate::mcp23017::MCP23017Config;
use crate::mcp23017_input::MCP23017InputConfig;
use crate::pca9685::PCA9685Config;
use crate::Result;
use alloy::{OutputValue, HIGH, LOW};
use failure::{err_msg, ResultExt};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// The data structure parsed directly from any config file.
/// This includes a top-level `config` block.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct ConfigFile {
    #[serde(rename = "submarine")]
    config: ProgramConfig,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct DeviceConfigFile {
    devices: Option<Vec<DeviceConfig>>,
}

/// The processed, filtered, aggregated config for the whole program and all devices.
/// This is aggregated from multiple config files.
/// Only one `program` block will be taken (the first, later ones will cause errors), and all
/// `devices` and `virtual_devices` blocks will be concatenated.
#[derive(Debug, Clone)]
pub(crate) struct AggregatedConfig {
    pub(crate) program: ProgramConfig,
    pub(crate) devices: Vec<DeviceConfig>,
}

impl AggregatedConfig {
    /// Aggregates multiple raw configs into one aggregated config.
    /// This will filter and check a few things, see the doc on `AggregatedConfig`.
    fn aggregate_from(raw: ConfigFile, devices: Vec<DeviceConfigFile>) -> Result<AggregatedConfig> {
        // TODO maybe move some of the sanitization from device.rs to here?
        let mut cfg = AggregatedConfig {
            program: raw.config,
            devices: Vec::new(),
        };

        for c in devices {
            if let Some(d) = c.devices {
                for dev in d {
                    if cfg
                        .devices
                        .iter()
                        .find(|ddev| ddev.alias == dev.alias)
                        .is_some()
                    {
                        return Err(err_msg(format!("duplicate device alias {}", dev.alias)));
                    }
                    cfg.devices.push(dev)
                }
            }
        }

        Ok(cfg)
    }

    pub(crate) fn read_recursively_from_file<P: AsRef<Path>>(path: P) -> Result<AggregatedConfig> {
        let main_config = ConfigFile::read_from_file(path)?;

        let device_configs = match main_config.config.device_configs {
            None => Vec::new(),
            Some(ref config_paths) => {
                debug!("reading device config files...");
                let mut configs = Vec::new();

                for p in config_paths {
                    debug!("reading config file \"{}\"", p);

                    let cfg = DeviceConfigFile::read_from_file(&p)
                        .context("unable to read device config file")?;
                    configs.push(cfg);
                }

                configs
            }
        };

        Self::aggregate_from(main_config, device_configs)
    }
}

impl ConfigFile {
    /// Reads a config from a file.
    fn read_from_file<P: AsRef<Path>>(path: P) -> Result<ConfigFile> {
        let contents = fs::read(path).context("unable to read file")?;

        let cfg: ConfigFile =
            serde_yaml::from_slice(contents.as_slice()).context("unable to parse config")?;

        Ok(cfg)
    }
}

impl DeviceConfigFile {
    /// Reads a config from a file.
    fn read_from_file<P: AsRef<Path>>(path: P) -> Result<DeviceConfigFile> {
        let contents = fs::read(path).context("unable to read file")?;

        let cfg: DeviceConfigFile =
            serde_yaml::from_slice(contents.as_slice()).context("unable to parse config")?;

        Ok(cfg)
    }
}

/// The config for the program as a whole, independent of devices.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct ProgramConfig {
    pub(crate) tcp_server_listen_address: String,
    pub(crate) prometheus_listen_address: String,
    pub(crate) device_configs: Option<Vec<String>>,
    pub(crate) log_to_file: bool,
}

impl Default for ProgramConfig {
    fn default() -> Self {
        ProgramConfig {
            tcp_server_listen_address: "localhost:3030".to_string(),
            prometheus_listen_address: "0.0.0.0:6969".to_string(),
            device_configs: None,
            log_to_file: true,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct DeviceConfig {
    pub(crate) alias: String,
    pub(crate) tags: Option<HashSet<String>>,

    pub(crate) inputs: Option<Vec<InputMappingConfig>>,
    pub(crate) outputs: Option<Vec<OutputMappingConfig>>,

    #[serde(flatten)]
    pub(crate) hardware_device_config: HardwareDeviceConfig,
}

#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq)]
pub(crate) struct BaseMappingConfig {
    pub(crate) port: u8,
    pub(crate) alias: Option<String>,
    pub(crate) tags: Option<HashSet<String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq)]
pub(crate) struct InputMappingConfig {
    #[serde(flatten)]
    pub(crate) base: BaseMappingConfig,
}

#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq)]
pub(crate) struct OutputMappingConfig {
    #[serde(flatten)]
    pub(crate) base: BaseMappingConfig,
    pub(crate) scaling: Option<ValueScaling>,
    pub(crate) default: Option<OutputValue>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Eq, Ord, PartialOrd, PartialEq)]
pub enum ValueScaling {
    #[serde(rename = "linear")]
    Linear { from: u16, to: u16 },
    #[serde(rename = "logarithmic")]
    Logarithmic,
}

impl Default for ValueScaling {
    fn default() -> Self {
        ValueScaling::Linear {
            from: LOW,
            to: HIGH,
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
pub(crate) enum HardwareDeviceConfig {
    #[serde(rename = "pca9685")]
    PCA9685 { config: PCA9685Config },
    #[serde(rename = "pca9685_synchronized")]
    PCA9685Synchronized { config: PCA9685Config },
    #[serde(rename = "ds18")]
    DS18 { config: DS18Config },
    #[serde(rename = "mcp23017")]
    MCP23017 { config: MCP23017Config },
    #[serde(rename = "bme280")]
    BME280 { config: BME280Config },
    #[serde(rename = "mcp23017_input")]
    MCP23017Input { config: MCP23017InputConfig },
    #[serde(rename = "dht22")]
    DHT22 { config: DHT22Config },
    #[serde(rename = "dht22_expander")]
    DHT22Expander { config: DHT22ExpanderConfig },
    #[serde(rename = "gpio")]
    GPIO { config: GPIOConfig },
}
