use actix_multipart::Multipart;
use actix_web::{http::header, web, App, Error, HttpResponse, HttpServer, Responder, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use config::{Config as ConfigFile, Environment, File};
use deadpool_postgres::{Pool, Runtime};
use futures::{StreamExt, TryStreamExt};
use log::{error, info, warn};
use lru::LruCache;
use serde::Deserialize;
use serde_json::json;
use std::num::NonZeroUsize;
use std::sync::Mutex;
use tokio_postgres::NoTls;
use uuid::Uuid;

const CACHE_SIZE: usize = 10_000;
const ICON_AND_AVATARS_TABLE: &str = "iconAndAvatars";
const MAX_UPLOAD_BYTES: usize = 50 * 1024 * 1024;

#[derive(Debug, Deserialize, Clone)]
struct Settings {
    server: ServerSettings,
    database: DatabaseSettings,
}

#[derive(Debug, Deserialize, Clone)]
struct ServerSettings {
    hostname: String,
    port: u16,
}

#[derive(Debug, Deserialize, Clone)]
struct DatabaseSettings {
    url: String,
}

struct AppState {
    images: Mutex<LruCache<String, Vec<u8>>>,
    db_pool: Pool,
}

#[derive(Deserialize)]
struct Base64Payload {
    base64: String,
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn redirect(location: &str) -> HttpResponse {
    HttpResponse::Found()
        .insert_header((header::LOCATION, location))
        .finish()
}

fn detect_content_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() < 4 {
        return None;
    }
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn render_layout(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>{title}</title>
    <style>
      :root {{
        --bg: #f3efe5;
        --paper: rgba(255, 251, 242, 0.9);
        --ink: #1b1a17;
        --muted: #6c6557;
        --line: rgba(27, 26, 23, 0.12);
        --brand: #c24b2d;
        --brand-dark: #7f2714;
        --good: #1f6f43;
        --bad: #9d2d2d;
        --shadow: 0 20px 60px rgba(68, 43, 24, 0.15);
      }}
      * {{ box-sizing: border-box; }}
      body {{
        margin: 0;
        font-family: Georgia, "Times New Roman", serif;
        color: var(--ink);
        background:
          radial-gradient(circle at top left, rgba(194, 75, 45, 0.18), transparent 30%),
          radial-gradient(circle at bottom right, rgba(31, 111, 67, 0.16), transparent 28%),
          linear-gradient(135deg, #f7f0de, #efe6d2 55%, #e9dfca);
        min-height: 100vh;
      }}
      a {{ color: var(--brand-dark); }}
      .shell {{ max-width: 1180px; margin: 0 auto; padding: 40px 24px 64px; }}
      .hero {{ display: flex; justify-content: space-between; gap: 24px; align-items: flex-start; margin-bottom: 28px; }}
      .hero h1 {{ margin: 0; font-size: clamp(2rem, 4vw, 3.4rem); line-height: 0.98; letter-spacing: -0.04em; }}
      .hero p {{ margin: 10px 0 0; max-width: 760px; color: var(--muted); font-size: 1.05rem; }}
      .paper {{
        background: var(--paper);
        border: 1px solid var(--line);
        border-radius: 24px;
        box-shadow: var(--shadow);
        backdrop-filter: blur(10px);
      }}
      .card {{ padding: 24px; }}
      .grid {{ display: grid; gap: 18px; }}
      .grid.two {{ grid-template-columns: repeat(auto-fit, minmax(280px, 1fr)); }}
      .grid.three {{ grid-template-columns: repeat(auto-fit, minmax(220px, 1fr)); }}
      .metric {{ padding: 20px; border-radius: 20px; background: rgba(255, 255, 255, 0.62); border: 1px solid rgba(27, 26, 23, 0.08); }}
      .metric span {{ display: block; font-size: 0.9rem; color: var(--muted); margin-bottom: 8px; text-transform: uppercase; letter-spacing: 0.08em; }}
      .metric strong {{ font-size: 2rem; line-height: 1; }}
      form {{ display: grid; gap: 14px; }}
      label {{ font-size: 0.95rem; color: var(--muted); display: grid; gap: 6px; }}
      input, textarea, select {{
        width: 100%;
        border: 1px solid rgba(27, 26, 23, 0.16);
        border-radius: 14px;
        padding: 14px 16px;
        background: rgba(255,255,255,0.88);
        color: var(--ink);
        font: inherit;
      }}
      textarea {{ resize: vertical; min-height: 120px; }}
      button {{
        border: 0;
        border-radius: 999px;
        background: linear-gradient(135deg, var(--brand), #d9823b);
        color: #fff;
        padding: 14px 18px;
        font: inherit;
        font-weight: 700;
        cursor: pointer;
      }}
      button.secondary {{ background: linear-gradient(135deg, #384d42, #58725d); }}
      .notice {{ padding: 14px 16px; border-radius: 16px; margin-bottom: 16px; font-size: 0.96rem; }}
      .notice.info {{ background: rgba(31, 111, 67, 0.1); color: var(--good); border: 1px solid rgba(31, 111, 67, 0.18); }}
      .notice.error {{ background: rgba(157, 45, 45, 0.12); color: var(--bad); border: 1px solid rgba(157, 45, 45, 0.18); }}
      .table {{ width: 100%; border-collapse: collapse; }}
      .table th, .table td {{ padding: 10px 0; border-bottom: 1px solid rgba(27,26,23,0.08); text-align: left; vertical-align: top; }}
      .table th {{ font-size: 0.84rem; color: var(--muted); text-transform: uppercase; letter-spacing: 0.08em; }}
      .mono {{ font-family: "SFMono-Regular", Consolas, monospace; font-size: 0.92rem; }}
      .stack {{ display: grid; gap: 12px; }}
      .split {{ display: flex; justify-content: space-between; align-items: center; gap: 16px; flex-wrap: wrap; }}
      .small {{ font-size: 0.92rem; color: var(--muted); }}
      .tabs {{ display: flex; gap: 0; border-bottom: 2px solid var(--line); margin-bottom: 18px; }}
      .tab {{ padding: 10px 20px; cursor: pointer; border: none; background: none; font: inherit; color: var(--muted); border-bottom: 2px solid transparent; margin-bottom: -2px; }}
      .tab.active {{ color: var(--brand); border-bottom-color: var(--brand); font-weight: 700; }}
      .tab-panel {{ display: none; }}
      .tab-panel.active {{ display: block; }}
      .img-grid {{ display: grid; grid-template-columns: repeat(auto-fill, minmax(120px, 1fr)); gap: 12px; }}
      .img-grid a {{ display: block; border-radius: 14px; overflow: hidden; border: 1px solid var(--line); aspect-ratio: 1; background: #fff; }}
      .img-grid img {{ width: 100%; height: 100%; object-fit: cover; }}
      .badge {{ display: inline-block; padding: 3px 10px; border-radius: 999px; font-size: 0.82rem; font-weight: 700; }}
      .badge.cache {{ background: rgba(194, 75, 45, 0.12); color: var(--brand); }}
      .badge.db {{ background: rgba(31, 111, 67, 0.12); color: var(--good); }}
      .result-box {{ padding: 18px; border-radius: 16px; background: rgba(31, 111, 67, 0.08); border: 1px solid rgba(31, 111, 67, 0.16); word-break: break-all; }}
      nav {{ display: flex; gap: 18px; align-items: center; }}
      nav a {{ text-decoration: none; font-weight: 700; }}
      @media (max-width: 720px) {{
        .shell {{ padding: 22px 16px 48px; }}
        .hero {{ flex-direction: column; }}
      }}
    </style>
  </head>
  <body>
    <div class="shell">{body}</div>
  </body>
</html>"#,
        title = escape_html(title),
        body = body
    )
}

fn render_notice(message: Option<&str>, level: &str) -> String {
    match message {
        Some(message) => format!(
            "<div class=\"notice {}\">{}</div>",
            level,
            escape_html(message)
        ),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

fn render_index_page(cache_count: usize, db_count: i64) -> String {
    render_layout(
        "Base64 Image Service",
        &format!(r#"
<section class="hero">
  <div>
    <h1>Base64 Image Service</h1>
    <p>Upload images or base64 data, explore cached entries, and retrieve images by UUID or MD5 hash.</p>
  </div>
  <nav class="paper card">
    <a href="/upload">Upload</a>
    <a href="/explore">Explore</a>
  </nav>
</section>
<section class="grid three" style="margin-bottom: 18px;">
  <article class="paper metric"><span>Cache Entries</span><strong>{}</strong></article>
  <article class="paper metric"><span>DB Images</span><strong>{}</strong></article>
  <article class="paper metric"><span>Cache Capacity</span><strong>{}</strong></article>
</section>
<section class="paper card stack">
  <h2 style="margin:0;">API Endpoints</h2>
  <div class="grid two">
    <div class="metric"><span>Upload</span><strong class="mono">POST /image</strong><div class="small">JSON or form body with a <span class="mono">base64</span> field.</div></div>
    <div class="metric"><span>Multipart</span><strong class="mono">POST /image/multipart</strong><div class="small">Multipart form upload with a <span class="mono">base64</span> field.</div></div>
    <div class="metric"><span>Retrieve</span><strong class="mono">GET /image/{{id}}</strong><div class="small">Reads from the in-memory cache.</div></div>
    <div class="metric"><span>Lookup</span><strong class="mono">GET /md5/{{hash}}</strong><div class="small">Reads the image payload from PostgreSQL by MD5 hash.</div></div>
  </div>
</section>
"#,
            cache_count,
            db_count,
            CACHE_SIZE,
        ),
    )
}

fn render_upload_page(result: Option<&str>, error: Option<&str>) -> String {
    render_layout(
        "Upload Image",
        &format!(r#"
<section class="hero">
  <div>
    <h1>Upload</h1>
    <p>Upload an image file (PNG, JPG, WebP, GIF) or paste raw base64 data. The image will be cached and you will receive a URL to retrieve it.</p>
  </div>
  <nav class="paper card">
    <a href="/">Home</a>
    <a href="/explore">Explore</a>
  </nav>
</section>
{error_html}
{result_html}
<section class="paper card">
  <div class="tabs">
    <button class="tab active" onclick="switchTab('file')">Upload File</button>
    <button class="tab" onclick="switchTab('base64')">Paste Base64</button>
  </div>
  <div id="tab-file" class="tab-panel active">
    <form method="post" action="/upload" enctype="multipart/form-data">
      <input type="hidden" name="mode" value="file">
      <label>Image file<input type="file" name="file" accept="image/png,image/jpeg,image/webp,image/gif" required></label>
      <button type="submit">Upload File</button>
    </form>
  </div>
  <div id="tab-base64" class="tab-panel">
    <form method="post" action="/upload" enctype="multipart/form-data">
      <input type="hidden" name="mode" value="base64">
      <label>Base64 data<textarea name="base64" placeholder="Paste your base64 encoded image data here..." required></textarea></label>
      <button type="submit">Upload Base64</button>
    </form>
  </div>
</section>
<script>
function switchTab(name) {{
  document.querySelectorAll('.tab').forEach(function(t) {{ t.classList.remove('active'); }});
  document.querySelectorAll('.tab-panel').forEach(function(p) {{ p.classList.remove('active'); }});
  event.target.classList.add('active');
  document.getElementById('tab-' + name).classList.add('active');
}}
</script>
"#,
            error_html = render_notice(error, "error"),
            result_html = match result {
                Some(url) => format!(
                    "<div class=\"result-box\" style=\"margin-bottom:18px\"><strong>Image stored!</strong> Retrieve it at: <a href=\"{}\">{}</a></div>",
                    escape_html(url), escape_html(url)
                ),
                None => String::new(),
            },
        ),
    )
}

struct ExploreCache {
    id: String,
}

struct ExploreDb {
    hash: String,
    meta_name: String,
    saved_at: String,
}

fn render_explore_page(
    cache_entries: &[ExploreCache],
    db_entries: &[ExploreDb],
) -> String {
    let cache_html = if cache_entries.is_empty() {
        "<p class=\"small\">No images in cache right now.</p>".to_string()
    } else {
        let items = cache_entries
            .iter()
            .map(|e| {
                format!(
                    "<a href=\"/image/{}\" title=\"{}\"><img src=\"/image/{}\" loading=\"lazy\" alt=\"cached\"></a>",
                    escape_html(&e.id),
                    escape_html(&e.id),
                    escape_html(&e.id)
                )
            })
            .collect::<Vec<_>>()
            .join("");
        format!("<div class=\"img-grid\">{}</div>", items)
    };

    let db_html = if db_entries.is_empty() {
        "<p class=\"small\">No database-backed images found.</p>".to_string()
    } else {
        let rows = db_entries
            .iter()
            .map(|e| {
                format!(
                    "<tr><td><a href=\"/md5/{}\" class=\"mono\">{}</a></td><td>{}</td><td class=\"mono\">{}</td></tr>",
                    escape_html(&e.hash),
                    escape_html(&e.hash),
                    escape_html(&e.meta_name),
                    escape_html(&e.saved_at)
                )
            })
            .collect::<Vec<_>>()
            .join("");
        format!(
            "<table class=\"table\"><thead><tr><th>Hash</th><th>Name</th><th>Saved</th></tr></thead><tbody>{}</tbody></table>",
            rows
        )
    };

    render_layout(
        "Explore Images",
        &format!(r#"
<section class="hero">
  <div>
    <h1>Explore</h1>
    <p>Browse images currently held in the in-memory cache and database-backed image records.</p>
  </div>
  <nav class="paper card">
    <a href="/">Home</a>
    <a href="/upload">Upload</a>
  </nav>
</section>
<section class="grid two">
  <article class="paper card stack">
    <div class="split">
      <h2 style="margin:0;">In-Memory Cache</h2>
      <span class="badge cache">{} entries</span>
    </div>
    {}
  </article>
  <article class="paper card stack">
    <div class="split">
      <h2 style="margin:0;">Database Images</h2>
      <span class="badge db">{} records</span>
    </div>
    {}
  </article>
</section>
"#,
            cache_entries.len(),
            cache_html,
            db_entries.len(),
            db_html,
        ),
    )
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn index(data: web::Data<AppState>) -> impl Responder {
    let cache_count = data.images.lock().unwrap().len();

    let db_count: i64 = match data.db_pool.get().await {
        Ok(client) => {
            client
                .query_one(
                    &format!("SELECT COUNT(*) FROM \"{}\"", ICON_AND_AVATARS_TABLE),
                    &[],
                )
                .await
                .map(|row| row.get(0))
                .unwrap_or(0)
        }
        Err(_) => 0,
    };

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(render_index_page(cache_count, db_count))
}

async fn explore_page(data: web::Data<AppState>) -> Result<impl Responder, Error> {
    let cache_entries: Vec<ExploreCache> = {
        let cache = data.images.lock().unwrap();
        cache
            .iter()
            .map(|(id, _)| ExploreCache { id: id.clone() })
            .collect()
    };

    let client = data.db_pool.get().await.map_err(|e| {
        error!("DB connection failed for explore: {}", e);
        actix_web::error::ErrorInternalServerError("Database connection failed")
    })?;

    let db_entries: Vec<ExploreDb> = client
        .query(
            &format!(
                "SELECT hash, COALESCE(\"metaNameData\", ''), TO_CHAR(\"savedAt\", 'YYYY-MM-DD HH24:MI:SS') FROM \"{}\" ORDER BY \"savedAt\" DESC LIMIT 50",
                ICON_AND_AVATARS_TABLE
            ),
            &[],
        )
        .await
        .map_err(|e| {
            error!("Failed to fetch DB images for explore: {}", e);
            actix_web::error::ErrorInternalServerError("Image query failed")
        })?
        .into_iter()
        .map(|row| ExploreDb {
            hash: row.get(0),
            meta_name: row.get(1),
            saved_at: row.get(2),
        })
        .collect();

    Ok(HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(render_explore_page(&cache_entries, &db_entries)))
}

async fn upload_page() -> impl Responder {
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(render_upload_page(None, None))
}

async fn handle_upload(
    mut payload: Multipart,
    data: web::Data<AppState>,
) -> Result<impl Responder, Error> {
    let mut mode = String::new();
    let mut file_bytes: Vec<u8> = Vec::new();
    let mut base64_text = String::new();

    while let Some(mut field) = payload.try_next().await? {
        let name = field.name().to_string();
        match name.as_str() {
            "mode" => {
                while let Some(chunk) = field.next().await {
                    let chunk = chunk?;
                    mode.push_str(&String::from_utf8_lossy(&chunk));
                }
            }
            "file" => {
                while let Some(chunk) = field.next().await {
                    let chunk = chunk?;
                    file_bytes.extend_from_slice(&chunk);
                    if file_bytes.len() > MAX_UPLOAD_BYTES {
                        return Ok(HttpResponse::Ok()
                            .content_type("text/html; charset=utf-8")
                            .body(render_upload_page(None, Some("File too large. Maximum size is 50 MB."))));
                    }
                }
            }
            "base64" => {
                while let Some(chunk) = field.next().await {
                    let chunk = chunk?;
                    base64_text.push_str(&String::from_utf8_lossy(&chunk));
                    if base64_text.len() > MAX_UPLOAD_BYTES * 4 / 3 + 4 {
                        return Ok(HttpResponse::Ok()
                            .content_type("text/html; charset=utf-8")
                            .body(render_upload_page(None, Some("Base64 data too large."))));
                    }
                }
            }
            _ => {
                while field.next().await.is_some() {}
            }
        }
    }

    let decoded_bytes = match mode.trim() {
        "file" => {
            if file_bytes.is_empty() {
                return Ok(HttpResponse::Ok()
                    .content_type("text/html; charset=utf-8")
                    .body(render_upload_page(None, Some("No file was uploaded."))));
            }
            if detect_content_type(&file_bytes).is_none() {
                return Ok(HttpResponse::Ok()
                    .content_type("text/html; charset=utf-8")
                    .body(render_upload_page(None, Some("Unsupported file type. Only PNG, JPG, WebP, and GIF are allowed."))));
            }
            file_bytes
        }
        "base64" => {
            let raw = base64_text.trim().to_string();
            if raw.is_empty() {
                return Ok(HttpResponse::Ok()
                    .content_type("text/html; charset=utf-8")
                    .body(render_upload_page(None, Some("No base64 data was provided."))));
            }
            let clean = if let Some(pos) = raw.find(',') {
                &raw[pos + 1..]
            } else {
                &raw
            };
            match STANDARD.decode(clean) {
                Ok(bytes) => {
                    if detect_content_type(&bytes).is_none() {
                        return Ok(HttpResponse::Ok()
                            .content_type("text/html; charset=utf-8")
                            .body(render_upload_page(None, Some("The decoded data is not a recognized image format (PNG, JPG, WebP, or GIF)."))));
                    }
                    bytes
                }
                Err(_) => {
                    return Ok(HttpResponse::Ok()
                        .content_type("text/html; charset=utf-8")
                        .body(render_upload_page(None, Some("Invalid base64 encoding."))));
                }
            }
        }
        _ => {
            return Ok(HttpResponse::Ok()
                .content_type("text/html; charset=utf-8")
                .body(render_upload_page(None, Some("Invalid upload mode."))));
        }
    };

    let id = Uuid::new_v4().to_string();
    {
        let mut cache = data.images.lock().unwrap();
        cache.put(id.clone(), decoded_bytes);
    }

    let path = format!("/image/{}", id);
    info!("Upload: stored image in cache with ID {}", id);

    Ok(HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(render_upload_page(Some(&path), None)))
}

// ---------------------------------------------------------------------------
// API handlers
// ---------------------------------------------------------------------------

async fn post_image(
    payload: web::Either<web::Json<Base64Payload>, web::Form<Base64Payload>>,
    data: web::Data<AppState>,
) -> Result<impl Responder, Error> {
    let base64_str = match payload {
        web::Either::Left(json) => json.base64.clone(),
        web::Either::Right(form) => form.base64.clone(),
    };

    let decoded_bytes = match STANDARD.decode(&base64_str) {
        Ok(bytes) => bytes,
        Err(e) => {
            error!("POST /image - Failed to decode base64: {}", e);
            return Ok(HttpResponse::BadRequest().body("Invalid Base64 encoding"));
        }
    };

    let id = Uuid::new_v4().to_string();
    let mut cache = data.images.lock().unwrap();
    cache.put(id.clone(), decoded_bytes);
    info!("POST /image - Stored image in cache with ID: {}", id);

    let path = format!("/image/{}", id);
    Ok(HttpResponse::Ok().json(json!({ "urlPath": path })))
}

async fn post_image_multipart(
    mut payload: Multipart,
    data: web::Data<AppState>,
) -> Result<impl Responder, Error> {
    let mut base64_str = String::new();

    while let Some(mut field) = payload.try_next().await? {
        if field.name() == "base64" {
            while let Some(chunk) = field.next().await {
                let chunk = chunk?;
                base64_str.push_str(&String::from_utf8_lossy(&chunk));
            }
        }
    }

    if base64_str.is_empty() {
        return Ok(HttpResponse::BadRequest().body("Missing 'base64' field"));
    }

    let decoded_bytes = match STANDARD.decode(&base64_str) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(HttpResponse::BadRequest().body("Invalid Base64 encoding")),
    };

    let id = Uuid::new_v4().to_string();
    let mut cache = data.images.lock().unwrap();
    cache.put(id.clone(), decoded_bytes);

    let path = format!("/image/{}", id);
    Ok(HttpResponse::Ok().json(json!({ "urlPath": path })))
}

async fn get_image(data: web::Data<AppState>, id: web::Path<String>) -> Result<impl Responder> {
    let id = id.into_inner();

    let mut cache = data.images.lock().unwrap();
    match cache.get(&id) {
        Some(image_data) => {
            let ct = detect_content_type(image_data).unwrap_or("image/png");
            Ok(HttpResponse::Ok()
                .content_type(ct)
                .body(image_data.clone()))
        }
        None => {
            warn!("GET /image/{} - Image not found in cache", id);
            Ok(HttpResponse::NotFound().body("Image not found"))
        }
    }
}

async fn get_image_by_md5(
    data: web::Data<AppState>,
    hash: web::Path<String>,
) -> Result<impl Responder, Error> {
    let hash = hash.into_inner().to_lowercase();

    if hash.len() != 32 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(HttpResponse::BadRequest().body("Invalid MD5 hash format"));
    }

    let client = data.db_pool.get().await.map_err(|e| {
        error!("GET /md5/{} - Database connection error: {}", hash, e);
        actix_web::error::ErrorInternalServerError("Database connection failed")
    })?;

    let query = format!(
        "SELECT \"imageData\" FROM \"{}\" WHERE hash = $1",
        ICON_AND_AVATARS_TABLE
    );

    let row = client.query_opt(&query, &[&hash]).await.map_err(|e| {
        error!("GET /md5/{} - Database query error: {}", hash, e);
        actix_web::error::ErrorInternalServerError("Database query failed")
    })?;

    match row {
        Some(row) => {
            let base64_str: String = row.get("imageData");
            match STANDARD.decode(&base64_str) {
                Ok(image_data) => {
                    let ct = detect_content_type(&image_data).unwrap_or("image/png");
                    Ok(HttpResponse::Ok().content_type(ct).body(image_data))
                }
                Err(e) => {
                    error!("GET /md5/{} - Failed to decode base64: {}", hash, e);
                    Ok(HttpResponse::InternalServerError().body("Failed to decode image data"))
                }
            }
        }
        None => Ok(HttpResponse::NotFound().body("Image not found")),
    }
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

async fn ensure_schema(pool: &Pool) -> std::io::Result<()> {
    let client = pool.get().await.map_err(|e| {
        error!("Failed to get database client: {}", e);
        std::io::Error::new(std::io::ErrorKind::Other, "Database client creation failed")
    })?;

    let schema = format!(
        r#"
CREATE TABLE IF NOT EXISTS "{icon_table}" (
    "snowflakeTargetId" TEXT NOT NULL,
    hash TEXT NOT NULL,
    "imageData" TEXT NOT NULL,
    "metaNameData" TEXT NOT NULL,
    "savedAt" TIMESTAMP DEFAULT NOW(),
    PRIMARY KEY ("snowflakeTargetId", hash)
);
"#,
        icon_table = ICON_AND_AVATARS_TABLE
    );

    client.batch_execute(&schema).await.map_err(|e| {
        error!("Failed to initialize database schema: {}", e);
        std::io::Error::new(std::io::ErrorKind::Other, "Database schema initialization failed")
    })
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .init();

    info!("Starting rust_base64_resolver application");

    let settings = ConfigFile::builder()
        .add_source(File::with_name("config").required(false))
        .add_source(Environment::with_prefix("APP").separator("__"))
        .build()
        .map_err(|e| {
            eprintln!("Failed to load configuration: {}", e);
            std::io::Error::new(std::io::ErrorKind::Other, "Configuration loading failed")
        })?;

    let settings: Settings = settings.try_deserialize().map_err(|e| {
        eprintln!("Failed to deserialize configuration: {}", e);
        std::io::Error::new(std::io::ErrorKind::Other, "Configuration deserialization failed")
    })?;

    let mut cfg = deadpool_postgres::Config::new();
    cfg.url = Some(settings.database.url.clone());

    let pool = cfg.create_pool(Some(Runtime::Tokio1), NoTls).map_err(|e| {
        error!("Failed to create database pool: {}", e);
        std::io::Error::new(std::io::ErrorKind::Other, "Database pool creation failed")
    })?;

    ensure_schema(&pool).await?;

    let app_state = web::Data::new(AppState {
        images: Mutex::new(LruCache::new(NonZeroUsize::new(CACHE_SIZE).unwrap())),
        db_pool: pool,
    });

    info!("Server starting on {}:{}", settings.server.hostname, settings.server.port);

    HttpServer::new(move || {
        App::new()
            .app_data(app_state.clone())
            .app_data(web::PayloadConfig::new(MAX_UPLOAD_BYTES))
            .wrap(actix_web::middleware::Logger::default())
            .route("/", web::get().to(index))
            .route("/explore", web::get().to(explore_page))
            .route("/upload", web::get().to(upload_page))
            .route("/upload", web::post().to(handle_upload))
            .route("/image", web::post().to(post_image))
            .route("/image/multipart", web::post().to(post_image_multipart))
            .route("/image/{id}", web::get().to(get_image))
            .route("/md5/{hash}", web::get().to(get_image_by_md5))
            .default_service(web::route().to(|| async { redirect("/") }))
    })
    .bind((settings.server.hostname.as_str(), settings.server.port))?
    .run()
    .await
}
