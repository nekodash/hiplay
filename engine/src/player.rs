//! 再生キューを回すエンジン。デコードは専用スレッドで行い、リングバッファ経由で IOProc に渡す。
//!
//! 同じサンプルレート・チャンネル数の曲が続く間はストリームを止めずに書き足すので、曲間は途切れない（ギャップレス）。
//! フォーマットが変わるときだけバッファを鳴らし切ってからデバイスを切り替える。
//!
//! 再生中にデバイスが切断されたら、その曲と位置を覚えて一時停止する。
//! 次にコマンドが来たら同じデバイスを開き直し、覚えた位置から再開する。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::backend::{Device, OutputFormat, Stream};
use crate::decoder::{Decoder, SourceFormat};

/// リングバッファの長さ（秒）
const BUFFER_SECS: usize = 2;

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    TogglePause,
    /// キューの末尾に曲を足す（再生は止めない）
    Append(Vec<PathBuf>),
    Next,
    Prev,
    Seek(f64),
    Stop,
}

#[derive(Debug, Clone)]
pub struct TrackStatus {
    pub index: usize,
    pub path: PathBuf,
    pub source: SourceFormat,
    pub codec: String,
    pub duration_secs: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub track: Option<TrackStatus>,
    pub position_secs: f64,
    pub paused: bool,
    pub output: Option<OutputFormat>,
    pub exclusive: bool,
    /// 出力デバイスが切断され、再接続を待っている
    pub device_lost: bool,
    pub finished: bool,
    pub last_error: Option<String>,
}

/// ストリーム内で、どのフレームからどの曲が始まるか
struct Segment {
    start_frame: u64,
    offset_frame: u64,
    track: TrackStatus,
}

#[derive(Default)]
struct Shared {
    paused: Arc<AtomicBool>,
    played_frames: Arc<AtomicU64>,
    segments: Mutex<Vec<Segment>>,
    output: Mutex<Option<OutputFormat>>,
    exclusive: AtomicBool,
    device_lost: AtomicBool,
    finished: AtomicBool,
    last_error: Mutex<Option<String>>,
}

impl Shared {
    fn status(&self) -> Status {
        let played = self.played_frames.load(Ordering::Relaxed);
        let output = *self.output.lock().unwrap();
        let segments = self.segments.lock().unwrap();
        let current = segments.iter().rev().find(|s| s.start_frame <= played);
        let rate = current.map_or(0, |s| s.track.source.sample_rate).max(1) as f64;
        Status {
            track: current.map(|s| s.track.clone()),
            position_secs: current.map_or(0.0, |s| (played - s.start_frame + s.offset_frame) as f64 / rate),
            paused: self.paused.load(Ordering::Relaxed),
            output,
            exclusive: self.exclusive.load(Ordering::Relaxed),
            device_lost: self.device_lost.load(Ordering::Relaxed),
            finished: self.finished.load(Ordering::Relaxed),
            last_error: self.last_error.lock().unwrap().clone(),
        }
    }

    fn error(&self, msg: String) {
        *self.last_error.lock().unwrap() = Some(msg);
    }
}

pub struct Player {
    tx: Sender<Command>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    pub device_name: String,
}

