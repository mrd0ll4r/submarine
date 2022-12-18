use crate::config::{
    AggregatedConfig, BaseMappingConfig, DeviceConfig, HardwareDeviceConfig, ValueScaling,
};
use crate::device_core::{DeviceRWCore, SynchronizedDeviceRWCore};
use crate::dht22::DHT22;
use crate::ds18::DS18;
use crate::gpio::Gpio;
use crate::pca9685::PCA9685Config;
use crate::pca9685_sync::PCA9685Synchronized;
use crate::Result;
use crate::{config, i2c_mock, mcp23017, mcp23017_input, pca9685};
use crate::{fan_heater, prom};
use alloy::config::{InputValue, InputValueType};
use alloy::event::{AddressedEvent, Event, EventKind};
use alloy::{Address, OutputValue};
use failure::{err_msg, format_err, ResultExt};
use futures::{Stream, StreamExt};
use itertools::Itertools;
use linux_embedded_hal as linux_hal;
use prometheus::core::{AtomicF64, AtomicU64, GenericCounter, GenericGauge};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fmt::{Debug, Formatter};
use std::ops::Deref;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::broadcast::Sender;
use tokio::sync::Mutex;

pub(crate) trait HardwareDevice: Send {
    fn port_alias(&self, port: u8) -> Result<String>;
}

pub(crate) trait OutputHardwareDevice: HardwareDevice {
    /// Triggers an asynchronous update and retrieves any error from previous update(s).
    ///
    /// If the hardware device does not implement asynchronous updates that need to be triggered
    /// (i.e. the device updates constantly), then this will only check for errors.
    fn update(&self) -> Result<()>;

    /// Gets a VirtualDevice and event source for the given port.
    ///
    /// Note that it is generally impossible to map the same port twice.
    fn get_output_port(
        &self,
        port: u8,
        scaling: Option<ValueScaling>,
    ) -> Result<(Box<dyn OutputPort>, EventStream)>;
}

pub(crate) trait InputHardwareDevice: HardwareDevice {
    fn get_input_port(&self, port: u8) -> Result<(InputValueType, EventStream)>;
}

/// An event stream is an asynchronous Stream of events.
///
/// The asynchronous nature does not usually matter, as this is all handled by some magic in tokio.
/// For all intents and purposes, this behaves like an iterator over events.
pub(crate) type EventStream = Pin<Box<dyn Stream<Item = Event> + Send>>;

/// A virtual device is what is exposed by the API to the outside world and mapped to a hardware
/// device.
pub(crate) trait OutputPort: Send {
    /// Sets the _buffered_ value of this virtual device.
    ///
    /// The value will only be written to the hardware once the `OutputHardwareDevice` updates.
    fn set(&self, value: OutputValue) -> Result<()>;
}

pub(crate) struct UniverseState {
    version: tokio::sync::Mutex<Cell<u64>>,
    devices: Vec<HardwareDeviceState>,

    output_ports: HashMap<Address, Arc<tokio::sync::Mutex<Box<dyn OutputPort>>>>,

    event_streams: HashMap<Address, Arc<tokio::sync::broadcast::Sender<AddressedEvent>>>,
}

