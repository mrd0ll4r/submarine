//! # i2c_mock
//!
//! A mock I2C library to be able to use embedded_hal I2C on systems that do not have I2C support.
//!
//! This is mostly a copy of the I2C mock from the [ht16k33](https://crates.io/crates/ht16k33)
//! crate, licensed MIT, with slight modifications to provide 256 registers and without anything
//! specific to that device.
use embedded_hal as hal;
use std::fmt;

/// Mock error to satisfy the I2C trait.
#[derive(Debug)]
pub struct I2cMockError;

#[cfg(feature = "std")]
impl std::error::Error for I2cMockError {}

impl fmt::Display for I2cMockError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "I2c MockError")
    }
}

/// The mock I2C state.
///
/// # Example
///
/// ```
/// // Create an I2cMock.
/// let i2c_mock = I2cMock::new();
/// ```
pub struct I2cMock {
    /// Registers
    pub data_values: [u8; 256],
}

impl I2cMock {
    /// Create an I2cMock.
    pub fn new() -> Self {
        I2cMock {
            data_values: [0; 256],
        }
    }
}

impl hal::blocking::i2c::WriteRead for I2cMock {
    type Error = I2cMockError;

    /// `write_read` implementation.
    ///
    /// # Arguments
    ///
    /// * `_address` - The slave address. Ignored.
    /// * `bytes` - The command/address instructions to be written.
    /// * `buffer` - The read results.
    ///
    /// # Examples
    ///
    /// ```
    /// # use embedded_hal::blocking::i2c::WriteRead;
    /// let mut i2c_mock = I2cMock::new();
    ///
    /// let mut read_buffer = [0u8; 16];
    /// i2c_mock.write_read(0, &[ht16k33::DisplayDataAddress::ROW_0.bits()], &mut read_buffer);
    /// ```
    fn write_read(
        &mut self,
        _address: u8,
        bytes: &[u8],
        buffer: &mut [u8],
    ) -> Result<(), Self::Error> {
        let mut data_offset = bytes[0] as usize;

        for value in buffer.iter_mut() {
            *value = self.data_values[data_offset];

            // We assume the chip supports auto-increment and wrap-around, emulate that.
            data_offset = (data_offset + 1) % self.data_values.len();
        }

        Ok(())
    }
}

impl hal::blocking::i2c::Write for I2cMock {
    type Error = I2cMockError;

    /// `write` implementation.
    ///
    /// # Arguments
    ///
    /// * `_address` - The slave address. Ignored.
    /// * `bytes` - The command/address instructions to be written.
    ///
    /// # Examples
    ///
    /// ```
    /// # use embedded_hal::blocking::i2c::Write;
    /// let mut i2c_mock = I2cMock::new();
    ///
    /// // First value is the data address, remaining values are to be written
    /// // starting at the data address which auto-increments and then wraps.
    /// let write_buffer = [ht16k33::DisplayDataAddress::ROW_0.bits(), 0u8, 0u8];
    /// # }
    /// ```
    fn write(&mut self, _address: u8, bytes: &[u8]) -> Result<(), Self::Error> {
        // "Command-only" writes are length 1 and write-only, and cannot be read back,
        // discard them for simplicity.
        if bytes.len() == 1 {
            return Ok(());
        }

        // Other writes have data, store them.
        let mut data_offset = bytes[0] as usize;
        let data = &bytes[1..];

        for value in data.iter() {
            self.data_values[data_offset] = *value;

            // We suppose the chip supports auto-increment and wrap-around, emulate that.
            data_offset = (data_offset + 1) % self.data_values.len();
        }

        Ok(())
    }
}
