//! Model-scoped measurements recovered from official SDKs and `P1_41M` captures.
use super::{LOAD_TIMESTAMP_COMMAND, client, scp};
use crate::{Error, Result};
use time::{Date, Duration, Month};

/// Read charger conversion data (0x80, hop 0).
pub const LOAD_CONVERSION_COMMAND: [u8; 5] = [0, 0xc0, 0, 0x20, 0xe0];
/// Read the ordinary charger battery layout (0x81, hop 0).
pub const LOAD_CHARGER_BATTERY_COMMAND: [u8; 5] = [0, 0xc0, 1, 0x20, 0xf5];
/// Read the calendar epoch (0x03, hop 0).
pub const LOAD_EPOCH_COMMAND: [u8; 5] = [0, 0xc0, 3, 0, 0x3f];

/// Independently cross-checked SDK model identity, not a sales-market identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerialIdentity {
    /// Official family name.
    pub family: &'static str,
    /// Internal SDK type number.
    pub native_type: u8,
}

/// Identify only serial family pairs checked in both Java and native SDK code.
#[must_use]
pub fn identify_serial(serial: &str) -> Option<SerialIdentity> {
    let s = serial.strip_prefix("0x").or_else(|| serial.strip_prefix("0X")).unwrap_or(serial);
    if s.len() != 20 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let first = u16::from_str_radix(&s[..4], 16).ok()?;
    let second = u16::from_str_radix(&s[4..8], 16).ok()?;
    let (family, native_type) = match (first, second) {
        (0x33, 0x57) => ("P1_40", 0x61),
        (0x33, 0x5a) => ("P1_40M", 0x64),
        (0x34, 0x5c) => ("P1_40D", 0x71),
        (0x33, 0x69) => ("P1_41P", 0x81),
        (0x33, 0x6a) => ("P1_41M", 0x82),
        (0x34, 0x68) => ("P1_41J", 0x85),
        (0x3a, 0x72) => ("P1_41A", 0x87),
        _ => return None,
    };
    Some(SerialIdentity { family, native_type })
}

/// New optional readings. Estimates never populate the old verified-voltage field.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Measurements {
    /// Family from serial words; absent for unknown or malformed serials.
    pub family: Option<String>,
    /// Unscaled voltage from the short charger battery response.
    pub voltage_raw: Option<u16>,
    /// Device-specific conversion factor, never a hardcoded calibration.
    pub voltage_factor: Option<u16>,
    /// Charge percentage; independently compared with vendor GATT on `P1_41M`.
    pub charge_percent: Option<u8>,
    /// Battery temperature in degrees Celsius; currently interpreted directly from the SDK byte.
    pub temperature_celsius: Option<u8>,
    /// Candidate volts using the official web estimator's conversion.
    pub estimated_voltage: Option<f32>,
    /// Calendar epoch as YYYY-MM-DD.
    pub epoch: Option<String>,
    /// Interpreted device clock; timezone and synchronization are not established.
    pub device_clock: Option<String>,
}

/// Read plan restricted to the family actually tested with these layouts.
#[must_use]
pub fn measurement_commands(serial: &str) -> Vec<Vec<u8>> {
    if identify_serial(serial).is_some_and(|i| i.native_type == 0x82) {
        [
            LOAD_CONVERSION_COMMAND.as_slice(),
            LOAD_CHARGER_BATTERY_COMMAND.as_slice(),
            LOAD_EPOCH_COMMAND.as_slice(),
            LOAD_TIMESTAMP_COMMAND.as_slice(),
        ]
        .iter()
        .map(|c| c.to_vec())
        .collect()
    } else {
        Vec::new()
    }
}

fn invalid(message: &str) -> Error {
    Error::ProtocolDecode(message.into())
}

fn date(data: &[u8]) -> Result<Date> {
    if data.len() != 4 {
        return Err(invalid("invalid epoch length"));
    }
    let year = i32::from(u16::from_le_bytes([data[0], data[1]]));
    if year == 0 {
        return Err(invalid("invalid epoch year"));
    }
    let month = Month::try_from(data[2]).map_err(|_| invalid("invalid epoch month"))?;
    Date::from_calendar_date(year, month, data[3]).map_err(|_| invalid("invalid epoch date"))
}

