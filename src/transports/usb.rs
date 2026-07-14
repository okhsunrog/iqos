//! IQOS USB HID transport implemented with `nusb`.
//!
//! IQOS uses the same SCP application frames over BLE and USB. USB wraps the
//! SCP bytes in 64-byte HID reports: report `0x3f` carries an atomic frame,
//! while report `0x40` carries one segment of a longer frame.

use core::fmt::{self, Write as _};
use std::time::Duration;

use async_trait::async_trait;
use nusb::io::{EndpointRead, EndpointWrite};
use nusb::{Interface, transfer::Interrupt};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Mutex,
    time::{Instant, timeout_at},
};

use crate::{
    Error, Result,
    protocol::{DeviceInfo, DeviceModel, scp},
    transport::{Transport, TransportKind},
};

/// Philip Morris Products USB vendor identifier.
pub const IQOS_USB_VENDOR_ID: u16 = 0x2759;
/// USB product identifier shared by supported IQOS pocket chargers.
pub const IQOS_USB_PRODUCT_ID: u16 = 0x0003;

const HID_INTERFACE_CLASS: u8 = 0x03;
const ENDPOINT_OUT: u8 = 0x01;
const ENDPOINT_IN: u8 = 0x81;
const REPORT_SIZE: usize = 64;
const ATOMIC_REPORT_ID: u8 = 0x3f;
const SEGMENTED_REPORT_ID: u8 = 0x40;
const ATOMIC_PAYLOAD_SIZE: usize = REPORT_SIZE - 2;
const SEGMENT_PAYLOAD_SIZE: usize = REPORT_SIZE - 3;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
const SCP_TRANSPORT_PREFIX: u8 = 0x00;
const SERIAL_NUMBER_COMMAND: [u8; 5] = [0x00, 0xc0, 0x01, 0x00, 0x15];

struct UsbIo {
    // Endpoints retain their own reference to the interface. Keeping this field
    // makes the claim lifetime explicit and lets nusb reattach usbhid on drop.
    _interface: Interface,
    reader: EndpointRead<Interrupt>,
    writer: EndpointWrite<Interrupt>,
}

/// Connected IQOS USB HID session.
pub struct IqosUsb {
    io: Mutex<UsbIo>,
    model: DeviceModel,
    device_info: DeviceInfo,
    location: String,
}

impl fmt::Debug for IqosUsb {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IqosUsb")
            .field("model", &self.model)
            .field("device_info", &self.device_info)
            .field("location", &self.location)
            .finish_non_exhaustive()
    }
}

impl IqosUsb {
    /// Find the first IQOS USB device, detach the Linux HID driver while the
    /// session is alive, and claim its interrupt interface.
    ///
    /// `nusb` automatically reattaches the detached kernel driver when the
    /// interface is dropped.
    ///
    /// # Errors
    ///
    /// Returns an error when no matching device is connected, the process lacks
    /// USB permissions, or the HID interface/endpoints cannot be opened.
    pub async fn connect() -> Result<Self> {
        let device_info = nusb::list_devices()
            .await
            .map_err(transport_error)?
            .find(|device| {
                device.vendor_id() == IQOS_USB_VENDOR_ID
                    && device.product_id() == IQOS_USB_PRODUCT_ID
            })
            .ok_or_else(|| {
                Error::Transport(format!(
                    "IQOS USB device {IQOS_USB_VENDOR_ID:04x}:{IQOS_USB_PRODUCT_ID:04x} not found"
                ))
            })?;

        let interface_number = device_info
            .interfaces()
            .find(|interface| interface.class() == HID_INTERFACE_CLASS)
            .map(nusb::InterfaceInfo::interface_number)
            .ok_or_else(|| Error::Transport("IQOS USB HID interface not found".to_string()))?;

        let product_name = device_info.product_string().map(str::to_owned);
        let model =
            product_name.as_deref().map_or(DeviceModel::Unknown, DeviceModel::from_local_name);
        let metadata = DeviceInfo {
            model_number: product_name,
            serial_number: device_info.serial_number().map(str::to_owned),
            software_revision: Some(format_bcd_version(device_info.device_version())),
            manufacturer_name: device_info.manufacturer_string().map(str::to_owned),
        };
        let location = format!("usb:{}:{}", device_info.bus_id(), device_info.device_address());

        let device = device_info.open().await.map_err(transport_error)?;
        let interface =
            device.detach_and_claim_interface(interface_number).await.map_err(|error| {
                Error::Transport(format!(
                    "failed to claim IQOS USB interface {interface_number}: {error}"
                ))
            })?;

        let reader = interface
            .endpoint::<Interrupt, _>(ENDPOINT_IN)
            .map_err(transport_error)?
            .reader(REPORT_SIZE)
            .with_num_transfers(1);
        let writer = interface
            .endpoint::<Interrupt, _>(ENDPOINT_OUT)
            .map_err(transport_error)?
            .writer(REPORT_SIZE)
            .with_num_transfers(1);

        let mut session = Self {
            io: Mutex::new(UsbIo { _interface: interface, reader, writer }),
            model,
            device_info: metadata,
            location,
        };

        if let Ok(response) = session.transact(&SERIAL_NUMBER_COMMAND).await {
            session.device_info.serial_number = parse_serial_number(&response);
        }

        Ok(session)
    }

