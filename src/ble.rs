//! BLE-focused IQOS session and metadata types.
//!
//! This module contains the first extracted BLE-specific core logic from the
//! legacy `iqos_cli` implementation: device-model detection, device information
//! loading, characteristic discovery, and the low-level request/response path
//! built around the SCP control characteristic.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Mutex, time::timeout};

use async_trait::async_trait;
use btleplug::api::{Characteristic, Peripheral as _, WriteType};
use btleplug::platform::Peripheral;
use futures::StreamExt;

use crate::{
    Error, Result,
    protocol::{
        BATTERY_CHARACTERISTIC_UUID, DEVICE_INFO_SERVICE_UUID, DeviceInfo, DeviceModel,
        MANUFACTURER_NAME_CHAR_UUID_PREFIX, MODEL_NUMBER_CHAR_UUID_PREFIX,
        SCP_CONTROL_CHARACTERISTIC_UUID, SERIAL_NUMBER_CHAR_UUID_PREFIX,
        SOFTWARE_REVISION_CHAR_UUID_PREFIX, scp,
    },
    transport::{Transport, TransportKind},
};

/// BLE session around a connected IQOS peripheral.
#[derive(Debug, Clone)]
pub struct IqosBle {
    peripheral: Peripheral,
    battery_characteristic: Characteristic,
    scp_control_characteristic: Characteristic,
    model: DeviceModel,
    device_info: DeviceInfo,
    request_lock: Arc<Mutex<()>>,
    invalid: Arc<AtomicBool>,
}

impl IqosBle {
    /// Connect to a peripheral, discover services, subscribe to the control
    /// channel, and load basic device metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if connection, service discovery, subscription, or
    /// metadata loading fails.
    pub async fn connect_and_discover(peripheral: Peripheral) -> Result<Self> {
        Self::connect_with_model_hint(peripheral, None).await
    }

    /// Connect using a saved local name when the platform has no cached advertisement.
    /// The GATT setup sequence is identical to `connect_and_discover`.
    ///
    /// # Errors
    /// Returns an error if connection, discovery, subscription or metadata reading fails.
    pub async fn connect_and_discover_with_name(
        peripheral: Peripheral,
        local_name: &str,
    ) -> Result<Self> {
        Self::connect_with_model_hint(peripheral, Some(DeviceModel::from_local_name(local_name)))
            .await
    }

    async fn connect_with_model_hint(
        peripheral: Peripheral,
        hint: Option<DeviceModel>,
    ) -> Result<Self> {
        if !peripheral.is_connected().await.map_err(|error| Error::Transport(error.to_string()))? {
            peripheral.connect().await.map_err(|error| Error::Transport(error.to_string()))?;
        }

        peripheral
            .discover_services()
            .await
            .map_err(|error| Error::Transport(error.to_string()))?;

        let battery_characteristic = find_characteristic(&peripheral, BATTERY_CHARACTERISTIC_UUID)?;
        let scp_control_characteristic =
            find_characteristic(&peripheral, SCP_CONTROL_CHARACTERISTIC_UUID)?;
        let model = match peripheral
            .properties()
            .await
            .map_err(|error| Error::Transport(error.to_string()))?
        {
            Some(properties) => properties
                .local_name
                .as_deref()
                .map(DeviceModel::from_local_name)
                .filter(|model| *model != DeviceModel::Unknown)
                .or(hint)
                .unwrap_or(DeviceModel::Unknown),
            None => hint.ok_or_else(|| Error::Transport("missing BLE properties".to_string()))?,
        };
        let device_info = load_device_info(&peripheral).await?;

        peripheral
            .subscribe(&scp_control_characteristic)
            .await
            .map_err(|error| Error::Transport(error.to_string()))?;

        Ok(Self {
            peripheral,
            battery_characteristic,
            scp_control_characteristic,
            model,
            device_info,
            request_lock: Arc::new(Mutex::new(())),
            invalid: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Borrow the detected device model.
    #[must_use]
    pub const fn model(&self) -> DeviceModel {
        self.model
    }

    /// Borrow the loaded device information snapshot.
    #[must_use]
    pub const fn device_info(&self) -> &DeviceInfo {
        &self.device_info
    }

    /// Read the battery level from the vendor-specific IQOS GATT characteristic.
    ///
    /// Returns a value in the range 0–100 representing the charge percentage.
    /// Use this during initial connection setup, alongside serial number and
    /// firmware version, to obtain a battery snapshot without an SCP round-trip.
    ///
    /// This same read can be used for later percentage refreshes. It is not
    /// a cell-voltage measurement.
    ///
    /// # Errors
    ///
    /// Returns an error if the BLE read fails or the response frame is shorter
    /// than the three bytes required to extract the level field.
    pub async fn read_battery_level(&self) -> Result<u8> {
        let frame = self
            .peripheral
            .read(&self.battery_characteristic)
            .await
            .map_err(|error| Error::Transport(error.to_string()))?;

        crate::protocol::client::battery(&frame)
    }

    /// Send a command over the SCP control characteristic.
    ///
    /// # Errors
    ///
    /// Returns an error if the BLE write fails.
    pub async fn send(&self, command: &[u8]) -> Result<()> {
        self.request(command).await.map(|_| ())
    }

    async fn write_command(&self, command: &[u8]) -> Result<()> {
        for chunk in crate::protocol::client::fragments(command)? {
            self.peripheral
                .write(&self.scp_control_characteristic, &chunk, WriteType::WithResponse)
                .await
                .map_err(|error| Error::Transport(error.to_string()))?;
        }
        Ok(())
    }

    /// Send a command and wait for the next notification frame.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails, notifications cannot be opened, or
    /// no response frame arrives.
    pub async fn request(&self, command: &[u8]) -> Result<Vec<u8>> {
        if let Some(error) = scp::checksum_error(command) {
            return Err(Error::ProtocolEncode(error));
        }
        let _lock = self.request_lock.lock().await;
        if self.invalid.load(Ordering::Acquire) {
            return Err(Error::Transport("SCP session invalid; reconnect the device".into()));
        }
        let mut notifications = self
            .peripheral
            .notifications()
            .await
            .map_err(|error| Error::Transport(error.to_string()))?;
        // A dropped/timed-out transaction must not leave a late reply available
        // to another clone of this session.
        let mut transaction = TransactionGuard { invalid: &self.invalid, complete: false };
        let result = timeout(Duration::from_secs(5), async {
            self.write_command(command).await?;
            let mut assembler = scp::BleFrameAssembler::default();
            loop {
                let notification = notifications
                    .next()
                    .await
                    .ok_or_else(|| Error::Transport("BLE notification stream ended".into()))?;
                if notification.uuid != self.scp_control_characteristic.uuid {
                    continue;
                }
                let Some(frame) =
                    assembler.push(&notification.value).map_err(Error::ProtocolDecode)?
                else {
                    continue;
                };
                if !scp::headers_match(command, &frame) {
                    continue;
                }
                if let Some(error) = scp::response_error(&frame) {
                    return Err(Error::ProtocolDecode(error));
                }
                return Ok(frame);
            }
        })
        .await
        .map_err(|_| {
            Error::Transport("timed out waiting for BLE SCP response; reconnect".into())
        })?;
        transaction.complete = result.is_ok();
        result
    }
}

struct TransactionGuard<'a> {
    invalid: &'a AtomicBool,
    complete: bool,
}
impl Drop for TransactionGuard<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.invalid.store(true, Ordering::Release);
        }
    }
}

