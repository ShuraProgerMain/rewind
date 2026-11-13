//! Chapters written into a finished MP4 in place, without copying the video.
//!
//! The chapter titles are tiny: they go into a small `mdat` box appended at the end of the
//! file, followed by a new index (`moov`) that has a chapter text track pointing at them and
//! a `chap` reference from every other track; the old index is renamed to `free`. The video
//! and sound data are not touched, so this takes milliseconds whatever the file's size.
//! It is the same layout FFmpeg writes (a QuickTime text track, which DaVinci Resolve reads
//! as clip markers), plus a Nero `chpl` list and the "Метки (N): …" comment for Explorer.
//! Each change appends a new index; the old one stays behind as dead space (a few MB at most
//! for an hour-long recording).

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::mux::{marker_comment, Chapter};

fn put_box(out: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
    out.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
}

fn make_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 8);
    put_box(&mut out, kind, payload);
    out
}

fn full_box(kind: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut body = ((version as u32) << 24 | flags).to_be_bytes().to_vec();
    body.extend_from_slice(payload);
    make_box(kind, &body)
}

/// Boxes directly inside `d[from..to]`: (offset, size, header size, type).
fn children(d: &[u8], from: usize, to: usize) -> Vec<(usize, usize, usize, [u8; 4])> {
    let mut out = Vec::new();
    let mut at = from;
    while at + 8 <= to {
        let mut size = u32::from_be_bytes(d[at..at + 4].try_into().unwrap()) as usize;
        let mut header = 8;
        if size == 1 && at + 16 <= to {
            size = u64::from_be_bytes(d[at + 8..at + 16].try_into().unwrap()) as usize;
            header = 16;
        } else if size == 0 {
            size = to - at;
        }
        if size < header || at + size > to {
            break;
        }
        out.push((at, size, header, d[at + 4..at + 8].try_into().unwrap()));
        at += size;
    }
    out
}

fn find(d: &[u8], from: usize, to: usize, path: &[&[u8; 4]]) -> Option<(usize, usize, usize)> {
    let (first, rest) = path.split_first()?;
    let (at, size, header, _) = *children(d, from, to).iter().find(|c| &&c.3 == first)?;
    if rest.is_empty() {
        Some((at, size, header))
    } else {
        find(d, at + header, at + size, rest)
    }
}

/// One chapter title as a QuickTime text sample: length, UTF-8 text, encoding box (UTF-8).
fn text_sample(title: &str) -> Vec<u8> {
    let text = title.as_bytes();
    let mut out = (text.len() as u16).to_be_bytes().to_vec();
    out.extend_from_slice(text);
    out.extend_from_slice(&[0, 0, 0, 12, b'e', b'n', b'c', b'd', 0, 0, 1, 0]);
    out
}

