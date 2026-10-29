//! macOS の CoreAudio HAL を直接使う出力。
//!
//! - Hog Mode でデバイスを専有し、他アプリの音とミックスされないようにする
//! - 曲のサンプルレートに合わせてデバイスの公称レートを切り替える（OS のリサンプリングを避ける）
//! - 物理フォーマットはそのレートで最もビット深度の大きいものを選ぶ
//!
//! IOProc へは f32 で渡し、整数への変換は HAL に任せる。24bit 以下の音源ならこの変換で値は変わらない。

use std::ffi::c_void;
use std::mem::{size_of, MaybeUninit};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use coreaudio_sys::*;

use crate::backend::{DeviceInfo, OutputFormat};

const ELEMENT_MAIN: u32 = 0;
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

fn addr(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: ELEMENT_MAIN,
    }
}

fn check(status: OSStatus, what: &str) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        let code = status as u32;
        let fourcc: String = code
            .to_be_bytes()
            .iter()
            .map(|&b| if b.is_ascii_graphic() { b as char } else { '?' })
            .collect();
        Err(anyhow!("{what} に失敗しました (OSStatus {status} '{fourcc}')"))
    }
}

fn get<T: Copy>(obj: AudioObjectID, a: AudioObjectPropertyAddress) -> Result<T> {
    let mut value = MaybeUninit::<T>::uninit();
    let mut size = size_of::<T>() as u32;
    let st = unsafe {
        AudioObjectGetPropertyData(obj, &a, 0, ptr::null(), &mut size, value.as_mut_ptr() as *mut c_void)
    };
    check(st, "プロパティの取得")?;
    Ok(unsafe { value.assume_init() })
}

fn get_vec<T: Copy>(obj: AudioObjectID, a: AudioObjectPropertyAddress) -> Result<Vec<T>> {
    let mut size = 0u32;
    check(
        unsafe { AudioObjectGetPropertyDataSize(obj, &a, 0, ptr::null(), &mut size) },
        "プロパティサイズの取得",
    )?;
    let n = size as usize / size_of::<T>();
    let mut v: Vec<T> = Vec::with_capacity(n);
    check(
        unsafe { AudioObjectGetPropertyData(obj, &a, 0, ptr::null(), &mut size, v.as_mut_ptr() as *mut c_void) },
        "プロパティの取得",
    )?;
    unsafe { v.set_len(size as usize / size_of::<T>()) };
    Ok(v)
}

fn set<T>(obj: AudioObjectID, a: AudioObjectPropertyAddress, value: &T, what: &str) -> Result<()> {
    let st = unsafe {
        AudioObjectSetPropertyData(obj, &a, 0, ptr::null(), size_of::<T>() as u32, value as *const T as *const c_void)
    };
    check(st, what)
}

fn device_name(id: AudioObjectID) -> String {
    let Ok(cf) = get::<CFStringRef>(id, addr(kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal)) else {
        return format!("device {id}");
    };
    let mut buf = [0i8; 512];
    let ok = unsafe { CFStringGetCString(cf, buf.as_mut_ptr(), buf.len() as _, CF_STRING_ENCODING_UTF8) };
    unsafe { CFRelease(cf as *const c_void) };
    if ok == 0 {
        return format!("device {id}");
    }
    unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
}

fn output_streams(id: AudioObjectID) -> Result<Vec<AudioStreamID>> {
    get_vec(id, addr(kAudioDevicePropertyStreams, kAudioObjectPropertyScopeOutput))
}

fn output_channels(id: AudioObjectID) -> u32 {
    output_streams(id)
        .unwrap_or_default()
        .iter()
        .filter_map(|&s| {
            get::<AudioStreamBasicDescription>(s, addr(kAudioStreamPropertyVirtualFormat, kAudioObjectPropertyScopeGlobal)).ok()
        })
        .map(|f| f.mChannelsPerFrame)
        .sum()
}

fn nominal_rate(id: AudioObjectID) -> Result<f64> {
    get(id, addr(kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal))
}

