use crate::Result;
use prometheus::exponential_buckets;
use prometheus::{HistogramVec, IntCounterVec};
use std::net::SocketAddr;

// Sensor-related metrics.
lazy_static! {
    pub static ref DHT22_MEASUREMENTS: IntCounterVec = register_int_counter_vec!(
        "dht22_measurements",
        "counts measurements for DHT22 sensors by alias and result",
        &["alias", "result"]
    )
    .unwrap();
    pub static ref DHT22_EXPANDER_READOUTS: IntCounterVec = register_int_counter_vec!(
        "dht22_expander_readouts",
        "counts readouts for DHT22-to-I2C expanders by alias and result",
        &["alias", "result"]
    )
    .unwrap();
    pub static ref DS18_MEASUREMENTS: IntCounterVec = register_int_counter_vec!(
        "ds18_measurements",
        "counts measurements for DS18 sensors by alias and result",
        &["alias", "result"]
    )
    .unwrap();
    pub static ref DS18_EXPANDER_MEASUREMENTS: IntCounterVec = register_int_counter_vec!(
        "ds18b20_expander_measurements",
        "counts measurements for DS18B20-to-I2C expanders by alias and result",
        &["alias", "result"]
    )
    .unwrap();
    pub static ref BME280_READ_DURATION: HistogramVec = register_histogram_vec!(
        "bme280_read_duration",
        "duration of reading from an BME280 in microseconds by alias",
        &["alias"],
        exponential_buckets(150_f64, (1.5_f64).sqrt(), 10).unwrap()
    )
    .unwrap();
    pub static ref BME280_MEASUREMENTS: IntCounterVec = register_int_counter_vec!(
        "bme280_measurements",
        "counts measurements for DS18 sensors by alias and result",
        &["alias", "result"]
    )
    .unwrap();
    pub static ref PCA9685_WRITE_DURATION: HistogramVec = register_histogram_vec!(
        "pca9685_write_duration",
        "duration of writing to a PCA9685 in microseconds by alias",
        &["alias"],
        exponential_buckets(200_f64, (1.5_f64).sqrt(), 10).unwrap()
    )
    .unwrap();
    pub static ref TASMOTA_RELAY_EXPANDER_WRITE_DURATION: HistogramVec = register_histogram_vec!(
        "tasmota_relay_expander_write_duration",
        "duration of writing to a Tasmota relay expander in microseconds by alias",
        &["alias"],
        exponential_buckets(150_f64, (1.5_f64).sqrt(), 10).unwrap()
    )
    .unwrap();
    pub static ref MCP23017_WRITE_DURATION: HistogramVec = register_histogram_vec!(
        "mcp23017_write_duration",
        "duration of writing to an MCP23017 in microseconds by alias",
        &["alias"],
        exponential_buckets(150_f64, (1.5_f64).sqrt(), 10).unwrap()
    )
    .unwrap();
    pub static ref MCP23017_READ_DURATION: HistogramVec = register_histogram_vec!(
        "mcp23017_read_duration",
        "duration of reading from an MCP23017 in microseconds by alias",
        &["alias"],
        exponential_buckets(150_f64, (1.5_f64).sqrt(), 10).unwrap()
    )
    .unwrap();
    pub static ref MCP23017_READS: IntCounterVec = register_int_counter_vec!(
        "mcp23017_reads",
        "reads performed for MCP23017 input expanders by alias and result",
        &["alias", "result"]
    )
    .unwrap();
    pub static ref BUTTON_EXPANDER_READ_DURATION: HistogramVec = register_histogram_vec!(
        "button_expander_read_duration",
        "duration of reading from a button expander in microseconds by alias",
        &["alias"],
        exponential_buckets(150_f64, (1.5_f64).sqrt(), 10).unwrap()
    )
    .unwrap();
    pub static ref BUTTON_EXPANDER_READS: IntCounterVec = register_int_counter_vec!(
        "button_expander_reads",
        "reads performed for button expanders by alias and result",
        &["alias", "result"]
    )
    .unwrap();
}

pub(crate) fn start_prometheus(addr: SocketAddr) -> Result<()> {
    prometheus_exporter::start(addr)?;
    Ok(())
}
