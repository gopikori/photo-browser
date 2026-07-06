use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use image::imageops::FilterType;
use serde::{Deserialize, Serialize};
use std::{
    env,
    fs,
    io::Cursor,
    net::SocketAddr,
    path::{Path as StdPath, PathBuf},
    process::Command,
    sync::{Arc, RwLock},
};
use tokio::net::TcpListener;

/// A loaded photo folder. Swapped at runtime when the user picks a folder in the UI.
#[derive(Clone)]
struct Session {
    root: PathBuf,
    jpeg_dir: PathBuf,
    raw_dir: Option<PathBuf>,
    video_dir: Option<PathBuf>,
    thumb_cache: PathBuf,
    trash_dir: PathBuf,
}

struct AppState {
    session: RwLock<Option<Session>>,
}

const INDEX_HTML: &str = include_str!("index.html");

const JPEG_EXTS: &[&str] = &["jpg", "jpeg"];
const RAW_EXTS: &[&str] = &[
    "arw", "cr2", "cr3", "nef", "dng", "raf", "orf", "rw2", "srw", "pef", "x3f",
];
const VIDEO_EXTS: &[&str] = &[
    "mp4", "mov", "m4v", "avi", "mts", "m2ts", "mkv", "3gp", "wmv", "webm",
];

fn home_dir() -> PathBuf {
    env::var("HOME")
        .or_else(|_| env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

/// The folder the picker opens to by default: ~/Downloads, falling back to ~.
fn default_browse_dir() -> PathBuf {
    let dl = home_dir().join("Downloads");
    if dl.is_dir() {
        dl
    } else {
        home_dir()
    }
}

fn ext_lower(p: &StdPath) -> String {
    p.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
}

fn find_subdir_ci(root: &StdPath, names: &[&str]) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let n = entry.file_name().to_string_lossy().to_lowercase();
        if names.iter().any(|t| &n == t) && entry.file_type().ok()?.is_dir() {
            return Some(entry.path());
        }
    }
    None
}

#[derive(Serialize, Clone, Copy, Default)]
struct SortReport {
    jpegs: usize,
    raws: usize,
    videos: usize,
}

