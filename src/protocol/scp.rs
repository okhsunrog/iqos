//! Shared SCP frame validation and request/response matching.

const CRC8_POLYNOMIAL: u8 = 0x07;
const CRC16_OPENSAFETY_B_POLYNOMIAL: u16 = 0x755b;
const EXTENDED_FLAG: u8 = 0x10;

/// Return whether an SCP frame uses the length-delimited CRC-16 format.
///
/// Ordinary SCP messages end in CRC-8. Extended messages set header bit 4,
/// carry an explicit payload length, and
/// end in little-endian CRC-16/OPENSAFETY-B.
pub(crate) fn uses_crc16(frame: &[u8]) -> bool {
    frame.get(2).is_some_and(|header| header & EXTENDED_FLAG != 0)
}

/// Return a checksum/length error, or `None` for a valid SCP frame.
pub(crate) fn checksum_error(frame: &[u8]) -> Option<String> {
    if frame.first() != Some(&0) {
        return Some("SCP frame has not been reassembled".into());
    }
    if uses_crc16(frame) {
        return crc16_error(frame);
    }

    if frame.len() < 5 {
        return Some("SCP frame is too short for CRC-8".to_string());
    }

    let expected_len = 5 + [0, 4, 14, 24][usize::from((frame[2] >> 2) & 3)];
    if frame.len() != expected_len {
        return Some(format!(
            "invalid SCP classic frame length: expected {expected_len}, got {}",
            frame.len()
        ));
    }

    let expected = frame[frame.len() - 1];
    let actual = crc8(&frame[2..frame.len() - 1]);
    (actual != expected).then(|| {
        format!(
            "invalid SCP CRC-8: expected 0x{expected:02x}, calculated 0x{actual:02x}, frame {frame:02x?}"
        )
    })
}

fn crc16_error(frame: &[u8]) -> Option<String> {
    if frame.len() < 6 {
        return Some("SCP CRC-16 frame is too short".to_string());
    }

    let payload_len = usize::from(frame[3]);
    let checksum_offset = 4 + payload_len;
    let expected_len = checksum_offset + 2;
    if frame.len() != expected_len {
        return Some(format!(
            "invalid SCP CRC-16 frame length: header declares {payload_len} payload bytes, expected {expected_len} total bytes, got {}",
            frame.len()
        ));
    }

    let expected = u16::from_le_bytes([frame[checksum_offset], frame[checksum_offset + 1]]);
    let actual = crc16_opensafety_b(&frame[2..checksum_offset]);
    (actual != expected).then(|| {
        format!(
            "invalid SCP CRC-16: expected 0x{expected:04x}, calculated 0x{actual:04x}, frame {frame:02x?}"
        )
    })
}

/// Compare the command identity shared by an SCP request and response.
pub(crate) fn headers_match(command: &[u8], response: &[u8]) -> bool {
    if response.get(2).is_none_or(|header| header & 0x80 == 0)
        || uses_crc16(command) != uses_crc16(response)
    {
        return false;
    }
    if uses_crc16(command) {
        return command.len() >= 8
            && response.len() >= 8
            && command[2] & 0x1f == response[2] & 0x1f
            && command[4..6] == response[4..6];
    }

    let header = |frame: &[u8]| {
        (frame.len() >= 4).then(|| u16::from(frame[2] & 3) | (u16::from(frame[3]) << 2))
    };
    header(command).is_some_and(|request| header(response) == Some(request))
}

/// Validate a complete response before interpreting its payload.
pub(crate) fn response_error(frame: &[u8]) -> Option<String> {
    if let Some(error) = checksum_error(frame) {
        return Some(error);
    }
    if frame[2] & 0x80 == 0 {
        return Some("SCP frame is not a response".into());
    }
    if frame[2] & 0x20 != 0 {
        return Some(format!(
            "SCP device error: {:02x?}",
            &frame[4..frame.len() - if uses_crc16(frame) { 2 } else { 1 }]
        ));
    }
    None
}

/// Reassemble countdown-prefixed BLE notifications, bounded to the extended
/// one-byte length limit. A single atomic notification may exceed 20 bytes.
#[derive(Default)]
pub(crate) struct BleFrameAssembler {
    bytes: Vec<u8>,
    remaining: Option<u8>,
}

impl BleFrameAssembler {
    pub(crate) fn push(&mut self, chunk: &[u8]) -> std::result::Result<Option<Vec<u8>>, String> {
        let Some(&remaining) = chunk.first() else {
            return Err("empty BLE fragment".into());
        };
        if self.remaining.is_none() && remaining == 0 {
            if chunk.len() > 261 {
                return Err("oversized BLE frame".into());
            }
            return Ok(Some(chunk.to_vec()));
        }
        if chunk.len() < 2
            || chunk.len() > 20
            || (remaining > 0 && chunk.len() != 20)
            || self.remaining.is_some_and(|previous| previous.checked_sub(1) != Some(remaining))
        {
            return Err("invalid BLE fragment length or countdown".into());
        }
        if self.bytes.is_empty() {
            self.bytes.push(0);
        }
        self.bytes.extend_from_slice(&chunk[1..]);
        if self.bytes.len() > 261 {
            return Err("oversized BLE frame".into());
        }
        self.remaining = Some(remaining);
        if remaining == 0 {
            self.remaining = None;
            Ok(Some(std::mem::take(&mut self.bytes)))
        } else {
            Ok(None)
        }
    }
}

