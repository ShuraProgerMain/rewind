//! Marked moments of a video, kept next to it:
//!   `<name>.markers.json`  what Rewind reads and edits (ids, times, titles);
//!   `<name>.otio`          the same as an editing timeline (OpenTimelineIO), for
//!                          File → Import → Timeline in DaVinci Resolve: the video on V1, each
//!                          sound track on its own audio track, the markers on the clip.
//! FCPXML was tried first: Resolve 21 drops markers from it on import (and does not write
//! them on export), while OTIO carries them both ways.
//! Both files follow the video when it is renamed, trimmed or deleted from Rewind.

use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ffi::*;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Marker {
    /// Stays the same across the files made from one press (recording, clip, trimmed copy).
    pub id: u64,
    /// Seconds from the start of the video.
    pub t: f64,
    #[serde(default)]
    pub title: String,
}

#[derive(Serialize, Deserialize, Default)]
struct MarkFile {
    #[serde(default)]
    markers: Vec<Marker>,
}

pub fn json_path(video: &Path) -> PathBuf {
    video.with_extension("markers.json")
}

pub fn timeline_path(video: &Path) -> PathBuf {
    video.with_extension("otio")
}

/// The files that belong to a video (only those that exist).
pub fn companions(video: &Path) -> Vec<PathBuf> {
    [json_path(video), timeline_path(video)].into_iter().filter(|p| p.exists()).collect()
}

pub fn load(video: &Path) -> Vec<Marker> {
    std::fs::read(json_path(video))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<MarkFile>(&bytes).ok())
        .map(|f| f.markers)
        .unwrap_or_default()
}

/// Stores the markers (sorted by time) and the editing timeline. No markers: both files go.
pub fn save(video: &Path, markers: &[Marker]) -> Result<()> {
    if markers.is_empty() {
        for p in companions(video) {
            let _ = std::fs::remove_file(p);
        }
        return Ok(());
    }
    let mut sorted = markers.to_vec();
    sorted.sort_by(|a, b| a.t.total_cmp(&b.t));
    std::fs::write(json_path(video), serde_json::to_vec_pretty(&MarkFile { markers: sorted.clone() })?)?;
    write_timeline(video, &sorted)?;
    Ok(())
}

/// Changes one marker's title; returns false when the video has no such marker.
pub fn set_title(video: &Path, id: u64, title: &str) -> Result<bool> {
    let mut markers = load(video);
    let Some(m) = markers.iter_mut().find(|m| m.id == id) else { return Ok(false) };
    m.title = title.trim().chars().take(200).collect();
    save(video, &markers)?;
    Ok(true)
}

pub fn remove(video: &Path, id: u64) -> Result<()> {
    let mut markers = load(video);
    markers.retain(|m| m.id != id);
    save(video, &markers)
}

/// What an editing timeline needs to know about the video.
struct Media {
    /// Frames per second (Rewind records whole rates).
    fps: f64,
    secs: f64,
    /// Sound tracks in file order: (channels, title).
    audio: Vec<(i32, String)>,
}

/// The track's name; MP4 keeps it as the handler name.
unsafe fn title_of(st: *mut AVStream) -> String {
    for key in [c"title", c"handler_name"] {
        let entry = av_dict_get((*st).metadata, key.as_ptr(), null(), 0);
        if !entry.is_null() && !(*entry).value.is_null() {
            let text = CStr::from_ptr((*entry).value).to_string_lossy().trim().to_string();
            if !text.is_empty() && text != "SoundHandler" {
                return text;
            }
        }
    }
    String::new()
}

fn probe(video: &Path) -> Result<Media> {
    let cpath = CString::new(video.to_string_lossy().as_bytes())?;
    unsafe {
        let mut fmt: *mut AVFormatContext = null_mut();
        chk(avformat_open_input(&mut fmt, cpath.as_ptr(), null(), null_mut()), "open")?;
        let mut m = Media { fps: 60.0, secs: 0.0, audio: Vec::new() };
        let mut found_video = false;
        for i in 0..(*fmt).nb_streams as usize {
            let st = *(*fmt).streams.add(i);
            let par = (*st).codecpar;
            if (*st).duration > 0 {
                let tb = (*st).time_base;
                m.secs = m.secs.max((*st).duration as f64 * tb.num as f64 / tb.den as f64);
            }
            let kind = (*par).codec_type;
            if kind == AVMEDIA_TYPE_VIDEO && !found_video {
                found_video = true;
                let r = if (*st).avg_frame_rate.num > 0 { (*st).avg_frame_rate } else { (*st).r_frame_rate };
                if r.num > 0 && r.den > 0 {
                    let rate = r.num as f64 / r.den as f64;
                    m.fps = if (rate - rate.round()).abs() < 0.05 { rate.round() } else { rate };
                }
            } else if kind == AVMEDIA_TYPE_AUDIO {
                m.audio.push(((*par).ch_layout.nb_channels.max(1), title_of(st)));
            }
        }
        avformat_close_input(&mut fmt);
        if !found_video {
            bail!("no picture in the file");
        }
        Ok(m)
    }
}

/// Track names as people say them: Rewind writes "System sound", "Sound: <program>", "Microphone".
fn track_name(title: &str, n: usize) -> String {
    match title {
        "Microphone" => "Микрофон".into(),
        "System sound" => "Звук программ".into(),
        t if t.starts_with("Sound: ") => format!("Звук: {}", &t[7..]),
        "" => format!("Звук {n}"),
        t => t.to_string(),
    }
}

