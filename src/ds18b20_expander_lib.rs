use embedded_hal as hal;
use itertools::Itertools;
use log::{debug, warn};
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

    /// Occurs if the maximum number of retries was exceeded.
    /// Contains the error encountered during the last retry.
    MaxRetriesExceeded(Box<ExpanderError>),
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
            ExpanderError::MaxRetriesExceeded(err) => {
                write!(f, "Max retries exceeded. Last error: {}", err)
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
        retry_count: usize,
        delay: &mut dyn hal::blocking::delay::DelayMs<u16>,
    ) -> Result<Vec<u64>, ExpanderError> {
        let sensor_ids = (0..=3)
            .map(|b| {
                let res = self.scan_bus_for_devices(b, retry_count, delay);
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
        retry_count: usize,
        delay: &mut dyn hal::blocking::delay::DelayMs<u16>,
    ) -> Result<Vec<u64>, ExpanderError> {
        debug!("scanning bus {} for devices...", bus);
        self.write_status_register(0x01 << (bus as usize))?;

        delay.delay_ms(2000);

        self.read_registers_with_retry(Some(30 * 8), retry_count, delay)
            .map(|num| self.data_bytes(num))
            .and_then(parse_device_id_results)
    }

    pub(crate) fn read_temperatures(
        &mut self,
        delay: &mut dyn hal::blocking::delay::DelayMs<u16>,
        retry_count: usize,
    ) -> Result<Vec<Result<f64, SensorError>>, ExpanderError> {
        assert!(retry_count > 0, "retry count must be > 0");

        debug!("initializing readout...");
        self.init_readout()?;

        delay.delay_ms(2000);

        self.fetch_temperatures(retry_count, delay)
    }

    fn init_readout(&mut self) -> Result<(), ExpanderError> {
        self.write_status_register(0b0001_0000)
    }

    fn fetch_temperatures(
        &mut self,
        retries: usize,
        delay: &mut dyn hal::blocking::delay::DelayMs<u16>,
    ) -> Result<Vec<Result<f64, SensorError>>, ExpanderError> {
        debug!("fetching temperatures");
        self.read_registers_with_retry(Some(self.num_sensors as usize * 2), retries, delay)
            .map(|num| self.data_bytes(num))
            .and_then(parse_temperature_results)
    }

    fn read_registers_with_retry(
        &mut self,
        num_data_bytes: Option<usize>,
        retries: usize,
        delay: &mut dyn hal::blocking::delay::DelayMs<u16>,
    ) -> Result<usize, ExpanderError> {
        for retry in 0..retries {
            debug!(
                "attempting readout of registers, try {} (of {})...",
                retry + 1,
                retries
            );

            let res = self.read_registers(num_data_bytes);
            match res {
                Ok(res) => return Ok(res),
                Err(err) => {
                    warn!(
                        "unable to read registers, try {} (of {}): {:?}",
                        retry + 1,
                        retries,
                        err
                    );

                    if retry == retries - 1 {
                        return Err(ExpanderError::MaxRetriesExceeded(Box::new(err)));
                    }

                    delay.delay_ms(100);
                }
            }
        }

        unreachable!()
    }

    fn read_registers(&mut self, num_data_bytes: Option<usize>) -> Result<usize, ExpanderError> {
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

        Ok(result_bytes.len())
    }

    fn write_status_register(&mut self, val: u8) -> Result<(), ExpanderError> {
        self.bus
            .write(self.address, &[0x00, val])
            .map_err(ExpanderError::from_i2c_error)
    }

    fn data_bytes(&self, res: usize) -> &[u8] {
        &self.buffer[5..res]
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
            .and_then(|v| {
                debug!("checking value {} for suspicious-ness...", v);
                if v < -50_f64 {
                    Err(SensorError::SuspiciousValue)
                } else {
                    Ok(v)
                }
            })
        })
        .collect();

    Ok(values)
}

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