/// Decode a partial set of validated responses. Missing responses remain absent.
///
/// # Errors
/// Rejects corrupt, duplicate, unexpected, routed-to-holder or unsupported layouts,
/// impossible charge/calendar values and conflicting epoch observations.
pub fn measurements(serial: &str, frames: &[Vec<u8>]) -> Result<Measurements> {
    let identity = identify_serial(serial);
    let mut out =
        Measurements { family: identity.map(|i| i.family.into()), ..Measurements::default() };
    if frames.is_empty() {
        return Ok(out);
    }
    if identity.is_none_or(|i| i.native_type != 0x82) {
        return Err(invalid("measurement layout is not validated for this serial family"));
    }
    let mut seen = std::collections::HashSet::new();
    let mut epoch = None;
    let mut clock_epoch = None;
    for frame in frames {
        client::validate(frame, true)?;
        if frame[2] & 0x40 != 0 || ((frame[1] >> 3) & 7) != 0 {
            return Err(invalid("expected a charger read response"));
        }
        let id = if scp::uses_crc16(frame) {
            if frame.len() != 16 || frame[4..6] != [0, 4] {
                return Err(invalid("unexpected extended measurement"));
            }
            let d = date(&frame[6..10])?;
            let seconds = u32::from_le_bytes([frame[10], frame[11], frame[12], frame[13]]);
            let clock = d
                .midnight()
                .checked_add(Duration::seconds(i64::from(seconds)))
                .ok_or_else(|| invalid("device clock out of range"))?;
            clock_epoch = Some(d);
            out.device_clock = Some(format!(
                "{} {:02}:{:02}:{:02}",
                clock.date(),
                clock.hour(),
                clock.minute(),
                clock.second()
            ));
            0x10004
        } else {
            if frame.len() != 9 {
                return Err(invalid("expected short four-byte measurement layout"));
            }
            let id = u32::from(frame[2] & 3) | (u32::from(frame[3]) << 2);
            let payload = &frame[4..8];
            match id {
                0x80 => {
                    if payload[2..] != [0, 0] {
                        return Err(invalid("unknown conversion layout"));
                    }
                    out.voltage_factor = Some(u16::from_le_bytes([payload[0], payload[1]]));
                }
                0x81 => {
                    if payload[2] > 100 {
                        return Err(invalid("invalid SCP battery percentage"));
                    }
                    out.voltage_raw = Some(u16::from_le_bytes([payload[0], payload[1]]));
                    out.charge_percent = Some(payload[2]);
                    out.temperature_celsius = Some(payload[3]);
                }
                3 => {
                    epoch = Some(date(payload)?);
                }
                _ => return Err(invalid("unexpected classic measurement command")),
            }
            id
        };
        if !seen.insert(id) {
            return Err(invalid("duplicate measurement response"));
        }
    }
    if epoch.zip(clock_epoch).is_some_and(|(a, b)| a != b) {
        return Err(invalid("conflicting device epochs"));
    }
    out.epoch = epoch.or(clock_epoch).map(|d| d.to_string());
    out.estimated_voltage = out
        .voltage_raw
        .zip(out.voltage_factor)
        .filter(|(raw, factor)| *raw != 0 && *factor != 0)
        .map(|(raw, factor)| (f32::from(factor) + 1.0) * f32::from(raw) / 65536.0);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    const SERIAL: &str = "0033006A000000000000";
    fn frames() -> Vec<Vec<u8>> {
        [
            vec![0, 0xc0, 0x84, 0x20, 0x0b, 2, 0, 0, 0x70],
            vec![0, 0xc0, 0x85, 0x20, 0xfe, 1, 0x41, 0x27, 0x12],
            vec![0, 0xc0, 0x87, 0, 0xda, 7, 1, 1, 0x66],
            vec![0, 0xc0, 0x90, 0x0a, 0, 4, 0xda, 7, 1, 1, 0x7a, 0xf4, 0x93, 0x1f, 0x4a, 0xf2],
        ]
        .to_vec()
    }
    #[test]
    fn captured_readings_and_clock_keep_estimates_separate() {
        let m = measurements(SERIAL, &frames()).unwrap();
        assert_eq!(m.family.as_deref(), Some("P1_41M"));
        assert_eq!(m.charge_percent, Some(65));
        assert_eq!(m.temperature_celsius, Some(39));
        assert_eq!(m.voltage_raw, Some(510));
        assert_eq!(m.voltage_factor, Some(523));
        assert!((m.estimated_voltage.unwrap() - 4.077_759).abs() < 0.000_01);
        assert_eq!(m.epoch.as_deref(), Some("2010-01-01"));
        assert_eq!(m.device_clock.as_deref(), Some("2026-10-15 19:54:34"));
        assert!(measurements(SERIAL, &frames()[1..2]).unwrap().estimated_voltage.is_none());
    }
    #[test]
    fn invalid_frames_and_other_layouts_are_rejected() {
        for index in 0..frames().len() {
            let mut f = frames();
            f[index][4] ^= 1;
            assert!(measurements(SERIAL, &f).is_err());
        }
        let mut f = frames();
        f.push(f[0].clone());
        assert!(measurements(SERIAL, &f).is_err());
        assert!(measurements("00330057000000000000", &frames()).is_err());
        assert!(measurement_commands("00330069000000000000").is_empty());
        for serial in ["", "33006a00000000000000", "0033006A00000000000é"] {
            assert!(identify_serial(serial).is_none());
        }
    }
    #[test]
    fn valid_crc_does_not_make_bad_values_valid() {
        for (index, offset, byte) in [(1, 6, 101), (2, 7, 32), (0, 6, 1)] {
            let mut f = frames();
            f[index][offset] = byte;
            let last = f[index].len() - 1;
            f[index][last] = scp::crc8(&f[index][2..last]);
            assert!(measurements(SERIAL, &f).is_err());
        }
        for c in measurement_commands(SERIAL) {
            client::validate(&c, false).unwrap();
        }
    }
}
