use crate::config::{AggregatedConfig, DeviceConfig};
use crate::device_core::{DeviceRWCore, SynchronizedDeviceRWCore};
use crate::dht22::DHT22;
use crate::pca9685::PCA9685Config;
use crate::pca9685_sync::PCA9685Synchronized;
use crate::Result;
use crate::{config, i2c_mock, mcp23017, mcp23017_input, pca9685};
use alloy::config::{MappingConfig, VirtualDeviceConfig};
use alloy::event::Event;
use alloy::{Address, Value};
use failure::{format_err, ResultExt};
use futures::Stream;
use itertools::Itertools;
use linux_embedded_hal as linux_hal;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// Encapsulates all functionality a hardware device must provide.
pub(crate) trait HardwareDevice {
    /// Triggers an asynchronous update and retrieves any error from previous update(s).
    ///
    /// If the hardware device does not implement asynchronous updates that need to be triggered
    /// (i.e. the device updates constantly), then this will only check for errors.
    fn update(&self) -> Result<()>;

    /// Gets a VirtualDevice for the given port.
    ///
    /// Note that it is usually possible to derive multiple virtual devices from the same port,
    /// but the current implementation of events forbids this. See `get_event_stream` for more info.
    ///
    /// Both `get_virtual_device` and `get_event_stream` are generally implemented using device
    /// cores.
    fn get_virtual_device(&self, port: u8) -> Result<Box<dyn VirtualDevice + Send>>;

    /// Gets an EventStream for the given port.
    ///
    /// Note that it is usually _not_ possible to get multiple event streams for the same port.
    /// In the current mapping of virtual devices to the address space, for each mapped virtual
    /// device, an event stream is created at the same address.
    /// This means that, even though multiple virtual device mapped to the same port of one hardware
    /// device could exist, this cannot happen in practice because only one of them can have an
    /// event stream, and as such mapping in general will fail.
    ///
    /// Both `get_virtual_device` and `get_event_stream` are generally implemented using device
    /// cores.
    fn get_event_stream(&self, port: u8) -> Result<EventStream>;
}

/// An event stream is an asynchronous Stream of groups of events.
///
/// The asynchronous nature does not usually matter, as this is all handled by some magic in tokio.
/// For all intents and purposes, this behaves like an iterator of vectors of events.
pub(crate) type EventStream = Pin<Box<dyn Stream<Item = Vec<Event>> + Send>>;

/// A virtual device is what is exposed by the API to the outside world and mapped to a hardware
/// device.
pub(crate) trait VirtualDevice {
    /// Sets the _buffered_ value of this virtual device.
    ///
    /// The value will only be written to the hardware once the `HardwareDevice` updates.
    fn set(&self, value: Value) -> Result<()>;

    /// Gets the _device_ value of this virtual device.
    ///
    /// The device value is the value that was last written to the hardware.
    /// This might not be the value that was previously set with `set`, because that might still
    /// be buffered.
    fn get(&self) -> Result<Value>;
}

pub(crate) struct DeviceState {
    hardware_devices: Vec<Box<dyn HardwareDevice + Send>>,
    virtual_devices: HashMap<Address, Box<dyn VirtualDevice + Send>>,
    virtual_device_aliases: HashMap<String, Address>,
    virtual_device_groups: HashMap<String, Vec<Address>>,

    /// The checked, normalized configs used to create and map virtual devices.
    ///
    /// These represent the actual devices present in the state and system.
    /// They are sorted by address.
    pub(crate) virtual_device_configs: Vec<VirtualDeviceConfig>,
}

impl DeviceState {
    pub(crate) fn update(&self) -> Result<()> {
        // We want to make sure that we call update on all hardware devices, even if some of them
        // fail to update.
        let mut errs: Vec<Box<dyn failure::Fail>> = Vec::new();
        for device in &self.hardware_devices {
            match device.update().context("unable to update device") {
                Ok(()) => {}
                Err(f) => errs.push(Box::new(f)),
            };
        }

        if errs.is_empty() {
            return Ok(());
        }

        let s = errs
            .into_iter()
            .map(|err| format!("{:?}", err))
            .join("\n===========\n");

        Err(format_err!("updating one or more devices failed: {}", s))
    }

