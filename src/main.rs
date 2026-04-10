mod templates;

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

use crate::templates::{
    build_template_env, render_explore_page, render_index_page, render_upload_page, ExploreCache,
    ExploreDb,
};

#[derive(Debug, Deserialize, Clone)]
struct Settings {
    server: ServerSettings,
    database: DatabaseSettings,
    app: AppSettings,
    auth: Option<AuthSettings>,
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

#[derive(Debug, Deserialize, Clone)]
struct AppSettings {
    cache_size: usize,
    #[serde(default = "default_legacy_enabled")]
    legacy_icon_and_avatars_enabled: bool,
    #[serde(default = "default_legacy_icon_table")]
    icon_and_avatars_table: String,
    #[serde(default = "default_ref_enabled")]
    ref_icon_and_avatar_enabled: bool,
    #[serde(default = "default_ref_table")]
    icon_and_avatar_ref_table: String,
    #[serde(default = "default_blob_table")]
    image_blob_table: String,
    max_upload_bytes: usize,
}

fn default_legacy_enabled() -> bool {
    true
}

fn default_ref_enabled() -> bool {
    false
}

fn default_legacy_icon_table() -> String {
    "IconAndAvatars".to_string()
}

fn default_ref_table() -> String {
    "IconAndAvatarRef".to_string()
}

fn default_blob_table() -> String {
    "ImageBlob".to_string()
}

struct AppState {
    images: Mutex<LruCache<String, Vec<u8>>>,
    db_pool: Pool,
    templates: minijinja::Environment<'static>,
    settings: Settings,
}

#[derive(Deserialize)]
struct Base64Payload {
    base64: String,
}

#[derive(Deserialize)]
struct ExploreQuery {
    page: Option<usize>,
    source: Option<String>,
}

#[derive(Clone, Copy)]
enum ExploreSource {
    Legacy,
    Ref,
    Both,
}

impl ExploreSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::Ref => "ref",
            Self::Both => "both",
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
struct AuthSettings {
    enabled: bool,
    username: String,
    password: String,
    protect_upload: bool,
    protect_explore: bool,
    protect_api: bool,
}

fn redirect(location: &str) -> HttpResponse {
    HttpResponse::Found()
        .insert_header((header::LOCATION, location))
        .finish()
}
fn check_basic_auth(req: &actix_web::HttpRequest, settings: &Settings) -> bool {
    let auth_cfg = match &settings.auth {
        Some(a) if a.enabled => a,
        _ => return true, // auth disabled or not configured → allow
    };

    let path = req.path();
    let needs_auth = (auth_cfg.protect_upload && path.starts_with("/upload"))
        || (auth_cfg.protect_explore && path.starts_with("/explore"))
        || (auth_cfg.protect_api && (path == "/image"
        || path == "/image/multipart"
        || path.starts_with("/image/")));

    if !needs_auth {
        return true;
    }

    // Parse Authorization: Basic <base64>
    let header_val = match req.headers().get("Authorization") {
        Some(v) => match v.to_str() {
            Ok(s) => s,
            Err(_) => return false,
        },
        None => return false,
    };

    if !header_val.starts_with("Basic ") {
        return false;
    }

    let decoded = match STANDARD.decode(&header_val[6..]) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => return false,
        },
        Err(_) => return false,
    };

    let (user, pass) = match decoded.split_once(':') {
        Some(pair) => pair,
        None => return false,
    };

    user == auth_cfg.username && pass == auth_cfg.password
}

