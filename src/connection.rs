//! Runtime-selectable IQOS transport.

use async_trait::async_trait;

use crate::{
    Result,
    protocol::{DeviceInfo, DeviceModel},
    transport::{Transport, TransportKind},
};

#[cfg(feature = "btleplug-support")]
use crate::IqosBle;
#[cfg(feature = "usb-support")]
use crate::{Error, IqosUsb};

/// Runtime transport used by applications that support both BLE and USB.
#[derive(Debug)]
pub enum IqosTransport {
    /// Bluetooth Low Energy session.
    #[cfg(feature = "btleplug-support")]
    Ble(IqosBle),
    /// USB HID session.
    #[cfg(feature = "usb-support")]
    Usb(IqosUsb),
}

impl IqosTransport {
    /// Return the detected device model.
    #[must_use]
    pub const fn model(&self) -> DeviceModel {
        match self {
            #[cfg(feature = "btleplug-support")]
            Self::Ble(transport) => transport.model(),
            #[cfg(feature = "usb-support")]
            Self::Usb(transport) => transport.model(),
        }
    }

    /// Return metadata collected while opening the transport.
    #[must_use]
    pub const fn device_info(&self) -> &DeviceInfo {
        match self {
            #[cfg(feature = "btleplug-support")]
            Self::Ble(transport) => transport.device_info(),
            #[cfg(feature = "usb-support")]
            Self::Usb(transport) => transport.device_info(),
        }
    }

    /// Return the BLE battery percentage when available.
    ///
    /// USB does not provide the direct GATT percentage characteristic.
    /// A model-specific voltage conversion has not been verified.
    ///
    /// # Errors
    ///
    /// Returns an error when the BLE read fails or when called on USB.
    #[cfg_attr(not(feature = "btleplug-support"), allow(clippy::unused_async))]
    pub async fn read_battery_level(&self) -> Result<u8> {
        match self {
            #[cfg(feature = "btleplug-support")]
            Self::Ble(transport) => transport.read_battery_level().await,
            #[cfg(feature = "usb-support")]
            Self::Usb(_) => {
                Err(Error::Unsupported("battery percentage is unavailable over USB".to_string()))
            }
        }
    }
}

#[cfg(feature = "btleplug-support")]
impl From<IqosBle> for IqosTransport {
    fn from(transport: IqosBle) -> Self {
        Self::Ble(transport)
    }
}

#[cfg(feature = "usb-support")]
impl From<IqosUsb> for IqosTransport {
    fn from(transport: IqosUsb) -> Self {
        Self::Usb(transport)
    }
}

#[async_trait]
impl Transport for IqosTransport {
    fn kind(&self) -> TransportKind {
        match self {
            #[cfg(feature = "btleplug-support")]
            Self::Ble(transport) => transport.kind(),
            #[cfg(feature = "usb-support")]
            Self::Usb(transport) => transport.kind(),
        }
    }

    async fn request(&self, command: &[u8]) -> Result<Vec<u8>> {
        match self {
            #[cfg(feature = "btleplug-support")]
            Self::Ble(transport) => transport.request(command).await,
            #[cfg(feature = "usb-support")]
            Self::Usb(transport) => transport.request(command).await,
        }
    }

    async fn send(&self, command: &[u8]) -> Result<()> {
        match self {
            #[cfg(feature = "btleplug-support")]
            Self::Ble(transport) => transport.send(command).await,
            #[cfg(feature = "usb-support")]
            Self::Usb(transport) => transport.send(command).await,
        }
    }
}