/// Move loose JPEG / RAW / video files in `root` into `jpegs/`, `raws/`, `videos/`
/// subfolders. Idempotent: already-sorted folders and files in subfolders are left
/// untouched, and a name collision skips the move rather than overwriting.
fn sort_folder(root: &StdPath) -> Result<SortReport, String> {
    let root_name = root
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    // Collect first so we don't mutate the directory while iterating it.
    let mut moves: Vec<(PathBuf, &'static str)> = Vec::new();
    for entry in fs::read_dir(root).map_err(|e| format!("read {}: {e}", root.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.file_type().map_err(|e| e.to_string())?.is_file() {
            continue;
        }
        let path = entry.path();
        let ext = ext_lower(&path);
        let sub = if JPEG_EXTS.contains(&ext.as_str()) {
            "jpegs"
        } else if RAW_EXTS.contains(&ext.as_str()) {
            "raws"
        } else if VIDEO_EXTS.contains(&ext.as_str()) {
            "videos"
        } else {
            continue;
        };
        // If the folder itself is already a category folder, leave files where they are.
        if root_name == sub {
            continue;
        }
        moves.push((path, sub));
    }

    let mut report = SortReport::default();
    for (src, sub) in moves {
        let dir = root.join(sub);
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let Some(fname) = src.file_name() else { continue };
        let dst = dir.join(fname);
        if dst.exists() {
            continue; // don't clobber an existing file with the same name
        }
        if fs::rename(&src, &dst).is_ok() {
            match sub {
                "jpegs" => report.jpegs += 1,
                "raws" => report.raws += 1,
                "videos" => report.videos += 1,
                _ => {}
            }
        }
    }
    Ok(report)
}

/// Sort `root`, then resolve its photo/raw/video subfolders into a `Session`.
fn build_session(root: &StdPath) -> Result<(Session, SortReport), String> {
    let report = sort_folder(root)?;
    let jpeg_dir =
        find_subdir_ci(root, &["jpegs", "jpeg", "jpg"]).unwrap_or_else(|| root.join("jpegs"));
    let raw_dir = find_subdir_ci(root, &["raws", "raw", "arw"]);
    let video_dir = find_subdir_ci(root, &["videos", "video", "movies"]);
    let thumb_cache = root.join(".thumb_cache");
    let trash_dir = root.join("_deleted");
    fs::create_dir_all(&thumb_cache).map_err(|e| e.to_string())?;
    Ok((
        Session {
            root: root.to_path_buf(),
            jpeg_dir,
            raw_dir,
            video_dir,
            thumb_cache,
            trash_dir,
        },
        report,
    ))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A folder argument is optional now — without it, the user picks a folder in the UI.
    let args: Vec<String> = env::args().collect();
    let initial = if args.len() >= 2 {
        match PathBuf::from(&args[1]).canonicalize() {
            Ok(root) if root.is_dir() => match build_session(&root) {
                Ok((session, rep)) => {
                    println!(
                        "loaded {} (jpegs:{} raws:{} videos:{})",
                        root.display(),
                        rep.jpegs,
                        rep.raws,
                        rep.videos
                    );
                    Some(session)
                }
                Err(e) => {
                    eprintln!("warning: could not open {}: {e}", root.display());
                    None
                }
            },
            _ => {
                eprintln!("warning: {} is not a directory — starting with the folder picker", args[1]);
                None
            }
        }
    } else {
        None
    };

    let state = Arc::new(AppState {
        session: RwLock::new(initial),
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/pick", post(pick_folder))
        .route("/api/open", post(open_folder))
        .route("/api/rescan", post(rescan))
        .route("/api/photos", get(list_photos))
        .route("/api/image/:name", get(serve_full))
        .route("/api/thumb/:name", get(serve_thumb))
        .route("/api/delete/:name", post(delete_photo))
        .with_state(state);

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr: SocketAddr = listener.local_addr()?;
    let url = format!("http://{}", addr);
    println!("\n  photo-browser ready → {}\n", url);

    // try to open in the default browser (best-effort)
    let _ = Command::new("open").arg(&url).spawn();

    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

fn current(state: &AppState) -> Option<Session> {
    state.session.read().unwrap().clone()
}

// ---------- native folder picker ----------

/// Pop the OS folder-picker dialog and return the chosen path, or `None` if the
/// user cancels. Uses the platform's standard scripting hook so we don't take on
/// a GUI dependency (osascript/zenity/kdialog/PowerShell are already present).
fn pick_folder_native(start_dir: &StdPath) -> Result<Option<PathBuf>, String> {
    let start = start_dir.display().to_string();

    #[cfg(target_os = "macos")]
    let out = {
        let script = format!(
            "POSIX path of (choose folder with prompt \"Pick a photo folder to cull\" default location POSIX file \"{}\")",
            start.replace('\\', "\\\\").replace('"', "\\\"")
        );
        Command::new("osascript")
            .args(["-e", &script])
            .output()
            .map_err(|e| format!("osascript failed: {e}"))?
    };

    #[cfg(target_os = "linux")]
    let out = {
        let zenity = Command::new("zenity")
            .args([
                "--file-selection",
                "--directory",
                "--title=Pick a photo folder to cull",
            ])
            .arg(format!("--filename={}/", start))
            .output();
        match zenity {
            Ok(o) => o,
            Err(_) => Command::new("kdialog")
                .args(["--getexistingdirectory", &start])
                .output()
                .map_err(|e| {
                    format!("no native folder dialog found (install zenity or kdialog): {e}")
                })?,
        }
    };

    #[cfg(target_os = "windows")]
    let out = {
        let script = format!(
            "Add-Type -AssemblyName System.Windows.Forms; \
             $d = New-Object System.Windows.Forms.FolderBrowserDialog; \
             $d.Description = 'Pick a photo folder to cull'; \
             $d.SelectedPath = '{}'; \
             if ($d.ShowDialog() -eq 'OK') {{ Write-Output $d.SelectedPath }}",
            start.replace('\'', "''")
        );
        Command::new("powershell")
            .args(["-NoProfile", "-STA", "-Command", &script])
            .output()
            .map_err(|e| format!("powershell failed: {e}"))?
    };

    // User cancelled → non-zero exit or empty stdout. Treat both as "no pick".
    if !out.status.success() {
        return Ok(None);
    }
    let picked = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if picked.is_empty() {
        return Ok(None);
    }
    // macOS `choose folder` returns paths with a trailing slash; strip it.
    let picked = picked.trim_end_matches('/').to_string();
    Ok(Some(PathBuf::from(picked)))
}

#[derive(Serialize)]
struct PickResponse {
    ok: bool,
    path: Option<String>,
}

async fn pick_folder() -> Response {
    let start = default_browse_dir();
    let res = tokio::task::spawn_blocking(move || pick_folder_native(&start)).await;
    match res {
        Ok(Ok(Some(p))) => Json(PickResponse {
            ok: true,
            path: Some(p.display().to_string()),
        })
        .into_response(),
        Ok(Ok(None)) => Json(PickResponse {
            ok: true,
            path: None,
        })
        .into_response(),
        Ok(Err(e)) => json_err(e),
        Err(e) => json_err(format!("task failed: {e}")),
    }
}

// ---------- open (sort + activate) ----------

#[derive(Deserialize)]
struct OpenRequest {
    path: String,
}

#[derive(Serialize)]
struct OpenResponse {
    ok: bool,
    report: SortReport,
    photos: PhotosResponse,
}

fn json_err(msg: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "ok": false, "error": msg.into() })),
    )
        .into_response()
}

async fn open_folder(
    State(state): State<Arc<AppState>>,
    Json(req): Json<OpenRequest>,
) -> Response {
    let root = match PathBuf::from(&req.path).canonicalize() {
        Ok(r) => r,
        Err(e) => return json_err(format!("invalid path: {e}")),
    };
    if !root.is_dir() {
        return json_err("not a directory");
    }

    // Sorting touches the filesystem — run it off the async runtime.
    let built = tokio::task::spawn_blocking(move || build_session(&root)).await;
    let (session, report) = match built {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return json_err(e),
        Err(e) => return json_err(format!("task failed: {e}")),
    };

    let photos = photos_response(&session);
    *state.session.write().unwrap() = Some(session);

    Json(OpenResponse {
        ok: true,
        report,
        photos,
    })
    .into_response()
}

/// Re-sort loose files in the current session's root and rebuild the photo list.
/// Called from the ⟳ Refresh button after the user drops new JPEGs/RAWs into the folder.
async fn rescan(State(state): State<Arc<AppState>>) -> Response {
    let Some(session) = current(&state) else {
        return json_err("no folder open");
    };
    let root = session.root.clone();

    let built = tokio::task::spawn_blocking(move || build_session(&root)).await;
    let (session, report) = match built {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return json_err(e),
        Err(e) => return json_err(format!("task failed: {e}")),
    };

    let photos = photos_response(&session);
    *state.session.write().unwrap() = Some(session);

    Json(OpenResponse {
        ok: true,
        report,
        photos,
    })
    .into_response()
}

// ---------- photos ----------

#[derive(Serialize, Clone)]
struct PhotosResponse {
    loaded: bool,
    files: Vec<String>,
    root: String,
    raw_available: bool,
    video_available: bool,
}

fn photos_response(session: &Session) -> PhotosResponse {
    let mut files: Vec<String> = fs::read_dir(&session.jpeg_dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().to_string();
                    let up = n.to_uppercase();
                    if up.ends_with(".JPG") || up.ends_with(".JPEG") {
                        Some(n)
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    PhotosResponse {
        loaded: true,
        files,
        root: session.root.display().to_string(),
        raw_available: session.raw_dir.is_some(),
        video_available: session.video_dir.is_some(),
    }
}

async fn list_photos(State(state): State<Arc<AppState>>) -> Json<PhotosResponse> {
    match current(&state) {
        Some(s) => Json(photos_response(&s)),
        None => Json(PhotosResponse {
            loaded: false,
            files: Vec::new(),
            root: String::new(),
            raw_available: false,
            video_available: false,
        }),
    }
}

fn safe_name(name: &str) -> Option<&str> {
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        None
    } else {
        Some(name)
    }
}

async fn serve_full(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    let Some(session) = current(&state) else {
        return (StatusCode::CONFLICT, "no folder selected").into_response();
    };
    let Some(safe) = safe_name(&name) else {
        return (StatusCode::BAD_REQUEST, "bad name").into_response();
    };
    let p = session.jpeg_dir.join(safe);
    match tokio::fs::read(&p).await {
        Ok(bytes) => {
            let mime = mime_guess::from_path(&p).first_or_octet_stream();
            Response::builder()
                .header(header::CONTENT_TYPE, mime.as_ref())
                .header(header::CACHE_CONTROL, "public, max-age=3600")
                .body(Body::from(bytes))
                .unwrap()
        }
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

async fn serve_thumb(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    let Some(session) = current(&state) else {
        return (StatusCode::CONFLICT, "no folder selected").into_response();
    };
    let Some(safe) = safe_name(&name) else {
        return (StatusCode::BAD_REQUEST, "bad name").into_response();
    };
    let src = session.jpeg_dir.join(safe);
    let thumb_path = session.thumb_cache.join(format!("{}.thumb.jpg", safe));

    // serve cached if present and newer than source
    let fresh = match (fs::metadata(&thumb_path), fs::metadata(&src)) {
        (Ok(tm), Ok(sm)) => tm.modified().ok() >= sm.modified().ok(),
        _ => false,
    };

    if !fresh {
        let src_c = src.clone();
        let thumb_c = thumb_path.clone();
        let res = tokio::task::spawn_blocking(move || generate_thumb(&src_c, &thumb_c)).await;
        match res {
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("join error: {e}")).into_response(),
            Ok(Err(e)) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("thumb error: {e}")).into_response(),
            Ok(Ok(())) => {}
        }
    }

    match tokio::fs::read(&thumb_path).await {
        Ok(bytes) => Response::builder()
            .header(header::CONTENT_TYPE, "image/jpeg")
            .header(header::CACHE_CONTROL, "public, max-age=86400")
            .body(Body::from(bytes))
            .unwrap(),
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

fn generate_thumb(src: &StdPath, dst: &StdPath) -> Result<(), String> {
    let img = image::open(src).map_err(|e| e.to_string())?;
    let thumb = img.resize(512, 512, FilterType::Triangle);
    let mut buf: Vec<u8> = Vec::new();
    thumb
        .to_rgb8()
        .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Jpeg)
        .map_err(|e| e.to_string())?;
    fs::write(dst, buf).map_err(|e| e.to_string())?;
    Ok(())
}

#[derive(Serialize)]
struct DeleteResult {
    deleted: Vec<String>,
    ok: bool,
}

async fn delete_photo(State(state): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    let Some(session) = current(&state) else {
        return (StatusCode::CONFLICT, "no folder selected").into_response();
    };
    let Some(safe) = safe_name(&name) else {
        return (StatusCode::BAD_REQUEST, "bad name").into_response();
    };
    if let Err(e) = fs::create_dir_all(&session.trash_dir) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("trash: {e}")).into_response();
    }

    let jpeg_src = session.jpeg_dir.join(safe);
    let mut moved: Vec<String> = Vec::new();

    if jpeg_src.exists() {
        let dst = session.trash_dir.join(safe);
        if fs::rename(&jpeg_src, &dst).is_ok() {
            moved.push(safe.to_string());
        } else {
            return (StatusCode::INTERNAL_SERVER_ERROR, "rename jpg failed").into_response();
        }
    }

    // move matching raw (same stem, common RAW extensions)
    if let Some(raw_dir) = &session.raw_dir {
        if let Some(stem) = StdPath::new(safe).file_stem().and_then(|s| s.to_str()) {
            for ext in ["ARW", "arw", "CR2", "cr2", "NEF", "nef", "DNG", "dng", "RAF", "raf"] {
                let raw_src = raw_dir.join(format!("{}.{}", stem, ext));
                if raw_src.exists() {
                    let raw_dst = session.trash_dir.join(format!("{}.{}", stem, ext));
                    if fs::rename(&raw_src, &raw_dst).is_ok() {
                        moved.push(format!("{}.{}", stem, ext));
                    }
                    break;
                }
            }
        }
    }

    // invalidate thumb cache
    let _ = fs::remove_file(session.thumb_cache.join(format!("{}.thumb.jpg", safe)));

    Json(DeleteResult {
        deleted: moved,
        ok: true,
    })
    .into_response()
}