fn available_rates(id: AudioObjectID) -> Vec<AudioValueRange> {
    get_vec(id, addr(kAudioDevicePropertyAvailableNominalSampleRates, kAudioObjectPropertyScopeGlobal)).unwrap_or_default()
}

fn hog_owner(id: AudioObjectID) -> i32 {
    get::<i32>(id, addr(kAudioDevicePropertyHogMode, kAudioObjectPropertyScopeGlobal)).unwrap_or(-1)
}

fn default_output() -> Result<AudioObjectID> {
    get(
        kAudioObjectSystemObject,
        addr(kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectPropertyScopeGlobal),
    )
}

pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    let ids: Vec<AudioObjectID> = get_vec(
        kAudioObjectSystemObject,
        addr(kAudioHardwarePropertyDevices, kAudioObjectPropertyScopeGlobal),
    )?;
    let default = default_output().unwrap_or(0);
    Ok(ids
        .into_iter()
        .filter(|&id| output_channels(id) > 0)
        .map(|id| {
            let mut rates: Vec<u32> = Vec::new();
            for r in available_rates(id) {
                // 範囲で返すデバイスもあるので、代表的なレートに展開する
                for std_rate in [44100.0, 48000.0, 88200.0, 96000.0, 176400.0, 192000.0, 352800.0, 384000.0, 705600.0, 768000.0] {
                    if std_rate >= r.mMinimum && std_rate <= r.mMaximum && !rates.contains(&(std_rate as u32)) {
                        rates.push(std_rate as u32);
                    }
                }
            }
            rates.sort_unstable();
            DeviceInfo {
                id: id.to_string(),
                name: device_name(id),
                is_default: id == default,
                channels: output_channels(id) as u16,
                current_rate: nominal_rate(id).unwrap_or(0.0) as u32,
                supported_rates: rates,
                hogged_by_other: {
                    let owner = hog_owner(id);
                    owner != -1 && owner != std::process::id() as i32
                },
            }
        })
        .collect())
}

pub struct Device {
    id: AudioObjectID,
    pub name: String,
    original_rate: f64,
    hogged: bool,
}

// AudioObjectID は単なる整数で、HAL の呼び出しはスレッドセーフ
unsafe impl Send for Device {}

impl Device {
    /// 名前に `selector` を含むデバイスを開く。None なら既定の出力デバイス。
    pub fn open(selector: Option<&str>) -> Result<Self> {
        let id = match selector {
            None => default_output()?,
            Some(sel) => {
                let sel_lower = sel.to_lowercase();
                let devices = list_devices()?;
                // 完全一致を優先し、なければ名前の一部で探す
                devices
                    .iter()
                    .find(|d| d.id == sel || d.name == sel)
                    .or_else(|| devices.iter().find(|d| d.name.to_lowercase().contains(&sel_lower)))
                    .cloned()
                    .ok_or_else(|| anyhow!("出力デバイス「{sel}」が見つかりません"))?
                    .id
                    .parse()?
            }
        };
        let name = device_name(id);
        let original_rate = nominal_rate(id)?;

        let owner = hog_owner(id);
        let pid = std::process::id() as i32;
        if owner != -1 && owner != pid {
            bail!("「{name}」は別のプロセス (pid {owner}) に専有されています");
        }
        set(id, addr(kAudioDevicePropertyHogMode, kAudioObjectPropertyScopeGlobal), &pid, "Hog Mode の取得")?;
        let hogged = hog_owner(id) == pid;

        Ok(Self { id, name, original_rate, hogged })
    }

    pub fn exclusive(&self) -> bool {
        self.hogged
    }

    /// 再接続のときに同じデバイスを探すためのキー。抜き差しで AudioObjectID は変わるので名前を使う。
    pub fn key(&self) -> String {
        self.name.clone()
    }