    /// Return the model inferred from the USB product string.
    #[must_use]
    pub const fn model(&self) -> DeviceModel {
        self.model
    }

    /// Return metadata obtained from USB descriptors.
    #[must_use]
    pub const fn device_info(&self) -> &DeviceInfo {
        &self.device_info
    }

    /// Return a bus/address identifier for the connected USB device.
    #[must_use]
    pub fn location(&self) -> &str {
        &self.location
    }

    async fn transact(&self, command: &[u8]) -> Result<Vec<u8>> {
        let reports = encode_reports(command)?;
        let mut io = self.io.lock().await;
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        let UsbIo { reader, writer, .. } = &mut *io;

        // Poll IN first and keep a transfer armed while OUT is pending. IQOS
        // may NAK an OUT report until it has room to publish the response; the
        // operating-system HID driver also keeps an IN transfer active.
        let (response, write_result) = tokio::join!(
            biased;
            read_matching_response(reader, command, deadline),
            write_reports(writer, &reports, deadline),
        );
        write_result?;
        response
    }
}

async fn write_reports(
    writer: &mut EndpointWrite<Interrupt>,
    reports: &[[u8; REPORT_SIZE]],
    deadline: Instant,
) -> Result<()> {
    for report in reports {
        timeout_at(deadline, writer.write_all(report))
            .await
            .map_err(|_| Error::Transport("timed out writing IQOS USB report".to_string()))?
            .map_err(transport_error)?;
        timeout_at(deadline, writer.flush())
            .await
            .map_err(|_| Error::Transport("timed out flushing IQOS USB report".to_string()))?
            .map_err(transport_error)?;
    }
    Ok(())
}

async fn read_matching_response(
    reader: &mut EndpointRead<Interrupt>,
    command: &[u8],
    deadline: Instant,
) -> Result<Vec<u8>> {
    let mut segments = SegmentCollector::default();

    loop {
        let mut report = [0_u8; REPORT_SIZE];
        let bytes_read = timeout_at(deadline, reader.read(&mut report))
            .await
            .map_err(|_| Error::Transport("timed out waiting for IQOS USB response".to_string()))?
            .map_err(transport_error)?;

        if bytes_read == 0 {
            return Err(Error::Transport("IQOS USB endpoint returned zero bytes".to_string()));
        }

        let response = match report[0] {
            ATOMIC_REPORT_ID => decode_atomic_report(&report[..bytes_read])?,
            SEGMENTED_REPORT_ID => segments.push(&report[..bytes_read])?,
            _ => None,
        };

        let Some(response) = response else {
            continue;
        };

        if scp::headers_match(command, &response) {
            validate_crc(&response)?;
            return Ok(response);
        }
    }
}