    pub(crate) fn set_group(&mut self, group: &str, value: Value) -> Result<()> {
        let addresses = self
            .virtual_device_groups
            .get(group.to_lowercase().as_str())
            .ok_or_else(|| format_err!("invalid group: {}", group))?;

        for address in addresses {
            let d = self
                .virtual_devices
                .get(address)
                .expect("invalid address for alias");

            d.set(value).context(format!(
                "unable to set value for device at address {}",
                address
            ))?;
        }

        Ok(())
    }

    pub(crate) fn set_alias(&mut self, alias: &str, value: Value) -> Result<()> {
        let address = self
            .virtual_device_aliases
            .get(alias.to_lowercase().as_str())
            .ok_or_else(|| format_err!("invalid alias: {}", alias))?;
        let d = self
            .virtual_devices
            .get(address)
            .expect("invalid address for alias");

        d.set(value).context(format!(
            "unable to set value for device at address {}",
            address
        ))?;

        Ok(())
    }

    pub(crate) fn set_address(&mut self, address: Address, value: Value) -> Result<()> {
        let d = self
            .virtual_devices
            .get(&address)
            .ok_or_else(|| format_err!("invalid address: {}", address))?;

        d.set(value).context(format!(
            "unable to set value for device at address {}",
            address
        ))?;

        Ok(())
    }

    pub(crate) fn get_address(&self, address: Address) -> Result<Value> {
        let d = self
            .virtual_devices
            .get(&address)
            .ok_or_else(|| format_err!("invalid address: {}", address))?;

        d.get()
    }
}

/// This is the result of mapping virtual devices to hardware devices.
/// The first element are event streams.
/// The second element are the actual mapped virtual devices
/// The third element are alias->Address mappings.
/// The last element are group->Vec<Address> mappings.
type VirtualDeviceMapping = (
    Vec<(Address, EventStream)>,
    HashMap<Address, Box<dyn VirtualDevice + Send>>,
    HashMap<String, Address>,
    HashMap<String, Vec<Address>>,
);

impl DeviceState {
    pub(crate) fn from_config(
        cfg: &AggregatedConfig,
    ) -> Result<(DeviceState, Vec<(u16, EventStream)>)> {
        info!("creating hardware devices...");
        let hardware_devices = Self::create_hardware_devices(cfg)?;

        debug!("created hardware devices {:?}", hardware_devices.keys());
        info!("created {} hardware devices", hardware_devices.len());

        info!("creating virtual devices...");
        // These will be checked and normalized by map_virtual_devices.
        let mut virtual_device_configs = cfg.virtual_devices.clone();
        let (event_streams, virtual_devices, virtual_device_aliases, virtual_device_groups) =
            DeviceState::map_virtual_devices(&mut virtual_device_configs, &hardware_devices)?;
        debug!(
            "created virtual devices at addresses {:?}",
            virtual_devices.keys()
        );
        debug!(
            "created virtual devices with aliases {:?}",
            virtual_device_aliases.keys()
        );
        debug!("created groups {:?}", virtual_device_groups.keys());
        info!(
            "created {} virtual devices and {} groups",
            virtual_devices.len(),
            virtual_device_groups.len()
        );
        virtual_device_configs.sort_unstable_by_key(|c| c.address);

        Ok((
            Self {
                hardware_devices: hardware_devices.into_iter().map(|k| k.1).collect(),
                virtual_devices,
                virtual_device_aliases,
                virtual_device_groups,
                virtual_device_configs,
            },
            event_streams,
        ))
    }