    /// デバイスを `rate` に切り替え、物理フォーマットを選ぶ。
    pub fn configure(&mut self, rate: u32) -> Result<OutputFormat> {
        let target = rate as f64;
        let supported = available_rates(self.id)
            .iter()
            .any(|r| target >= r.mMinimum && target <= r.mMaximum);
        if !supported {
            bail!("「{}」は {rate} Hz に対応していません", self.name);
        }

        if nominal_rate(self.id)? != target {
            set(
                self.id,
                addr(kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal),
                &target,
                "サンプルレートの変更",
            )?;
            // 切替は非同期なので反映を待つ
            let deadline = Instant::now() + Duration::from_secs(3);
            while nominal_rate(self.id)? != target {
                if Instant::now() > deadline {
                    bail!("サンプルレートが {rate} Hz に切り替わりません");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        let streams = output_streams(self.id)?;
        let mut bits = 0;
        for &stream in &streams {
            let formats: Vec<AudioStreamRangedDescription> = get_vec(
                stream,
                addr(kAudioStreamPropertyAvailablePhysicalFormats, kAudioObjectPropertyScopeGlobal),
            )?;
            let best = formats
                .iter()
                .filter(|f| {
                    f.mFormat.mFormatID == kAudioFormatLinearPCM
                        && f.mFormat.mFormatFlags & kAudioFormatFlagIsNonMixable == 0
                        && target >= f.mSampleRateRange.mMinimum
                        && target <= f.mSampleRateRange.mMaximum
                })
                // ビット深度が大きいものを優先し、同じなら整数フォーマットを選ぶ
                .max_by_key(|f| {
                    (
                        f.mFormat.mBitsPerChannel,
                        f.mFormat.mFormatFlags & kAudioFormatFlagIsFloat == 0,
                    )
                });
            if let Some(best) = best {
                let mut desc = best.mFormat;
                desc.mSampleRate = target;
                let current: AudioStreamBasicDescription =
                    get(stream, addr(kAudioStreamPropertyPhysicalFormat, kAudioObjectPropertyScopeGlobal))?;
                if current.mBitsPerChannel != desc.mBitsPerChannel
                    || current.mFormatFlags != desc.mFormatFlags
                    || current.mSampleRate != desc.mSampleRate
                {
                    set(
                        stream,
                        addr(kAudioStreamPropertyPhysicalFormat, kAudioObjectPropertyScopeGlobal),
                        &desc,
                        "物理フォーマットの変更",
                    )?;
                    std::thread::sleep(Duration::from_millis(50));
                }
                let actual: AudioStreamBasicDescription =
                    get(stream, addr(kAudioStreamPropertyPhysicalFormat, kAudioObjectPropertyScopeGlobal))?;
                bits = bits.max(actual.mBitsPerChannel);
            }

            let virt: AudioStreamBasicDescription =
                get(stream, addr(kAudioStreamPropertyVirtualFormat, kAudioObjectPropertyScopeGlobal))?;
            if virt.mFormatFlags & kAudioFormatFlagIsFloat == 0 || virt.mBitsPerChannel != 32 {
                bail!("仮想フォーマットが Float32 ではありません（未対応のデバイスです）");
            }
        }

        Ok(OutputFormat {
            sample_rate: nominal_rate(self.id)? as u32,
            device_bits: bits,
            channels: output_channels(self.id) as u16,
        })
    }

    /// 再生を開始する。返した `Stream` を drop すると止まる。
    pub fn start(
        &self,
        consumer: rtrb::Consumer<f32>,
        src_channels: usize,
        played_frames: Arc<AtomicU64>,
        paused: Arc<AtomicBool>,
    ) -> Result<Stream> {
        let ctx = Box::into_raw(Box::new(IoCtx {
            consumer,
            src_channels,
            played_frames,
            paused,
        }));
        let mut proc_id: AudioDeviceIOProcID = None;
        let st = unsafe { AudioDeviceCreateIOProcID(self.id, Some(io_proc), ctx as *mut c_void, &mut proc_id) };
        if let Err(e) = check(st, "IOProc の作成") {
            drop(unsafe { Box::from_raw(ctx) });
            return Err(e);
        }
        let stream = Stream { device: self.id, proc_id, ctx };
        check(unsafe { AudioDeviceStart(self.id, proc_id) }, "再生の開始")?;
        Ok(stream)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // 元のレートに戻してから専有を解く
        let _ = set(
            self.id,
            addr(kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal),
            &self.original_rate,
            "サンプルレートの復元",
        );
        if self.hogged {
            let release: i32 = -1;
            let _ = set(self.id, addr(kAudioDevicePropertyHogMode, kAudioObjectPropertyScopeGlobal), &release, "Hog Mode の解放");
        }
    }
}

struct IoCtx {
    consumer: rtrb::Consumer<f32>,
    src_channels: usize,
    played_frames: Arc<AtomicU64>,
    paused: Arc<AtomicBool>,
}

pub struct Stream {
    device: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    ctx: *mut IoCtx,
}

unsafe impl Send for Stream {}

impl Stream {
    /// 出力が続けられなくなっていればその理由を返す。
    pub fn error(&self) -> Option<String> {
        match get::<u32>(self.device, addr(kAudioDevicePropertyDeviceIsAlive, kAudioObjectPropertyScopeGlobal)) {
            Ok(alive) if alive != 0 => None,
            _ => Some("出力デバイスが切断されました".into()),
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe {
            // IO スレッド外からの AudioDeviceStop は、IOProc が止まるまで戻らない
            AudioDeviceStop(self.device, self.proc_id);
            AudioDeviceDestroyIOProcID(self.device, self.proc_id);
            drop(Box::from_raw(self.ctx));
        }
    }
}

/// リアルタイムスレッドで呼ばれる。ロック・確保・システムコールをしないこと。
unsafe extern "C" fn io_proc(
    _device: AudioObjectID,
    _now: *const AudioTimeStamp,
    _input: *const AudioBufferList,
    _input_time: *const AudioTimeStamp,
    output: *mut AudioBufferList,
    _output_time: *const AudioTimeStamp,
    client: *mut c_void,
) -> OSStatus {
    let ctx = &mut *(client as *mut IoCtx);
    let abl = &mut *output;
    let bufs = std::slice::from_raw_parts_mut(abl.mBuffers.as_mut_ptr(), abl.mNumberBuffers as usize);
    if bufs.is_empty() {
        return 0;
    }
    for b in bufs.iter_mut() {
        ptr::write_bytes(b.mData as *mut u8, 0, b.mDataByteSize as usize);
    }
    if ctx.paused.load(Ordering::Relaxed) {
        return 0;
    }

    let first_ch = bufs[0].mNumberChannels.max(1) as usize;
    let frames = bufs[0].mDataByteSize as usize / (size_of::<f32>() * first_ch);
    let src_ch = ctx.src_channels;
    let n = (frames * src_ch).min(ctx.consumer.slots()) / src_ch * src_ch;
    if n == 0 {
        return 0;
    }
    let Ok(chunk) = ctx.consumer.read_chunk(n) else {
        return 0;
    };
    let (a, b) = chunk.as_slices();
    for (i, &sample) in a.iter().chain(b.iter()).enumerate() {
        let frame = i / src_ch;
        let ch = i % src_ch;
        // モノラルは左右両方へ出す
        let targets: &[usize] = if src_ch == 1 { &[0, 1] } else { std::slice::from_ref(&ch) };
        for &dev_ch in targets {
            write_sample(bufs, dev_ch, frame, sample);
        }
    }
    chunk.commit_all();
    ctx.played_frames.fetch_add((n / src_ch) as u64, Ordering::Relaxed);
    0
}

/// デバイス全体で `dev_ch` 番目のチャンネルへ書く。ストリームが複数に分かれていても扱える。
#[inline]
unsafe fn write_sample(bufs: &mut [AudioBuffer], mut dev_ch: usize, frame: usize, sample: f32) {
    for b in bufs.iter_mut() {
        let nch = b.mNumberChannels as usize;
        if dev_ch < nch {
            let len = b.mDataByteSize as usize / size_of::<f32>();
            let idx = frame * nch + dev_ch;
            if idx < len {
                *(b.mData as *mut f32).add(idx) = sample;
            }
            return;
        }
        dev_ch -= nch;
    }
}
