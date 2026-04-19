use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use image::imageops::FilterType;
use serde::Serialize;
use std::{
    env,
    fs,
    io::Cursor,
    net::SocketAddr,
    path::{Path as StdPath, PathBuf},
    process::Command,
    sync::Arc,
};
use tokio::net::TcpListener;

#[derive(Clone)]
struct AppState {
    root: PathBuf,
    jpeg_dir: PathBuf,
    raw_dir: Option<PathBuf>,
    thumb_cache: PathBuf,
    trash_dir: PathBuf,
}

const INDEX_HTML: &str = include_str!("index.html");

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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: {} <root-folder>", args[0]);
        eprintln!("       root-folder should contain a 'jpegs' subfolder (optionally a 'raws' one)");
        std::process::exit(1);
    }
    let root = PathBuf::from(&args[1]).canonicalize()?;
    if !root.is_dir() {
        eprintln!("error: {} is not a directory", root.display());
        std::process::exit(1);
    }

    let jpeg_dir = find_subdir_ci(&root, &["jpegs", "jpeg", "jpg"])
        .ok_or("no 'jpegs' subfolder found")?;
    let raw_dir = find_subdir_ci(&root, &["raws", "raw", "arw"]);
    let thumb_cache = root.join(".thumb_cache");
    let trash_dir = root.join("_deleted");
    fs::create_dir_all(&thumb_cache)?;

    println!("root:   {}", root.display());
    println!("jpegs:  {}", jpeg_dir.display());
    if let Some(r) = &raw_dir {
        println!("raws:   {}", r.display());
    } else {
        println!("raws:   (none found)");
    }

    let state = Arc::new(AppState {
        root: root.clone(),
        jpeg_dir,
        raw_dir,
        thumb_cache,
        trash_dir,
    });

    let app = Router::new()
        .route("/", get(index))
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

#[derive(Serialize)]
struct PhotosResponse {
    files: Vec<String>,
    root: String,
    raw_available: bool,
}

async fn list_photos(State(state): State<Arc<AppState>>) -> Json<PhotosResponse> {
    let mut files: Vec<String> = fs::read_dir(&state.jpeg_dir)
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
    Json(PhotosResponse {
        files,
        root: state.root.display().to_string(),
        raw_available: state.raw_dir.is_some(),
    })
}

fn safe_name(name: &str) -> Option<&str> {
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        None
    } else {
        Some(name)
    }
}

async fn serve_full(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let Some(safe) = safe_name(&name) else {
        return (StatusCode::BAD_REQUEST, "bad name").into_response();
    };
    let p = state.jpeg_dir.join(safe);
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

async fn serve_thumb(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let Some(safe) = safe_name(&name) else {
        return (StatusCode::BAD_REQUEST, "bad name").into_response();
    };
    let src = state.jpeg_dir.join(safe);
    let thumb_path = state.thumb_cache.join(format!("{}.thumb.jpg", safe));

    // serve cached if present and newer than source
    let fresh = match (fs::metadata(&thumb_path), fs::metadata(&src)) {
        (Ok(tm), Ok(sm)) => tm.modified().ok() >= sm.modified().ok(),
        _ => false,
    };

    if !fresh {
        let src_c = src.clone();
        let thumb_c = thumb_path.clone();
        let res = tokio::task::spawn_blocking(move || generate_thumb(&src_c, &thumb_c)).await;
        if let Err(e) = res {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("join error: {e}")).into_response();
        }
        if let Ok(Err(e)) = res {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("thumb error: {e}")).into_response();
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

async fn delete_photo(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let Some(safe) = safe_name(&name) else {
        return (StatusCode::BAD_REQUEST, "bad name").into_response();
    };
    if let Err(e) = fs::create_dir_all(&state.trash_dir) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("trash: {e}")).into_response();
    }

    let jpeg_src = state.jpeg_dir.join(safe);
    let mut moved: Vec<String> = Vec::new();

    if jpeg_src.exists() {
        let dst = state.trash_dir.join(safe);
        if fs::rename(&jpeg_src, &dst).is_ok() {
            moved.push(safe.to_string());
        } else {
            return (StatusCode::INTERNAL_SERVER_ERROR, "rename jpg failed").into_response();
        }
    }

    // move matching raw (same stem, common RAW extensions)
    if let Some(raw_dir) = &state.raw_dir {
        if let Some(stem) = StdPath::new(safe).file_stem().and_then(|s| s.to_str()) {
            for ext in ["ARW", "arw", "CR2", "cr2", "NEF", "nef", "DNG", "dng", "RAF", "raf"] {
                let raw_src = raw_dir.join(format!("{}.{}", stem, ext));
                if raw_src.exists() {
                    let raw_dst = state.trash_dir.join(format!("{}.{}", stem, ext));
                    if fs::rename(&raw_src, &raw_dst).is_ok() {
                        moved.push(format!("{}.{}", stem, ext));
                    }
                    break;
                }
            }
        }
    }

    // invalidate thumb cache
    let _ = fs::remove_file(state.thumb_cache.join(format!("{}.thumb.jpg", safe)));

    Json(DeleteResult { deleted: moved, ok: true }).into_response()
}
