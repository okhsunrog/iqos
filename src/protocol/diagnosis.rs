use super::scp;
use crate::{Error, Result};

/// Read the raw extended 0x0101 records; their physical meaning is unverified.
pub const LOAD_TELEMETRY_COMMAND: [u8; 8] = [0x00, 0xC9, 0x10, 0x02, 0x01, 0x01, 0x75, 0xD6];
/// Read the raw extended device-time payload (not days of use).
pub const LOAD_TIMESTAMP_COMMAND: [u8; 8] = [0x00, 0xC0, 0x10, 0x02, 0x00, 0x04, 0x38, 0xEF];
/// Read classic command 0x084 (model-specific LifeData).
pub const LOAD_LIFE_DATA_COMMAND: [u8; 5] = [0x00, 0xC0, 0x00, 0x21, 0xE7];
/// Compatibility alias. This command reads LifeData, NOT battery voltage.
pub const LOAD_BATTERY_VOLTAGE_COMMAND: [u8; 5] = LOAD_LIFE_DATA_COMMAND;
/// Diagnostic reads, retaining the established sequence.
pub const ALL_DIAGNOSIS_COMMANDS: [&[u8]; 4] = [
    &LOAD_TELEMETRY_COMMAND,
    &LOAD_TIMESTAMP_COMMAND,
    &LOAD_TELEMETRY_COMMAND,
    &LOAD_LIFE_DATA_COMMAND,
];

/// Validated diagnostic data without guessed units or counter meanings.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct DiagnosticData {
    /// Uninterpreted 8-byte records from extended identifier 0x0101.
    pub telemetry_records: Vec<[u8; 8]>,
    /// Raw 8-byte device-time payload, kept separate from usage statistics.
    pub device_time_raw: Option<[u8; 8]>,
    /// Raw model-specific LifeData payload, without fabricated voltage units.
    pub life_data_raw: Option<[u8; 14]>,
    /// Compatibility field: always None until the counter's meaning is verified.
    pub total_smoking_count: Option<u16>,
    /// Compatibility field: always None; the device clock is not days used.
    pub days_used: Option<u16>,
    /// Compatibility field: always None until model-specific conversion is verified.
    pub battery_voltage: Option<f32>,
}

impl DiagnosticData {
    /// Create empty diagnostic data.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct DiagnosticDataBuilder {
    inner: DiagnosticData,
}

impl DiagnosticDataBuilder {
    pub fn parse(mut self, bytes: &[u8]) -> Result<Self> {
        if let Some(error) = scp::response_error(bytes) {
            return Err(Error::ProtocolDecode(error));
        }
        if scp::uses_crc16(bytes) {
            if bytes.len() < 8 {
                return Err(Error::ProtocolDecode("missing extended data identifier".into()));
            }
            let payload = &bytes[6..bytes.len() - 2];
            match &bytes[4..6] {
                [1, 1] => {
                    if !payload.len().is_multiple_of(8) {
                        return Err(Error::ProtocolDecode("partial telemetry record".into()));
                    }
                    self.inner.telemetry_records = payload
                        .chunks_exact(8)
                        .map(|record| record.try_into().expect("exact 8-byte chunk"))
                        .collect();
                }
                [0, 4] => {
                    self.inner.device_time_raw = Some(payload.try_into().map_err(|_| {
                        Error::ProtocolDecode("unknown device-time payload length".into())
                    })?);
                }
                _ => {}
            }
        } else if (u16::from(bytes[2] & 3) | (u16::from(bytes[3]) << 2)) == 0x84 {
            self.inner.life_data_raw =
                Some(bytes[4..bytes.len() - 1].try_into().map_err(|_| {
                    Error::ProtocolDecode("unknown LifeData payload length".into())
                })?);
        }
        Ok(self)
    }
    pub fn build(self) -> DiagnosticData {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const TELEMETRY: [u8; 40] = [
        0, 8, 0x90, 0x22, 1, 1, 0, 0, 0, 0, 0x56, 0xb, 0, 0x8e, 1, 0, 0, 0, 0x57, 0xa, 0, 0x20, 2,
        0, 0, 0, 0x11, 6, 0, 0x17, 3, 0, 0, 0, 0x24, 0, 0, 0x18, 0xed, 0xee,
    ];
    const TIME: [u8; 16] =
        [0, 0xc0, 0x90, 0xa, 0, 4, 0xda, 7, 1, 1, 0xbd, 0xd1, 0x93, 0x1f, 0xd6, 0xa2];
    const LIFE: [u8; 19] =
        [0, 0xc0, 0x88, 0x21, 0xb0, 0x1d, 0x29, 0, 0xac, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0x94];

    #[test]
    fn captured_diagnostics_preserve_bytes_without_inventing_units() {
        let data = DiagnosticDataBuilder::default()
            .parse(&TELEMETRY)
            .unwrap()
            .parse(&TIME)
            .unwrap()
            .parse(&TELEMETRY)
            .unwrap()
            .parse(&LIFE)
            .unwrap()
            .build();
        assert_eq!(data.telemetry_records.len(), 4);
        assert_eq!(data.telemetry_records[0], [0, 0, 0, 0, 0x56, 0xb, 0, 0x8e]);
        assert_eq!(data.device_time_raw, Some([0xda, 7, 1, 1, 0xbd, 0xd1, 0x93, 0x1f]));
        assert_eq!(data.life_data_raw.unwrap()[..4], [0xb0, 0x1d, 0x29, 0]);
        assert_eq!(
            (data.total_smoking_count, data.days_used, data.battery_voltage),
            (None, None, None)
        );
    }
    #[test]
    fn rejects_every_truncated_capture_and_bad_crc() {
        for frame in [&TELEMETRY[..], &TIME[..], &LIFE[..]] {
            for length in 0..frame.len() {
                assert!(DiagnosticDataBuilder::default().parse(&frame[..length]).is_err());
            }
            let mut damaged = frame.to_vec();
            damaged[4] ^= 1;
            assert!(DiagnosticDataBuilder::default().parse(&damaged).is_err());
        }
    }
    #[test]
    fn synthetic_legacy_voltage_and_days_are_not_accepted() {
        for frame in
            [&[0, 8, 0x88, 0x21, 0, 0x68, 0x10, 0, 0][..], &[0, 8, 0x80, 2, 30, 0, 0, 0][..]]
        {
            assert!(DiagnosticDataBuilder::default().parse(frame).is_err());
        }
    }
    #[test]
    fn valid_but_partial_telemetry_record_is_rejected() {
        let mut frame = vec![0, 0xc0, 0x90, 3, 1, 1, 0xff];
        let mut crc = 0_u16;
        for byte in &frame[2..] {
            crc ^= u16::from(*byte) << 8;
            for _ in 0..8 {
                crc = if crc & 0x8000 == 0 { crc << 1 } else { (crc << 1) ^ 0x755b };
            }
        }
        frame.extend(crc.to_le_bytes());
        assert!(DiagnosticDataBuilder::default().parse(&frame).is_err());
    }
}
