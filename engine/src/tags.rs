//! 曲のタグとジャケット画像の読み取り（表示用）。

use std::fs::File;
use std::path::Path;

use anyhow::Result;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, MetadataRevision, StandardTagKey, StandardVisualKey};
use symphonia::core::probe::{Hint, ProbeResult};

#[derive(Debug, Clone, Default)]
pub struct TrackTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub duration_secs: Option<f64>,
}

pub struct Cover {
    pub data: Vec<u8>,
    pub mime: String,
}

fn probe(path: &Path) -> Result<ProbeResult> {
    let file = File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    Ok(symphonia::default::get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    )?)
}

/// コンテナ前置きのタグ（MP3 の ID3 など）とコンテナ内のタグの両方を、見つかった順に渡す。
fn each_revision(probed: &mut ProbeResult, mut f: impl FnMut(&MetadataRevision)) {
    if let Some(rev) = probed.metadata.get().as_ref().and_then(|m| m.current()) {
        f(rev);
    }
    if let Some(rev) = probed.format.metadata().current() {
        f(rev);
    }
}

fn leading_number(s: &str) -> Option<u32> {
    s.split('/').next()?.trim().parse().ok()
}

pub fn read_tags(path: &Path) -> Result<TrackTags> {
    let mut probed = probe(path)?;
    let mut tags = TrackTags::default();
    if let Some(track) = probed.format.default_track() {
        let p = &track.codec_params;
        if let (Some(n), Some(rate)) = (p.n_frames, p.sample_rate) {
            tags.duration_secs = Some(n as f64 / rate as f64);
        }
    }
    each_revision(&mut probed, |rev| {
        for tag in rev.tags() {
            let value = tag.value.to_string();
            if value.trim().is_empty() {
                continue;
            }
            match tag.std_key {
                Some(StandardTagKey::TrackTitle) => tags.title.get_or_insert(value),
                Some(StandardTagKey::Artist) => tags.artist.get_or_insert(value),
                Some(StandardTagKey::AlbumArtist) if tags.artist.is_none() => tags.artist.get_or_insert(value),
                Some(StandardTagKey::Album) => tags.album.get_or_insert(value),
                Some(StandardTagKey::TrackNumber) => {
                    tags.track_number = tags.track_number.or(leading_number(&value));
                    continue;
                }
                Some(StandardTagKey::DiscNumber) => {
                    tags.disc_number = tags.disc_number.or(leading_number(&value));
                    continue;
                }
                _ => continue,
            };
        }
    });
    Ok(tags)
}

/// 埋め込みのジャケットを優先し、なければ同じフォルダの cover.jpg などを探す。
pub fn read_cover(path: &Path) -> Option<Cover> {
    if let Ok(mut probed) = probe(path) {
        let mut found: Option<Cover> = None;
        each_revision(&mut probed, |rev| {
            let visuals = rev.visuals();
            let best = visuals
                .iter()
                .find(|v| v.usage == Some(StandardVisualKey::FrontCover))
                .or_else(|| visuals.first());
            if let (None, Some(v)) = (&found, best) {
                found = Some(Cover {
                    data: v.data.to_vec(),
                    mime: v.media_type.clone(),
                });
            }
        });
        if found.is_some() {
            return found;
        }
    }

    let dir = path.parent()?;
    let mut candidates: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
            let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
            matches!(stem.as_str(), "cover" | "folder" | "front" | "album")
                && matches!(ext.as_str(), "jpg" | "jpeg" | "png")
        })
        .collect();
    candidates.sort();
    let image = candidates.first()?;
    let mime = if image.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")) {
        "image/png"
    } else {
        "image/jpeg"
    };
    Some(Cover {
        data: std::fs::read(image).ok()?,
        mime: mime.into(),
    })
}
