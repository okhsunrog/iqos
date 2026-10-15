//! Transport-independent entry points for foreign-language clients.
//! All wire knowledge stays here or in the typed protocol modules.
use super::*;
use crate::{Error, Result};

fn invalid(message: impl Into<String>) -> Error {
    Error::ProtocolDecode(message.into())
}

/// Validate a normalized command or response, including length and checksum.
pub fn validate(bytes: &[u8], response: bool) -> Result<()> {
    let error = if response { scp::response_error(bytes) } else { scp::checksum_error(bytes) };
    error.map_or(Ok(()), |error| Err(invalid(error)))
}

/// Compare request/response identities, ignoring acknowledgements.
pub fn matches(command: &[u8], response: &[u8]) -> bool {
    scp::headers_match(command, response)
}

/// BLE countdown framing, independent of the platform's GATT implementation.
pub fn fragments(command: &[u8]) -> Result<Vec<Vec<u8>>> {
    validate(command, false)?;
    let chunks: Vec<_> = command[1..].chunks(19).collect();
    Ok(chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            let mut bytes = vec![(chunks.len() - i - 1) as u8];
            bytes.extend_from_slice(chunk);
            bytes
        })
        .collect())
}

/// Bounded BLE frame assembler. Discard the instance after a transport error.
#[derive(Default)]
pub struct Assembler(scp::BleFrameAssembler);
impl Assembler {
    /// Start a fresh stream.
    pub fn new() -> Self {
        Self::default()
    }
    /// Return a complete normalized frame once the countdown reaches zero.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Option<Vec<u8>>> {
        self.0.push(bytes).map_err(invalid)
    }
}

/// Replay a bounded set of fragments; clients only retain opaque chunks.
pub fn assemble(chunks: &[Vec<u8>]) -> Result<Option<Vec<u8>>> {
    if chunks.len() > 14 {
        return Err(invalid("too many BLE fragments"));
    }
    let mut assembler = Assembler::new();
    let mut frame = None;
    for chunk in chunks {
        if frame.is_some() {
            return Err(invalid("multiple frames in one assembly"));
        }
        frame = assembler.push(chunk)?;
    }
    Ok(frame)
}

/// Named commands shared by language adapters. Names are stable API identifiers.
pub fn command(name: &str) -> Result<Vec<u8>> {
    let bytes: &[u8] = match name {
        "firmware" => &LOAD_STICK_FIRMWARE_VERSION_COMMAND,
        "holder_firmware" => &LOAD_HOLDER_FIRMWARE_VERSION_COMMAND,
        "product" => &PRODUCT_NUMBER_COMMAND,
        "holder_product" => &HOLDER_PRODUCT_NUMBER_COMMAND,
        "telemetry" => &LOAD_TELEMETRY_COMMAND,
        "timestamp" => &LOAD_TIMESTAMP_COMMAND,
        "life_data" => &LOAD_LIFE_DATA_COMMAND,
        "brightness" => &LOAD_BRIGHTNESS_COMMAND,
        "vibration" => &LOAD_VIBRATION_SETTINGS_COMMAND,
        "charge_vibration" => &LOAD_VIBRATE_CHARGE_START_COMMAND,
        "flexpuff" => &LOAD_FLEXPUFF_COMMAND,
        "flexbattery" => &LOAD_FLEXBATTERY_COMMAND,
        "pause" => &LOAD_PAUSEMODE_COMMAND,
        "autostart" => &LOAD_AUTOSTART_COMMAND,
        "locate_start" => &START_VIBRATE_COMMAND,
        "locate_stop" => &STOP_VIBRATE_COMMAND,
        _ => return Err(invalid(format!("unknown command: {name}"))),
    };
    validate(bytes, false)?;
    Ok(bytes.to_vec())
}

/// Build a settings write sequence using the same builders as native clients.
pub fn setting_commands(name: &str, enabled: bool) -> Result<Vec<Vec<u8>>> {
    let commands: Vec<Vec<u8>> = match name {
        "brightness" => (if enabled { BrightnessLevel::High } else { BrightnessLevel::Low })
            .write_commands()
            .iter()
            .map(|b| b.to_vec())
            .collect(),
        "flexpuff" => vec![FlexPuffSetting::new(enabled).write_command().to_vec()],
        "flexbattery" => vec![
            (if enabled { FlexBatteryMode::Eco } else { FlexBatteryMode::Performance })
                .write_command()
                .to_vec(),
        ],
        "pause" => vec![super::flexbattery::pausemode_command(enabled).to_vec()],
        "autostart" => vec![autostart_command(enabled).to_vec()],
        "gesture" => vec![smartgesture_command(enabled).to_vec()],
        "lock" => (if enabled { lock_commands() } else { unlock_commands() })
            .iter()
            .map(|b| b.to_vec())
            .collect(),
        _ => return Err(invalid(format!("unknown setting: {name}"))),
    };
    for bytes in &commands {
        validate(bytes, false)?;
    }
    Ok(commands)
}

/// Decode a validated firmware response.
pub fn firmware(bytes: &[u8], holder: bool) -> Result<String> {
    validate(bytes, true)?;
    let v = FirmwareVersion::from_response(
        bytes,
        if holder { FirmwareKind::Holder } else { FirmwareKind::Stick },
    )?;
    Ok(format!("{}.{}.{}.{}", v.major, v.minor, v.patch, v.year))
}

/// Decode a validated product number, excluding the checksum for both targets.
pub fn product(bytes: &[u8], holder: bool) -> Result<String> {
    validate(bytes, true)?;
    product_number_from_response(
        bytes,
        if holder { ProductNumberKind::Holder } else { ProductNumberKind::Stick },
    )
}