fn unauthorized_response() -> HttpResponse {
    HttpResponse::Unauthorized()
        .insert_header(("WWW-Authenticate", "Basic realm=\"Image Service\""))
        .body("Unauthorized")
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

async fn index(data: web::Data<AppState>) -> impl Responder {
    let cache_count = data.images.lock().unwrap().len();
    let source = resolve_explore_source(None, &data.settings.app);

    let db_count: i64 = match data.db_pool.get().await {
        Ok(client) => get_total_count(&client, &data.settings.app, source)
            .await
            .unwrap_or(0),
        Err(_) => 0,
    };

    match render_index_page(&data.templates, cache_count, db_count, data.settings.app.cache_size) {
        Ok(html) => HttpResponse::Ok()
            .content_type("text/html; charset=utf-8")
            .body(html),
        Err(e) => {
            error!("Template render error on index: {}", e);
            HttpResponse::InternalServerError().body("Failed to render page")
        }
    }
}

async fn explore_page(
    req: actix_web::HttpRequest,
    data: web::Data<AppState>,
    query: web::Query<ExploreQuery>,
) -> Result<impl Responder, Error> {
    if !check_basic_auth(&req, &data.settings) {
        return Ok(unauthorized_response());
    }

    let per_page: i64 = 50;
    let page = query.page.unwrap_or(1).max(1) as i64;
    let offset = (page - 1) * per_page;
    let selected_source = resolve_explore_source(query.source.as_deref(), &data.settings.app);

    let cache_entries: Vec<ExploreCache> = {
        let cache = data.images.lock().unwrap();
        cache.iter().map(|(id, _)| ExploreCache { id: id.clone() }).collect()
    };

    let client = data.db_pool.get().await.map_err(|e| {
        error!("DB connection failed for explore: {}", e);
        actix_web::error::ErrorInternalServerError("Database connection failed")
    })?;

    let db_total = get_total_count(&client, &data.settings.app, selected_source)
        .await
        .map_err(|e| {
            error!("Failed to count DB images for explore: {}", e);
            actix_web::error::ErrorInternalServerError("Image count query failed")
        })
        .unwrap_or(0);

    let db_entries = get_explore_entries(
        &client,
        &data.settings.app,
        selected_source,
        per_page,
        offset,
    )
        .await
        .map_err(|e| {
            error!("Failed to fetch DB images for explore: {}", e);
            actix_web::error::ErrorInternalServerError("Image query failed")
        })?;

    let total_pages = if db_total == 0 {
        1
    } else {
        ((db_total + per_page - 1) / per_page) as usize
    };

    let html = render_explore_page(
        &data.templates,
        &cache_entries,
        &db_entries,
        page as usize,
        total_pages,
        db_total,
        selected_source.as_str(),
        data.settings.app.legacy_icon_and_avatars_enabled,
        data.settings.app.ref_icon_and_avatar_enabled,
    )
    .map_err(|e| {
        error!("Template render error on explore: {}", e);
        actix_web::error::ErrorInternalServerError("Failed to render page")
    })?;

    Ok(HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(html))
}

async fn upload_page(
    req: actix_web::HttpRequest,
    data: web::Data<AppState>,
) -> impl Responder {
    if !check_basic_auth(&req, &data.settings) {
        return unauthorized_response();
    }
    
    match render_upload_page(&data.templates, None, None) {
        Ok(html) => HttpResponse::Ok()
            .content_type("text/html; charset=utf-8")
            .body(html),
        Err(e) => {
            error!("Template render error on upload page: {}", e);
            HttpResponse::InternalServerError().body("Failed to render page")
        }
    }
}

async fn handle_upload(
    req: actix_web::HttpRequest,
    mut payload: Multipart,
    data: web::Data<AppState>,
) -> Result<impl Responder, Error> {
    if !check_basic_auth(&req, &data.settings) {
        return Ok(unauthorized_response());
    }
    
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
                    if file_bytes.len() > data.settings.app.max_upload_bytes {
                        let html = render_upload_page(
                            &data.templates,
                            None,
                            Some("File too large. Maximum size is 50 MB."),
                        )
                        .unwrap_or_else(|_| "Failed to render page".to_string());

                        return Ok(HttpResponse::Ok()
                            .content_type("text/html; charset=utf-8")
                            .body(html));
                    }
                }
            }
            "base64" => {
                while let Some(chunk) = field.next().await {
                    let chunk = chunk?;
                    base64_text.push_str(&String::from_utf8_lossy(&chunk));
                    if base64_text.len() > data.settings.app.max_upload_bytes * 4 / 3 + 4 {
                        let html = render_upload_page(
                            &data.templates,
                            None,
                            Some("Base64 data too large."),
                        )
                        .unwrap_or_else(|_| "Failed to render page".to_string());

                        return Ok(HttpResponse::Ok()
                            .content_type("text/html; charset=utf-8")
                            .body(html));
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
                let html = render_upload_page(
                    &data.templates,
                    None,
                    Some("No file was uploaded."),
                )
                .unwrap_or_else(|_| "Failed to render page".to_string());

                return Ok(HttpResponse::Ok()
                    .content_type("text/html; charset=utf-8")
                    .body(html));
            }
            if detect_content_type(&file_bytes).is_none() {
                let html = render_upload_page(
                    &data.templates,
                    None,
                    Some("Unsupported file type. Only PNG, JPG, WebP, and GIF are allowed."),
                )
                .unwrap_or_else(|_| "Failed to render page".to_string());

                return Ok(HttpResponse::Ok()
                    .content_type("text/html; charset=utf-8")
                    .body(html));
            }
            file_bytes
        }
        "base64" => {
            let raw = base64_text.trim().to_string();
            if raw.is_empty() {
                let html = render_upload_page(
                    &data.templates,
                    None,
                    Some("No base64 data was provided."),
                )
                .unwrap_or_else(|_| "Failed to render page".to_string());

                return Ok(HttpResponse::Ok()
                    .content_type("text/html; charset=utf-8")
                    .body(html));
            }

            let clean = if let Some(pos) = raw.find(',') {
                &raw[pos + 1..]
            } else {
                &raw
            };

            match STANDARD.decode(clean) {
                Ok(bytes) => {
                    if detect_content_type(&bytes).is_none() {
                        let html = render_upload_page(
                            &data.templates,
                            None,
                            Some("The decoded data is not a recognized image format (PNG, JPG, WebP, or GIF)."),
                        )
                        .unwrap_or_else(|_| "Failed to render page".to_string());

                        return Ok(HttpResponse::Ok()
                            .content_type("text/html; charset=utf-8")
                            .body(html));
                    }
                    bytes
                }
                Err(_) => {
                    let html = render_upload_page(
                        &data.templates,
                        None,
                        Some("Invalid base64 encoding."),
                    )
                    .unwrap_or_else(|_| "Failed to render page".to_string());

                    return Ok(HttpResponse::Ok()
                        .content_type("text/html; charset=utf-8")
                        .body(html));
                }
            }
        }
        _ => {
            let html = render_upload_page(
                &data.templates,
                None,
                Some("Invalid upload mode."),
            )
            .unwrap_or_else(|_| "Failed to render page".to_string());

            return Ok(HttpResponse::Ok()
                .content_type("text/html; charset=utf-8")
                .body(html));
        }
    };

    let id = Uuid::new_v4().to_string();
    {
        let mut cache = data.images.lock().unwrap();
        cache.put(id.clone(), decoded_bytes);
    }

    let path = format!("/image/{}", id);
    info!("Upload: stored image in cache with ID {}", id);

    let html = render_upload_page(&data.templates, Some(&path), None)
        .unwrap_or_else(|_| "Failed to render page".to_string());

    Ok(HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(html))
}

