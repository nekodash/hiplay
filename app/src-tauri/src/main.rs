//! GUI 本体。再生はすべて hifi-engine に任せ、ここは画面とのやり取りだけを持つ。
//!
//! キューは画面側が持つ。再生を始めるときにキュー全体を渡して `Player` を作り直し、
//! 再生中の追加だけは `Command::Append` でエンジンへ送る。
//!
//! 同期コマンドはメインスレッドで動き、デバイスの切り替え待ちで画面が固まるので、すべて async にしている。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Mutex;

use hifi_engine::backend::list_devices;
use hifi_engine::decoder::collect_files;
use hifi_engine::tags::{read_cover, read_tags};
use hifi_engine::{Command, Player};
use serde::Serialize;
use tauri::ipc::Response;
use tauri::State;

#[derive(Default)]
struct AppState {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    player: Option<Player>,
    queue: Vec<PathBuf>,
    /// 選んだ出力デバイス名。None なら既定の出力
    device: Option<String>,
}

impl Inner {
    /// 鳴らしているプレイヤーを止め、キューの `index` 番目の `at` 秒から作り直す。
    fn restart(&mut self, index: usize, at: Option<f64>) -> Result<(), String> {
        // 先に止めて、デバイスの専有を解いておく
        self.player = None;
        if index >= self.queue.len() {
            return Ok(());
        }
        let player = Player::new(self.device.as_deref(), self.queue.clone(), index, at).map_err(|e| format!("{e:#}"))?;
        self.player = Some(player);
        Ok(())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceDto {
    name: String,
    is_default: bool,
    current_rate: u32,
    supported_rates: Vec<u32>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TrackDto {
    path: String,
    title: String,
    artist: Option<String>,
    album: Option<String>,
    track_number: Option<u32>,
    duration_secs: Option<f64>,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct StatusDto {
    active: bool,
    index: Option<usize>,
    path: Option<String>,
    position_secs: f64,
    duration_secs: Option<f64>,
    paused: bool,
    finished: bool,
    device_lost: bool,
    exclusive: bool,
    device_name: Option<String>,
    codec: Option<String>,
    source_rate: Option<u32>,
    source_bits: Option<u32>,
    source_channels: Option<u16>,
    lossless: bool,
    output_rate: Option<u32>,
    output_bits: Option<u32>,
    error: Option<String>,
}

#[tauri::command]
async fn devices() -> Result<Vec<DeviceDto>, String> {
    let list = list_devices().map_err(|e| format!("{e:#}"))?;
    Ok(list
        .into_iter()
        .map(|d| DeviceDto {
            name: d.name,
            is_default: d.is_default,
            current_rate: d.current_rate,
            supported_rates: d.supported_rates,
        })
        .collect())
}

/// フォルダやファイルを展開し、タグを読んで返す。
#[tauri::command]
async fn scan(paths: Vec<String>) -> Result<Vec<TrackDto>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut files = Vec::new();
        for p in &paths {
            collect_files(&PathBuf::from(p), &mut files);
        }
        files
            .into_iter()
            .map(|path| {
                let tags = read_tags(&path).unwrap_or_default();
                let file_title = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                TrackDto {
                    title: tags.title.unwrap_or(file_title),
                    artist: tags.artist,
                    album: tags.album,
                    track_number: tags.track_number,
                    duration_secs: tags.duration_secs,
                    path: path.to_string_lossy().into_owned(),
                }
            })
            .collect()
    })
    .await
    .map_err(|e| e.to_string())
}

/// ジャケット画像のバイト列。ない場合は空。
#[tauri::command]
async fn cover(path: String) -> Response {
    let data = tauri::async_runtime::spawn_blocking(move || read_cover(&PathBuf::from(path)).map(|c| c.data))
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    Response::new(data)
}

#[tauri::command]
async fn play(state: State<'_, AppState>, queue: Vec<String>, index: usize) -> Result<(), String> {
    let mut inner = state.inner.lock().unwrap();
    inner.queue = queue.into_iter().map(PathBuf::from).collect();
    inner.restart(index, None)
}

/// 再生中のキューの末尾に足す。止まっていれば次に再生するときのキューに入るだけ。
#[tauri::command]
async fn append(state: State<'_, AppState>, paths: Vec<String>) -> Result<(), String> {
    let mut inner = state.inner.lock().unwrap();
    let paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
    inner.queue.extend(paths.iter().cloned());
    if let Some(p) = inner.player.as_ref().filter(|p| !p.status().finished) {
        p.send(Command::Append(paths));
    }
    Ok(())
}

#[tauri::command]
async fn control(state: State<'_, AppState>, action: String) -> Result<(), String> {
    let mut inner = state.inner.lock().unwrap();
    let cmd = match action.as_str() {
        "toggle" => Command::TogglePause,
        "next" => Command::Next,
        "prev" => Command::Prev,
        "stop" => {
            inner.player = None;
            return Ok(());
        }
        other => return Err(format!("不明な操作です: {other}")),
    };
    match inner.player.as_ref() {
        Some(p) if !p.status().finished => p.send(cmd),
        // 再生し終わっていたら、再生ボタンで先頭からかけ直す
        _ if cmd == Command::TogglePause => inner.restart(0, None)?,
        _ => {}
    }
    Ok(())
}

#[tauri::command]
async fn seek(state: State<'_, AppState>, secs: f64) -> Result<(), String> {
    if let Some(p) = state.inner.lock().unwrap().player.as_ref() {
        p.send(Command::Seek(secs));
    }
    Ok(())
}

/// 出力デバイスを変える。再生中ならその位置から新しいデバイスで続ける。
#[tauri::command]
async fn set_device(state: State<'_, AppState>, name: Option<String>) -> Result<(), String> {
    let mut inner = state.inner.lock().unwrap();
    inner.device = name;
    let resume = inner
        .player
        .as_ref()
        .map(|p| p.status())
        .filter(|s| !s.finished)
        .and_then(|s| s.track.map(|t| (t.index, s.position_secs)));
    match resume {
        Some((index, at)) => inner.restart(index, Some(at)),
        None => {
            inner.player = None;
            Ok(())
        }
    }
}

#[tauri::command]
async fn status(state: State<'_, AppState>) -> Result<StatusDto, String> {
    let inner = state.inner.lock().unwrap();
    let Some(player) = inner.player.as_ref() else {
        return Ok(StatusDto::default());
    };
    let st = player.status();
    let track = st.track.as_ref();
    Ok(StatusDto {
        active: true,
        index: track.map(|t| t.index),
        path: track.map(|t| t.path.to_string_lossy().into_owned()),
        position_secs: st.position_secs,
        duration_secs: track.and_then(|t| t.duration_secs),
        paused: st.paused,
        finished: st.finished,
        device_lost: st.device_lost,
        exclusive: st.exclusive,
        device_name: Some(player.device_name.clone()),
        codec: track.map(|t| t.codec.clone()),
        source_rate: track.map(|t| t.source.sample_rate),
        source_bits: track.and_then(|t| t.source.bits_per_sample),
        source_channels: track.map(|t| t.source.channels),
        lossless: track.is_some_and(|t| t.source.lossless),
        output_rate: st.output.map(|o| o.sample_rate),
        output_bits: st.output.map(|o| o.device_bits),
        error: st.last_error,
    })
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            devices, scan, cover, play, append, control, seek, set_device, status
        ])
        .run(tauri::generate_context!())
        .expect("アプリを起動できませんでした");
}
