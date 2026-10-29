//! Windows の WASAPI 排他モード出力。
//!
//! - 排他モードで開くので、Windows のミキサー（オーディオエンジン）を通らない
//! - 曲のサンプルレートで直接デバイスを開く（排他モードでは OS がリサンプリングしない）
//! - サンプル形式は、デバイスが受け付けるもののうち最もビット深度の大きいものを選ぶ
//! - USB DAC の標準ドライバはイベント駆動の排他モードで途切れることがあるため、ポーリングで回す
//!
//! f32 から整数への変換は 2^(bits-1) 倍するだけなので、24bit 以下の音源なら元の値に戻る。
//!
//! COM は呼び出し元スレッドの初期化方式（GUI のメインスレッドは STA のことが多い）とぶつからないよう、
//! WASAPI の処理はすべて自前のスレッドで MTA として行う。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use wasapi::{
    calculate_period_100ns, initialize_mta, AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode,
    WasapiError, WaveFormat,
};

use crate::backend::{DeviceInfo, OutputFormat};

const STANDARD_RATES: [u32; 10] = [
    44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000, 705600, 768000,
];

/// 試す形式（格納ビット数, 有効ビット数, 種類）。上から順に優先する。
const CANDIDATES: [(usize, usize, SampleType); 5] = [
    (32, 32, SampleType::Int),
    (32, 24, SampleType::Int),
    (24, 24, SampleType::Int),
    (32, 32, SampleType::Float),
    (16, 16, SampleType::Int),
];

const AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED: u32 = 0x8889_0019;
const AUDCLNT_E_DEVICE_IN_USE: u32 = 0x8889_000A;
const AUDCLNT_E_UNSUPPORTED_FORMAT: u32 = 0x8889_0008;
const AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED: u32 = 0x8889_000E;

/// MTA で初期化したスレッドで `f` を実行する。
fn with_mta<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    thread::spawn(move || {
        let _ = initialize_mta();
        f()
    })
    .join()
    .map_err(|_| anyhow!("WASAPI スレッドが異常終了しました"))?
}

fn hresult(e: &WasapiError) -> Option<u32> {
    match e {
        WasapiError::Windows(w) => Some(w.code().0 as u32),
        _ => None,
    }
}

fn describe(e: WasapiError) -> anyhow::Error {
    match hresult(&e) {
        Some(AUDCLNT_E_DEVICE_IN_USE) => anyhow!("デバイスを別のアプリが排他モードで使っています"),
        Some(AUDCLNT_E_UNSUPPORTED_FORMAT) => anyhow!("デバイスがこの形式に対応していません"),
        Some(AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED) => anyhow!(
            "排他モードが許可されていません（サウンド設定 → デバイスのプロパティ → 詳細 で「アプリケーションによりこのデバイスを排他的に制御できるようにする」を有効にしてください）"
        ),
        _ => anyhow!("WASAPI のエラー: {e}"),
    }
}

fn find_device(enumerator: &DeviceEnumerator, selector: Option<&str>) -> Result<wasapi::Device> {
    let Some(sel) = selector else {
        return enumerator.get_default_device(&Direction::Render).map_err(describe);
    };
    let sel_lower = sel.to_lowercase();
    let collection = enumerator.get_device_collection(&Direction::Render).map_err(describe)?;
    for device in &collection {
        let device = device.map_err(describe)?;
        let id = device.get_id().unwrap_or_default();
        let name = device.get_friendlyname().unwrap_or_default();
        if id == sel || name.to_lowercase().contains(&sel_lower) {
            return Ok(device);
        }
    }
    bail!("出力デバイス「{sel}」が見つかりません")
}

/// `rate` でデバイスが排他モードで受け付ける最良の形式を探す。
fn best_format(client: &AudioClient, rate: u32, channels: u16) -> Option<WaveFormat> {
    CANDIDATES.iter().find_map(|(store, valid, kind)| {
        let fmt = WaveFormat::new(*store, *valid, kind, rate as usize, channels as usize, None);
        client.is_supported_exclusive_with_quirks(&fmt).ok()
    })
}

pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    with_mta(|| {
        let enumerator = DeviceEnumerator::new().map_err(describe)?;
        let default_id = enumerator
            .get_default_device(&Direction::Render)
            .and_then(|d| d.get_id())
            .unwrap_or_default();
        let collection = enumerator.get_device_collection(&Direction::Render).map_err(describe)?;
        let mut out = Vec::new();
        for device in &collection {
            let Ok(device) = device else { continue };
            let id = device.get_id().unwrap_or_default();
            let mix = device.get_device_format().ok();
            let channels = mix.as_ref().map_or(2, |f| f.get_nchannels());
            let supported_rates = device
                .get_iaudioclient()
                .map(|client| {
                    STANDARD_RATES
                        .iter()
                        .copied()
                        .filter(|&r| best_format(&client, r, channels).is_some())
                        .collect()
                })
                .unwrap_or_default();
            out.push(DeviceInfo {
                is_default: id == default_id,
                name: device.get_friendlyname().unwrap_or_else(|_| id.clone()),
                id,
                channels,
                current_rate: mix.map_or(0, |f| f.get_samplespersec()),
                supported_rates,
                // 排他モードの使用中かどうかは開くまで分からない
                hogged_by_other: false,
            });
        }
        Ok(out)
    })
}

pub struct Device {
    id: String,
    pub name: String,
    channels: u16,
    format: Option<WaveFormat>,
}

impl Device {
    /// 名前に `selector` を含むデバイスを選ぶ。None なら既定の出力デバイス。
    /// 排他モードで掴むのは `start` のとき。
    pub fn open(selector: Option<&str>) -> Result<Self> {
        let selector = selector.map(str::to_owned);
        with_mta(move || {
            let enumerator = DeviceEnumerator::new().map_err(describe)?;
            let device = find_device(&enumerator, selector.as_deref())?;
            let id = device.get_id().map_err(describe)?;
            let name = device.get_friendlyname().unwrap_or_else(|_| id.clone());
            let channels = device.get_device_format().map_or(2, |f| f.get_nchannels());
            Ok(Self { id, name, channels, format: None })
        })
    }

    pub fn exclusive(&self) -> bool {
        true
    }

    /// `rate` で使う形式を決める。
    pub fn configure(&mut self, rate: u32) -> Result<OutputFormat> {
        let id = self.id.clone();
        let channels = self.channels;
        let name = self.name.clone();
        let format = with_mta(move || {
            let enumerator = DeviceEnumerator::new().map_err(describe)?;
            let client = enumerator.get_device(&id).map_err(describe)?.get_iaudioclient().map_err(describe)?;
            best_format(&client, rate, channels).ok_or_else(|| anyhow!("「{name}」は {rate} Hz に対応していません"))
        })?;
        let out = OutputFormat {
            sample_rate: rate,
            device_bits: format.get_validbitspersample() as u32,
            channels,
        };
        self.format = Some(format);
        Ok(out)
    }

    /// 排他モードで開いて再生を始める。返した `Stream` を drop すると止まり、デバイスを手放す。
    pub fn start(
        &self,
        consumer: rtrb::Consumer<f32>,
        src_channels: usize,
        played_frames: Arc<AtomicU64>,
        paused: Arc<AtomicBool>,
    ) -> Result<Stream> {
        let format = self.format.clone().ok_or_else(|| anyhow!("configure の前に start が呼ばれました"))?;
        let id = self.id.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = {
            let stop = stop.clone();
            thread::Builder::new().name("hifi-wasapi".into()).spawn(move || {
                let _ = initialize_mta();
                let mut ctx = RenderCtx {
                    consumer,
                    src_channels,
                    played_frames,
                    paused,
                    format,
                };
                if let Err(e) = ctx.run(&id, &stop, &ready_tx) {
                    // 開始前の失敗は start の戻り値で伝える。開始後の失敗はここで止まる
                    let _ = ready_tx.send(Err(e));
                }
            })?
        };
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Stream { stop, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                bail!("WASAPI スレッドが異常終了しました")
            }
        }
    }
}

