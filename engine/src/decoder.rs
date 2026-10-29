//! Symphonia によるファイルのデコード。出力は f32 のインターリーブ。
//!
//! 24bit 以下の整数 PCM は f32 の仮数部（24bit）に収まるため、
//! f32 に変換しても値は失われない。

use std::fs::File;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    "flac", "alac", "m4a", "mp4", "wav", "aif", "aiff", "aifc", "mp3", "aac",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceFormat {
    pub sample_rate: u32,
    pub channels: u16,
    /// 音源のビット深度。非可逆圧縮や不明なときは None。
    pub bits_per_sample: Option<u32>,
    pub lossless: bool,
}

pub struct Decoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
    track_id: u32,
    pub source: SourceFormat,
    pub codec: String,
    pub total_frames: Option<u64>,
    sample_buf: Option<SampleBuffer<f32>>,
    /// `sample_buf` にまだ返していないブロックがある
    pending: bool,
    /// シーク直後に捨てるフレーム数（パケット境界と目標位置の差）。
    skip_frames: u64,
}

impl Decoder {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("{} を開けません", path.display()))?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let format_opts = FormatOptions {
            enable_gapless: true,
            ..Default::default()
        };
        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &format_opts, &MetadataOptions::default())
            .with_context(|| format!("{} の形式を判別できません", path.display()))?;
        let format = probed.format;

        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or_else(|| anyhow!("音声トラックがありません"))?;
        let params = track.codec_params.clone();
        let codec = symphonia::default::get_codecs()
            .get_codec(params.codec)
            .map(|d| d.short_name.to_string())
            .unwrap_or_else(|| "unknown".into());
        let lossless = matches!(codec.as_str(), "flac" | "alac") || codec.starts_with("pcm");
        let track_id = track.id;
        let decoder = symphonia::default::get_codecs()
            .make(&params, &DecoderOptions::default())
            .context("デコーダを作れません")?;

        let mut this = Self {
            format,
            decoder,
            track_id,
            source: SourceFormat {
                sample_rate: params.sample_rate.unwrap_or(0),
                channels: params.channels.map_or(0, |c| c.count() as u16),
                bits_per_sample: None,
                lossless,
            },
            codec,
            total_frames: params.n_frames,
            sample_buf: None,
            pending: false,
            skip_frames: 0,
        };
        // MP4 内の ALAC/AAC などは、デコードするまでチャンネル数が分からないことがある
        if this.source.sample_rate == 0 || this.source.channels == 0 {
            if !this.decode_packet()? {
                bail!("音声データがありません");
            }
            this.pending = true;
        }
        if lossless {
            this.source.bits_per_sample = this.decoder.codec_params().bits_per_sample.or(params.bits_per_sample);
        }
        Ok(this)
    }

    /// 指定秒へシークし、実際の位置（フレーム）を返す。
    pub fn seek(&mut self, secs: f64) -> Result<u64> {
        let seeked = self.format.seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time: Time::from(secs.max(0.0)),
                track_id: Some(self.track_id),
            },
        )?;
        self.decoder.reset();
        self.pending = false;
        self.skip_frames = seeked.required_ts.saturating_sub(seeked.actual_ts);
        Ok(seeked.required_ts)
    }

    /// 次のブロックをデコードする。終端なら None。
    pub fn next_block(&mut self) -> Result<Option<&[f32]>> {
        loop {
            if !std::mem::take(&mut self.pending) && !self.decode_packet()? {
                return Ok(None);
            }
            let buf = self.sample_buf.as_ref().unwrap();
            let ch = self.source.channels as usize;
            let frames = buf.samples().len() / ch;
            let skip = (self.skip_frames as usize).min(frames);
            self.skip_frames -= skip as u64;
            if skip == frames {
                continue;
            }
            return Ok(Some(&self.sample_buf.as_ref().unwrap().samples()[skip * ch..]));
        }
    }

    /// 1 パケットをデコードして `sample_buf` に入れる。終端なら false。
    fn decode_packet(&mut self) -> Result<bool> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(false)
                }
                Err(SymError::ResetRequired) => return Ok(false),
                Err(e) => return Err(e.into()),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(d) => d,
                // 壊れたパケットは飛ばして続ける
                Err(SymError::DecodeError(_)) => continue,
                Err(e) => return Err(e.into()),
            };
            let spec = *decoded.spec();
            let capacity = decoded.capacity() as u64;
            let needs_new = self
                .sample_buf
                .as_ref()
                .is_none_or(|b| (b.capacity() as u64) < capacity * spec.channels.count() as u64);
            if needs_new {
                self.sample_buf = Some(SampleBuffer::new(capacity, spec));
            }
            self.sample_buf.as_mut().unwrap().copy_interleaved_ref(decoded);
            self.source.sample_rate = spec.rate;
            self.source.channels = spec.channels.count() as u16;
            return Ok(true);
        }
    }
}