pub(crate) fn crc8(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0_u8, |mut crc, byte| {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 == 0 { crc << 1 } else { (crc << 1) ^ CRC8_POLYNOMIAL };
        }
        crc
    })
}

pub(crate) fn crc16_opensafety_b(bytes: &[u8]) -> u16 {
    bytes.iter().fold(0_u16, |mut crc, byte| {
        crc ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 == 0 {
                crc << 1
            } else {
                (crc << 1) ^ CRC16_OPENSAFETY_B_POLYNOMIAL
            };
        }
        crc
    })
}

#[cfg(test)]
mod tests {
    use super::{checksum_error, crc8, crc16_opensafety_b, headers_match, uses_crc16};

    const TELEMETRY_REQUEST: [u8; 8] = [0x00, 0xc9, 0x10, 0x02, 0x01, 0x01, 0x75, 0xd6];
    const TELEMETRY_RESPONSE: [u8; 40] = [
        0x00, 0x08, 0x90, 0x22, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x0d, 0x05, 0x00, 0x8e, 0x01,
        0x00, 0x00, 0x00, 0x6c, 0x04, 0x00, 0x20, 0x02, 0x00, 0x00, 0x00, 0x32, 0x04, 0x00, 0x17,
        0x03, 0x00, 0x00, 0x00, 0x18, 0x00, 0x00, 0x18, 0xb2, 0x28,
    ];
    const TIMESTAMP_REQUEST: [u8; 8] = [0x00, 0xc0, 0x10, 0x02, 0x00, 0x04, 0x38, 0xef];
    const TIMESTAMP_RESPONSE: [u8; 16] = [
        0x00, 0xc0, 0x90, 0x0a, 0x00, 0x04, 0xda, 0x07, 0x01, 0x01, 0x78, 0x53, 0x19, 0x1f, 0xa0,
        0x35,
    ];

    #[test]
    fn validates_known_crc8_frames() {
        assert_eq!(crc8(&[0x00, 0x03]), 0x09);
        assert_eq!(crc8(&[0x01, 0x00]), 0x15);
        assert!(checksum_error(&[0x00, 0xc0, 0x00, 0x21, 0xe7]).is_none());
    }

    #[test]
    fn validates_ble_captured_crc16_frames() {
        assert!(uses_crc16(&TELEMETRY_REQUEST));
        assert_eq!(crc16_opensafety_b(&TELEMETRY_REQUEST[2..6]), 0xd675);
        assert!(checksum_error(&TELEMETRY_REQUEST).is_none());
        assert_eq!(crc16_opensafety_b(&TELEMETRY_RESPONSE[2..38]), 0x28b2);
        assert!(checksum_error(&TELEMETRY_RESPONSE).is_none());
        assert_eq!(crc16_opensafety_b(&TIMESTAMP_REQUEST[2..6]), 0xef38);
        assert!(checksum_error(&TIMESTAMP_REQUEST).is_none());
        assert_eq!(crc16_opensafety_b(&TIMESTAMP_RESPONSE[2..14]), 0x35a0);
        assert!(checksum_error(&TIMESTAMP_RESPONSE).is_none());
    }

    #[test]
    fn matches_crc16_request_to_response_opcode() {
        assert!(headers_match(&TELEMETRY_REQUEST, &TELEMETRY_RESPONSE));
    }

    #[test]
    fn rejects_corrupted_crc16_frame() {
        let mut response = TELEMETRY_RESPONSE;
        response[10] ^= 0x01;

        assert!(checksum_error(&response).is_some());
    }

    #[test]
    fn unrelated_extended_reply_and_ack_do_not_match() {
        assert!(!headers_match(&TIMESTAMP_REQUEST, &TELEMETRY_RESPONSE));
        assert!(!headers_match(&TELEMETRY_REQUEST, &TIMESTAMP_RESPONSE));
        let mut ack = TELEMETRY_RESPONSE;
        ack[2] &= !0x80;
        assert!(!headers_match(&TELEMETRY_REQUEST, &ack));
    }

    #[test]
    fn assembles_countdown_and_rejects_out_of_order_fragments() {
        let mut assembler = super::BleFrameAssembler::default();
        let chunks = [
            [&[2][..], &TELEMETRY_RESPONSE[1..20]].concat(),
            [&[1][..], &TELEMETRY_RESPONSE[20..39]].concat(),
            vec![0, TELEMETRY_RESPONSE[39]],
        ];
        assert_eq!(assembler.push(&chunks[0]), Ok(None));
        assert_eq!(assembler.push(&chunks[1]), Ok(None));
        assert_eq!(assembler.push(&chunks[2]), Ok(Some(TELEMETRY_RESPONSE.to_vec())));
        assert_eq!(assembler.push(&TELEMETRY_RESPONSE), Ok(Some(TELEMETRY_RESPONSE.to_vec())));
        let mut assembler = super::BleFrameAssembler::default();
        assembler.push(&chunks[0]).unwrap();
        assert!(assembler.push(&chunks[2]).is_err());
    }

    #[test]
    fn rejects_classic_size_mismatch_even_with_valid_checksum() {
        let mut frame = vec![0, 0xc0, 0x88, 0x21, 0, 0, 0, 0];
        frame.push(crc8(&frame[2..]));
        assert!(checksum_error(&frame).is_some());
    }

    #[test]
    fn extended_flag_is_independent_of_response_and_error_flags() {
        assert!(uses_crc16(&[0, 0xc0, 0xb0]));
        assert!(uses_crc16(&[0, 0xc0, 0x50]));
    }
}