pub struct Stream {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct RenderCtx {
    consumer: rtrb::Consumer<f32>,
    src_channels: usize,
    played_frames: Arc<AtomicU64>,
    paused: Arc<AtomicBool>,
    format: WaveFormat,
}

impl RenderCtx {
    fn run(&mut self, id: &str, stop: &AtomicBool, ready: &mpsc::Sender<Result<()>>) -> Result<()> {
        let enumerator = DeviceEnumerator::new().map_err(describe)?;
        let device = enumerator.get_device(id).map_err(describe)?;
        let mut client = device.get_iaudioclient().map_err(describe)?;
        let (default_period, _) = client.get_device_period().map_err(describe)?;
        // Intel HDA 系は 128 バイト単位の整列を要求する
        let mut period = client
            .calculate_aligned_period_near(default_period, Some(128), &self.format)
            .map_err(describe)?;
        let mode = |period: i64| StreamMode::PollingExclusive {
            period_hns: period,
            buffer_duration_hns: 16 * period,
        };
        if let Err(e) = client.initialize_client(&self.format, &Direction::Render, &mode(period)) {
            if hresult(&e) != Some(AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED) {
                return Err(describe(e));
            }
            // ドライバが示す整列済みのサイズで作り直す
            let frames = client.get_buffer_size().map_err(describe)?;
            period = calculate_period_100ns(frames as i64, self.format.get_samplespersec() as i64);
            client = device.get_iaudioclient().map_err(describe)?;
            client
                .initialize_client(&self.format, &Direction::Render, &mode(period))
                .map_err(describe)?;
        }
        let render = client.get_audiorenderclient().map_err(describe)?;
        let block_align = self.format.get_blockalign() as usize;
        let mut bytes: Vec<u8> = Vec::with_capacity(client.get_buffer_size().map_err(describe)? as usize * block_align);

        // 無音から始まらないよう、先にバッファを埋めてから開始する
        let frames = client.get_available_space_in_frames().map_err(describe)? as usize;
        self.fill(&mut bytes, frames);
        render.write_to_device(frames, &bytes, None).map_err(describe)?;
        client.start_stream().map_err(describe)?;
        let _ = ready.send(Ok(()));

        let sleep = Duration::from_nanos(period as u64 * 100);
        while !stop.load(Ordering::Relaxed) {
            thread::sleep(sleep);
            let frames = client.get_available_space_in_frames().map_err(describe)? as usize;
            if frames == 0 {
                continue;
            }
            self.fill(&mut bytes, frames);
            render.write_to_device(frames, &bytes, None).map_err(describe)?;
        }
        client.stop_stream().map_err(describe)?;
        Ok(())
    }

    /// `frames` フレーム分をデバイスの形式で `out` に詰める。足りない分は無音。
    fn fill(&mut self, out: &mut Vec<u8>, frames: usize) {
        let dev_ch = self.format.get_nchannels() as usize;
        let store_bytes = self.format.get_bitspersample() as usize / 8;
        let is_float = matches!(self.format.get_subformat(), Ok(SampleType::Float));
        out.clear();
        out.resize(frames * dev_ch * store_bytes, 0);
        if self.paused.load(Ordering::Relaxed) {
            return;
        }

        let src_ch = self.src_channels;
        let n = (frames * src_ch).min(self.consumer.slots()) / src_ch * src_ch;
        if n == 0 {
            return;
        }
        let Ok(chunk) = self.consumer.read_chunk(n) else {
            return;
        };
        let (a, b) = chunk.as_slices();
        for (i, &sample) in a.iter().chain(b.iter()).enumerate() {
            let frame = i / src_ch;
            let ch = i % src_ch;
            // モノラルは左右両方へ出す
            let targets: &[usize] = if src_ch == 1 { &[0, 1] } else { std::slice::from_ref(&ch) };
            for &dev in targets.iter().filter(|&&c| c < dev_ch) {
                let at = (frame * dev_ch + dev) * store_bytes;
                write_sample(&mut out[at..at + store_bytes], sample, is_float);
            }
        }
        chunk.commit_all();
        self.played_frames.fetch_add((n / src_ch) as u64, Ordering::Relaxed);
    }
}

/// リトルエンディアンで 1 サンプル書く。整数は 32bit にしてから上位バイトを使う。
#[inline]
fn write_sample(dst: &mut [u8], sample: f32, is_float: bool) {
    if is_float {
        dst.copy_from_slice(&sample.to_le_bytes());
        return;
    }
    let v = (sample as f64 * 2_147_483_648.0).round().clamp(i32::MIN as f64, i32::MAX as f64) as i32;
    let bytes = v.to_le_bytes();
    // 16bit なら上位 2 バイト、24bit なら上位 3 バイト
    dst.copy_from_slice(&bytes[4 - dst.len()..]);
}