/// Decode a named setting through the native typed parser.
pub fn setting(bytes: &[u8], name: &str) -> Result<bool> {
    validate(bytes, true)?;
    match name {
        "brightness" => Ok(BrightnessLevel::from_response(bytes)? == BrightnessLevel::High),
        "flexpuff" => Ok(FlexPuffSetting::from_response(bytes)?.is_enabled()),
        "flexbattery" => Ok(FlexBatteryMode::from_response(bytes)? == FlexBatteryMode::Eco),
        "pause" => super::flexbattery::pausemode_from_response(bytes),
        "autostart" => autostart_from_response(bytes),
        "charge_vibration" => VibrationSettings::charge_start_from_response(bytes),
        _ => Err(invalid(format!("unknown setting: {name}"))),
    }
}

/// Decode vibration flags in heating, use, end, manual-stop order.
pub fn vibration(bytes: &[u8], model_name: &str) -> Result<Vec<bool>> {
    validate(bytes, true)?;
    let v = VibrationSettings::from_response(bytes, DeviceModel::from_local_name(model_name))?;
    Ok(vec![
        v.when_heating_start(),
        v.when_starting_to_use(),
        v.when_puff_end(),
        v.when_manually_terminated(),
    ])
}

/// Encode all vibration flags. Holder models require an observed charge flag.
pub fn vibration_commands(
    flags: &[bool],
    charge: Option<bool>,
    model_name: &str,
) -> Result<Vec<Vec<u8>>> {
    let [heat, use_, end, stop] = flags else {
        return Err(invalid("expected four vibration flags"));
    };
    let model = DeviceModel::from_local_name(model_name);
    if model.supports_holder_features() && charge.is_none() {
        return Err(invalid("read charge-start vibration first"));
    }
    let mut v = VibrationSettings::new(*heat, *use_, *end, *stop);
    if let Some(charge) = charge {
        v = v.with_observed_charge_start(charge);
    }
    v.build_update_commands(model)
}

/// Partial diagnostic update; absent fields leave previous observations intact.
pub struct DiagnosticUpdate {
    /// Present even for an empty telemetry response.
    pub telemetry_records: Option<Vec<Vec<u8>>>,
    /// Raw device time with unverified interpretation.
    pub device_time_raw: Option<Vec<u8>>,
    /// Model-specific LifeData with no assumed units.
    pub life_data_raw: Option<Vec<u8>>,
}

/// Decode one diagnostic frame without attributing unverified physical units.
pub fn diagnosis(bytes: &[u8]) -> Result<DiagnosticUpdate> {
    let data = DiagnosticDataBuilder::default().parse(bytes)?.build();
    Ok(DiagnosticUpdate {
        telemetry_records: (scp::uses_crc16(bytes) && bytes[4..6] == [1, 1])
            .then(|| data.telemetry_records.into_iter().map(|r| r.to_vec()).collect()),
        device_time_raw: data.device_time_raw.map(|b| b.to_vec()),
        life_data_raw: data.life_data_raw.map(|b| b.to_vec()),
    })
}

/// Parse the vendor battery characteristic's percentage field.
pub fn battery(bytes: &[u8]) -> Result<u8> {
    bytes.get(2).copied().filter(|v| *v <= 100).ok_or_else(|| invalid("invalid battery percentage"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_client_commands_and_write_sequences_have_valid_wire_format() {
        for name in [
            "firmware",
            "holder_firmware",
            "product",
            "holder_product",
            "telemetry",
            "timestamp",
            "life_data",
            "brightness",
            "vibration",
            "charge_vibration",
            "flexpuff",
            "flexbattery",
            "pause",
            "autostart",
            "locate_start",
            "locate_stop",
        ] {
            validate(&command(name).unwrap(), false).unwrap();
        }
        for name in
            ["brightness", "flexpuff", "flexbattery", "pause", "autostart", "gesture", "lock"]
        {
            for enabled in [false, true] {
                for frame in setting_commands(name, enabled).unwrap() {
                    validate(&frame, false).unwrap();
                }
            }
        }
        assert!(command("typo").is_err());
        assert!(setting_commands("typo", true).is_err());
    }

    #[test]
    fn vibration_sequences_roundtrip_through_ble_fragments_for_all_flags() {
        for mask in 0..16 {
            let flags: Vec<_> = (0..4).map(|i| mask & (1 << i) != 0).collect();
            for charge in [false, true] {
                let commands = vibration_commands(&flags, Some(charge), "IQOS ILUMA i").unwrap();
                for command in commands {
                    let chunks = fragments(&command).unwrap();
                    assert!(chunks.iter().all(|b| b.len() <= 20));
                    let mut assembler = Assembler::new();
                    let mut result = None;
                    for chunk in chunks {
                        result = assembler.push(&chunk).unwrap();
                    }
                    assert_eq!(result.unwrap(), command);
                }
            }
        }
        assert!(vibration_commands(&[true; 4], None, "IQOS ILUMA i").is_err());
        assert!(vibration_commands(&[], None, "IQOS ILUMA ONE").is_err());
    }

    #[test]
    fn strict_front_door_rejects_bad_crc_before_payload_decode() {
        let mut response = vec![0, 0xc0, 0x86, 0x23, 30, 0, 0, 0];
        response.push(scp::crc8(&response[2..]));
        assert!(!setting(&response, "brightness").unwrap());
        response[4] ^= 1;
        assert!(setting(&response, "brightness").is_err());
        for end in 0..response.len() {
            assert!(setting(&response[..end], "brightness").is_err());
        }
        assert!(battery(&[0, 0, 101]).is_err());
        assert_eq!(battery(&[0, 0, 32]).unwrap(), 32);
    }
}
