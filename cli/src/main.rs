//! 動作確認用の CLI。
//!
//!   hiplay devices
//!   hiplay [-d デバイス名] <ファイルまたはフォルダ>...
//!
//! 再生中は p+Enter で一時停止、n で次、b で前、s 秒数 でシーク、q で終了。

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use anyhow::{bail, Result};
use hifi_engine::backend::list_devices;
use hifi_engine::decoder::SUPPORTED_EXTENSIONS;
use hifi_engine::{Command, Player};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1).peekable();
    let mut device: Option<String> = None;
    let mut inputs: Vec<PathBuf> = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "devices" if inputs.is_empty() => return print_devices(),
            "-d" | "--device" => device = args.next(),
            _ => inputs.push(arg.into()),
        }
    }
    if inputs.is_empty() {
        bail!("使い方: hiplay devices | hiplay [-d デバイス名] <ファイルまたはフォルダ>...");
    }

    let mut queue = Vec::new();
    for input in &inputs {
        collect(input, &mut queue);
    }
    if queue.is_empty() {
        bail!("再生できるファイルがありません");
    }

    let player = Player::new(device.as_deref(), queue)?;
    println!("出力: {}（専有: {}）", player.device_name, if player.status().exclusive { "はい" } else { "いいえ" });

    let quit = Arc::new(AtomicBool::new(false));
    {
        let quit = quit.clone();
        ctrlc::set_handler(move || quit.store(true, Ordering::Relaxed))?;
    }

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut last_track: Option<usize> = None;
    let mut last_error: Option<String> = None;
    loop {
        if quit.load(Ordering::Relaxed) {
            break;
        }
        while let Ok(line) = rx.try_recv() {
            let line = line.trim();
            match line.split_whitespace().next().unwrap_or("p") {
                "p" => player.send(Command::TogglePause),
                "n" => player.send(Command::Next),
                "b" => player.send(Command::Prev),
                "s" => {
                    if let Some(secs) = line.split_whitespace().nth(1).and_then(|s| s.parse().ok()) {
                        player.send(Command::Seek(secs));
                    }
                }
                "q" => quit.store(true, Ordering::Relaxed),
                _ => {}
            }
        }

        let st = player.status();
        if st.last_error != last_error {
            if let Some(e) = &st.last_error {
                println!("\nエラー: {e}");
            }
            last_error = st.last_error.clone();
        }
        if let Some(t) = &st.track {
            if last_track != Some(t.index) {
                last_track = Some(t.index);
                let bits = match (t.source.bits_per_sample, t.source.lossless) {
                    (Some(b), _) => format!("{b}bit"),
                    (None, true) => "ロスレス".into(),
                    (None, false) => "非可逆".into(),
                };
                let out = st.output.map_or("-".into(), |o| format!("{} Hz / {}bit", o.sample_rate, o.device_bits));
                println!(
                    "\n♪ [{}] {}\n   音源: {} {} Hz {} {}ch → デバイス: {}",
                    t.index + 1,
                    t.path.file_name().unwrap_or_default().to_string_lossy(),
                    t.codec,
                    t.source.sample_rate,
                    bits,
                    t.source.channels,
                    out,
                );
            }
            let dur = t.duration_secs.map_or("--:--".into(), fmt_time);
            print!(
                "\r   {} / {} {}   ",
                fmt_time(st.position_secs),
                dur,
                if st.device_lost {
                    "[切断中: つなぎ直して p で再開]"
                } else if st.paused {
                    "[一時停止]"
                } else {
                    ""
                }
            );
            io::stdout().flush().ok();
        }
        if st.finished {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    println!();
    Ok(())
}

fn print_devices() -> Result<()> {
    for d in list_devices()? {
        println!(
            "{}{} [id {}] {}ch 現在 {} Hz{}\n    対応レート: {}",
            if d.is_default { "* " } else { "  " },
            d.name,
            d.id,
            d.channels,
            d.current_rate,
            if d.hogged_by_other { "（他プロセスが専有中）" } else { "" },
            d.supported_rates.iter().map(|r| r.to_string()).collect::<Vec<_>>().join(", "),
        );
    }
    Ok(())
}

fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else { return };
        let mut children: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        children.sort();
        for child in children {
            collect(&child, out);
        }
    } else if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| SUPPORTED_EXTENSIONS.contains(&e.to_lowercase().as_str()))
    {
        out.push(path.to_path_buf());
    }
}

fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}