impl Debug for UniverseState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UniverseState")
            .field("version", &self.version)
            .field("devices", &self.devices)
            .field(
                "output_ports",
                &self
                    .output_ports
                    .iter()
                    .map(|(k, _)| k)
                    .sorted()
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl UniverseState {
    pub(crate) async fn to_alloy_universe_config(&self) -> alloy::config::UniverseConfig {
        let mut dev_configs = Vec::new();
        for dev in self.devices.iter() {
            dev_configs.push(dev.to_alloy_device_config().await)
        }
        alloy::config::UniverseConfig {
            version: self.version.lock().await.get(),
            devices: dev_configs,
        }
    }

    pub(crate) async fn get_update_events_for_current_values(&self) -> Vec<AddressedEvent> {
        let mut events = Vec::new();
        for dev in self.devices.iter() {
            for p in dev.input_ports.iter() {
                if let Some(event) = p.port.get_update_event_for_current_value().await {
                    events.push(event)
                }
            }
            for p in dev.output_ports.iter() {
                if let Some(event) = p.port.get_update_event_for_current_value().await {
                    events.push(event)
                }
            }
        }

        events
    }
}

pub(crate) enum DeviceType {
    DHT22(Box<dyn InputHardwareDevice>),
    DHT22Expander(Box<dyn InputHardwareDevice>),
    PCA9685(Box<dyn OutputHardwareDevice>),
    DS18(Box<dyn InputHardwareDevice>),
    MCP23017Input(Box<dyn InputHardwareDevice>),
    MCP23017(Box<dyn OutputHardwareDevice>),
    BME280(Box<dyn InputHardwareDevice>),
    Gpio(Box<dyn InputHardwareDevice>),
    FanHeater(Box<dyn InputHardwareDevice>, Box<dyn OutputHardwareDevice>),
}

impl Debug for DeviceType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.to_alloy_device_type())
    }
}

impl DeviceType {
    fn to_alloy_device_type(&self) -> alloy::config::DeviceType {
        match self {
            DeviceType::DHT22(_) => alloy::config::DeviceType::DHT22,
            DeviceType::DHT22Expander(_) => alloy::config::DeviceType::DHT22Expander,
            DeviceType::PCA9685(_) => alloy::config::DeviceType::PCA9685,
            DeviceType::DS18(_) => alloy::config::DeviceType::DS18,
            DeviceType::MCP23017Input(_) => alloy::config::DeviceType::MCP23017,
            DeviceType::MCP23017(_) => alloy::config::DeviceType::MCP23017,
            DeviceType::BME280(_) => alloy::config::DeviceType::BME280,
            DeviceType::Gpio(_) => alloy::config::DeviceType::GPIO,
            DeviceType::FanHeater(_, _) => alloy::config::DeviceType::FanHeater,
        }
    }
}

#[derive(Debug)]
pub(crate) struct HardwareDeviceState {
    alias: String,
    tags: HashSet<String>,
    device_type: tokio::sync::Mutex<DeviceType>,

    input_ports: Vec<InputPortState>,
    output_ports: Vec<OutputPortState>,
}

