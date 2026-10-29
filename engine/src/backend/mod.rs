//! OS ごとの出力。各 backend は同じ名前の `Device` / `Stream` / `list_devices` を公開する。

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub channels: u16,
    pub current_rate: u32,
    pub supported_rates: Vec<u32>,
    pub hogged_by_other: bool,
}

/// デバイスに実際に設定されたフォーマット。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputFormat {
    pub sample_rate: u32,
    /// DAC へ送られるビット深度（物理フォーマット）
    pub device_bits: u32,
    pub channels: u16,
}

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{list_devices, Device, Stream};

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::{list_devices, Device, Stream};
