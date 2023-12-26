use embedded_hal as hal;
use itertools::Itertools;
use log::debug;
use std::fmt::{Display, Formatter};

/// Error code signaling that the device did not answer to its address.
const ERROR_CODE_DEVICE_NOT_FOUND: u16 = 0xA800;
/// Error code signaling that the CRC check for the data read from the device failed.
const ERROR_CODE_CRC_CHECK_FAILED: u16 = 0x5000;
/// The temperature value read from an uninitialized device.
const ERROR_CODE_INITIAL_TEMPERATURE: u16 = 0x0550;

/// Errors that may occur when reading the expander.
#[derive(Debug, Clone)]
pub(crate) enum ExpanderError {
    /// Occurs for a failed I2C bus operation.
    Bus(String),

    /// Occurs if the CRC check for the entire data of the expander failed.
    CrcMismatch,

    /// Occurs if the expander misbehaves.
    BadExpander(String),
}

impl Display for ExpanderError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ExpanderError::Bus(err) => {
                write!(f, "I2C operation failed: {}", err)
            }
            ExpanderError::CrcMismatch => {
                write!(f, "CRC mismatch")
            }
            ExpanderError::BadExpander(err) => {
                write!(f, "Expander misbehaved: {}", err)
            }
        }
    }
}

impl std::error::Error for ExpanderError {}

impl ExpanderError {
    fn from_i2c_error<E>(err: E) -> Self
    where
        E: Sync + Send + std::fmt::Debug + 'static,
    {
        Self::Bus(format!("{:?}", err))
    }
}

/// Errors that may occur when interpreting the data for one sensor attached to the expander.
#[derive(Debug, Clone, Copy)]
pub(crate) enum SensorError {
    /// Occurs if the device does not answer to its address.
    DeviceNotFound,

    /// Occurs if the checksum value from the sensor is incorrect.
    Checksum,

    /// Occurs if a suspicious value is returned from the sensor.
    /// This is currently detected temperature==85°C.
    SuspiciousValue,
}

pub(crate) struct Ds18b20Expander<I2C> {
    bus: I2C,
    buffer: [u8; 254],
    address: u8,
    num_sensors: u8,
}

impl<I2C, E> Ds18b20Expander<I2C>
where
    I2C: hal::blocking::i2c::Write<Error = E>
        + Send
        + hal::blocking::i2c::WriteRead<Error = E>
        + 'static,
    E: Sync + Send + std::fmt::Debug + 'static,
{
    pub(crate) fn new(bus: I2C, address: u8) -> Self {
        Ds18b20Expander {
            bus,
            buffer: [0; 254],
            address,
            num_sensors: 0,
        }
    }

    pub(crate) fn scan_for_devices(
        &mut self,
        delay: &mut dyn hal::blocking::delay::DelayMs<u16>,
    ) -> Result<Vec<u64>, ExpanderError> {
        let sensor_ids = (0..=3)
            .map(|b| {
                let res = self.scan_bus_for_devices(b, delay);
                debug!(
                    "got scan result {:?}",
                    res.as_ref().map(|vals| vals
                        .iter()
                        .map(|v| format!("{:016x}", *v))
                        .collect::<Vec<_>>())
                );

                res
            })
            .fold_ok(Vec::new(), |mut acc, v| {
                acc.extend_from_slice(&v);
                acc
            });

        if let Ok(ids) = sensor_ids.as_ref() {
            self.num_sensors = ids.len() as u8;
        }

        sensor_ids
    }

    fn scan_bus_for_devices(
        &mut self,
        bus: u8,
        delay: &mut dyn hal::blocking::delay::DelayMs<u16>,
    ) -> Result<Vec<u64>, ExpanderError> {
        debug!("scanning bus {} for devices...", bus);
        self.write_status_register(0x01 << (bus as usize))?;

        delay.delay_ms(2000);

        self.read_registers(Some(30 * 8))
            .map(data_bytes)
            .and_then(parse_device_id_results)
    }

    pub(crate) fn read_temperatures(
        &mut self,
        delay: &mut dyn hal::blocking::delay::DelayMs<u16>,
    ) -> Result<Vec<Result<f64, SensorError>>, ExpanderError> {
        self.init_readout()?;

        delay.delay_ms(2000);

        self.fetch_temperatures()
    }

    fn init_readout(&mut self) -> Result<(), ExpanderError> {
        debug!("initializing readout");
        self.write_status_register(0b0001_0000)
    }

    fn fetch_temperatures(&mut self) -> Result<Vec<Result<f64, SensorError>>, ExpanderError> {
        debug!("fetching temperatures");
        self.read_registers(Some(self.num_sensors as usize * 2))
            .map(data_bytes)
            .and_then(parse_temperature_results)
    }

    fn read_registers(&mut self, num_data_bytes: Option<usize>) -> Result<&[u8], ExpanderError> {
        self.buffer.iter_mut().for_each(|b| *b = 0);

        self.bus
            .write_read(
                self.address,
                &[0x00],
                if let Some(num_data_bytes) = num_data_bytes {
                    &mut self.buffer[..num_data_bytes + 5]
                } else {
                    &mut self.buffer
                },
            )
            .map_err(ExpanderError::from_i2c_error)?;
        debug!("read raw bytes {:?}", self.buffer);

        let result_bytes = truncate_to_length(&self.buffer);
        debug!("extracted result bytes: {:?}", result_bytes);

        validate_crc(result_bytes)?;

        Ok(result_bytes)
    }

    fn write_status_register(&mut self, val: u8) -> Result<(), ExpanderError> {
        self.bus
            .write(self.address, &[0x00, val])
            .map_err(ExpanderError::from_i2c_error)
    }
}