/// The chapter track, as FFmpeg makes it. Times: movie in its own timescale, chapters in ms.
fn chapter_track(id: u32, movie_duration: u32, lengths_ms: &[u32], sizes: &[u32], data_at: u64) -> Vec<u8> {
    let total_ms: u32 = lengths_ms.iter().sum();
    let mut tkhd = Vec::new();
    tkhd.extend_from_slice(&[0; 8]); // created, modified
    tkhd.extend_from_slice(&id.to_be_bytes());
    tkhd.extend_from_slice(&[0; 4]);
    tkhd.extend_from_slice(&movie_duration.to_be_bytes());
    tkhd.extend_from_slice(&[0; 16]); // reserved, layer, group, volume, reserved
    for v in [0x10000u32, 0, 0, 0, 0x10000, 0, 0, 0, 0x4000_0000] {
        tkhd.extend_from_slice(&v.to_be_bytes()); // unity matrix
    }
    tkhd.extend_from_slice(&[0; 8]); // width, height
    let tkhd = full_box(b"tkhd", 0, 0, &tkhd); // flags 0: not shown as a track of its own

    let mut elst = 1u32.to_be_bytes().to_vec();
    elst.extend_from_slice(&movie_duration.to_be_bytes());
    elst.extend_from_slice(&[0, 0, 0, 0, 0, 1, 0, 0]);
    let edts = make_box(b"edts", &full_box(b"elst", 0, 0, &elst));

    let mut mdhd = [0u8; 8].to_vec();
    mdhd.extend_from_slice(&1000u32.to_be_bytes());
    mdhd.extend_from_slice(&total_ms.to_be_bytes());
    mdhd.extend_from_slice(&[0x55, 0xC4, 0, 0]); // language "und"
    let mdhd = full_box(b"mdhd", 0, 0, &mdhd);
    let mut hdlr = vec![0, 0, 0, 0];
    hdlr.extend_from_slice(b"text");
    hdlr.extend_from_slice(&[0; 12]);
    hdlr.extend_from_slice(b"SubtitleHandler\0");
    let hdlr = full_box(b"hdlr", 0, 0, &hdlr);

    let gmin = full_box(b"gmin", 0, 0, &[0, 0x40, 0x80, 0, 0x80, 0, 0x80, 0, 0, 0, 0, 0]);
    let mut text = vec![0, 1, 0, 0];
    text.extend_from_slice(&[0; 12]);
    text.extend_from_slice(&[0, 1, 0, 0]);
    text.extend_from_slice(&[0; 12]);
    text.extend_from_slice(&[0x40, 0, 0, 0]);
    let mut gmhd = gmin;
    gmhd.extend_from_slice(&make_box(b"text", &text));
    let gmhd = make_box(b"gmhd", &gmhd);
    let mut dref = 1u32.to_be_bytes().to_vec();
    dref.extend_from_slice(&full_box(b"url ", 0, 1, &[]));
    let dinf = make_box(b"dinf", &full_box(b"dref", 0, 0, &dref));

    // QuickTime text sample description (the one FFmpeg writes for chapters).
    let entry: [u8; 75] = [
        0x74, 0x65, 0x78, 0x74, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x0d, 0x66, 0x74, 0x61, 0x62, 0, 1, 0, 1, 0, 0, 0, 0, 0x14, 0x62, 0x74, 0x72,
        0x74, 0, 0, 0, 0, 0, 0, 0, 0x64, 0, 0, 0, 0x64,
    ];
    let mut stsd = 1u32.to_be_bytes().to_vec();
    stsd.extend_from_slice(&((entry.len() + 4) as u32).to_be_bytes());
    stsd.extend_from_slice(&entry);
    let stsd = full_box(b"stsd", 0, 0, &stsd);
    let mut stts = (lengths_ms.len() as u32).to_be_bytes().to_vec();
    for len in lengths_ms {
        stts.extend_from_slice(&1u32.to_be_bytes());
        stts.extend_from_slice(&len.to_be_bytes());
    }
    let stts = full_box(b"stts", 0, 0, &stts);
    let mut stsz = 0u32.to_be_bytes().to_vec();
    stsz.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
    for s in sizes {
        stsz.extend_from_slice(&s.to_be_bytes());
    }
    let stsz = full_box(b"stsz", 0, 0, &stsz);
    let mut stsc = 1u32.to_be_bytes().to_vec();
    for v in [1u32, sizes.len() as u32, 1] {
        stsc.extend_from_slice(&v.to_be_bytes());
    }
    let stsc = full_box(b"stsc", 0, 0, &stsc);
    let mut co64 = 1u32.to_be_bytes().to_vec();
    co64.extend_from_slice(&data_at.to_be_bytes());
    let co64 = full_box(b"co64", 0, 0, &co64);
    let stbl = make_box(b"stbl", &[stsd, stts, stsz, stsc, co64].concat());
    let minf = make_box(b"minf", &[gmhd, dinf, stbl].concat());
    let mdia = make_box(b"mdia", &[mdhd, hdlr, minf].concat());
    make_box(b"trak", &[tkhd, edts, mdia].concat())
}

/// The user data box: the comment Explorer shows, and the Nero chapter list.
fn user_data(chapters: &[Chapter]) -> Vec<u8> {
    let mut udta = Vec::new();
    if let Some(comment) = marker_comment(chapters) {
        let mut data = vec![0, 0, 0, 0]; // locale
        data.extend_from_slice(comment.as_bytes());
        let cmt = make_box(&[0xA9, b'c', b'm', b't'], &full_box(b"data", 0, 1, &data));
        let ilst = make_box(b"ilst", &cmt);
        let mut hdlr = vec![0, 0, 0, 0];
        hdlr.extend_from_slice(b"mdir");
        hdlr.extend_from_slice(b"appl");
        hdlr.extend_from_slice(&[0; 9]);
        let hdlr = full_box(b"hdlr", 0, 0, &hdlr);
        udta.extend_from_slice(&full_box(b"meta", 0, 0, &[hdlr, ilst].concat()));
    }
    if !chapters.is_empty() {
        let mut chpl = vec![0, 0, 0, 0, chapters.len().min(255) as u8];
        for c in chapters.iter().take(255) {
            chpl.extend_from_slice(&((c.start * 1e7).round() as u64).to_be_bytes());
            let title: Vec<u8> = c.title.bytes().take(255).collect();
            chpl.push(title.len() as u8);
            chpl.extend_from_slice(&title);
        }
        udta.extend_from_slice(&full_box(b"chpl", 1, 0, &chpl));
    }
    udta
}

