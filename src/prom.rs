use crate::Result;
use log::warn;
use prometheus::exponential_buckets;
use prometheus::{Gauge, GaugeVec, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec};
use std::net::SocketAddr;
use std::thread;
use std::time::Duration;
use systemstat::{saturating_sub_bytes, Platform, System};

// Sensor-related metrics.
lazy_static! {
    pub static ref FAN_HEATER_WRITES: IntCounterVec = register_int_counter_vec!(
        "fan_heater_writes",
        "counts writes for the fan/heater board combo by alias and result",
        &["alias", "result"]
    )
    .unwrap();
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

// Event-related metrics.
lazy_static! {
    pub static ref EVENTS_GENERATED: IntCounter = register_int_counter!(
    "events_generated",
    "number of events generated"
    ).unwrap();

    pub static ref EVENTS_MATCHED: IntCounter = register_int_counter!(
    "events_matched",
    "number of events matched, this _does_ count matches against multiple receivers as multiple matches, i.e. this counts how many events were sent to subscribers"
    ).unwrap();

    pub static ref EVENTS_DROPPED: IntCounter = register_int_counter!(
    "events_dropped",
    "number of events dropped instead of being sent out, due to full buffers/slow receivers"
    ).unwrap();
}

// System-related metrics
lazy_static! {
    pub static ref SYSTEM_MEMORY_USED: IntGauge = register_int_gauge!(
        "system_memory_used",
        "amount of memory used (=total-free) in bytes"
    )
    .unwrap();
    pub static ref SYSTEM_LOAD_AVERAGE: GaugeVec =
        register_gauge_vec!("system_load_average", "Linux load average", &["duration"]).unwrap();
    pub static ref SYSTEM_CPU_TEMPERATURE: Gauge =
        register_gauge!("system_cpu_temperature", "CPU temperature in celsius").unwrap();
    pub static ref SYSTEM_NETWORK_STATS: IntGaugeVec = register_int_gauge_vec!(
        "system_network_stats",
        "network statistics by interface and value",
        &["interface", "value"]
    )
    .unwrap();
}

pub(crate) fn start_prometheus(addr: SocketAddr) -> Result<()> {
    thread::Builder::new()
        .name("prom-system-stats".to_string())
        .spawn(track_system_stats)?;
    prometheus_exporter::start(addr)?;
    Ok(())
}

fn track_system_stats() {
    let sys = System::new();
    let load_avg_one = SYSTEM_LOAD_AVERAGE
        .get_metric_with_label_values(&["1m"])
        .unwrap();
    let load_avg_five = SYSTEM_LOAD_AVERAGE
        .get_metric_with_label_values(&["5m"])
        .unwrap();
    let load_avg_fifteen = SYSTEM_LOAD_AVERAGE
        .get_metric_with_label_values(&["15m"])
        .unwrap();

    loop {
        match sys.memory() {
            Ok(mem) => {
                let used = saturating_sub_bytes(mem.total, mem.free);
                SYSTEM_MEMORY_USED.set(used.as_u64() as i64);
            }
            Err(x) => warn!("unable to get memory stats: {}", x),
        }

        match sys.load_average() {
            Ok(load_avg) => {
                load_avg_one.set(load_avg.one as f64);
                load_avg_five.set(load_avg.five as f64);
                load_avg_fifteen.set(load_avg.fifteen as f64)
            }
            Err(x) => warn!("unable to get load average: {}", x),
        }

        match sys.cpu_temp() {
            Ok(cpu_temp) => {
                SYSTEM_CPU_TEMPERATURE.set(cpu_temp as f64);
            }
            Err(x) => warn!("unable to get CPU temperature: {}", x),
        }

        match sys.networks() {
            Ok(netifs) => {
                for netif in netifs.values() {
                    let stats = sys.network_stats(&netif.name);
                    match stats {
                        Ok(stats) => {
                            SYSTEM_NETWORK_STATS
                                .get_metric_with_label_values(&[netif.name.as_str(), "rx_bytes"])
                                .unwrap()
                                .set(stats.rx_bytes.as_u64() as i64);
                            SYSTEM_NETWORK_STATS
                                .get_metric_with_label_values(&[netif.name.as_str(), "tx_bytes"])
                                .unwrap()
                                .set(stats.tx_bytes.as_u64() as i64);
                            SYSTEM_NETWORK_STATS
                                .get_metric_with_label_values(&[netif.name.as_str(), "rx_packets"])
                                .unwrap()
                                .set(stats.rx_packets as i64);
                            SYSTEM_NETWORK_STATS
                                .get_metric_with_label_values(&[netif.name.as_str(), "tx_packets"])
                                .unwrap()
                                .set(stats.tx_packets as i64);
                            SYSTEM_NETWORK_STATS
                                .get_metric_with_label_values(&[netif.name.as_str(), "rx_errors"])
                                .unwrap()
                                .set(stats.rx_errors as i64);
                            SYSTEM_NETWORK_STATS
                                .get_metric_with_label_values(&[netif.name.as_str(), "tx_errors"])
                                .unwrap()
                                .set(stats.tx_errors as i64);
                        }
                        Err(e) => {
                            warn!("unable to get stats for interface {}: {}", netif.name, e);
                        }
                    }
                }
            }
            Err(x) => warn!("unable to get interfaces: {}", x),
        }

        thread::sleep(Duration::from_secs(2));
    }
}