    fn create_hardware_devices(
        cfg: &AggregatedConfig,
    ) -> Result<HashMap<String, Box<dyn HardwareDevice + Send>>> {
        let mut hardware_devices: HashMap<String, Box<dyn HardwareDevice + Send>> = HashMap::new();
        let mut synchronized_pca9685s: HashMap<
            String,
            Vec<(SynchronizedDeviceRWCore, PCA9685Config, String)>,
        > = HashMap::new();
        for device_config in &cfg.devices {
            match device_config {
                config::DeviceConfig::PCA9685 { alias, config: cfg } => {
                    debug!("creating PCA9685 {}...", alias);
                    let alias = alias.to_lowercase();
                    debug!("normalized alias to {}", alias);
                    ensure!(
                        !hardware_devices.contains_key(&alias),
                        "duplicate alias: {}",
                        alias
                    );
                    let dev = if cfg.i2c_bus == "" {
                        warn!("using I2C mock");
                        let i2c = i2c_mock::I2cMock::new();
                        pca9685::PCA9685::new(i2c, &cfg, alias.clone())?
                    } else {
                        debug!("using I2C at {}", cfg.i2c_bus);
                        let i2c = linux_hal::I2cdev::new(cfg.i2c_bus.clone())?;
                        pca9685::PCA9685::new(i2c, &cfg, alias.clone())?
                    };
                    hardware_devices.insert(alias.clone(), Box::new(dev));
                }
                config::DeviceConfig::PCA9685Synchronized { alias, config: cfg } => {
                    debug!("creating synchronized PCA9685 {}...", alias);
                    let alias = alias.to_lowercase();
                    debug!("normalized alias to {}", alias);
                    ensure!(
                        !hardware_devices.contains_key(&alias),
                        "duplicate alias: {}",
                        alias
                    );
                    let i2c_bus = cfg.i2c_bus.to_lowercase();
                    debug!("normalized I2C bus to {}", i2c_bus);

                    let core = Arc::new(Mutex::new(DeviceRWCore::new_dirty(16)));
                    synchronized_pca9685s.entry(i2c_bus).or_default().push((
                        core.clone(),
                        cfg.clone(),
                        alias.clone(),
                    ));

                    hardware_devices.insert(alias.clone(), Box::new(core));
                }
                DeviceConfig::MCP23017 { alias, config: cfg } => {
                    debug!("creating MCP23017 {}...", alias);
                    let alias = alias.to_lowercase();
                    debug!("normalized alias to {}", alias);
                    ensure!(
                        !hardware_devices.contains_key(&alias),
                        "duplicate alias: {}",
                        alias
                    );
                    let dev = if cfg.i2c_bus == "" {
                        warn!("using I2C mock");
                        let i2c = i2c_mock::I2cMock::new();
                        mcp23017::MCP23017::new(i2c, &cfg, alias.clone())?
                    } else {
                        debug!("using I2C at {}", cfg.i2c_bus);
                        let i2c = linux_hal::I2cdev::new(cfg.i2c_bus.clone())?;
                        mcp23017::MCP23017::new(i2c, &cfg, alias.clone())?
                    };
                    hardware_devices.insert(alias.clone(), Box::new(dev));
                }
                DeviceConfig::MCP23017Input { alias, config: cfg } => {
                    debug!("creating MCP23017Input {}...", alias);
                    let alias = alias.to_lowercase();
                    debug!("normalized alias to {}", alias);
                    ensure!(
                        !hardware_devices.contains_key(&alias),
                        "duplicate alias: {}",
                        alias
                    );
                    let dev = if cfg.i2c_bus == "" {
                        warn!("using I2C mock");
                        let i2c = i2c_mock::I2cMock::new();
                        mcp23017_input::MCP23017Input::new(i2c, &cfg, alias.clone())?
                    } else {
                        debug!("using I2C at {}", cfg.i2c_bus);
                        let i2c = linux_hal::I2cdev::new(cfg.i2c_bus.clone())?;
                        mcp23017_input::MCP23017Input::new(i2c, &cfg, alias.clone())?
                    };
                    hardware_devices.insert(alias.clone(), Box::new(dev));
                }
                DeviceConfig::DHT22 { alias, config } => {
                    debug!("creating DHT22 {}...", alias);
                    let alias = alias.to_lowercase();
                    debug!("normalized alias to {}", alias);
                    ensure!(
                        !hardware_devices.contains_key(&alias),
                        "duplicate alias: {}",
                        alias
                    );
                    let dev =
                        DHT22::new(alias.clone(), &config).context("unable to create DHT22")?;
                    hardware_devices.insert(alias.clone(), Box::new(dev));
                }
            }
        }

        if !synchronized_pca9685s.is_empty() {
            info!("synchronizing PCA9685s ...");
            Self::synchronize_pcas(synchronized_pca9685s)?;
        }

        Ok(hardware_devices)
    }