fn truncate_to_length(res: &[u8]) -> &[u8] {
    let expected_length = res[4];
    debug!("extracted expected length {}", expected_length);

    if expected_length as usize + 4 >= res.len() {
        return res;
    }

    &res[..(expected_length as usize) + 4 + 1]
}

fn validate_length(res: &[u8]) -> Result<&[u8], ExpanderError> {
    let expected_length = res[4];
    debug!("extracted expected length {}", expected_length);
    if res[(expected_length as usize + 4 + 1)..]
        .iter()
        .any(|b| *b != 0)
    {
        return Err(ExpanderError::BadExpander("length mismatch".to_string()));
    }

    Ok(&res[..(expected_length as usize) + 4 + 1])
}

fn validate_crc(res: &[u8]) -> Result<(), ExpanderError> {
    let data_and_length = &res[4..];

    debug!("calculating CRC over {:?}", data_and_length);
    let expected_crc = res[3];
    let calculated_crc = calculate_crc(data_and_length);
    debug!(
        "extracted CRC {:08b}, calculated {:08b}",
        expected_crc, calculated_crc
    );

    if expected_crc != calculated_crc {
        Err(ExpanderError::CrcMismatch)
    } else {
        Ok(())
    }
}

fn data_bytes(res: &[u8]) -> &[u8] {
    &res[5..]
}

fn parse_temperature_results(res: &[u8]) -> Result<Vec<Result<f64, SensorError>>, ExpanderError> {
    debug!(
        "attempting to parse temperature values from bytes {:?}",
        res
    );
    if res.len() % 2 != 0 {
        return Err(ExpanderError::BadExpander("invalid length".to_string()));
    }

    let values = res
        .into_iter()
        .tuples::<(_, _)>()
        .map(|(a, b)| {
            let val = (*b as u16) << 8 | *a as u16;
            debug!(
                "combined bytes {:02x} and {:02x} to {:04x} ({})",
                a, b, val, val
            );

            val
        })
        .map(|v| {
            let res = match v {
                ERROR_CODE_CRC_CHECK_FAILED => Err(SensorError::Checksum),
                ERROR_CODE_DEVICE_NOT_FOUND => Err(SensorError::DeviceNotFound),
                ERROR_CODE_INITIAL_TEMPERATURE => Err(SensorError::SuspiciousValue),
                _ => {
                    if v & 0xF000 != 0xF000 && v & 0xF000 != 0 {
                        Err(SensorError::SuspiciousValue)
                    } else {
                        if v & 0x8000 != 0 {
                            // negative
                            Ok((v & 0x0FFF) as i16 * -1)
                        } else {
                            Ok((v & 0x0FFF) as i16)
                        }
                    }
                }
            };
            debug!("examining {} ... {:?}", v, res);

            res
        })
        .map(|v| {
            v.map(|v| {
                let f = f64::from(v);
                let res = f / 16_f64;
                debug!(
                    "parsed {:04x} ({}) as {}, temperature value is {}",
                    v, v, f, res
                );

                res
            })
        })
        .collect();

    Ok(values)
}