impl HardwareDeviceState {
    async fn to_alloy_device_config(&self) -> alloy::config::DeviceConfig {
        alloy::config::DeviceConfig {
            alias: self.alias.clone(),
            device_type: self.device_type.lock().await.to_alloy_device_type(),
            inputs: self
                .input_ports
                .iter()
                .map(|p| p.to_alloy_port_config())
                .collect(),
            outputs: self
                .output_ports
                .iter()
                .map(|p| p.to_alloy_port_config())
                .collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TimestampedInputValue {
    ts: chrono::DateTime<chrono::Utc>,
    value: std::result::Result<InputValue, String>,
}

#[derive(Debug)]
pub(crate) struct PortState {
    port: u8,
    alias: String,
    tags: HashSet<String>,
    address: Address,
    last_value: Mutex<Option<TimestampedInputValue>>,

    value_metric: GenericGauge<AtomicF64>,
    ok_metric: GenericGauge<AtomicF64>,
    update_ok_counter: GenericCounter<AtomicU64>,
    update_error_counter: GenericCounter<AtomicU64>,
}

impl PortState {
    async fn update_last_value(
        &self,
        ts: chrono::DateTime<chrono::Utc>,
        val: std::result::Result<InputValue, String>,
    ) {
        match &val {
            Ok(v) => {
                self.value_metric.set(match v {
                    InputValue::Binary(b) => {
                        if *b {
                            1_f64
                        } else {
                            0_f64
                        }
                    }
                    InputValue::Temperature(t) => *t,
                    InputValue::Humidity(h) => *h,
                    InputValue::Pressure(p) => *p,
                    InputValue::Continuous(c) => *c as f64,
                });
                self.ok_metric.set(1_f64);
                self.update_ok_counter.inc();
            }
            Err(_) => {
                self.ok_metric.set(0_f64);
                self.update_error_counter.inc();
            }
        }

        {
            let mut v = self.last_value.lock().await;
            *v = Some(TimestampedInputValue { ts, value: val })
        }
    }

    async fn get_update_event_for_current_value(&self) -> Option<AddressedEvent> {
        let v = self.last_value.lock().await;
        v.clone().map(|v| AddressedEvent {
            address: self.address,
            event: Event {
                timestamp: v.ts,
                inner: v.value.map(|val| EventKind::Update { new_value: val }),
            },
        })
    }
}

#[derive(Debug)]
pub(crate) struct InputPortState {
    port: Arc<PortState>,
    value_type: InputValueType,
}

impl InputPortState {
    fn to_alloy_port_config(&self) -> alloy::config::InputPortConfig {
        alloy::config::InputPortConfig {
            alias: self.port.alias.clone(),
            input_type: self.value_type,
            tags: self.port.tags.clone(),
            port: self.port.port,
            address: self.port.address,
        }
    }
}

pub(crate) struct OutputPortState {
    port: Arc<PortState>,

    dev: Arc<tokio::sync::Mutex<Box<dyn OutputPort>>>,
}

impl Debug for OutputPortState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputPortState")
            .field("port", &self.port)
            .finish()
    }
}

impl OutputPortState {
    fn to_alloy_port_config(&self) -> alloy::config::OutputPortConfig {
        alloy::config::OutputPortConfig {
            alias: self.port.alias.clone(),
            tags: self.port.tags.clone(),
            port: self.port.port,
            address: self.port.address,
        }
    }
}

impl UniverseState {
    pub(crate) async fn update(&mut self) -> Result<()> {
        for device in &self.devices {
            let dev = device.device_type.lock().await;
            match dev.deref() {
                DeviceType::DHT22(_)
                | DeviceType::DHT22Expander(_)
                | DeviceType::DS18(_)
                | DeviceType::MCP23017Input(_)
                | DeviceType::BME280(_)
                | DeviceType::Gpio(_) => {}
                DeviceType::PCA9685(dev)
                | DeviceType::MCP23017(dev)
                | DeviceType::FanHeater(_, dev) => {
                    if let Err(e) = dev.update() {
                        error!("unable to update output device {}: {:?}", &device.alias, e)
                    }
                }
            }
        }

        Ok(())
    }

    pub(crate) async fn set_address(&mut self, address: Address, value: OutputValue) -> Result<()> {
        let d = self
            .output_ports
            .get(&address)
            .ok_or_else(|| format_err!("invalid address: {}", address))?;

        d.lock().await.set(value).context(format!(
            "unable to set value for device at address {}",
            address
        ))?;

        Ok(())
    }
}

impl UniverseState {
    pub(crate) async fn from_config(cfg: &AggregatedConfig) -> Result<UniverseState> {
        info!("creating universe state...");
        let universe = Self::create_universe_state(cfg).await?;
        debug!("created universe state {:?}", universe);

        info!(
            "created universe state with {} hardware devices and {} output ports",
            universe.devices.len(),
            universe.output_ports.len()
        );

        Ok(universe)
    }

    pub(crate) fn get_event_stream(
        &self,
        addr: Address,
    ) -> Result<tokio::sync::broadcast::Receiver<AddressedEvent>> {
        let sender = self
            .event_streams
            .get(&addr)
            .ok_or_else(|| err_msg("invalid address"))?;
        Ok(sender.subscribe())
    }