impl Player {
    /// `device` は出力デバイス名の一部（None なら既定の出力）。
    /// キューの `start` 番目の曲の `start_at` 秒から再生を始める。
    pub fn new(device: Option<&str>, queue: Vec<PathBuf>, start: usize, start_at: Option<f64>) -> Result<Self> {
        let device = Device::open(device)?;
        let device_name = device.name.clone();
        let shared = Arc::new(Shared::default());
        shared.exclusive.store(device.exclusive(), Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        let thread = {
            let shared = shared.clone();
            thread::Builder::new()
                .name("hifi-decoder".into())
                .spawn(move || Engine::new(device, queue, rx, shared).run(start, start_at))?
        };
        Ok(Self {
            tx,
            shared,
            thread: Some(thread),
            device_name,
        })
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }

    pub fn status(&self) -> Status {
        self.shared.status()
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// デコードを中断する理由
enum Interrupt {
    Cmd(Command),
    DeviceLost(String),
}

struct Active {
    stream: Stream,
    producer: rtrb::Producer<f32>,
    capacity: usize,
    written_frames: u64,
    rate: u32,
    channels: usize,
}

struct Engine {
    /// 切断中は None
    device: Option<Device>,
    device_key: String,
    queue: Vec<PathBuf>,
    rx: Receiver<Command>,
    shared: Arc<Shared>,
    configured_rate: Option<u32>,
}

impl Engine {
    fn new(device: Device, queue: Vec<PathBuf>, rx: Receiver<Command>, shared: Arc<Shared>) -> Self {
        Self {
            device_key: device.key(),
            device: Some(device),
            queue,
            rx,
            shared,
            configured_rate: None,
        }
    }

    fn run(mut self, start: usize, start_at: Option<f64>) {
        let mut out: Option<Active> = None;
        let mut idx = start;
        let mut seek_to = start_at;

        while idx < self.queue.len() {
            let path = self.queue[idx].clone();
            let mut dec = match Decoder::open(&path) {
                Ok(d) => d,
                Err(e) => {
                    self.shared.error(format!("{e:#}"));
                    idx += 1;
                    continue;
                }
            };
            let mut offset = 0;
            if let Some(secs) = seek_to.take() {
                match dec.seek(secs) {
                    Ok(f) => offset = f,
                    Err(e) => self.shared.error(format!("シークに失敗しました: {e:#}")),
                }
            }

            let src = dec.source;
            let same_format = out
                .as_ref()
                .is_some_and(|a| a.rate == src.sample_rate && a.channels == src.channels as usize);
            if !same_format {
                if let Some(a) = out.as_mut() {
                    if let Some(intr) = self.drain(a) {
                        out = None;
                        if !self.handle(intr, idx, &mut idx, &mut seek_to) {
                            return self.finish();
                        }
                        continue;
                    }
                }
                out = None;
                match self.start_stream(src) {
                    Ok(a) => out = Some(a),
                    Err(e) => {
                        self.shared.error(format!("{e:#}"));
                        idx += 1;
                        continue;
                    }
                }
            }

            let a = out.as_mut().unwrap();
            self.shared.segments.lock().unwrap().push(Segment {
                start_frame: a.written_frames,
                offset_frame: offset,
                track: TrackStatus {
                    index: idx,
                    path,
                    source: src,
                    codec: dec.codec.clone(),
                    duration_secs: dec.total_frames.map(|f| f as f64 / src.sample_rate as f64),
                },
            });

            let interrupted = loop {
                if let Some(cmd) = self.poll() {
                    break Some(Interrupt::Cmd(cmd));
                }
                match dec.next_block() {
                    Ok(Some(samples)) => {
                        if let Some(intr) = self.push(a, samples) {
                            break Some(intr);
                        }
                    }
                    Ok(None) => break None,
                    Err(e) => {
                        self.shared.error(format!("デコードに失敗しました: {e:#}"));
                        break None;
                    }
                }
            };

            match interrupted {
                None => idx += 1,
                Some(intr) => {
                    // バッファに残った分を捨てる
                    out = None;
                    if !self.handle(intr, idx, &mut idx, &mut seek_to) {
                        return self.finish();
                    }
                }
            }
        }

        if let Some(a) = out.as_mut() {
            self.drain(a);
        }
        self.finish();
    }

    fn finish(&self) {
        self.shared.finished.store(true, Ordering::Relaxed);
    }

    fn start_stream(&mut self, src: SourceFormat) -> Result<Active> {
        let device = self.device.as_mut().ok_or_else(|| anyhow!("出力デバイスが開かれていません"))?;
        if self.configured_rate != Some(src.sample_rate) {
            let fmt = device.configure(src.sample_rate)?;
            *self.shared.output.lock().unwrap() = Some(fmt);
            self.configured_rate = Some(src.sample_rate);
        }
        let channels = src.channels as usize;
        let capacity = src.sample_rate as usize * channels * BUFFER_SECS;
        let (producer, consumer) = rtrb::RingBuffer::new(capacity);
        self.shared.played_frames.store(0, Ordering::Relaxed);
        self.shared.segments.lock().unwrap().clear();
        let stream = device.start(
            consumer,
            channels,
            self.shared.played_frames.clone(),
            self.shared.paused.clone(),
        )?;
        Ok(Active {
            stream,
            producer,
            capacity,
            written_frames: 0,
            rate: src.sample_rate,
            channels,
        })
    }

    /// 一時停止とキューへの追加はここで処理し、それ以外のコマンドを返す。
    fn poll(&mut self) -> Option<Command> {
        loop {
            match self.rx.try_recv() {
                Ok(Command::TogglePause) => {
                    self.shared.paused.fetch_xor(true, Ordering::Relaxed);
                }
                Ok(Command::Append(paths)) => self.queue.extend(paths),
                Ok(cmd) => return Some(cmd),
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => return Some(Command::Stop),
            }
        }
    }

    fn push(&mut self, a: &mut Active, samples: &[f32]) -> Option<Interrupt> {
        let mut off = 0;
        while off < samples.len() {
            let n = a.producer.slots().min(samples.len() - off);
            if n == 0 {
                if let Some(intr) = self.check(a) {
                    return Some(intr);
                }
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            let chunk = a.producer.write_chunk_uninit(n).unwrap();
            chunk.fill_from_iter(samples[off..off + n].iter().copied());
            off += n;
        }
        a.written_frames += (samples.len() / a.channels) as u64;
        None
    }

    /// バッファを鳴らし切るまで待つ。途中でコマンドが来たらそれを返す。
    fn drain(&mut self, a: &mut Active) -> Option<Interrupt> {
        while a.producer.slots() < a.capacity {
            if let Some(intr) = self.check(a) {
                return Some(intr);
            }
            thread::sleep(Duration::from_millis(5));
        }
        // デバイス側のバッファに残っている分
        thread::sleep(Duration::from_millis(100));
        None
    }

    /// バッファの空き待ちの間に、コマンドとデバイスの状態を確かめる。
    /// デコードはバッファより速く進むので、再生中はほぼ常にここを通る。
    fn check(&mut self, a: &Active) -> Option<Interrupt> {
        if let Some(cmd) = self.poll() {
            return Some(Interrupt::Cmd(cmd));
        }
        a.stream.error().map(Interrupt::DeviceLost)
    }

    /// 中断の理由に応じて次に再生する位置を決める。停止なら false。
    fn handle(&mut self, intr: Interrupt, decoding: usize, idx: &mut usize, seek_to: &mut Option<f64>) -> bool {
        match intr {
            Interrupt::Cmd(cmd) => self.apply(cmd, decoding, idx, seek_to),
            Interrupt::DeviceLost(reason) => {
                let st = self.shared.status();
                *idx = st.track.as_ref().map_or(decoding, |t| t.index);
                *seek_to = st.track.is_some().then_some(st.position_secs);
                self.wait_reconnect(reason, idx, seek_to)
            }
        }
    }

    /// 切断後、コマンドが来るたびに同じデバイスを開き直す。開けたら true、停止なら false。
    fn wait_reconnect(&mut self, reason: String, idx: &mut usize, seek_to: &mut Option<f64>) -> bool {
        // 古いデバイスの専有を先に解く（同じデバイスが戻ってきたときに新しい側を解放しないように）
        self.device = None;
        self.configured_rate = None;
        self.shared.paused.store(true, Ordering::Relaxed);
        self.shared.device_lost.store(true, Ordering::Relaxed);
        self.shared.error(reason);
        loop {
            match self.rx.recv() {
                Err(_) | Ok(Command::Stop) => return false,
                Ok(Command::Next) => {
                    *idx += 1;
                    *seek_to = None;
                }
                Ok(Command::Prev) => {
                    *idx = idx.saturating_sub(1);
                    *seek_to = None;
                }
                Ok(Command::Seek(secs)) => *seek_to = Some(secs),
                Ok(Command::TogglePause) => {}
                Ok(Command::Append(paths)) => {
                    self.queue.extend(paths);
                    continue;
                }
            }
            // 既定のデバイスへは切り替えない（DAC を抜いたらスピーカーから鳴る、を避ける）
            match Device::open(Some(&self.device_key)) {
                Ok(device) => {
                    self.shared.exclusive.store(device.exclusive(), Ordering::Relaxed);
                    self.device = Some(device);
                    self.shared.device_lost.store(false, Ordering::Relaxed);
                    self.shared.paused.store(false, Ordering::Relaxed);
                    *self.shared.last_error.lock().unwrap() = None;
                    return true;
                }
                Err(e) => self.shared.error(format!("再接続できません: {e:#}")),
            }
        }
    }

    /// コマンドに従って次に再生する位置を決める。停止なら false。
    fn apply(&self, cmd: Command, decoding: usize, idx: &mut usize, seek_to: &mut Option<f64>) -> bool {
        let st = self.shared.status();
        // 鳴っている曲を基準にする（先読み中の曲ではなく）
        let current = st.track.map_or(decoding, |t| t.index);
        match cmd {
            Command::Next => *idx = current + 1,
            Command::Prev => {
                *idx = if st.position_secs > 3.0 { current } else { current.saturating_sub(1) }
            }
            Command::Seek(secs) => {
                *idx = current;
                *seek_to = Some(secs);
            }
            Command::Stop => return false,
            Command::TogglePause | Command::Append(_) => {}
        }
        true
    }
}