#[async_trait]
impl Transport for IqosUsb {
    fn kind(&self) -> TransportKind {
        TransportKind::Usb
    }

    async fn request(&self, command: &[u8]) -> Result<Vec<u8>> {
        self.transact(command).await
    }

    async fn send(&self, command: &[u8]) -> Result<()> {
        // USB responses must be consumed even when the caller does not need
        // their payload; otherwise the next request could observe a stale frame.
        self.transact(command).await.map(|_| ())
    }
}

fn encode_reports(command: &[u8]) -> Result<Vec<[u8; REPORT_SIZE]>> {
    let Some((&prefix, scp_frame)) = command.split_first() else {
        return Err(Error::ProtocolEncode("empty SCP command".to_string()));
    };
    if prefix != SCP_TRANSPORT_PREFIX {
        return Err(Error::ProtocolEncode(format!(
            "unsupported SCP transport prefix 0x{prefix:02x}; USB currently accepts standard 0x00 frames"
        )));
    }
    if scp_frame.len() < 4 {
        return Err(Error::ProtocolEncode("SCP command is too short".to_string()));
    }
    if scp_frame.len() > u8::MAX as usize {
        return Err(Error::ProtocolEncode("SCP command exceeds USB framing limit".to_string()));
    }

    validate_outbound_crc(command)?;

    if scp_frame.len() <= ATOMIC_PAYLOAD_SIZE {
        let mut report = [0_u8; REPORT_SIZE];
        report[0] = ATOMIC_REPORT_ID;
        report[1] = u8::try_from(scp_frame.len())
            .map_err(|_| Error::ProtocolEncode("invalid USB atomic frame length".to_string()))?;
        report[2..2 + scp_frame.len()].copy_from_slice(scp_frame);
        return Ok(vec![report]);
    }

    let chunks = scp_frame.chunks(SEGMENT_PAYLOAD_SIZE).collect::<Vec<_>>();
    if chunks.len() > 128 {
        return Err(Error::ProtocolEncode("SCP command needs too many USB segments".to_string()));
    }
    let last_segment_id = u8::try_from(chunks.len() - 1)
        .map_err(|_| Error::ProtocolEncode("invalid USB segment count".to_string()))?;

    Ok(chunks
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| {
            let mut report = [0_u8; REPORT_SIZE];
            let remaining = last_segment_id - u8::try_from(index).expect("segment index fits u8");
            report[0] = SEGMENTED_REPORT_ID;
            report[1] = u8::try_from(chunk.len() + 1).expect("USB segment length always fits u8");
            report[2] = if index == 0 { remaining | 0x80 } else { remaining };
            report[3..3 + chunk.len()].copy_from_slice(chunk);
            report
        })
        .collect())
}

fn decode_atomic_report(report: &[u8]) -> Result<Option<Vec<u8>>> {
    if report.len() < 3 {
        return Err(Error::ProtocolDecode("USB HID report is too short".to_string()));
    }
    let frame_len = usize::from(report[1]);
    if frame_len == 1 && report[2] == 0 {
        return Ok(None);
    }
    if frame_len < 4 || frame_len + 2 > report.len() {
        return Err(Error::ProtocolDecode(format!("invalid USB atomic frame length {frame_len}")));
    }

    let mut response = Vec::with_capacity(frame_len + 1);
    response.push(SCP_TRANSPORT_PREFIX);
    response.extend_from_slice(&report[2..2 + frame_len]);
    Ok(Some(response))
}

#[derive(Default)]
struct SegmentCollector {
    data: Vec<u8>,
    next_id: Option<u8>,
}