    async fn create_universe_state(cfg: &AggregatedConfig) -> Result<UniverseState> {
        let mut aliases = HashSet::new();
        let mut synchronized_pca9685s: HashMap<
            String,
            Vec<(SynchronizedDeviceRWCore, PCA9685Config, String)>,
        > = HashMap::new();
        let mut address_counter = 1;
        let mut devices = Vec::new();
        let mut addressed_output_ports = HashMap::new();
        let mut event_broadcasters = HashMap::new();

        for device_config in cfg.devices.clone() {
            debug!("creating device {}...", device_config.alias);
            let device_alias = device_config.alias.to_lowercase();
            debug!("normalized alias to {}", device_alias);
            ensure!(
                !aliases.contains(&device_alias),
                "duplicate alias: {}",
                device_alias
            );
            aliases.insert(device_alias.clone());
            let dev = UniverseState::create_device_type(
                &mut synchronized_pca9685s,
                &device_config,
                device_alias.clone(),
            )
            .context(format!("unable to create device {}", device_alias))?;
            let mut input_ports = Vec::new();
            let mut output_ports = Vec::new();

            match &dev {
                DeviceType::DHT22(input_dev)
                | DeviceType::DHT22Expander(input_dev)
                | DeviceType::DS18(input_dev)
                | DeviceType::MCP23017Input(input_dev)
                | DeviceType::BME280(input_dev)
                | DeviceType::Gpio(input_dev) => UniverseState::create_input_port_mappings(
                    &mut aliases,
                    &mut address_counter,
                    &mut event_broadcasters,
                    &device_config,
                    &device_alias,
                    &mut input_ports,
                    input_dev,
                )
                .await
                .context("unable to map input ports")?,
                DeviceType::MCP23017(output_dev) | DeviceType::PCA9685(output_dev) => {
                    UniverseState::create_output_port_mappings(
                        &mut aliases,
                        &mut address_counter,
                        &mut addressed_output_ports,
                        &mut event_broadcasters,
                        &device_config,
                        &device_alias,
                        &mut output_ports,
                        output_dev,
                    )
                    .await
                    .context("unable to map output ports")?
                }
                DeviceType::FanHeater(input_dev, output_dev) => {
                    // Supports both input and output ports
                    let mut cfg_only_inputs = device_config.clone();
                    cfg_only_inputs.outputs = None;
                    let mut cfg_only_outputs = device_config.clone();
                    cfg_only_outputs.inputs = None;

                    UniverseState::create_input_port_mappings(
                        &mut aliases,
                        &mut address_counter,
                        &mut event_broadcasters,
                        &cfg_only_inputs,
                        &device_alias,
                        &mut input_ports,
                        input_dev,
                    )
                    .await
                    .context("unable to map input ports")?;

                    UniverseState::create_output_port_mappings(
                        &mut aliases,
                        &mut address_counter,
                        &mut addressed_output_ports,
                        &mut event_broadcasters,
                        &cfg_only_outputs,
                        &device_alias,
                        &mut output_ports,
                        output_dev,
                    )
                    .await
                    .context("unable to map output ports")?
                }
            }

            let state = HardwareDeviceState {
                alias: device_alias,
                tags: device_config.tags.unwrap_or_default(),
                device_type: Mutex::new(dev),
                input_ports,
                output_ports,
            };

            devices.push(state);
        }

        if !synchronized_pca9685s.is_empty() {
            info!("synchronizing PCA9685s ...");
            Self::synchronize_pcas(synchronized_pca9685s)?;
        }

        Ok(UniverseState {
            version: Mutex::new(Cell::new(1)),
            devices,
            output_ports: addressed_output_ports,
            event_streams: event_broadcasters,
        })
    }

