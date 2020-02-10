use crate::Result;
use embedded_hal as hal;
use failure::Fail;
use failure::ResultExt;
use spidev::{SpiModeFlags, Spidev, SpidevOptions, SpidevTransfer};

pub struct MCP23S17 {
    com: Spidev,
    address: u8,
}

impl MCP23S17 {
    pub fn new(spi_dev_path: &String, a0: bool, a1: bool, a2: bool) -> Result<MCP23S17> {
        let options = SpidevOptions::new()
            .max_speed_hz(1_000_000)
            .mode(SpiModeFlags::SPI_MODE_0)
            .lsb_first(false)
            .build();

        let mut address = 0b0100_0000;
        if a0 {
            address = address | 0b0000_0010;
        }
        if a1 {
            address = address | 0b0000_0100;
        }
        if a2 {
            address = address | 0b0000_1000;
        }

        let mut spi = Spidev::open(spi_dev_path)?;

        spi.configure(&options).context("unable to configure")?;

        // TODO set up registers
        // TODO set up pins
        // TODO enable auto increment
        // TODO set up HW address enable

        Ok(MCP23S17 { com: spi, address })
    }

    pub fn read_gpio(&mut self) -> Result<u16> {
        let mut tx_buf: [u8; 2] = [0; 2];
        tx_buf[0] = self.address | 0x1;
        tx_buf[1] = Register::GPIOA as u8;
        let mut rx_buf = [0_u8; 2];

        let mut transfer = SpidevTransfer::read_write(&tx_buf, &mut rx_buf);

        self.com.transfer(&mut transfer)?;

        let left = ((rx_buf[0] as u16) << 8) & 0xFF00;
        let right = rx_buf[1] as u16;

        Ok(left | right)
    }
}

#[derive(Debug, Copy, Clone)]
enum Register {
    IODIRA = 0x00,
    IPOLA = 0x02,
    GPINTENA = 0x04,
    DEFVALA = 0x06,
    INTCONA = 0x08,
    IOCONA = 0x0A,
    GPPUA = 0x0C,
    INTFA = 0x0E,
    INTCAPA = 0x10,
    GPIOA = 0x12,
    OLATA = 0x14,
    IODIRB = 0x01,
    IPOLB = 0x03,
    GPINTENB = 0x05,
    DEFVALB = 0x07,
    INTCONB = 0x09,
    IOCONB = 0x0B,
    GPPUB = 0x0D,
    INTFB = 0x0F,
    INTCAPB = 0x11,
    GPIOB = 0x13,
    OLATB = 0x15,
}