impl SegmentCollector {
    fn push(&mut self, report: &[u8]) -> Result<Option<Vec<u8>>> {
        if report.len() < 4 {
            return Err(Error::ProtocolDecode("USB segmented report is too short".to_string()));
        }
        let frame_len = usize::from(report[1]);
        if frame_len < 2 || frame_len + 2 > report.len() {
            return Err(Error::ProtocolDecode(format!("invalid USB segment length {frame_len}")));
        }

        let raw_id = report[2];
        let is_initial = raw_id & 0x80 != 0;
        let segment_id = raw_id & 0x7f;

        if is_initial {
            self.data.clear();
            self.next_id = Some(segment_id);
        }

        if self.next_id != Some(segment_id) {
            return Err(Error::ProtocolDecode(format!(
                "unexpected USB segment id {segment_id}, expected {:?}",
                self.next_id
            )));
        }

        self.data.extend_from_slice(&report[3..frame_len + 2]);
        if segment_id == 0 {
            self.next_id = None;
            let mut response = Vec::with_capacity(self.data.len() + 1);
            response.push(SCP_TRANSPORT_PREFIX);
            response.append(&mut self.data);
            return Ok(Some(response));
        }

        self.next_id = Some(segment_id - 1);
        Ok(None)
    }
}

fn validate_crc(frame: &[u8]) -> Result<()> {
    scp::checksum_error(frame).map_or(Ok(()), |error| Err(Error::ProtocolDecode(error)))
}

fn validate_outbound_crc(frame: &[u8]) -> Result<()> {
    scp::checksum_error(frame).map_or(Ok(()), |error| Err(Error::ProtocolEncode(error)))
}

fn parse_serial_number(response: &[u8]) -> Option<String> {
    const RESPONSE_HEADER: [u8; 4] = [0x00, 0xc0, 0x89, 0x00];
    if response.len() < 15 || response[..RESPONSE_HEADER.len()] != RESPONSE_HEADER {
        return None;
    }

    let payload = &response[RESPONSE_HEADER.len()..response.len() - 1];
    let mut serial_bytes = Vec::<u8>::with_capacity(10);
    serial_bytes.extend(payload[0..2].iter().rev().copied());
    serial_bytes.extend(payload[2..4].iter().rev().copied());
    serial_bytes.extend(payload[4..6].iter().rev().copied());
    serial_bytes.extend(payload[6..10].iter().rev().copied());

    let mut serial = String::from("0x");
    for byte in serial_bytes {
        write!(&mut serial, "{byte:02x}").expect("writing to a String cannot fail");
    }

    Some(serial)
}

fn format_bcd_version(version: u16) -> String {
    format!("{:x}.{:02x}", version >> 8, version & 0xff)
}

fn transport_error(error: impl fmt::Display) -> Error {
    Error::Transport(error.to_string())
}

#[cfg(test)]
mod tests {
    use crate::{
        Error,
        protocol::{LOAD_TELEMETRY_COMMAND, scp},
    };

    use super::{
        ATOMIC_REPORT_ID, REPORT_SIZE, SEGMENTED_REPORT_ID, SegmentCollector, decode_atomic_report,
        encode_reports, parse_serial_number,
    };