    async fn create_output_port_mappings(
        aliases: &mut HashSet<String>,
        address_counter: &mut u16,
        addressed_output_ports: &mut HashMap<Address, Arc<tokio::sync::Mutex<Box<dyn OutputPort>>>>,
        event_broadcasters: &mut HashMap<
            Address,
            Arc<tokio::sync::broadcast::Sender<AddressedEvent>>,
        >,
        device_config: &DeviceConfig,
        device_alias: &String,
        output_ports: &mut Vec<OutputPortState>,
        output_dev: &Box<dyn OutputHardwareDevice>,
    ) -> Result<()> {
        Self::ensure_no_input_port_mappings_configured(device_config, device_alias)?;

        for port_config in device_config.outputs.clone().unwrap_or_else(|| {
            warn!(
                "output device {} has no output port mappings configured",
                &device_alias
            );
            Vec::new()
        }) {
            let generic_port_alias = output_dev
                .port_alias(port_config.base.port)
                .context("unable to generate port alias")?;
            let port_alias = UniverseState::generate_port_alias(
                aliases,
                device_alias,
                &port_config.base,
                generic_port_alias,
            )?;

            let (output_port, event_stream) = output_dev
                .get_output_port(port_config.base.port, port_config.scaling.clone())
                .context(format!(
                    "unable to map input port {}:{}",
                    device_alias, port_config.base.port,
                ))?;
            let address = *address_counter;

            // Set default value
            if let Some(default_value) = port_config.default {
                output_port
                    .set(default_value)
                    .context("unable to set default value")?;
            }

            let mut port = PortState {
                port: port_config.base.port,
                alias: port_alias.clone(),
                tags: port_config.base.tags.unwrap_or_default(),
                address,
                last_value: Mutex::new(None),
                value_metric: prom::CONTINUOUS.with_label_values(&[port_alias.as_str()]),
                ok_metric: prom::VALUE_OK.with_label_values(&[port_alias.as_str()]),
                update_ok_counter: prom::VALUE_UPDATES
                    .with_label_values(&[port_alias.as_str(), "ok"]),
                update_error_counter: prom::VALUE_UPDATES
                    .with_label_values(&[port_alias.as_str(), "error"]),
            };
            port.tags
                .extend(device_config.tags.clone().unwrap_or_default().into_iter());

            let port = Arc::new(port);
            let port_2 = port.clone();

            let (event_broadcast_sender, _) = tokio::sync::broadcast::channel(100);
            let event_broadcast_sender = Arc::new(event_broadcast_sender);
            let broadcast_sender_2 = event_broadcast_sender.clone();

            UniverseState::spawn_event_to_broadcast_worker(
                event_stream,
                port_2,
                broadcast_sender_2,
                address,
            );

            let output_port_state = OutputPortState {
                port,
                dev: Arc::new(tokio::sync::Mutex::new(output_port)),
            };

            addressed_output_ports.insert(address, output_port_state.dev.clone());
            event_broadcasters.insert(address, event_broadcast_sender);
            *address_counter += 1;
            output_ports.push(output_port_state);
        }
        Ok(())
    }

    fn spawn_event_to_broadcast_worker(
        mut event_stream: EventStream,
        port: Arc<PortState>,
        broadcast_sender: Arc<Sender<AddressedEvent>>,
        address: u16,
    ) {
        tokio::spawn(async move {
            debug!(
                "starting event consumer -> broadcaster for address {}",
                address
            );
            while let Some(event) = event_stream.next().await {
                // Update port value
                match event.inner.clone() {
                    Ok(ek) => {
                        if let alloy::event::EventKind::Update { new_value } = ek {
                            port.update_last_value(event.timestamp, Ok(new_value)).await
                        }
                    }
                    Err(err) => port.update_last_value(event.timestamp, Err(err)).await,
                }

                // Push into broadcast channel
                // We ignore a potential error, because they can happen if there are no receivers
                // at the moment. We can create more receivers though, so we just ignore it.
                let _ = broadcast_sender.send(AddressedEvent { address, event });
            }
        });
    }

    fn generate_port_alias(
        aliases: &mut HashSet<String>,
        device_alias: &String,
        port_config: &BaseMappingConfig,
        generic_port_alias: String,
    ) -> Result<String> {
        let port_alias = port_config
            .alias
            .clone()
            .unwrap_or_else(|| {
                warn!(
                    "no alias given for port {}:{}, using device-specific port alias {}-{}",
                    device_alias, port_config.port, device_alias, generic_port_alias
                );
                format!("{}-{}", device_alias, generic_port_alias)
            })
            .to_lowercase();
        debug!(
            "creating port {}:{}, alias {}...",
            device_alias, port_config.port, port_alias
        );
        ensure!(
            !aliases.contains(&port_alias),
            "duplicate alias: {}",
            port_alias
        );
        aliases.insert(port_alias.clone());
        Ok(port_alias)
    }

