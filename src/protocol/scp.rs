//! Shared SCP frame validation and request/response matching.

const CRC8_POLYNOMIAL: u8 = 0x07;
const CRC16_OPENSAFETY_B_POLYNOMIAL: u16 = 0x755b;
const CRC16_OPCODE: u8 = 0x10;

/// Return whether an SCP frame uses the length-delimited CRC-16 format.
///
/// Ordinary SCP messages end in CRC-8. Data/telemetry messages use opcode
/// `0x10` (or response opcode `0x90`), carry an explicit payload length, and
/// end in little-endian CRC-16/OPENSAFETY-B.
pub(crate) fn uses_crc16(frame: &[u8]) -> bool {
    frame.get(2).is_some_and(|opcode| opcode & 0x7f == CRC16_OPCODE)
}

/// Return a checksum/length error, or `None` for a valid SCP frame.
pub(crate) fn checksum_error(frame: &[u8]) -> Option<String> {
    if uses_crc16(frame) {
        return crc16_error(frame);
    }

    if frame.len() < 5 {
        return Some("SCP frame is too short for CRC-8".to_string());
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
    if uses_crc16(command) {
        return uses_crc16(response)
            && command
                .get(2)
                .zip(response.get(2))
                .is_some_and(|(request, reply)| *reply == (*request | 0x80));
    }

    let header = |frame: &[u8]| {
        (frame.len() >= 4).then(|| u16::from_be_bytes([frame[2], frame[3]]) & 0x03ff)
    };
    header(command).is_some_and(|request| header(response) == Some(request))
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

fn crc16_opensafety_b(bytes: &[u8]) -> u16 {
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
}