    #[test]
    fn wraps_ble_scp_command_in_atomic_hid_report() {
        let reports = encode_reports(&[0x00, 0xc0, 0x00, 0x21, 0xe7]).unwrap();

        assert_eq!(reports.len(), 1);
        assert_eq!(&reports[0][..6], &[ATOMIC_REPORT_ID, 0x04, 0xc0, 0x00, 0x21, 0xe7]);
        assert!(reports[0][6..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn rejects_invalid_outbound_crc_as_encode_error() {
        let error = encode_reports(&[0x00, 0xc0, 0x00, 0x21, 0x00]).unwrap_err();

        assert!(
            matches!(error, Error::ProtocolEncode(message) if message.contains("invalid SCP CRC"))
        );
    }

    #[test]
    fn wraps_crc16_telemetry_command_without_rewriting_ble_bytes() {
        let reports = encode_reports(&LOAD_TELEMETRY_COMMAND).unwrap();

        assert_eq!(reports.len(), 1);
        assert_eq!(&reports[0][..9], &[0x3f, 0x07, 0xc9, 0x10, 0x02, 0x01, 0x01, 0x75, 0xd6]);
    }

    #[test]
    fn decodes_atomic_report_back_to_ble_shape() {
        let mut report = [0_u8; REPORT_SIZE];
        report[..10].copy_from_slice(&[
            ATOMIC_REPORT_ID,
            0x08,
            0xc0,
            0x88,
            0x21,
            0x00,
            0x00,
            0x10,
            0x68,
            0x00,
        ]);
        let crc = scp::crc8(&report[3..9]);
        report[9] = crc;

        let response = decode_atomic_report(&report).unwrap().unwrap();

        assert_eq!(response, [0x00, 0xc0, 0x88, 0x21, 0x00, 0x00, 0x10, 0x68, crc]);
    }

    #[test]
    fn decodes_usb_telemetry_to_the_same_frame_observed_over_ble() {
        let ble_frame = [
            0x00, 0x08, 0x90, 0x22, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x0d, 0x05, 0x00, 0x8e,
            0x01, 0x00, 0x00, 0x00, 0x6c, 0x04, 0x00, 0x20, 0x02, 0x00, 0x00, 0x00, 0x32, 0x04,
            0x00, 0x17, 0x03, 0x00, 0x00, 0x00, 0x18, 0x00, 0x00, 0x18, 0xb2, 0x28,
        ];
        let mut report = [0_u8; REPORT_SIZE];
        report[0] = ATOMIC_REPORT_ID;
        report[1] = u8::try_from(ble_frame.len() - 1).unwrap();
        report[2..2 + ble_frame.len() - 1].copy_from_slice(&ble_frame[1..]);

        let usb_frame = decode_atomic_report(&report).unwrap().unwrap();

        assert_eq!(usb_frame, ble_frame);
        assert!(scp::checksum_error(&usb_frame).is_none());
    }

    #[test]
    fn ignores_idle_report() {
        let mut report = [0_u8; REPORT_SIZE];
        report[..3].copy_from_slice(&[ATOMIC_REPORT_ID, 0x01, 0x00]);

        assert!(decode_atomic_report(&report).unwrap().is_none());
    }

    #[test]
    fn segments_and_reassembles_long_scp_frame() {
        let mut command = vec![0x00, 0xc0, 0x44, 0x02];
        command.extend(0_u8..=119);
        let checksum = scp::crc8(&command[2..]);
        command.push(checksum);

        let reports = encode_reports(&command).unwrap();
        assert_eq!(reports.len(), 3);
        assert!(reports.iter().all(|report| report[0] == SEGMENTED_REPORT_ID));
        assert_eq!(reports[0][2], 0x82);
        assert_eq!(reports[1][2], 0x01);
        assert_eq!(reports[2][2], 0x00);

        let mut collector = SegmentCollector::default();
        let mut response = None;
        for report in reports {
            response = collector.push(&report).unwrap().or(response);
        }
        assert_eq!(response.unwrap(), command);
    }

    #[test]
    fn compares_the_ten_bit_scp_command_header() {
        assert!(scp::headers_match(
            &[0x00, 0xc0, 0x02, 0x23, 0xc3],
            &[0x00, 0xc0, 0x86, 0x23, 0x64, 0x00, 0x00, 0x00, 0x00]
        ));
    }

    #[test]
    fn crc_matches_known_scp_commands() {
        assert_eq!(scp::crc8(&[0x00, 0x03]), 0x09);
        assert_eq!(scp::crc8(&[0x01, 0x00]), 0x15);
        assert_eq!(scp::crc8(&[0x00, 0x21]), 0xe7);
        assert_eq!(scp::crc8(&[0x02, 0x23]), 0xc3);
    }

    #[test]
    fn parses_usb_device_serial_response() {
        let response = [
            0x00, 0xc0, 0x89, 0x00, 0x33, 0x00, 0x6a, 0x00, 0x02, 0x00, 0x59, 0x02, 0x23, 0x00,
            0x05, 0x00, 0x00, 0x00, 0x5c,
        ];

        assert_eq!(parse_serial_number(&response).as_deref(), Some("0x0033006a000200230259"));
    }
}
