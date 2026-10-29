//! 再生キューを回すエンジン。デコードは専用スレッドで行い、リングバッファ経由で IOProc に渡す。
//!
//! 同じサンプルレート・チャンネル数の曲が続く間はストリームを止めずに書き足すので、曲間は途切れない（ギャップレス）。
//! フォーマットが変わるときだけバッファを鳴らし切ってからデバイスを切り替える。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Result;

use crate::backend::{Device, OutputFormat, Stream};
use crate::decoder::{Decoder, SourceFormat};

/// リングバッファの長さ（秒）
const BUFFER_SECS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    TogglePause,
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
    pub fn new(device: Option<&str>, queue: Vec<PathBuf>) -> Result<Self> {
        let device = Device::open(device)?;
        let device_name = device.name.clone();
        let shared = Arc::new(Shared::default());
        shared.exclusive.store(device.exclusive(), Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        let thread = {
            let shared = shared.clone();
            thread::Builder::new()
                .name("hifi-decoder".into())
                .spawn(move || Engine::new(device, queue, rx, shared).run())?
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

struct Active {
    _stream: Stream,
    producer: rtrb::Producer<f32>,
    capacity: usize,
    written_frames: u64,
    rate: u32,
    channels: usize,
}

struct Engine {
    device: Device,
    queue: Vec<PathBuf>,
    rx: Receiver<Command>,
    shared: Arc<Shared>,
    configured_rate: Option<u32>,
}

impl Engine {
    fn new(device: Device, queue: Vec<PathBuf>, rx: Receiver<Command>, shared: Arc<Shared>) -> Self {
        Self {
            device,
            queue,
            rx,
            shared,
            configured_rate: None,
        }
    }

    fn run(mut self) {
        let mut out: Option<Active> = None;
        let mut idx = 0;
        let mut seek_to: Option<f64> = None;

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
                    if let Some(cmd) = self.drain(a) {
                        out = None;
                        if !self.apply(cmd, idx, &mut idx, &mut seek_to) {
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
                    break Some(cmd);
                }
                match dec.next_block() {
                    Ok(Some(samples)) => {
                        if let Some(cmd) = self.push(a, samples) {
                            break Some(cmd);
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
                Some(cmd) => {
                    // バッファに残った分を捨てる
                    out = None;
                    if !self.apply(cmd, idx, &mut idx, &mut seek_to) {
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
        if self.configured_rate != Some(src.sample_rate) {
            let fmt = self.device.configure(src.sample_rate)?;
            *self.shared.output.lock().unwrap() = Some(fmt);
            self.configured_rate = Some(src.sample_rate);
        }
        let channels = src.channels as usize;
        let capacity = src.sample_rate as usize * channels * BUFFER_SECS;
        let (producer, consumer) = rtrb::RingBuffer::new(capacity);
        self.shared.played_frames.store(0, Ordering::Relaxed);
        self.shared.segments.lock().unwrap().clear();
        let stream = self.device.start(
            consumer,
            channels,
            self.shared.played_frames.clone(),
            self.shared.paused.clone(),
        )?;
        Ok(Active {
            _stream: stream,
            producer,
            capacity,
            written_frames: 0,
            rate: src.sample_rate,
            channels,
        })
    }

    /// 一時停止はここで処理し、それ以外のコマンドを返す。
    fn poll(&self) -> Option<Command> {
        loop {
            match self.rx.try_recv() {
                Ok(Command::TogglePause) => {
                    self.shared.paused.fetch_xor(true, Ordering::Relaxed);
                }
                Ok(cmd) => return Some(cmd),
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => return Some(Command::Stop),
            }
        }
    }

    fn push(&self, a: &mut Active, samples: &[f32]) -> Option<Command> {
        let mut off = 0;
        while off < samples.len() {
            let n = a.producer.slots().min(samples.len() - off);
            if n == 0 {
                if let Some(cmd) = self.poll() {
                    return Some(cmd);
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
    fn drain(&self, a: &mut Active) -> Option<Command> {
        while a.producer.slots() < a.capacity {
            if let Some(cmd) = self.poll() {
                return Some(cmd);
            }
            thread::sleep(Duration::from_millis(5));
        }
        // デバイス側のバッファに残っている分
        thread::sleep(Duration::from_millis(100));
        None
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
            Command::TogglePause => {}
        }
        true
    }
}