/// Opens a file for writing, waiting a few seconds for programs that hold it for a moment
/// (Explorer reading the comment, antivirus, indexers).
fn open_for_writing(path: &Path) -> Result<std::fs::File> {
    let mut attempt = 0;
    loop {
        match OpenOptions::new().read(true).write(true).open(path) {
            Ok(file) => return Ok(file),
            Err(e) if attempt < 20 => {
                attempt += 1;
                let _ = e;
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            Err(e) => {
                crate::log::line(&format!("cannot open {} for writing: {e}", path.display()));
                bail!("файл открыт в другой программе. Закройте его и нажмите «Записать в видео»")
            }
        }
    }
}

/// Gives a finished MP4 these chapters (none: takes its chapters away), in place.
pub fn write(path: &Path, chapters: &[Chapter]) -> Result<()> {
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let mut file = open_for_writing(path)?;
    let length = file.seek(SeekFrom::End(0))?;

    // The index (moov) among the top-level boxes.
    let (mut at, mut moov) = (0u64, None);
    while at + 8 <= length {
        let mut head = [0u8; 16];
        file.seek(SeekFrom::Start(at))?;
        file.read_exact(&mut head[..8])?;
        let mut size = u32::from_be_bytes(head[..4].try_into()?) as u64;
        if size == 1 {
            file.read_exact(&mut head[8..16])?;
            size = u64::from_be_bytes(head[8..16].try_into()?);
        } else if size == 0 {
            size = length - at;
        }
        if size < 8 {
            bail!("damaged MP4 box at {at}");
        }
        if &head[4..8] == b"moov" {
            moov = Some((at, size));
        }
        at += size;
    }
    let (moov_at, moov_size) = moov.context("no index (moov) in the file")?;
    let mut d = vec![0u8; moov_size as usize];
    file.seek(SeekFrom::Start(moov_at))?;
    file.read_exact(&mut d)?;
    if find(&d, 8, d.len(), &[b"mvex"]).is_some() {
        bail!("a fragmented file (unfinished recording)");
    }

    // Movie header: timescale, duration, next track id.
    let (mvhd_at, mvhd_size, _) = find(&d, 8, d.len(), &[b"mvhd"]).context("no movie header")?;
    let mut mvhd = d[mvhd_at..mvhd_at + mvhd_size].to_vec();
    let (timescale, duration) = if mvhd[8] == 1 {
        (u32::from_be_bytes(mvhd[28..32].try_into()?), u64::from_be_bytes(mvhd[32..40].try_into()?))
    } else {
        (u32::from_be_bytes(mvhd[20..24].try_into()?), u32::from_be_bytes(mvhd[24..28].try_into()?) as u64)
    };
    let next = mvhd.len() - 4;
    let chapter_id = u32::from_be_bytes(mvhd[next..].try_into()?);
    mvhd[next..].copy_from_slice(&(chapter_id + 1).to_be_bytes());

    // The titles, appended in their own small data box.
    let samples: Vec<Vec<u8>> = chapters.iter().map(|c| text_sample(&c.title)).collect();
    let mdat = make_box(b"mdat", &samples.concat());
    let data_at = length + 8;

    let mut moov = mvhd;
    for (child, size, _, kind) in children(&d, 8, d.len()) {
        match &kind {
            b"mvhd" | b"udta" => {}
            b"trak" => {
                let handler = find(&d, child + 8, child + size, &[b"mdia", b"hdlr"]);
                if handler.is_some_and(|(h, _, _)| &d[h + 16..h + 20] == b"text") {
                    continue; // the old chapter track
                }
                let mut body = Vec::new();
                for (c, csize, _, ckind) in children(&d, child + 8, child + size) {
                    if &ckind == b"tref" {
                        continue; // the old reference to the chapter track
                    }
                    if &ckind == b"mdia" && !chapters.is_empty() {
                        body.extend_from_slice(&make_box(b"tref", &make_box(b"chap", &chapter_id.to_be_bytes())));
                    }
                    body.extend_from_slice(&d[c..c + csize]);
                }
                put_box(&mut moov, b"trak", &body);
            }
            _ => moov.extend_from_slice(&d[child..child + size]),
        }
    }
    if !chapters.is_empty() {
        let lengths: Vec<u32> = chapters
            .iter()
            .map(|c| ((c.end - c.start).max(0.001) * 1000.0).round().max(1.0) as u32)
            .collect();
        let sizes: Vec<u32> = samples.iter().map(|s| s.len() as u32).collect();
        let movie_duration = duration.min(u32::MAX as u64) as u32;
        moov.extend_from_slice(&chapter_track(chapter_id, movie_duration, &lengths, &sizes, data_at));
    }
    let udta = user_data(chapters);
    if !udta.is_empty() {
        put_box(&mut moov, b"udta", &udta);
    }
    let _ = timescale;
    let moov = make_box(b"moov", &moov);

    // New boxes first, then the old index stops counting: a reader never sees no index.
    file.seek(SeekFrom::Start(length))?;
    if !chapters.is_empty() {
        file.write_all(&mdat)?;
    }
    file.write_all(&moov)?;
    file.flush()?;
    file.seek(SeekFrom::Start(moov_at + 4))?;
    file.write_all(b"free")?;
    file.flush()?;
    if let Some(modified) = modified {
        let _ = file.set_modified(modified); // same date, so the list keeps its order
    }
    Ok(())
}