    async fn create_input_port_mappings(
        aliases: &mut HashSet<String>,
        address_counter: &mut u16,
        event_broadcasters: &mut HashMap<
            Address,
            Arc<tokio::sync::broadcast::Sender<AddressedEvent>>,
        >,
        device_config: &DeviceConfig,
        device_alias: &String,
        input_ports: &mut Vec<InputPortState>,
        input_dev: &Box<dyn InputHardwareDevice>,
    ) -> Result<()> {
        Self::ensure_no_output_port_mappings_configured(device_config, device_alias)?;

        for port_config in device_config.inputs.clone().unwrap_or_else(|| {
            warn!(
                "input device {} has no input port mappings configured",
                &device_alias
            );
            Vec::new()
        }) {
            let generic_port_alias = input_dev
                .port_alias(port_config.base.port)
                .context("unable to generate port alias")?;
            let port_alias = UniverseState::generate_port_alias(
                aliases,
                device_alias,
                &port_config.base,
                generic_port_alias,
            )?;

            let (value_type, event_stream) = input_dev
                .get_input_port(port_config.base.port)
                .context(format!(
                    "unable to map input port {}:{}",
                    device_alias, port_config.base.port
                ))?;
            let address = *address_counter;

            let mut port = PortState {
                port: port_config.base.port,
                alias: port_alias.clone(),
                tags: port_config.base.tags.unwrap_or_default(),
                address,
                last_value: Mutex::new(None),
                value_metric: match value_type {
                    InputValueType::Binary => {
                        prom::BINARY.with_label_values(&[port_alias.as_str()])
                    }
                    InputValueType::Temperature => {
                        prom::TEMPERATURE.with_label_values(&[port_alias.as_str()])
                    }
                    InputValueType::Humidity => {
                        prom::HUMIDITY.with_label_values(&[port_alias.as_str()])
                    }
                    InputValueType::Pressure => {
                        prom::PRESSURE.with_label_values(&[port_alias.as_str()])
                    }
                    InputValueType::Continuous => {
                        prom::CONTINUOUS.with_label_values(&[port_alias.as_str()])
                    }
                },
                ok_metric: prom::VALUE_OK.with_label_values(&[port_alias.as_str()]),
                update_ok_counter: prom::VALUE_UPDATES
                    .with_label_values(&[port_alias.as_str(), "ok"]),
                update_error_counter: prom::VALUE_UPDATES
                    .with_label_values(&[port_alias.as_str(), "error"]),
            };
            port.tags
                .extend(device_config.tags.clone().unwrap_or_default().into_iter());

            let port = Arc::new(port);
            let port_2 = port.clone();

            let (event_broadcast_sender, _) = tokio::sync::broadcast::channel(100);
            let event_broadcast_sender = Arc::new(event_broadcast_sender);
            let broadcast_sender_2 = event_broadcast_sender.clone();

            UniverseState::spawn_event_to_broadcast_worker(
                event_stream,
                port_2,
                broadcast_sender_2,
                address,
            );

            let port = InputPortState { port, value_type };

            *address_counter += 1;
            event_broadcasters.insert(address, event_broadcast_sender);
            input_ports.push(port);
        }

        Ok(())
    }

    fn ensure_no_output_port_mappings_configured(
        device_config: &DeviceConfig,
        alias: &str,
    ) -> Result<()> {
        ensure!(
            device_config
                .outputs
                .as_ref()
                .map_or_else(|| true, |cfgs| cfgs.is_empty()),
            "{} is an input device but output port mappings were supplied",
            alias
        );
        Ok(())
    }

    fn ensure_no_input_port_mappings_configured(
        device_config: &DeviceConfig,
        alias: &str,
    ) -> Result<()> {
        ensure!(
            device_config
                .inputs
                .as_ref()
                .map_or_else(|| true, |cfgs| cfgs.is_empty()),
            "{} is an output device but input port mappings were supplied",
            alias
        );
        Ok(())
    }

