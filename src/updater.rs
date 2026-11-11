//! Updates without an installer. The program looks at the latest release on GitHub,
//! downloads the new exe, checks it against the .sha256 from the same release and leaves it
//! next to the running one as `rewind.exe.new`. It is swapped in at the next start, never
//! in the middle of a session: a restart would throw away the replay buffer.
//!
//! Release files this expects: `rewind.exe`, `rewind.exe.sha256`, `update.json`
//! (`{"ffmpeg": "<set>"}`: which FFmpeg DLLs the exe needs). If the DLL set changed, the
//! exe alone is not enough and the user is pointed to the release page instead.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Networking::WinHttp::{
    WinHttpAddRequestHeaders, WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest,
    WinHttpQueryDataAvailable, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
    WinHttpSetTimeouts, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ADDREQ_FLAG_ADD, WINHTTP_FLAG_SECURE,
    WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
};
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};

pub const REPO: &str = "ShuraProgerMain/rewind";
/// Version of this build (Cargo's, unless REWIND_VERSION was set when building).
pub const VERSION: &str = env!("REWIND_VERSION");
/// The FFmpeg DLLs this exe was built against. Bump when their major versions change.
pub const FFMPEG_SET: &str = "avcodec-62";
const AGENT: &str = concat!("Rewind-updater/", env!("REWIND_VERSION"));
const MAX_DOWNLOAD: usize = 200 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    UpToDate,
    /// Downloaded and verified; goes live at the next start.
    Staged { version: String },
    /// A newer release exists but needs the full archive.
    Manual { version: String, page: String },
}

fn staged_exe(exe: &Path) -> PathBuf {
    PathBuf::from(format!("{}.new", exe.display()))
}
fn staged_note(exe: &Path) -> PathBuf {
    PathBuf::from(format!("{}.new.version", exe.display()))
}
fn old_exe(exe: &Path) -> PathBuf {
    PathBuf::from(format!("{}.old", exe.display()))
}

fn newer(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim_start_matches('v').split(['.', '-']).take(3).map(|p| p.parse().unwrap_or(0)).collect()
    };
    parse(latest) > parse(current)
}

/// Tests: REWIND_UPDATE_FEED=<folder> replaces GitHub with `latest.json` and files in it.
fn feed() -> Option<PathBuf> {
    std::env::var_os("REWIND_UPDATE_FEED").map(PathBuf::from)
}

unsafe fn https_get(url: &str, accept: &str) -> Result<(u32, Vec<u8>)> {
    struct Handle(*mut c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let _ = WinHttpCloseHandle(self.0);
                }
            }
        }
    }
    let rest = url.strip_prefix("https://").ok_or_else(|| anyhow!("only https is allowed: {url}"))?;
    let (host, path) = match rest.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (rest, "/".to_string()),
    };

    let session = Handle(WinHttpOpen(&HSTRING::from(AGENT), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0));
    if session.0.is_null() {
        bail!("WinHttpOpen failed");
    }
    let _ = WinHttpSetTimeouts(session.0, 10_000, 10_000, 30_000, 30_000);
    let connect = Handle(WinHttpConnect(session.0, &HSTRING::from(host), 443, 0));
    if connect.0.is_null() {
        bail!("cannot connect to {host}");
    }
    let request = Handle(WinHttpOpenRequest(
        connect.0,
        w!("GET"),
        &HSTRING::from(path),
        PCWSTR::null(),
        PCWSTR::null(),
        std::ptr::null(),
        WINHTTP_FLAG_SECURE,
    ));
    if request.0.is_null() {
        bail!("cannot open a request to {host}");
    }
    let header: Vec<u16> = format!("Accept: {accept}").encode_utf16().collect();
    WinHttpAddRequestHeaders(request.0, &header, WINHTTP_ADDREQ_FLAG_ADD)?;
    WinHttpSendRequest(request.0, None, None, 0, 0, 0)?;
    WinHttpReceiveResponse(request.0, std::ptr::null_mut())?;

    let mut status = 0u32;
    let mut size = 4u32;
    WinHttpQueryHeaders(
        request.0,
        WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
        PCWSTR::null(),
        Some(&mut status as *mut u32 as *mut c_void),
        &mut size,
        std::ptr::null_mut(),
    )?;

    let mut body = Vec::new();
    loop {
        let mut available = 0u32;
        WinHttpQueryDataAvailable(request.0, &mut available)?;
        if available == 0 {
            break;
        }
        let start = body.len();
        if start + available as usize > MAX_DOWNLOAD {
            bail!("the download is suspiciously large");
        }
        body.resize(start + available as usize, 0);
        let mut read = 0u32;
        WinHttpReadData(request.0, body[start..].as_mut_ptr() as *mut c_void, available, &mut read)?;
        body.truncate(start + read as usize);
        if read == 0 {
            break;
        }
    }
    Ok((status, body))
}

