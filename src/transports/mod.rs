//! Backend transport implementations.
//!
//! This module contains transport-specific adapters.

#[cfg(feature = "btleplug-support")]
pub mod ble_btleplug;

#[cfg(feature = "usb-support")]
pub mod usb;
