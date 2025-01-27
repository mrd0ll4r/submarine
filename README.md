# Submarine

An opinionated program to abstract over hardware attached to a Raspberry Pi, used to control lighting equipment and
other stuff.

## Overview
This is a low-level, (so far) reliable program that interfaces with hardware and exposes an HTTP interface for it.
Additionally, events that are generated by the hardware or other sources are pushed to AMQP.

### Architecture
The high-level API is a 16-bit address space of 16-bit values.
Mapped into this address space are *virtual devices*, which each have exactly one *alias*.
Virtual devices are derived from/mapped to *hardware devices* on numerical *ports*, which start at `0` by convention.
Virtual devices also generate *events*, which are also mapped from hardware devices.

Hardware devices interact with the actual hardware attached to the RPi.
Hardware devices run asynchronously to the rest of the program and independently of each other.
They use a number of different strategies to interface with the hardware, which are abstracted away.

Events can be subscribed to by address and type:

- Any change in hardware state generates at least one `Change` event.
    These are generated by hardware devices when a change in state is detected or initiated.
    For example: A DHT22 temperature sensor generates a `Change` event for both temperature and humidity, if one of
    those values change.
    On the other hand, a PCA9685 PWM generator, which is an output device, generates `Change` events once the hardware
    device successfully set new target values on the device.  

- MCP23017 GPIO expanders and other binary input devices generate `Button` events.
    These are specific to this type of device, because we use them to interface with many buttons.
    The `Button` events generated help keep track of button states, such as clicks (down+up) or long presses (which
    generate an event every second).

There is an API via HTTP to set the values of output devices.

### Configuration

See [dist/config.yaml](dist/config.yaml) and [src/config.rs](src/config.rs).
Submarine expects a file named `config.yaml` with the `submarine` block in it.
The `devices` block can be split up into multiple files for easier management and then be
imported in the main config file.

#### Configuration of Hardware Devices

Hardware devices are configured according to the config sections given below.
All hardware device configurations are aggregated and evaluated before any virtual device is created.

Generally, hardware device configurations look like this:
```
devices:
  - type: mcp23017          # The device type, see below.
    alias: mcp-0            # The alias, must be unique over all hardware devices.
    tags: [ "mcp23017" ]    # (Optional) tags
    config:                 # Hardware device configuration (specific for each type of device)
      i2c_bus: "/dev/i2c-1"
      i2c_slave_address: 32
    outputs:                # Output port mappings
      - port: 0             # Port number (0-based)
        alias: outlet-8-s1  # Alias, must be unique over _all_ output ports of _all_ devices.
        tags: [ "outlet", "relais", "R2" ]
  ...
```

### Supported Devices

We currently support these hardware devices:

- `pca9685`, which is a PCA9685 PWM generator via I2C ([spec](https://www.nxp.com/docs/en/data-sheet/PCA9685.pdf)).
    This device is output-only.

    They are configured like this:
    ```
    i2c_bus: ""                     # The i2cdev device, e.g. "/dev/i2c-1", or an empty string to use an I2C mock.
    i2c_slave_address: 64           # The decimal(!) I2C slave address.
    use_external_clock: false       # Whether to use the external clock source, see spec.
    inverted: true                  # Whether to invert the outputs.
    output_driver_totem_pole: true  # Output driver mode, see spec.
    update_interval_millis: 5       # The update interval. Specifically, the interval at which values will be written
                                    # to the device, if changes were made through virtual devices.
                                    # This tries to keep a steady rhythm, i.e. if the update takes 1 ms, the next update
                                    # will be started 4 ms after that, to keep a 5 ms cycle.
    ```
    It is noteworthy that multiple PCA9685s on one I2C bus work asynchronously to each other, which can cause weird
    behaviour.
    See the `pca9685_sync` device for a way around this. 

- `pca9685_sync` the synchronized version of `pca9685`.
    This device groups all instances of itself on the same I2C bus and triggers updates to all of them.
    This is useful to synchronize multiple PCA9685s.
    Specifically, using this, the bandwidth of an I2C bus can be utilized better, because the three transactions are
    done right after each other, in the same order, in one go.
    
    Configuration is the same as for `pca9685` devices, and it is recommended to set the same `update_interval_millis`
    for all instances on one bus.

- `mcp23017` is the MCP23017 GPIO expander via I2C, see [the spec](http://ww1.microchip.com/downloads/en/DeviceDoc/20001952C.pdf).
    This device is output-only.
    
    The configuration works like this:
    ```
    i2c_bus: ""             # The i2cdev bus, see above.
    i2c_slave_address: 32   # The decimal(!) slave address.
    ```

    These devices do not periodically check for updated values and write them out, but rather flush as soon as a new
    value is written.
    Note however that a single `set` API request can contain many key-value pairs, which will most likely be set
    atomically for this device, at least as long as there is only one writer.
    
- `mcp23017_input` is the input variant of the MCP23017.
    This device is frequently polled to detect changes in its pins.
    It is advised to adjust the polling interval in such a way that buttons are properly debounced, we use a capacitor
    for this (but I'm not an expert on those things).
    
    Configuration:
    ```
    i2c_bus: ""                 # See above.
    i2c_slave_address: 34       # See above
    polling_interval_millis: 5  # The polling interval.
    enable_pullups: true        # Whether to activate the (weak) built-in pull-up resistors (for all pins).
    invert: true                # Whether to logically invert values read.
    ```

- `dht22` is a DHT22 humidity and temperature sensor, see [the spec](https://cdn-shop.adafruit.com/datasheets/Digital+humidity+and+temperature+sensor+AM2302.pdf).
    This device uses a handwritten bit-banging-via-gpio driver with some error-correction specific to the observed
    behaviour on a Raspberry Pi.
    
    Configuration:
    ```
    bcm_pin: 26                     # The GPIO pin (in BCM numbering) to which the data pin is attached.
    adjust_priority: true           # Whether to adjust thread priority during readout.
                                    # Because we bit-bang the protocol, doing this can sometimes improve success rate of
                                    # reading.
    use_experimental_version: true  # Whether to use the experimental handwritten driver.
                                    # If set to false, a slightly modified version of the dht22_pi crate implementation
                                    # will be used.
                                    # If set to true, an optimized implementation is used, see src/dht22_lib.rs for more
                                    # information.
    readout_interval_seconds: 2     # The readout interval, plus minus some milliseconds.
    ```
    
    The [dht22_pi](https://crates.io/crates/dht22_pi) crate, and the Adafruit C implementation that is based off, use a
    spin-and-count approach to try to calculate the different pulse lengths of the DHT22's protocol.
    This works on microcontrollers, because they have a fixed clock speed and no preemptive scheduling, but it does not
    work very well on a multi-core non-realtime OS with clock frequency scaling.
    
    The custom driver implementation also spins and repeatedly polls the GPIO pin, but instead uses a timer to measure
    pulse lengths (this works surprisingly accurately).
    Additionally, it compensates for lost and/or blurred pulses, as observed on our hardware configuration.
    Using this driver we achieve > 95% successful readouts, which is good enough for our case. 

- `gpio` represents a single GPIO pin.
    Currently used for input only.
    
    Configuration:
    ```
    bcm_pin: 24                         # The pin number, in BCM numbering.
    pull: down                          # The pull direction, either "up" or "down".
    invert: false                       # Whether to logically invert values read.
    readout_interval_milliseconds: 500  # The polling interval in milliseconds.
    ```

### API

There is an HTTP API to control outputs.
Additionally, events are published to AMQP.

The [alloy](../alloy) crate contains all types relevant to the API and events.

## Compilation
You need a working installation of Rust, see [rustup.rs](https://rustup.rs/).
We use the most recent stable version, which you should have installed by default.

This project is configure to compile for Linux on ARMv8 (64 bit) by default, no matter from where you're compiling.
First of all, add the appropriate target to your Rust installation:
```
rustup target add aarch64-unknown-linux-gnu
# if you also want ARMv7
#rustup target add armv7-unknown-linux-gnueabihf
```
Then tell cargo how to link for that target, in `~/.cargo/config`:
```
[target.aarch64-unknown-linux-gnu]
linker = "aarch64-linux-gnu-gcc"

# if you also want ARMv7
#[target.armv7-unknown-linux-gnueabihf]
#linker = "arm-linux-gnueabihf-gcc"
```

Now you actually need to get the linker.
On Linux you just need to get the appropriate package (Debian):
```
sudo apt install gcc-aarch64-linux-gnu
# if you want to build for ARMv7
#sudo apt install gcc-arm-linux-gnueabihf
```
On Windows you can get pre-built binaries for this from [here](https://gnutoolchains.com/raspberry/) (8.3.0 worked for me).
Additionally, before building on Windows, invoke `env.bat`, which will set `AR` to the appropriate value.

The default target is already set to Linux ARMv7 (see `.cargo/config`), so `cargo build` will produce a debug build for that target.
`cargo build --release` produces a release build.

## Running

Submarine is a single statically-linked binary which runs on the Raspberry Pi (and should be kept running).
It uses the usual Rust logging facilities, which can be controlled via the `RUST_LOG` environment variable.
A good general setting for this might be `RUST_LOG="info"`.

To use the new DHT22 implementation, SYS_CAP_NICE capabilities are needed.
These can be given to a binary like so:
```
sudo setcap 'cap_sys_nice=eip' submarine
```
Unfortunately, this is cleared whenever the binary is replaced.

It has been observed that a current stable Raspbian shows somewhat high variance in network latencies.
For this reason it is recommended to run [Kaleidoscope](../kaleidoscope), the lighting execution engine, on the same
device and communicate via loopback.
This still(!) shows high variance, but is generally fast enough to not matter much in practice.