/// GitHub now and then answers 5xx or drops the connection: up to three tries.
fn get(url: &str, accept: &str) -> Result<(u32, Vec<u8>)> {
    if let Some(file) = url.strip_prefix("file:") {
        return match std::fs::read(file) {
            Ok(bytes) => Ok((200, bytes)),
            Err(_) => Ok((404, Vec::new())),
        };
    }
    let mut last = anyhow!("no attempt made");
    for attempt in 0..3u64 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(2 * attempt));
        }
        match unsafe { https_get(url, accept) } {
            Ok((status, _)) if status >= 500 => last = anyhow!("HTTP {status}"),
            Ok(done) => return Ok(done),
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn asset(release: &serde_json::Value, name: &str) -> Option<String> {
    release["assets"].as_array()?.iter().find(|a| a["name"] == name)?["browser_download_url"].as_str().map(String::from)
}

fn download(release: &serde_json::Value, name: &str) -> Result<Vec<u8>> {
    let url = asset(release, name).ok_or_else(|| anyhow!("the release has no {name}"))?;
    let (status, body) = get(&url, "application/octet-stream").with_context(|| format!("downloading {name}"))?;
    if !(200..300).contains(&status) {
        bail!("downloading {name}: HTTP {status}");
    }
    Ok(body)
}

/// Looks for a newer release and, if the exe alone is enough, downloads and verifies it.
/// Blocking; call from a worker thread.
pub fn check(exe: &Path) -> Result<Outcome> {
    let url = match feed() {
        Some(dir) => format!("file:{}", dir.join("latest.json").display()),
        None => format!("https://api.github.com/repos/{REPO}/releases/latest"),
    };
    let (status, body) = get(&url, "application/vnd.github+json").context("GitHub")?;
    if status == 404 {
        return Ok(Outcome::UpToDate); // no releases yet
    }
    if !(200..300).contains(&status) {
        bail!("GitHub: HTTP {status}");
    }
    let release: serde_json::Value = serde_json::from_slice(&body).context("GitHub answer")?;
    let tag = release["tag_name"].as_str().unwrap_or_default();
    if !newer(tag, VERSION) {
        return Ok(Outcome::UpToDate);
    }
    let version = tag.trim_start_matches('v').to_string();

    if staged_exe(exe).exists() && std::fs::read_to_string(staged_note(exe)).ok().as_deref() == Some(version.as_str()) {
        return Ok(Outcome::Staged { version }); // already downloaded earlier
    }

    let needs = download(&release, "update.json")
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|manifest| manifest["ffmpeg"].as_str().map(String::from));
    if needs.as_deref() != Some(FFMPEG_SET) {
        let page = release["html_url"]
            .as_str()
            .filter(|u| u.starts_with("https://github.com/"))
            .map(String::from)
            .unwrap_or_else(|| format!("https://github.com/{REPO}/releases"));
        return Ok(Outcome::Manual { version, page });
    }

    let file = download(&release, "rewind.exe")?;
    let wanted = String::from_utf8_lossy(&download(&release, "rewind.exe.sha256")?)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase();
    let actual: String = Sha256::digest(&file).iter().map(|b| format!("{b:02x}")).collect();
    if wanted.len() != 64 || wanted != actual {
        bail!("the downloaded file does not match its checksum");
    }

    let partial = PathBuf::from(format!("{}.part", staged_exe(exe).display()));
    std::fs::write(&partial, &file).context("saving the update")?;
    let _ = std::fs::remove_file(staged_exe(exe));
    std::fs::rename(&partial, staged_exe(exe)).context("saving the update")?;
    std::fs::write(staged_note(exe), &version).context("saving the update")?;
    Ok(Outcome::Staged { version })
}

/// The version waiting to be swapped in, if any.
pub fn staged(exe: &Path) -> Option<String> {
    staged_exe(exe).exists().then(|| std::fs::read_to_string(staged_note(exe)).unwrap_or_default())
}

/// At start: removes what the previous update left behind.
pub fn cleanup(exe: &Path) {
    let _ = std::fs::remove_file(old_exe(exe));
}

/// At start: puts a downloaded update in place. The running exe is renamed (Windows allows
/// that) and the new one takes its name. Returns true when the caller should relaunch.
pub fn swap_in(exe: &Path) -> Result<bool> {
    let new = staged_exe(exe);
    if !new.exists() {
        return Ok(false);
    }
    let old = old_exe(exe);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).context("moving the current exe aside")?;
    if let Err(e) = std::fs::rename(&new, exe) {
        let _ = std::fs::rename(&old, exe); // put it back: better the old version than none
        return Err(e).context("putting the new exe in place");
    }
    let _ = std::fs::remove_file(staged_note(exe));
    Ok(true)
}

/// Waits (up to 15 s) until the process that launched us has gone.
pub fn wait_for(pid: u32) {
    unsafe {
        if let Ok(process) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            WaitForSingleObject(process, 15_000);
            let _ = CloseHandle(process);
        }
    }
}

/// Starts the exe again, telling it to wait for this process to finish first.
pub fn relaunch(exe: &Path, args: &[String]) -> Result<()> {
    std::process::Command::new(exe)
        .args(args)
        .arg("--wait-pid")
        .arg(std::process::id().to_string())
        .spawn()
        .context("restarting")?;
    Ok(())
}
