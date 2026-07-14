# IQOS

[![CI](https://github.com/V-VX/iqos/actions/workflows/ci.yml/badge.svg)](https://github.com/V-VX/iqos/actions/workflows/ci.yml)
[![License: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0-blue.svg)](LICENSE)

Async Rust library for controlling IQOS devices over BLE or USB, exposing device internals not accessible through the official IQOS app.

> **Early development.** The public API is still taking shape and should be considered unstable.

---

## What This Exposes

The official IQOS app surfaces only basic status and settings. This library goes further.

### Diagnostic telemetry — not available in the official app

| Field | Description |
|---|---|
| Total puff count | Lifetime usage counter |
| Days used | How long the device has been in service |
| Battery voltage | Raw cell voltage, not just a percentage |

### Programmatic device control

- Brightness and vibration
- FlexPuff, FlexBattery mode, and Pause Mode _(ILUMA i / ILUMA i PRIME)_
- Smart Gesture _(ILUMA / ILUMA PRIME / ILUMA i series)_; Auto Start _(ILUMA i series)_
- Device lock / unlock
- Firmware version (stick and holder)

---

## Transport Support

| Transport | Status | Feature flag |
|---|---|---|
| BLE (Bluetooth Low Energy) | ✅ Implemented | `btleplug-support` |
| USB HID | 🧪 Experimental | `usb-support` |

Both transports carry the same SCP application messages. USB adds 64-byte HID framing around them: report `0x3f` carries an atomic message and report `0x40` carries a segment of a longer message. The USB backend uses `nusb` with Tokio and implements the same async `Transport` trait as BLE.

Ordinary SCP messages use CRC-8 (`0x07`). Length-delimited data messages with opcode `0x10`/`0x90`, including diagnosis telemetry, use CRC-16/OPENSAFETY-B (`0x755b`) stored little-endian. Checksum validation and request/response matching are shared by BLE and USB.

USB discovery, device metadata, serial number, product numbers, firmware versions, battery voltage, brightness, vibration settings, FlexPuff, FlexBattery, AutoStart, and diagnosis reads have been validated on an IQOS ILUMA i and compared with BLE results from the same device. Stateful commands are not yet hardware-validated over USB.

---

## Architecture

```text
src/
├── lib.rs              # Public facade — Iqos<T> device handle
├── connection.rs       # Runtime BLE/USB transport enum
├── error.rs            # Error types and Result alias
├── transport.rs        # Transport trait shared by BLE/USB backends
├── protocol/           # Command builders, response parsers, typed domain values
│   ├── ble.rs
│   ├── brightness.rs
│   ├── diagnosis.rs
│   ├── firmware.rs
│   ├── flexbattery.rs
│   ├── flexpuff.rs
│   ├── gesture.rs
│   ├── lock.rs
│   ├── scp.rs           # Shared BLE/USB checksum validation and frame matching
│   ├── types.rs
│   └── vibration.rs
└── transports/
    ├── ble_btleplug.rs # BLE backend (btleplug-support feature)
    └── usb.rs          # USB HID backend (usb-support feature)
```

### Design principles

- **Library-first** — no stdout/stderr output, no `unwrap()`/`panic!()` in library code
- **Typed protocol models** — all device state is represented as typed Rust values, not raw bytes
- **Transport-agnostic** — BLE and USB share the same `Transport` trait
- **Testable without hardware** — full coverage via `MockTransport`

---

## Development

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

## Local Hardware Debug

The repository includes a local BLE debug binary at [`debug/hardware_ble.rs`](/Users/vvx/projekt/rs/iqos/debug/hardware_ble.rs).

Basic run:

```bash
cargo run --features btleplug-support --bin hardware_ble
```

Run against a specific device name:

```bash
IQOS_TEST_NAME_SUBSTRING="ILUMA" \
  cargo run --features btleplug-support --bin hardware_ble
```

Enable setting changes and verification:

```bash
IQOS_TEST_ALLOW_STATEFUL_WRITES=1 \
  cargo run --features btleplug-support --bin hardware_ble
```

Useful environment variables:

- `IQOS_TEST_NAME_SUBSTRING` — optional device-name filter
- `IQOS_TEST_ALLOW_STATEFUL_WRITES` — enable write operations and verification loops (`1` or `true`)
- `IQOS_TEST_TRACE_DIAGNOSIS` — print raw diagnosis request/response frames
- `IQOS_TEST_VIBRATE_MILLIS` — vibration duration for locate-device steps (default: `500`)

When `IQOS_TEST_ALLOW_STATEFUL_WRITES=1` is set, the binary reads the current setting, writes the opposite value, reads again to verify the change, restores the original value, and reads again to verify restoration for settings that support read-back (`brightness`, `FlexPuff`, `vibration`, `FlexBattery`).

`autostart` and `smartgesture` are also checked, but their status read is still experimental and based on a reverse-engineered probe in the debug binary rather than a stable library API.

Direct commands such as vibration bursts and lock/unlock do not currently have a matching read-back status in the library. Those steps are still executed, and the binary finishes them in a known end state: vibration stopped and device unlocked.

### USB

The USB probe is read-only:

```bash
cargo run --features usb-support --bin hardware_usb
```

The CLI can exercise the same path with `iqos --usb diagnosis`.

On Linux, access to the USB device may require `sudo` or a udev rule granting the current user access to IQOS USB devices (`2759:0003`). The backend temporarily detaches the kernel HID driver while the interface is open and reattaches it when the handle is dropped normally.

### Focused Feature Debug Binaries

Implementation-local real-device probes should live in a single file directly under `debug/`, not
inside the integrated `hardware_ble` harness. Register each file as its own binary target in
`Cargo.toml`.

Example:

```toml
[[bin]]
name = "autostart"
path = "debug/autostart.rs"
required-features = ["btleplug-support"]
```

Then run only that focused probe:

```bash
IQOS_TEST_ALLOW_STATEFUL_WRITES=1 \
  cargo run --features btleplug-support --bin autostart
```

---

## License

GPL-3.0 — see [LICENSE](LICENSE).
