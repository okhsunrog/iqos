//! P1_41M holder availability, cell readings and experimental use counter.
use super::{LOAD_TELEMETRY_COMMAND, client, identify_serial};
use crate::{Error, Result};

/// Charger status read (classic 0x85, hop 0).
pub const LOAD_CHARGER_STATUS_COMMAND: [u8; 5] = [0, 0xc0, 1, 0x21, 0xf2];
/// Holder cell read (classic 0x91, hop 1).
pub const LOAD_HOLDER_CELL_COMMAND: [u8; 5] = [0, 0xc9, 1, 0x24, 0xe9];

/// One refresh's holder data. Missing observations never reuse an earlier value.
#[derive(Debug, Clone, PartialEq)]
pub struct HolderSnapshot {
    /// Whether this serial family has a validated status layout.
    pub supported: bool,
    /// Whether routed reads should be attempted in this refresh.
    /// Untested families retain their pre-existing behavior.
    pub can_query_holder: bool,
    /// Raw holderConnection; 0 means available, 3 means unavailable, others unknown.
    pub connection_raw: Option<u8>,
    /// Raw holderState; 3 does not mean 100% charged.
    pub state_raw: Option<u8>,
    /// Relative charge percentage, corroborated by use/charging captures.
    pub charge_percent: Option<u8>,
    /// SDK health byte, not independently calibrated.
    pub health_raw: Option<u8>,
    /// Candidate LE16 remaining capacity; units are unconfirmed.
    pub capacity_raw: Option<u16>,
    /// Candidate use/session count; started versus completed uses remain unknown.
    pub experimental_use_count: Option<u32>,
}

fn supported(serial: &str) -> bool {
    identify_serial(serial).is_some_and(|i| i.native_type == 0x82)
}

/// Model-scoped preflight request; no speculative requests for other families.
#[must_use]
pub fn holder_status_command(serial: &str) -> Option<Vec<u8>> {
    supported(serial).then(|| LOAD_CHARGER_STATUS_COMMAND.to_vec())
}

fn invalid(message: &str) -> Error {
    Error::ProtocolDecode(message.into())
}

fn validate_read(frame: &[u8], command: &[u8], hop: u8) -> Result<()> {
    client::validate(frame, true)?;
    if !client::matches(command, frame) || frame[2] & 0x40 != 0 || (frame[1] >> 3) & 7 != hop {
        return Err(invalid("holder response identity, route or read flag mismatch"));
    }
    Ok(())
}

/// Decode a fresh preflight and optional routed responses using one shared codec.
///
/// # Errors
/// Rejects malformed, duplicate, wrongly routed or stale/unavailable-holder data.
pub fn holder_snapshot(
    serial: &str,
    status: Option<&[u8]>,
    frames: &[Vec<u8>],
) -> Result<HolderSnapshot> {
    let known = supported(serial);
    let mut out = HolderSnapshot {
        supported: known,
        can_query_holder: !known,
        connection_raw: None,
        state_raw: None,
        charge_percent: None,
        health_raw: None,
        capacity_raw: None,
        experimental_use_count: None,
    };
    if !known {
        if status.is_some() || !frames.is_empty() {
            return Err(invalid("holder layout not validated for this family"));
        }
        return Ok(out);
    }
    if let Some(status) = status {
        validate_read(status, &LOAD_CHARGER_STATUS_COMMAND, 0)?;
        if status.len() != 9 {
            return Err(invalid("unknown charger status layout"));
        }
        let connection = (status[5] >> 4) & 3;
        out.connection_raw = Some(connection);
        out.state_raw = Some(status[5] & 15);
        out.can_query_holder = connection == 0;
    }
    if !out.can_query_holder && !frames.is_empty() {
        return Err(invalid("routed data without an available holder preflight"));
    }
    let (mut seen_cell, mut seen_records) = (false, false);
    for frame in frames {
        if client::matches(&LOAD_HOLDER_CELL_COMMAND, frame) {
            validate_read(frame, &LOAD_HOLDER_CELL_COMMAND, 1)?;
            if seen_cell || frame.len() != 9 || frame[4] > 100 {
                return Err(invalid("duplicate or invalid holder cell layout"));
            }
            seen_cell = true;
            out.charge_percent = Some(frame[4]);
            out.health_raw = Some(frame[5]);
            out.capacity_raw = Some(u16::from_le_bytes([frame[6], frame[7]]));
        } else if client::matches(&LOAD_TELEMETRY_COMMAND, frame) {
            validate_read(frame, &LOAD_TELEMETRY_COMMAND, 1)?;
            if seen_records || frame.len() != 40 {
                return Err(invalid("duplicate or unknown use counter layout"));
            }
            seen_records = true;
            let records = &frame[6..38];
            // Only interpret the exact four-record family observed on hardware.
            if records.chunks_exact(8).enumerate().all(|(i, r)| {
                r[..4] == (i as u32).to_le_bytes() && r[7] == [0x8e, 0x20, 0x17, 0x18][i]
            }) {
                out.experimental_use_count =
                    Some(u32::from_le_bytes([records[4], records[5], records[6], 0]));
            }
        } else {
            return Err(invalid("unexpected holder snapshot response"));
        }
    }
    Ok(out)
}

