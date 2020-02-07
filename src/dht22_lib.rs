//! This is a Rust API to obtain temperature and humidity measurements from a DHT22 connected to
//! a Raspberry Pi.
//!
//! This library is essentially a port of the
//! [Adafruit_Python_DHT](https://github.com/adafruit/Adafruit_Python_DHT) library from C to Rust.
//!
//! This library has been tesed on a DHT22 from Adafruit using a Raspberry Pi Module B+.
//!
//! This is a copy of the [dht22_pi](https://crates.io/crates/dht22_pi) crate, licensed MIT, with
//! slight modifications for logging and re-using the GPIO pin.
//!
use std::ptr::read_volatile;
use std::ptr::write_volatile;

use std::thread::sleep;
use std::time::{Duration, Instant};

use rppal::gpio::{IoPin, Level, Mode, PullUpDown};

use itertools::Itertools;
use libc::sched_param;
use libc::sched_setscheduler;
use libc::SCHED_FIFO;
use libc::SCHED_OTHER;
use std::cmp;

/// A temperature and humidity reading from the DHT22.
#[derive(Debug)]
pub struct Reading {
    pub temperature: f32,
    pub humidity: f32,
}

/// Errors that may occur when reading temperature.
#[derive(Debug)]
pub enum ReadingError {
    /// Occurs if a timeout occured reading the pin.
    Timeout,

    /// Occurs if the checksum value from the DHT22 is incorrect.
    Checksum,

    /// Occurs if there is a problem accessing gpio itself on the Raspberry PI.
    Gpio(rppal::gpio::Error),
}

impl From<rppal::gpio::Error> for ReadingError {
    fn from(err: rppal::gpio::Error) -> ReadingError {
        ReadingError::Gpio(err)
    }
}

const MAX_COUNT: usize = 32000;
const DHT_PULSES: usize = 41;

fn tiny_sleep() {
    let mut i = 0;
    unsafe {
        while read_volatile(&i) < 50 {
            write_volatile(&mut i, read_volatile(&i) + 1);
        }
    }
}

fn set_max_priority() {
    unsafe {
        let param = sched_param { sched_priority: 32 };
        let result = sched_setscheduler(0, SCHED_FIFO, &param);

        if result != 0 {
            panic!("Error setting priority, you may not have cap_sys_nice capability");
        }
    }
}

fn set_default_priority() {
    unsafe {
        let param = sched_param { sched_priority: 0 };
        let result = sched_setscheduler(0, SCHED_OTHER, &param);

        if result != 0 {
            panic!("Error setting priority, you may not have cap_sys_nice capability");
        }
    }
}

fn decode(arr: [usize; DHT_PULSES * 2]) -> Result<Reading, ReadingError> {
    let mut threshold: usize = 0;

    let mut i = 2;
    while i < DHT_PULSES * 2 {
        threshold += arr[i];

        i += 2;
    }

    threshold /= DHT_PULSES - 1;

    let mut data = [0 as u8; 5];
    let mut i = 3;
    while i < DHT_PULSES * 2 {
        let index = (i - 3) / 16;
        data[index] <<= 1;
        if arr[i] >= threshold {
            data[index] |= 1;
        } else {
            // else zero bit for short pulse
        }

        i += 2;
    }

    let checksum = data[0]
        .wrapping_add(data[1])
        .wrapping_add(data[2])
        .wrapping_add(data[3]);
    debug!(
        "threshold (50µs) = {}, checksum is {:#010b}, expected {:#010b}, match={}",
        threshold,
        checksum,
        data[4],
        checksum == data[4]
    );

    if data[4] != checksum {
        return Result::Err(ReadingError::Checksum);
    }

    let h_dec = data[0] as u16 * 256 + data[1] as u16;
    let h = h_dec as f32 / 10.0f32;

    let t_dec = (data[2] & 0x7f) as u16 * 256 + data[3] as u16;
    let mut t = t_dec as f32 / 10.0f32;
    if (data[2] & 0x80) != 0 {
        t *= -1.0f32;
    }

    Result::Ok(Reading {
        temperature: t,
        humidity: h,
    })
}

/// Read temperature and humidity from a DHT22 connected to a Gpio pin on a Raspberry Pi.
///
/// On a Raspberry Pi this is implemented using bit-banging which is very error-prone.  It will
/// fail 30% of the time.  You should write code to handle this.  In addition you should not
/// attempt a reading more frequently than once every 2 seconds because the DHT22 hardware does
/// not support that.
///
pub fn read_pin(pin: &mut IoPin, set_prio: bool) -> Result<Reading, ReadingError> {
    let mut pulse_counts: [usize; DHT_PULSES * 2] = [0; DHT_PULSES * 2];
    pin.set_mode(Mode::Output);

    if set_prio {
        set_max_priority();
    }

    let count = read_pin_inner(pin, &mut pulse_counts)?;

    if set_prio {
        set_default_priority();
    }

    debug!(
        "count: {}, pulse_counts: [{}]",
        count,
        pulse_counts.iter().join(", ").as_str()
    );

    decode(pulse_counts)
}

