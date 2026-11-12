//! Marked moments of a video. They live in two places:
//!   inside the video, as MP4 chapters: DaVinci Resolve (and players such as VLC) show each
//!     one as a marker when the file is simply dragged in;
//!   `<name>.markers.json` next to it: what Rewind reads and edits (ids, times, titles), and
//!     what the chapters are written from again when a title changes later.
//! FCPXML and OTIO timelines were tried first; the user works by dragging whole folders into
//! Resolve, and chapters need no extra step there.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::mux::Chapter;

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

/// The files that belong to a video (only those that exist).
pub fn companions(video: &Path) -> Vec<PathBuf> {
    // `.otio` was written by a test build before chapters; it goes with the video too.
    [json_path(video), video.with_extension("otio")].into_iter().filter(|p| p.exists()).collect()
}

pub fn load(video: &Path) -> Vec<Marker> {
    std::fs::read(json_path(video))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<MarkFile>(&bytes).ok())
        .map(|f| f.markers)
        .unwrap_or_default()
}

/// Stores the markers (sorted by time); no markers removes the file. The chapters inside the
/// video are not touched here (see `library::rewrite_chapters`).
pub fn save(video: &Path, markers: &[Marker]) -> Result<()> {
    if markers.is_empty() {
        for p in companions(video) {
            let _ = std::fs::remove_file(p);
        }
        return Ok(());
    }
    let mut sorted = markers.to_vec();
    sorted.sort_by(|a, b| a.t.total_cmp(&b.t));
    std::fs::write(json_path(video), serde_json::to_vec_pretty(&MarkFile { markers: sorted })?)?;
    Ok(())
}

/// Changes one marker's title; returns false when the video has no such marker.
pub fn set_title(video: &Path, id: u64, title: &str) -> Result<bool> {
    let mut markers = load(video);
    let Some(m) = markers.iter_mut().find(|m| m.id == id) else { return Ok(false) };
    m.title = clean_title(title);
    save(video, &markers)?;
    Ok(true)
}

pub fn remove(video: &Path, id: u64) -> Result<()> {
    let mut markers = load(video);
    markers.retain(|m| m.id != id);
    save(video, &markers)
}

pub fn clean_title(title: &str) -> String {
    title.trim().chars().filter(|c| !c.is_control()).take(200).collect()
}

/// Chapters for markers in a video `duration` seconds long. A chapter starts at each marker
/// and runs to the next one. MP4 chapters must begin at zero (a first chapter that starts
/// later is moved to zero by players and editors), so unless a marker is right at the start,
/// an extra "Начало" chapter covers the time before the first marker.
pub fn chapters(markers: &[Marker], duration: f64) -> Vec<Chapter> {
    let mut sorted: Vec<&Marker> = markers.iter().filter(|m| m.t >= 0.0 && m.t < duration.max(0.001)).collect();
    sorted.sort_by(|a, b| a.t.total_cmp(&b.t));
    if sorted.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    if sorted[0].t > 0.05 {
        out.push(Chapter { start: 0.0, end: sorted[0].t, title: "Начало".into(), marker: false });
    }
    for (i, m) in sorted.iter().enumerate() {
        let end = sorted.get(i + 1).map_or(duration, |next| next.t);
        let title = if m.title.trim().is_empty() { format!("Метка {}", i + 1) } else { m.title.trim().to_string() };
        out.push(Chapter { start: m.t, end: end.max(m.t + 0.001), title, marker: true });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chapter_list() {
        let marks = [
            Marker { id: 2, t: 7.0, title: "Баг с текстурой".into() },
            Marker { id: 1, t: 2.5, title: String::new() },
            Marker { id: 3, t: 99.0, title: "after the end".into() },
        ];
        let c = chapters(&marks, 10.0);
        assert_eq!(c.len(), 3);
        assert_eq!((c[0].start, c[0].end, c[0].title.as_str()), (0.0, 2.5, "Начало"));
        assert_eq!((c[1].start, c[1].end, c[1].title.as_str()), (2.5, 7.0, "Метка 1"));
        assert_eq!((c[2].start, c[2].end, c[2].title.as_str()), (7.0, 10.0, "Баг с текстурой"));
        // A marker right at the start needs no extra chapter.
        let c = chapters(&[Marker { id: 1, t: 0.0, title: "Старт".into() }], 5.0);
        assert_eq!(c.len(), 1);
        assert!(chapters(&[], 5.0).is_empty());
        let c = chapters(&marks, 10.0);
        assert_eq!(crate::mux::marker_comment(&c).unwrap(), "Метки (2): Метка 1 · Баг с текстурой");
        assert_eq!(crate::mux::marker_comment(&[]), None);
    }
}
