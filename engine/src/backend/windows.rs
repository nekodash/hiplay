//! Windows（WASAPI 排他モード）出力。未実装。
//! macOS 版と同じ API で、`wasapi` クレートの排他モードを使って実装する予定。

use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;

use anyhow::{bail, Result};

use crate::backend::{DeviceInfo, OutputFormat};

pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    bail!("Windows の出力はまだ実装していません")
}

pub struct Device {
    pub name: String,
}

impl Device {
    pub fn open(_selector: Option<&str>) -> Result<Self> {
        bail!("Windows の出力はまだ実装していません")
    }

    pub fn exclusive(&self) -> bool {
        false
    }

    pub fn configure(&mut self, _rate: u32) -> Result<OutputFormat> {
        unreachable!()
    }

    pub fn start(
        &self,
        _consumer: rtrb::Consumer<f32>,
        _src_channels: usize,
        _played_frames: Arc<AtomicU64>,
        _paused: Arc<AtomicBool>,
    ) -> Result<Stream> {
        unreachable!()
    }
}

pub struct Stream;