async fn post_image(
    req: actix_web::HttpRequest,
    payload: web::Either<web::Json<Base64Payload>, web::Form<Base64Payload>>,
    data: web::Data<AppState>,
) -> Result<impl Responder, Error> {
    if !check_basic_auth(&req, &data.settings) {
        return Ok(HttpResponse::Unauthorized()
            .insert_header(("WWW-Authenticate", "Basic realm=\"Image Service\""))
            .json(serde_json::json!({ "error": "Unauthorized" })));
    }
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
    req: actix_web::HttpRequest,
    mut payload: Multipart,
    data: web::Data<AppState>,
) -> Result<impl Responder, Error> {
    if !check_basic_auth(&req, &data.settings) {
        return Ok(HttpResponse::Unauthorized()
            .insert_header(("WWW-Authenticate", "Basic realm=\"Image Service\""))
            .json(serde_json::json!({ "error": "Unauthorized" })));
    }
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

    match get_image_by_hash(&client, &data.settings.app, &hash).await {
        Ok(Some(image_data)) => {
            let ct = detect_content_type(&image_data).unwrap_or("image/png");
            Ok(HttpResponse::Ok().content_type(ct).body(image_data))
        }
        Ok(None) => Ok(HttpResponse::NotFound().body("Image not found")),
        Err(e) => {
            error!("GET /md5/{} - Database query error: {}", hash, e);
            Err(actix_web::error::ErrorInternalServerError(
                "Database query failed",
            ))
        }
    }
}

fn resolve_explore_source(requested: Option<&str>, app: &AppSettings) -> ExploreSource {
    match requested.map(|s| s.to_ascii_lowercase()) {
        Some(s) if s == "legacy" && app.legacy_icon_and_avatars_enabled => ExploreSource::Legacy,
        Some(s) if s == "ref" && app.ref_icon_and_avatar_enabled => ExploreSource::Ref,
        Some(s) if s == "both" && app.legacy_icon_and_avatars_enabled && app.ref_icon_and_avatar_enabled => ExploreSource::Both,
        _ if app.legacy_icon_and_avatars_enabled && app.ref_icon_and_avatar_enabled => ExploreSource::Both,
        _ if app.legacy_icon_and_avatars_enabled => ExploreSource::Legacy,
        _ if app.ref_icon_and_avatar_enabled => ExploreSource::Ref,
        _ => ExploreSource::Both,
    }
}