    fn create_device_type(
        synchronized_pca9685s: &mut HashMap<
            String,
            Vec<(SynchronizedDeviceRWCore, PCA9685Config, String)>,
        >,
        device_config: &DeviceConfig,
        alias: String,
    ) -> Result<DeviceType> {
        let dev = match &device_config.hardware_device_config {
            config::HardwareDeviceConfig::FanHeater { config: cfg } => {
                debug!("creating FanHeater combo {}...", alias);
                let dev = if cfg.i2c_bus.is_empty() {
                    warn!("using I2C mock");
                    let i2c = i2c_mock::I2cMock::new();
                    fan_heater::FanHeater::new(i2c, alias, cfg)?
                } else {
                    debug!("using I2C at {}", cfg.i2c_bus);
                    let i2c = linux_hal::I2cdev::new(cfg.i2c_bus.clone())?;
                    fan_heater::FanHeater::new(i2c, alias, cfg)?
                };
                DeviceType::FanHeater(Box::new(dev.clone()), Box::new(dev))
            }
            config::HardwareDeviceConfig::PCA9685 { config: cfg } => {
                debug!("creating PCA9685 {}...", alias);
                let dev = if cfg.i2c_bus.is_empty() {
                    warn!("using I2C mock");
                    let i2c = i2c_mock::I2cMock::new();
                    pca9685::PCA9685::new(i2c, cfg, alias)?
                } else {
                    debug!("using I2C at {}", cfg.i2c_bus);
                    let i2c = linux_hal::I2cdev::new(cfg.i2c_bus.clone())?;
                    pca9685::PCA9685::new(i2c, cfg, alias)?
                };
                DeviceType::PCA9685(Box::new(dev))
            }
            config::HardwareDeviceConfig::PCA9685Synchronized { config: cfg } => {
                debug!("creating synchronized PCA9685 {}...", alias);
                let i2c_bus = cfg.i2c_bus.to_lowercase();
                debug!("normalized I2C bus to {}", i2c_bus);

                let core = SynchronizedDeviceRWCore::new_from_core(DeviceRWCore::new_dirty(
                    alias.clone(),
                    16,
                    ValueScaling::Logarithmic,
                ));
                synchronized_pca9685s.entry(i2c_bus).or_default().push((
                    core.clone(),
                    cfg.clone(),
                    alias,
                ));

                DeviceType::PCA9685(Box::new(core))
            }
            config::HardwareDeviceConfig::DS18 { config: cfg } => {
                debug!("creating DS18 {}...", alias);

                let dev = DS18::new(alias, cfg)?;
                DeviceType::DS18(Box::new(dev))
            }
            config::HardwareDeviceConfig::MCP23017 { config: cfg } => {
                debug!("creating MCP23017 {}...", alias);

                let dev = if cfg.i2c_bus.is_empty() {
                    warn!("using I2C mock");
                    let i2c = i2c_mock::I2cMock::new();
                    mcp23017::MCP23017::new(i2c, cfg, alias)?
                } else {
                    debug!("using I2C at {}", cfg.i2c_bus);
                    let i2c = linux_hal::I2cdev::new(cfg.i2c_bus.clone())?;
                    mcp23017::MCP23017::new(i2c, cfg, alias)?
                };
                DeviceType::MCP23017(Box::new(dev))
            }
            config::HardwareDeviceConfig::MCP23017Input { config: cfg } => {
                debug!("creating MCP23017Input {}...", alias);

                let dev = if cfg.i2c_bus.is_empty() {
                    warn!("using I2C mock");
                    let i2c = i2c_mock::I2cMock::new();
                    mcp23017_input::MCP23017Input::new(i2c, cfg, alias)?
                } else {
                    debug!("using I2C at {}", cfg.i2c_bus);
                    let i2c = linux_hal::I2cdev::new(cfg.i2c_bus.clone())?;
                    mcp23017_input::MCP23017Input::new(i2c, cfg, alias)?
                };
                DeviceType::MCP23017Input(Box::new(dev))
            }
            config::HardwareDeviceConfig::BME280 { config: cfg } => {
                debug!("creating BME280 {}...", alias);

                let dev = if cfg.i2c_bus.is_empty() {
                    warn!("using I2C mock");
                    let i2c = i2c_mock::I2cMock::new();
                    crate::bme280::BME280::new(i2c, cfg, alias)?
                } else {
                    debug!("using I2C at {}", cfg.i2c_bus);
                    let i2c = linux_hal::I2cdev::new(cfg.i2c_bus.clone())?;
                    crate::bme280::BME280::new(i2c, cfg, alias)?
                };
                DeviceType::BME280(Box::new(dev))
            }
            config::HardwareDeviceConfig::DHT22 { config } => {
                debug!("creating DHT22 {}...", alias);

                let dev = DHT22::new(alias, config).context("unable to create DHT22")?;
                DeviceType::DHT22(Box::new(dev))
            }
            config::HardwareDeviceConfig::Gpio { config } => {
                debug!("creating GPIO {}...", alias);

                let dev = Gpio::new(alias, config).context("unable to create GPIO")?;
                DeviceType::Gpio(Box::new(dev))
            }
            HardwareDeviceConfig::DHT22Expander { config } => {
                debug!("creating DHT22Expander {}...", alias);

                let dev = if config.i2c_bus.is_empty() {
                    warn!("using I2C mock");
                    let i2c = i2c_mock::I2cMock::new();
                    crate::dht22_expander::DHT22Expander::new(i2c, alias, config)?
                } else {
                    debug!("using I2C at {}", config.i2c_bus);
                    let i2c = linux_hal::I2cdev::new(config.i2c_bus.clone())?;
                    crate::dht22_expander::DHT22Expander::new(i2c, alias, config)?
                };
                DeviceType::DHT22Expander(Box::new(dev))
            }
        };

        Ok(dev)
    }