/// Builds the timeline: one clip, the whole video, the markers on it. It starts at
/// 01:00:00:00 like a new Resolve timeline.
fn timeline(video: &Path, media: &Media, markers: &[Marker]) -> Value {
    let rate = media.fps;
    let frames = |secs: f64| (secs * rate).round().max(0.0);
    let total = frames(media.secs).max(1.0);
    let time = |v: f64| json!({ "OTIO_SCHEMA": "RationalTime.1", "rate": rate, "value": v });
    let range = |start: f64, dur: f64| json!({ "OTIO_SCHEMA": "TimeRange.1", "start_time": time(start), "duration": time(dur) });
    let file_name = video.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let name = video.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let reference = json!({ "DEFAULT_MEDIA": {
        "OTIO_SCHEMA": "ExternalReference.1",
        "metadata": {},
        "name": file_name,
        "available_range": range(0.0, total),
        "available_image_bounds": null,
        "target_url": video.to_string_lossy(),
    }});
    let clip = |marks: Vec<Value>, meta: Value| {
        json!({
            "OTIO_SCHEMA": "Clip.2",
            "metadata": { "Resolve_OTIO": meta },
            "name": file_name,
            "source_range": range(0.0, total),
            "effects": [],
            "markers": marks,
            "enabled": true,
            "media_references": reference,
            "active_media_reference_key": "DEFAULT_MEDIA",
        })
    };
    let track = |kind: &str, track_name: String, children: Vec<Value>, meta: Value| {
        json!({
            "OTIO_SCHEMA": "Track.1",
            "metadata": { "Resolve_OTIO": meta },
            "name": track_name,
            "source_range": null,
            "effects": [],
            "markers": [],
            "enabled": true,
            "children": children,
            "kind": kind,
        })
    };

    let marks: Vec<Value> = markers
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let title = if m.title.trim().is_empty() { format!("Метка {}", i + 1) } else { m.title.trim().to_string() };
            json!({
                "OTIO_SCHEMA": "Marker.2",
                "metadata": { "Resolve_OTIO": { "Keywords": [], "Note": "" } },
                "name": title,
                "color": "BLUE",
                "marked_range": range(frames(m.t).min(total - 1.0), 1.0),
            })
        })
        .collect();
    let mut tracks = vec![track("Video", "Видео".into(), vec![clip(marks, json!({ "Link Group ID": 1 }))], json!({ "Locked": false }))];
    for (n, (channels, title)) in media.audio.iter().enumerate() {
        let mapping: Vec<Value> =
            (0..(*channels).min(2)).map(|c| json!({ "Source Channel ID": c, "Source Track ID": n })).collect();
        let kind = if *channels == 1 { "Mono" } else { "Stereo" };
        tracks.push(track(
            "Audio",
            track_name(title, n + 1),
            vec![clip(Vec::new(), json!({ "Channels": mapping, "Link Group ID": 1 }))],
            json!({ "Audio Type": kind, "Locked": false, "SoloOn": false }),
        ));
    }
    json!({
        "OTIO_SCHEMA": "Timeline.1",
        "metadata": { "Resolve_OTIO": { "Resolve OTIO Meta Version": "1.0" } },
        "name": name,
        "global_start_time": time(3600.0 * rate),
        "tracks": {
            "OTIO_SCHEMA": "Stack.1",
            "metadata": {},
            "name": "",
            "source_range": null,
            "effects": [],
            "markers": [],
            "enabled": true,
            "children": tracks,
        },
    })
}

/// Writes `<name>.otio` for a video (also used for a video without markers, when it is
/// sent to Resolve). Returns its path.
pub fn write_timeline(video: &Path, markers: &[Marker]) -> Result<PathBuf> {
    let media = probe(video)?;
    let path = timeline_path(video);
    std::fs::write(&path, serde_json::to_vec_pretty(&timeline(video, &media, markers))?)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeline_json() {
        let media = Media { fps: 60.0, secs: 10.0, audio: vec![(2, "Sound: ELDEN RING".into()), (2, "Microphone".into())] };
        let marks = [
            Marker { id: 1, t: 2.5, title: String::new() },
            Marker { id: 2, t: 7.0, title: "Баг с текстурой".into() },
        ];
        let t = timeline(Path::new(r"E:\Видео\Rewind\Клип 1.mp4"), &media, &marks);
        let tracks = t["tracks"]["children"].as_array().unwrap();
        assert_eq!(tracks.len(), 3);
        assert_eq!(t["global_start_time"]["value"], 216000.0);
        let clip = &tracks[0]["children"][0];
        assert_eq!(clip["source_range"]["duration"]["value"], 600.0);
        assert_eq!(clip["markers"][0]["name"], "Метка 1");
        assert_eq!(clip["markers"][0]["marked_range"]["start_time"]["value"], 150.0);
        assert_eq!(clip["markers"][1]["name"], "Баг с текстурой");
        assert_eq!(clip["media_references"]["DEFAULT_MEDIA"]["target_url"], r"E:\Видео\Rewind\Клип 1.mp4");
        assert_eq!(tracks[1]["name"], "Звук: ELDEN RING");
        assert_eq!(tracks[2]["name"], "Микрофон");
        assert_eq!(tracks[2]["children"][0]["metadata"]["Resolve_OTIO"]["Channels"][1]["Source Track ID"], 1);
    }
}