/*
[2023-12-26 21:46:12.713699 +00:00] DEBUG [submarine::ds18b20_expander_lib] src/ds18b20_expander_lib.rs:172: read raw bytes [0, 0, 127, 175, 20, 60, 1, 41, 1, 170, 0, 54, 1, 45, 1, 24, 0, 52, 0, 127, 0, 15, 1, 115, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
[2023-12-26 21:46:12.713841 +00:00] DEBUG [submarine::ds18b20_expander_lib] src/ds18b20_expander_lib.rs:191: extracted expected length 20
[2023-12-26 21:46:12.713888 +00:00] DEBUG [submarine::ds18b20_expander_lib] src/ds18b20_expander_lib.rs:175: extracted result bytes: [0, 0, 127, 175, 20, 60, 1, 41, 1, 170, 0, 54, 1, 45, 1, 24, 0, 52, 0, 127, 0, 15, 1, 115, 1]
[2023-12-26 21:46:12.713942 +00:00] DEBUG [submarine::ds18b20_expander_lib] src/ds18b20_expander_lib.rs:216: calculating CRC over [20, 60, 1, 41, 1, 170, 0, 54, 1, 45, 1, 24, 0, 52, 0, 127, 0, 15, 1, 115, 1]
[2023-12-26 21:46:12.714005 +00:00] DEBUG [submarine::ds18b20_expander_lib] src/ds18b20_expander_lib.rs:219: extracted CRC 10101111, calculated 10101111
[2023-12-26 21:46:12.714050 +00:00] DEBUG [submarine::ds18b20_expander_lib] src/ds18b20_expander_lib.rs:236: attempting to parse temperature values from bytes [60, 1, 41, 1, 170, 0, 54, 1, 45, 1, 24, 0, 52, 0, 127, 0, 15, 1, 115, 1]
[2023-12-26 21:46:12.714101 +00:00] DEBUG [submarine::ds18b20_expander_lib] src/ds18b20_expander_lib.rs:249: combined bytes 3c and 01 to 013c (316)
[2023-12-26 21:46:12.714150 +00:00] DEBUG [submarine::ds18b20_expander_lib] src/ds18b20_expander_lib.rs:249: combined bytes 29 and 01 to 0129 (297)
[2023-12-26 21:46:12.714197 +00:00] DEBUG [submarine::ds18b20_expander_lib] src/ds18b20_expander_lib.rs:249: combined bytes aa and 00 to 00aa (170)
[2023-12-26 21:46:12.714242 +00:00] DEBUG [submarine::ds18b20_expander_lib] src/ds18b20_expander_lib.rs:249: combined bytes 36 and 01 to 0136 (310)
[2023-12-26 21:46:12.714287 +00:00] DEBUG [submarine::ds18b20_expander_lib] src/ds18b20_expander_lib.rs:249: combined bytes 2d and 01 to 012d (301)
[2023-12-26 21:46:12.714334 +00:00] DEBUG [submarine::ds18b20_expander_lib] src/ds18b20_expander_lib.rs:249: combined bytes 18 and 00 to 0018 (24)
[2023-12-26 21:46:12.714379 +00:00] DEBUG [submarine::ds18b20_expander_lib] src/ds18b20_expander_lib.rs:249: combined bytes 34 and 00 to 0034 (52)
[2023-12-26 21:46:12.714424 +00:00] DEBUG [submarine::ds18b20_expander_lib] src/ds18b20_expander_lib.rs:249: combined bytes 7f and 00 to 007f (127)
[2023-12-26 21:46:12.714468 +00:00] DEBUG [submarine::ds18b20_expander_lib] src/ds18b20_expander_lib.rs:249: combined bytes 0f and 01 to 010f (271)
[2023-12-26 21:46:12.714514 +00:00] DEBUG [submarine::ds18b20_expander_lib] src/ds18b20_expander_lib.rs:249: combined bytes 73 and 01 to 0173 (371)
[2023-12-26 21:46:12.714563 +00:00] DEBUG [submarine::ds18b20_expander] src/ds18b20_expander.rs:265: ds18b20-expander-1: got reading Err(SuspiciousValue) for sensor with ID 28aae1531a1302ef
[2023-12-26 21:46:12.714611 +00:00] WARN [submarine::ds18b20_expander] src/ds18b20_expander.rs:272: ds18b20-expander-1: failed to read sensor with ID 28aae1531a1302ef: SuspiciousValue
[2023-12-26 21:46:12.714683 +00:00] DEBUG [submarine::ds18b20_expander] src/ds18b20_expander.rs:265: ds18b20-expander-1: got reading Err(SuspiciousValue) for sensor with ID 28997275d0013c3e
[2023-12-26 21:46:12.714749 +00:00] WARN [submarine::ds18b20_expander] src/ds18b20_expander.rs:272: ds18b20-expander-1: failed to read sensor with ID 28997275d0013c3e: SuspiciousValue
[2023-12-26 21:46:12.714802 +00:00] DEBUG [submarine::ds18b20_expander] src/ds18b20_expander.rs:265: ds18b20-expander-1: got reading Err(SuspiciousValue) for sensor with ID 28cd7a75d0013cf2
[2023-12-26 21:46:12.714848 +00:00] WARN [submarine::ds18b20_expander] src/ds18b20_expander.rs:272: ds18b20-expander-1: failed to read sensor with ID 28cd7a75d0013cf2: SuspiciousValue
[2023-12-26 21:46:12.714897 +00:00] DEBUG [submarine::ds18b20_expander] src/ds18b20_expander.rs:265: ds18b20-expander-1: got reading Err(SuspiciousValue) for sensor with ID 285c4175d0013c80
[2023-12-26 21:46:12.714943 +00:00] WARN [submarine::ds18b20_expander] src/ds18b20_expander.rs:272: ds18b20-expander-1: failed to read sensor with ID 285c4175d0013c80: SuspiciousValue
[2023-12-26 21:46:12.714992 +00:00] DEBUG [submarine::ds18b20_expander] src/ds18b20_expander.rs:265: ds18b20-expander-1: got reading Err(SuspiciousValue) for sensor with ID 28aab575d0013cf9
[2023-12-26 21:46:12.715038 +00:00] WARN [submarine::ds18b20_expander] src/ds18b20_expander.rs:272: ds18b20-expander-1: failed to read sensor with ID 28aab575d0013cf9: SuspiciousValue
[2023-12-26 21:46:12.715086 +00:00] DEBUG [submarine::ds18b20_expander] src/ds18b20_expander.rs:265: ds18b20-expander-1: got reading Err(SuspiciousValue) for sensor with ID 28aa0dd4191302af
[2023-12-26 21:46:12.715132 +00:00] WARN [submarine::ds18b20_expander] src/ds18b20_expander.rs:272: ds18b20-expander-1: failed to read sensor with ID 28aa0dd4191302af: SuspiciousValue
[2023-12-26 21:46:12.715180 +00:00] DEBUG [submarine::ds18b20_expander] src/ds18b20_expander.rs:265: ds18b20-expander-1: got reading Err(SuspiciousValue) for sensor with ID 282f5e75d0013c52
[2023-12-26 21:46:12.715225 +00:00] WARN [submarine::ds18b20_expander] src/ds18b20_expander.rs:272: ds18b20-expander-1: failed to read sensor with ID 282f5e75d0013c52: SuspiciousValue
[2023-12-26 21:46:12.715273 +00:00] DEBUG [submarine::ds18b20_expander] src/ds18b20_expander.rs:265: ds18b20-expander-1: got reading Err(SuspiciousValue) for sensor with ID 284a2775d0013ce4
[2023-12-26 21:46:12.715321 +00:00] WARN [submarine::ds18b20_expander] src/ds18b20_expander.rs:272: ds18b20-expander-1: failed to read sensor with ID 284a2775d0013ce4: SuspiciousValue
[2023-12-26 21:46:12.715370 +00:00] DEBUG [submarine::ds18b20_expander] src/ds18b20_expander.rs:265: ds18b20-expander-1: got reading Err(SuspiciousValue) for sensor with ID 28aa553a1a1302bc
[2023-12-26 21:46:12.715416 +00:00] WARN [submarine::ds18b20_expander] src/ds18b20_expander.rs:272: ds18b20-expander-1: failed to read sensor with ID 28aa553a1a1302bc: SuspiciousValue
[2023-12-26 21:46:12.715463 +00:00] DEBUG [submarine::ds18b20_expander] src/ds18b20_expan
 */

fn parse_device_id_results(res: &[u8]) -> Result<Vec<u64>, ExpanderError> {
    debug!("attempting to parse sensor IDs from bytes {:?}", res);
    if res.len() % 8 != 0 {
        return Err(ExpanderError::BadExpander("invalid length".to_string()));
    }

    let ids = res
        .into_iter()
        .tuples::<(_, _, _, _, _, _, _, _)>()
        .map(|(b7, b6, b5, b4, b3, b2, b1, b0)| {
            (*b7 as u64) << 56
                | (*b6 as u64) << 48
                | (*b5 as u64) << 40
                | (*b4 as u64) << 32
                | (*b3 as u64) << 24
                | (*b2 as u64) << 16
                | (*b1 as u64) << 8
                | (*b0 as u64)
        })
        .collect();

    Ok(ids)
}

fn calculate_crc(data: &[u8]) -> u8 {
    let maxim_calculator = crc::Crc::<u8>::new(&crc::CRC_8_MAXIM_DOW);

    maxim_calculator.checksum(data)
}