/// Reads that are permitted after this model's fresh preflight.
///
/// # Errors
/// Returns an error if the preflight is malformed or belongs to another route.
pub fn holder_data_commands(serial: &str, status: Option<&[u8]>) -> Result<Vec<Vec<u8>>> {
    let snapshot = holder_snapshot(serial, status, &[])?;
    Ok(if snapshot.supported && snapshot.can_query_holder {
        vec![LOAD_HOLDER_CELL_COMMAND.to_vec(), LOAD_TELEMETRY_COMMAND.to_vec()]
    } else {
        Vec::new()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const SERIAL: &str = "0033006A000000000000";
    const IN: &[u8] = &[0, 0xc0, 0x85, 0x21, 0, 3, 0, 0, 0xda];
    const OUT: &[u8] = &[0, 0xc0, 0x85, 0x21, 0, 0x30, 0, 0, 0x86];
    const CELL: &[u8] = &[0, 8, 0x85, 0x24, 0x25, 0x64, 0x7a, 0, 0x44];

    #[test]
    fn captured_presence_roundtrip_gates_routed_reads() {
        for (status, available) in [(IN, true), (OUT, false), (IN, true)] {
            let snapshot = holder_snapshot(SERIAL, Some(status), &[]).unwrap();
            assert_eq!(snapshot.can_query_holder, available);
            assert_eq!(
                holder_data_commands(SERIAL, Some(status)).unwrap().len(),
                if available { 2 } else { 0 }
            );
        }
        assert!(holder_data_commands(SERIAL, None).unwrap().is_empty());
        assert!(holder_status_command("00330057000000000000").is_none());
        assert!(holder_data_commands("unknown", None).unwrap().is_empty());
        assert!(holder_snapshot("unknown", None, &[]).unwrap().can_query_holder);
    }

    #[test]
    fn captured_cell_has_no_invented_capacity_units() {
        let s = holder_snapshot(SERIAL, Some(IN), &[CELL.to_vec()]).unwrap();
        assert_eq!(s.charge_percent, Some(37));
        assert_eq!(s.health_raw, Some(100));
        assert_eq!(s.capacity_raw, Some(122));
        assert_eq!(s.experimental_use_count, None);
        assert!(holder_snapshot(SERIAL, Some(OUT), &[CELL.to_vec()]).is_err());
        assert!(holder_snapshot(SERIAL, Some(IN), &[CELL.to_vec(), CELL.to_vec()]).is_err());
    }

    #[test]
    fn rejects_corruption_errors_routes_and_unknown_status_shapes() {
        for index in 0..CELL.len() {
            if index == 1 {
                continue;
            } // Network parity isn't covered by the SCP checksum.
            let mut bytes = CELL.to_vec();
            bytes[index] ^= 1;
            assert!(holder_snapshot(SERIAL, Some(IN), &[bytes]).is_err());
        }
        let error = vec![0, 0xc0, 0xa5, 0x24, 2, 0, 0, 0, 0x9d];
        assert!(holder_snapshot(SERIAL, Some(IN), &[error]).is_err());
        let mut wrong_route = CELL.to_vec();
        wrong_route[1] = 0xc0;
        assert!(holder_snapshot(SERIAL, Some(IN), &[wrong_route]).is_err());
        for connection in [1, 2] {
            let mut status = IN.to_vec();
            status[5] = connection << 4;
            status[8] = super::super::scp::crc8(&status[2..8]);
            assert!(!holder_snapshot(SERIAL, Some(&status), &[]).unwrap().can_query_holder);
        }
    }

    #[test]
    fn captured_three_use_frame_decodes_experimental_full_width_counter() {
        let mut f = vec![
            0, 8, 0x90, 0x22, 1, 1, 0, 0, 0, 0, 0x5f, 0x0b, 0, 0x8e, 1, 0, 0, 0, 0x5c, 0x0a, 0,
            0x20, 2, 0, 0, 0, 0x15, 6, 0, 0x17, 3, 0, 0, 0, 0x29, 0, 0, 0x18, 0x53, 0xa0,
        ];
        assert_eq!(
            holder_snapshot(SERIAL, Some(IN), &[f.clone()]).unwrap().experimental_use_count,
            Some(2911)
        );
        f[12] = 1;
        let crc = super::super::scp::crc16_opensafety_b(&f[2..38]);
        f[38..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(
            holder_snapshot(SERIAL, Some(IN), &[f.clone()]).unwrap().experimental_use_count,
            Some(68447)
        );
        f[13] ^= 1;
        let crc = super::super::scp::crc16_opensafety_b(&f[2..38]);
        f[38..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(holder_snapshot(SERIAL, Some(IN), &[f]).unwrap().experimental_use_count, None);
    }
}