    fn synchronize_pcas(
        synchronized_pca9685s: HashMap<
            String,
            Vec<(SynchronizedDeviceRWCore, PCA9685Config, String)>,
        >,
    ) -> Result<()> {
        for (i2c_bus, cores) in synchronized_pca9685s.into_iter() {
            let mut update_interval = 0;
            if i2c_bus == "" {
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

    fn map_virtual_devices(
        configs: &mut Vec<VirtualDeviceConfig>,
        hardware_devices: &HashMap<String, Box<dyn HardwareDevice + Send>>,
    ) -> Result<VirtualDeviceMapping> {
        let mut event_streams = Vec::new();
        let mut virtual_devices: HashMap<Address, Box<dyn VirtualDevice + Send>> = HashMap::new();
        let mut virtual_device_aliases: HashMap<String, Address> = HashMap::new();
        let mut virtual_device_groups: HashMap<String, Vec<Address>> = HashMap::new();

        for cfg in configs.iter_mut() {
            debug!(
                "creating virtual device {} at address {}, mapped at {}:{}",
                cfg.alias, cfg.address, cfg.mapping.device, cfg.mapping.port
            );

            cfg.alias = cfg.alias.to_lowercase();
            debug!("normalized alias to {}", cfg.alias);
            cfg.mapping = MappingConfig {
                device: cfg.mapping.device.to_lowercase(),
                port: cfg.mapping.port,
            };
            debug!("normalized mapping to {:?}", cfg.mapping);
            cfg.groups = cfg.groups.iter().map(|x| x.to_lowercase()).collect_vec();
            debug!("normalized groups to {:?}", cfg.groups);
            ensure!(
                !virtual_device_aliases.contains_key(&cfg.alias),
                "duplicate alias {} for virtual device at address {}",
                cfg.alias,
                cfg.address
            );
            ensure!(
                !virtual_devices.contains_key(&cfg.address),
                "duplicate address {} for virtual device {}",
                cfg.address,
                cfg.alias
            );
            let has_duplicates =
                (1..cfg.groups.len()).any(|i| cfg.groups[i..].contains(&cfg.groups[i - 1]));
            ensure!(
                !has_duplicates,
                "duplicate group for virtual device {}",
                cfg.alias
            );

            let hw_dev = hardware_devices.get(&cfg.mapping.device).ok_or_else(|| {
                format_err!(
                    "unknown hardware device {} for virtual device {}",
                    cfg.mapping.device,
                    cfg.alias
                )
            })?;
            let vdev = hw_dev
                .get_virtual_device(cfg.mapping.port)
                .context(format!(
                    "unable to create virtual device at mapping {}:{}",
                    cfg.mapping.device, cfg.mapping.port
                ))?;
            let stream = hw_dev.get_event_stream(cfg.mapping.port).context(format!(
                "unable to create event stream at mapping {}:{}",
                cfg.mapping.device, cfg.mapping.port
            ))?;

            virtual_devices.insert(cfg.address, vdev);
            virtual_device_aliases.insert(cfg.alias.clone(), cfg.address);
            for g in cfg.groups.clone() {
                let group = virtual_device_groups.entry(g).or_default();
                group.push(cfg.address);
            }
            event_streams.push((cfg.address, stream));
        }
        Ok((
            event_streams,
            virtual_devices,
            virtual_device_aliases,
            virtual_device_groups,
        ))
    }
}