    fn synchronize_pcas(
        synchronized_pca9685s: HashMap<
            String,
            Vec<(SynchronizedDeviceRWCore, PCA9685Config, String)>,
        >,
    ) -> Result<()> {
        for (i2c_bus, cores) in synchronized_pca9685s.into_iter() {
            let mut update_interval = 0;
            if i2c_bus.is_empty() {
                warn!("using I2C mock");
                let mut synced_pca = PCA9685Synchronized::new();
                for (core, cfg, alias) in cores.into_iter() {
                    ensure!(
                        cfg.update_interval_millis != 0,
                        "synchronized PCA9685 needs update_interval_millis != 0"
                    );
                    if update_interval == 0 {
                        update_interval = cfg.update_interval_millis;
                    }
                    ensure!(
                        update_interval == cfg.update_interval_millis,
                        "synchronized PCA9685 need to have the same update interval"
                    );
                    let i2c = i2c_mock::I2cMock::new();
                    synced_pca
                        .add(i2c, core, &cfg, alias)
                        .context(format!("unable to create PCA9685 with config {:?}", cfg))?;
                }
                synced_pca
                    .start(update_interval)
                    .context("unable to start synchronized PCA9685")?;
            } else {
                debug!("using I2C at {}", i2c_bus);
                let mut synced_pca = PCA9685Synchronized::new();
                for (core, cfg, alias) in cores.into_iter() {
                    ensure!(
                        cfg.update_interval_millis != 0,
                        "synchronized PCA9685 needs update_interval_millis != 0"
                    );
                    if update_interval == 0 {
                        update_interval = cfg.update_interval_millis;
                    }
                    ensure!(
                        update_interval == cfg.update_interval_millis,
                        "synchronized PCA9685 need to have the same update interval"
                    );
                    let i2c = linux_hal::I2cdev::new(i2c_bus.clone())?;
                    synced_pca
                        .add(i2c, core, &cfg, alias)
                        .context(format!("unable to create PCA9685 with config {:?}", cfg))?;
                }
                synced_pca
                    .start(update_interval)
                    .context("unable to start synchronized PCA9685")?;
            }
        }

        Ok(())
    }
}