#[async_trait]
impl Transport for IqosBle {
    fn kind(&self) -> TransportKind {
        TransportKind::Ble
    }

    async fn request(&self, command: &[u8]) -> Result<Vec<u8>> {
        Self::request(self, command).await
    }

    async fn send(&self, command: &[u8]) -> Result<()> {
        Self::request(self, command).await.map(|_| ())
    }
}

async fn load_device_info(peripheral: &Peripheral) -> Result<DeviceInfo> {
    let service = peripheral
        .services()
        .into_iter()
        .find(|service| service.uuid == DEVICE_INFO_SERVICE_UUID)
        .ok_or_else(|| Error::Transport("device information service not found".to_string()))?;

    let mut info = DeviceInfo::default();

    for characteristic in &service.characteristics {
        let uuid_prefix = characteristic.uuid.to_string();
        let Some(prefix) = uuid_prefix.split('-').next() else {
            continue;
        };

        let value = peripheral
            .read(characteristic)
            .await
            .map_err(|error| Error::Transport(error.to_string()))?;
        let value = device_info_text(&value);

        match prefix {
            MODEL_NUMBER_CHAR_UUID_PREFIX => info.model_number = Some(value),
            SERIAL_NUMBER_CHAR_UUID_PREFIX => info.serial_number = Some(value),
            SOFTWARE_REVISION_CHAR_UUID_PREFIX => info.software_revision = Some(value),
            MANUFACTURER_NAME_CHAR_UUID_PREFIX => info.manufacturer_name = Some(value),
            _ => {}
        }
    }

    Ok(info)
}

fn device_info_text(value: &[u8]) -> String {
    String::from_utf8_lossy(value).trim_end_matches('\0').to_owned()
}

fn find_characteristic(peripheral: &Peripheral, target_uuid: uuid::Uuid) -> Result<Characteristic> {
    peripheral
        .services()
        .into_iter()
        .flat_map(|service| service.characteristics.into_iter())
        .find(|characteristic| characteristic.uuid == target_uuid)
        .ok_or_else(|| Error::Transport(format!("characteristic not found: {target_uuid}")))
}

#[cfg(test)]
mod tests {
    use crate::protocol::DeviceModel;

    #[test]
    fn nul_terminated_gatt_serial_enables_model_scoped_measurements() {
        let serial = super::device_info_text(b"0033006A000000000000\0");
        assert_eq!(crate::protocol::identify_serial(&serial).unwrap().native_type, 0x82);
        assert_eq!(crate::protocol::measurement_commands(&serial).len(), 4);
        // Embedded NULs still indicate malformed identity data.
        let invalid = super::device_info_text(b"0033006A\0000000000000\0");
        assert!(crate::protocol::identify_serial(&invalid).is_none());
    }

    #[test]
    fn classifies_iluma_i_prime_before_generic_matches() {
        assert_eq!(DeviceModel::from_local_name("IQOS ILUMA i PRIME"), DeviceModel::IlumaIPrime,);
    }

    #[test]
    fn classifies_iluma_i_one_before_generic_one_matches() {
        assert_eq!(DeviceModel::from_local_name("IQOS ILUMA i ONE"), DeviceModel::IlumaIOne,);
    }

    #[test]
    fn classifies_unknown_names() {
        assert_eq!(DeviceModel::from_local_name("mystery device"), DeviceModel::Unknown);
    }
}