pub fn read_pin_2(pin: &mut IoPin, set_prio: bool) -> Result<Reading, ReadingError> {
    if set_prio {
        set_max_priority();
    }

    let pulses = read_pin_inner_2(pin)?;
    debug!(
        "pulses: {}",
        pulses
            .iter()
            .map(|p| format!("{}µs", p))
            .join(", ")
            .as_str()
    );

    if set_prio {
        set_default_priority();
    }

    decode_pulses(&pulses)
}

fn decode_pulses(pulses: &[u16]) -> Result<Reading, ReadingError> {
    assert!(pulses.len() >= 80);

    let mut data = [0_u8; 5];
    data[0] = pulses_to_binary(&pulses[0..16]);
    data[1] = pulses_to_binary(&pulses[16..32]);
    data[2] = pulses_to_binary(&pulses[32..48]);
    data[3] = pulses_to_binary(&pulses[48..64]);
    data[4] = pulses_to_binary(&pulses[64..80]);

    let checksum = data[0]
        .wrapping_add(data[1])
        .wrapping_add(data[2])
        .wrapping_add(data[3]);
    debug!(
        "checksum is {:#010b}, expected {:#010b}, match={}",
        checksum,
        data[4],
        checksum == data[4]
    );

    if data[4] != checksum {
        return Result::Err(ReadingError::Checksum);
    }

    let h_dec = data[0] as u16 * 256 + data[1] as u16;
    let h = h_dec as f32 / 10.0f32;

    let t_dec = (data[2] & 0x7f) as u16 * 256 + data[3] as u16;
    let mut t = t_dec as f32 / 10.0f32;
    if (data[2] & 0x80) != 0 {
        t *= -1.0f32;
    }

    Result::Ok(Reading {
        temperature: t,
        humidity: h,
    })
}

fn pulses_to_binary(pulses: &[u16]) -> u8 {
    assert_eq!(pulses.len(), 16);

    let high_threshold = 51_u16;

    let mut binary = 0_u8;
    let mut is_high = false;
    for pulse in pulses.iter() {
        if is_high {
            let bit = if *pulse > high_threshold { 1_u8 } else { 0_u8 };
            binary <<= 1;
            binary |= bit;
        }

        is_high = !is_high;
    }

    binary
}

// 40 (wait time) + 2*80 (start signal) + 40*120 (data) + 40 (end signal, measured) = 5040 µs, so
// we just take twice that and hope for the best
const TIMEOUT_DURATION: Duration = Duration::from_millis(10);