async fn get_total_count(
    client: &tokio_postgres::Client,
    app: &AppSettings,
    source: ExploreSource,
) -> Result<i64, tokio_postgres::Error> {
    if matches!(source, ExploreSource::Both)
        && app.legacy_icon_and_avatars_enabled
        && app.ref_icon_and_avatar_enabled
    {
        let query = format!(
            r#"
            SELECT
                (SELECT COUNT(*) FROM "{legacy_table}") +
                (SELECT COUNT(*) FROM "{ref_table}") AS total
            "#,
            legacy_table = app.icon_and_avatars_table,
            ref_table = app.icon_and_avatar_ref_table,
        );
        return client.query_one(&query, &[]).await.map(|r| r.get(0));
    }

    if matches!(source, ExploreSource::Legacy) && app.legacy_icon_and_avatars_enabled {
        let query = format!(r#"SELECT COUNT(*) FROM "{}""#, app.icon_and_avatars_table);
        return client.query_one(&query, &[]).await.map(|r| r.get(0));
    }

    if matches!(source, ExploreSource::Ref) && app.ref_icon_and_avatar_enabled {
        let query = format!(r#"SELECT COUNT(*) FROM "{}""#, app.icon_and_avatar_ref_table);
        return client.query_one(&query, &[]).await.map(|r| r.get(0));
    }

    Ok(0)
}

async fn get_explore_entries(
    client: &tokio_postgres::Client,
    app: &AppSettings,
    source: ExploreSource,
    per_page: i64,
    offset: i64,
) -> Result<Vec<ExploreDb>, tokio_postgres::Error> {
    if matches!(source, ExploreSource::Both)
        && app.legacy_icon_and_avatars_enabled
        && app.ref_icon_and_avatar_enabled
    {
        let query = format!(
            r#"
            SELECT hash, meta_name, saved_at FROM (
                SELECT
                    hash,
                    COALESCE("metaNameData", '') AS meta_name,
                    TO_CHAR("savedAt", 'YYYY-MM-DD HH24:MI:SS') AS saved_at,
                    "savedAt" AS sort_saved_at
                FROM "{legacy_table}"
                UNION ALL
                SELECT
                    hash,
                    COALESCE("metaNameData", '') AS meta_name,
                    TO_CHAR("savedAt", 'YYYY-MM-DD HH24:MI:SS') AS saved_at,
                    "savedAt" AS sort_saved_at
                FROM "{ref_table}"
            ) merged
            ORDER BY sort_saved_at DESC
            LIMIT $1 OFFSET $2
            "#,
            legacy_table = app.icon_and_avatars_table,
            ref_table = app.icon_and_avatar_ref_table,
        );

        return client
            .query(&query, &[&per_page, &offset])
            .await
            .map(|rows| {
                rows.into_iter()
                    .map(|row| ExploreDb {
                        hash: row.get(0),
                        meta_name: row.get(1),
                        saved_at: row.get(2),
                    })
                    .collect()
            });
    }

    if matches!(source, ExploreSource::Legacy) && app.legacy_icon_and_avatars_enabled {
        let query = format!(
            r#"
            SELECT
                hash,
                COALESCE("metaNameData", ''),
                TO_CHAR("savedAt", 'YYYY-MM-DD HH24:MI:SS')
            FROM "{}"
            ORDER BY "savedAt" DESC
            LIMIT $1 OFFSET $2
            "#,
            app.icon_and_avatars_table
        );

        return client.query(&query, &[&per_page, &offset]).await.map(|rows| {
            rows.into_iter()
                .map(|row| ExploreDb {
                    hash: row.get(0),
                    meta_name: row.get(1),
                    saved_at: row.get(2),
                })
                .collect()
        });
    }

    if matches!(source, ExploreSource::Ref) && app.ref_icon_and_avatar_enabled {
        let query = format!(
            r#"
            SELECT
                hash,
                COALESCE("metaNameData", ''),
                TO_CHAR("savedAt", 'YYYY-MM-DD HH24:MI:SS')
            FROM "{}"
            ORDER BY "savedAt" DESC
            LIMIT $1 OFFSET $2
            "#,
            app.icon_and_avatar_ref_table
        );

        return client.query(&query, &[&per_page, &offset]).await.map(|rows| {
            rows.into_iter()
                .map(|row| ExploreDb {
                    hash: row.get(0),
                    meta_name: row.get(1),
                    saved_at: row.get(2),
                })
                .collect()
        });
    }

    Ok(Vec::new())
}

async fn get_image_by_hash(
    client: &tokio_postgres::Client,
    app: &AppSettings,
    hash: &str,
) -> Result<Option<Vec<u8>>, tokio_postgres::Error> {
    if app.legacy_icon_and_avatars_enabled {
        let legacy_query = format!(
            r#"SELECT "imageData" FROM "{}" WHERE hash = $1 LIMIT 1"#,
            app.icon_and_avatars_table
        );

        if let Some(row) = client.query_opt(&legacy_query, &[&hash]).await? {
            let base64_str: String = row.get("imageData");
            match STANDARD.decode(&base64_str) {
                Ok(image_data) => return Ok(Some(image_data)),
                Err(e) => {
                    error!("GET /md5/{} - Failed to decode legacy base64 data: {}", hash, e);
                }
            }
        }
    }

    if app.ref_icon_and_avatar_enabled {
        let ref_query = format!(
            r#"
            SELECT b."imageData"
            FROM "{ref_table}" r
            INNER JOIN "{blob_table}" b ON r.hash = b.hash
            WHERE r.hash = $1
            ORDER BY r."savedAt" DESC
            LIMIT 1
            "#,
            ref_table = app.icon_and_avatar_ref_table,
            blob_table = app.image_blob_table,
        );

        if let Some(row) = client.query_opt(&ref_query, &[&hash]).await? {
            let image_data: Vec<u8> = row.get(0);
            return Ok(Some(image_data));
        }
    }

    Ok(None)
}

async fn ensure_schema(pool: &Pool, app: &AppSettings) -> std::io::Result<()> {
    let client = pool.get().await.map_err(|e| {
        error!("Failed to get database client: {}", e);
        std::io::Error::new(std::io::ErrorKind::Other, "Database client creation failed")
    })?;

    if app.legacy_icon_and_avatars_enabled {
        let legacy_schema = format!(
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
            icon_table = app.icon_and_avatars_table
        );

        client.batch_execute(&legacy_schema).await.map_err(|e| {
            error!("Failed to initialize legacy database schema: {}", e);
            std::io::Error::new(std::io::ErrorKind::Other, "Database schema initialization failed")
        })?;
    }

    if app.ref_icon_and_avatar_enabled {
        let new_schema = format!(
            r#"
CREATE TABLE IF NOT EXISTS "{blob_table}" (
    hash TEXT PRIMARY KEY,
    "mimeType" TEXT,
    "sizeBytes" INT NOT NULL,
    "imageData" BYTEA NOT NULL,
    "createdAt" TIMESTAMP DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS "{ref_table}" (
    "snowflakeTargetId" TEXT NOT NULL,
    hash TEXT NOT NULL,
    "metaNameData" TEXT NOT NULL,
    "savedAt" TIMESTAMP DEFAULT NOW(),
    PRIMARY KEY ("snowflakeTargetId", hash),
    CONSTRAINT "{ref_table}_hash_fkey"
        FOREIGN KEY (hash) REFERENCES "{blob_table}" (hash)
        ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS "{ref_table}_hash_idx" ON "{ref_table}" (hash);
"#,
            ref_table = app.icon_and_avatar_ref_table,
            blob_table = app.image_blob_table,
        );

        client.batch_execute(&new_schema).await.map_err(|e| {
            error!("Failed to initialize new reference/blob schema: {}", e);
            std::io::Error::new(std::io::ErrorKind::Other, "Database schema initialization failed")
        })?;
    }

    Ok(())
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
        std::io::Error::new(
            std::io::ErrorKind::Other,
            "Configuration deserialization failed",
        )
    })?;

    let mut cfg = deadpool_postgres::Config::new();
    cfg.url = Some(settings.database.url.clone());

    let pool = cfg.create_pool(Some(Runtime::Tokio1), NoTls).map_err(|e| {
        error!("Failed to create database pool: {}", e);
        std::io::Error::new(std::io::ErrorKind::Other, "Database pool creation failed")
    })?;

    ensure_schema(&pool, &settings.app).await?;

    let templates = build_template_env();

    let cache_size = NonZeroUsize::new(settings.app.cache_size)
    .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "app.cache_size must be > 0"))?;

    let app_state = web::Data::new(AppState {
        images: Mutex::new(LruCache::new(cache_size)),
        db_pool: pool,
        templates,
        settings: settings.clone(),
    });

    info!(
        "Server starting on {}:{}",
        settings.server.hostname, settings.server.port
    );

    HttpServer::new(move || {
        App::new()
            .app_data(app_state.clone())
            .app_data(web::PayloadConfig::new(settings.app.max_upload_bytes))
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