/// This is basically the new Adafruit Python implementation.
fn read_pin_inner_2(pin: &mut IoPin) -> Result<Vec<u16>, ReadingError> {
    // The DHT sends 40 bits = 80 transitions + 2 transitions before and 2 after = 84 transitions.
    let mut transitions: Vec<Instant> = Vec::with_capacity(85);

    // No clue why we need this. It does not work if we transform the pin into an output after
    // the iteration and hold it high for two seconds.
    pin.set_mode(Mode::Output);
    pin.write(Level::High);
    sleep(Duration::from_millis(100));

    // The spec says we should leave the pin low for 1-10ms. The Adafruit python implementation
    // sleeps for 1ms, the old Adafruit C implementation sleeps for 20...
    // In any case, sleep is guaranteed to sleep for at least the duration, so this seems to work.
    pin.write(Level::Low);
    sleep(Duration::from_millis(1));

    // This is pretty early, but we don't need the first two transitions for timing anyway.
    let before = Instant::now();
    let mut old_level = Level::High;
    pin.set_mode(Mode::Input);
    // No clue if we need this either, as the signal line is pulled up already in hardware..
    //pin.set_pullupdown(PullUpDown::PullUp);
    pin.set_pullupdown(PullUpDown::Off);

    // Now loop on the input and time the transitions.
    let mut i = 0_u64;
    loop {
        let level = pin.read();
        if level != old_level {
            // Getting the current time is possibly expensive, so do it only when needed.
            let ts = Instant::now();
            old_level = level;
            transitions.push(ts);
        }

        // If the sensor works as expected, we will record 84 transitions :)
        if transitions.len() == 84 {
            break;
        }

        // This is potentially expensive because elapsed gets the current time, which might incur
        // a syscall, so we try to not do it very often.
        if i % 1000 == 0 && before.elapsed() > TIMEOUT_DURATION {
            break;
        }

        i += 1;
    }

    debug!(
        "recorded {} transitions in {} iterations ({}µs) from pin {}: {}",
        transitions.len(),
        i,
        before.elapsed().as_micros(),
        pin.pin(),
        transitions
            .clone()
            .into_iter()
            .tuple_windows()
            .map(|(x, y)| format!("{}µs", (y - x).as_micros()))
            .join(", ")
            .as_str()
    );

    // Theoretically we can make do with 82 transitions.
    // In practice I only ever saw 0,82, or 84 pulses. Zero pulses is a complete loss, but 82 pulses
    // can be fixed up (read below) :)
    if transitions.len() < 82 {
        return Err(ReadingError::Timeout);
    }

    // TODO sometimes we have 25-50-25 pulses missing
    // TODO sometimes we have 80 transitions and a ~200µs pulses for 50+25+50+25 or so
    // TODO we can use the index to figure out the grouping!

    // We only need the last 82 transitions = 81 pulses, of which the first 80 should be the data
    // pulses.
    let transition_start = cmp::max(1, transitions.len() - 81);
    let mut pulses = Vec::with_capacity(81);
    for i in transition_start..transitions.len() {
        let pulse_micros = (transitions[i] - transitions[i - 1]).as_micros() as u16;
        pulses.push(pulse_micros);
    }

    // If we got 82 transitions that usually means we missed one pulse, and this usually happens
    // somewhere in the data. We can try to fix it!
    // If we find something that is > 120µs, we probably have a 50+(27/70)+50 series registered as one
    // pulse. We can try to find it:
    if transitions.len() == 82 {
        // These 82-transition records always have correct start (and end) markers, so we skip
        // those.
        let elem = pulses.iter().skip(2).find_position(|p| **p > 120);
        match elem {
            Some((index, elem)) => {
                // We skipped two elements...
                let index = index + 2;
                let elem = *elem;

                // The long pulse is usually either ~130µs or ~180-200µs, so we can actually distinguish
                // the missing bit!
                let pulse = if elem < 160 { 27 } else { 70 };

                // Now we insert that
                pulses[index] = 50;
                pulses.insert(index + 1, pulse);
                pulses.insert(index + 2, 50);

                // Now we gotta take the last 81 pulses again because we shifted stuff.
                // we just remove the first two instead, but same thing...
                pulses.drain(0..2);
                assert_eq!(pulses.len(), 81);

                debug!(
                    "attempting to fix missing pulses with 50-{}-50µs (was {}) at index {}, pulses now: {}",
                    pulse,
                    elem,
                    index,
                    pulses
                        .iter()
                        .map(|p| format!("{}µs", *p))
                        .join(", ")
                        .as_str()
                );
            }
            None => {}
        }
    }

    Ok(pulses)
}

fn read_pin_inner(
    pin: &mut IoPin,
    pulse_counts: &mut [usize; DHT_PULSES * 2],
) -> Result<usize, ReadingError> {
    pin.write(Level::High);
    sleep(Duration::from_millis(500));

    pin.write(Level::Low);
    sleep(Duration::from_millis(20));

    pin.set_mode(Mode::Input);

    // Sometimes the pin is briefly low.
    tiny_sleep();

    let mut count: usize = 0;

    while pin.read() == Level::High {
        count += 1;

        if count > MAX_COUNT {
            debug!("timed out in count");
            return Result::Err(ReadingError::Timeout);
        }
    }

    for c in 0..DHT_PULSES {
        let i = c * 2;

        while pin.read() == Level::Low {
            pulse_counts[i] += 1;

            if pulse_counts[i] > MAX_COUNT {
                debug!(
                    "timed out in low, c={}, pulse_counts=[{}]",
                    c,
                    pulse_counts.iter().join(", ").as_str()
                );
                return Result::Err(ReadingError::Timeout);
            }
        }

        while pin.read() == Level::High {
            pulse_counts[i + 1] += 1;

            if pulse_counts[i + 1] > MAX_COUNT {
                debug!(
                    "timed out in high, c={}, pulse_counts=[{}]",
                    c,
                    pulse_counts.iter().join(", ").as_str()
                );
                return Result::Err(ReadingError::Timeout);
            }
        }
    }

    Result::Ok(count)
}

/// Read temperature and humidity from a DHT22 connected to a Gpio pin on a Raspberry Pi.
///
/// On a Raspberry Pi this is implemented using bit-banging which is very error-prone.  It will
/// fail 30% of the time.  You should write code to handle this.  In addition you should not
/// attempt a reading more frequently than once every 2 seconds because the DHT22 hardware does
/// not support that.
///
/*
pub fn read(pin: u8, set_prio: bool) -> Result<Reading, ReadingError> {
    let mut gpio = match Gpio::new() {
        Err(e) => return Err(ReadingError::Gpio(e)),
        Ok(g) => match g.get(pin) {
            Err(e) => return Err(ReadingError::Gpio(e)),
            Ok(pin) => pin.into_io(Mode::Output),
        },
    };

    read_pin(&mut gpio, set_prio)
}
*/

#[cfg(test)]
mod tests {
    use super::decode;
    use super::ReadingError;

    #[test]
    fn from_spec_positive_temp() {
        let arr = [
            80, // initial 80us low period
            80, // initial 80us high period
            // humidity
            50, 26, 50, 26, 50, 26, 50, 26, 50, 26, 50, 26, 50, 70, 50, 26, 50, 70, 50, 26, 50, 26,
            50, 26, 50, 70, 50, 70, 50, 26, 50, 26, // temp
            50, 26, 50, 26, 50, 26, 50, 26, 50, 26, 50, 26, 50, 26, 50, 70, 50, 26, 50, 70, 50, 26,
            50, 70, 50, 70, 50, 70, 50, 70, 50, 70, // checksum
            50, 70, 50, 70, 50, 70, 50, 26, 50, 70, 50, 70, 50, 70, 50, 26,
        ];

        let x = decode(arr).unwrap();
        assert_eq!(x.humidity, 65.2);
        assert_eq!(x.temperature, 35.1);
    }

    #[test]
    fn from_spec_negative_temp() {
        let arr = [
            80, // initial 80us low period
            80, // initial 80us high period
            // humidity
            50, 26, 50, 26, 50, 26, 50, 26, 50, 26, 50, 26, 50, 70, 50, 26, 50, 70, 50, 26, 50, 26,
            50, 26, 50, 70, 50, 70, 50, 26, 50, 26, // temp
            50, 70, 50, 26, 50, 26, 50, 26, 50, 26, 50, 26, 50, 26, 50, 26, 50, 26, 50, 70, 50, 70,
            50, 26, 50, 26, 50, 70, 50, 26, 50, 70, // checksum
            50, 26, 50, 70, 50, 70, 50, 70, 50, 26, 50, 26, 50, 70, 50, 70,
        ];

        let x = decode(arr).unwrap();
        assert_eq!(x.humidity, 65.2);
        assert_eq!(x.temperature, -10.1);
    }

    #[test]
    fn checksum() {
        let arr = [
            80, // initial 80us low period
            80, // initial 80us high period
            // humidity
            50, 26, 50, 26, 50, 26, 50, 26, 50, 70, 50, 26, 50, 70, 50, 26, 50, 70, 50, 26, 50, 26,
            50, 26, 50, 70, 50, 70, 50, 26, 50, 26, // temp
            50, 70, 50, 26, 50, 26, 50, 26, 50, 26, 50, 26, 50, 26, 50, 26, 50, 26, 50, 70, 50, 70,
            50, 26, 50, 26, 50, 70, 50, 26, 50, 70, // checksum
            50, 26, 50, 70, 50, 70, 50, 70, 50, 26, 50, 26, 50, 70, 50, 70,
        ];

        match decode(arr) {
            Ok(_) => {
                panic!("should have failed");
            }
            Err(e) => {
                match e {
                    ReadingError::Checksum => {
                        // ok
                    }
                    _ => {
                        panic!("should have Checksum, got {:?} instead", e);
                    }
                }
            }
        }
    }

    #[test]
    fn sample1() {
        let arr = [
            458, // initial 80us low period
            328, // initial 80us high period
            // humidity
            320, 101, 249, 153, 314, 153, 320, 154, 317, 153, 316, 153, 321, 431, 320, 147, 397,
            154, 315, 435, 316, 154, 320, 431, 320, 430, 319, 431, 320, 431, 320, 426,
            // temperature
            401, 148, 319, 154, 316, 154, 320, 150, 320, 154, 315, 154, 320, 149, 320, 148, 397,
            154, 319, 430, 321, 430, 321, 431, 320, 429, 318, 432, 320, 150, 320, 147,
            // checksum
            379, 434, 316, 434, 317, 153, 320, 431, 317, 435, 316, 435, 317, 153, 320, 425,
        ];

        let x = decode(arr).unwrap();
        assert_eq!(x.humidity, 60.7);
        assert_eq!(x.temperature, 12.4);
    }
}
