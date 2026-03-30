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
    templates: minijinja::Environment<'static>,
}

#[derive(Deserialize)]
struct Base64Payload {
    base64: String,
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

    match render_index_page(&data.templates, cache_count, db_count) {
        Ok(html) => HttpResponse::Ok()
            .content_type("text/html; charset=utf-8")
            .body(html),
        Err(e) => {
            error!("Template render error on index: {}", e);
            HttpResponse::InternalServerError().body("Failed to render page")
        }
    }
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

    let html = render_explore_page(&data.templates, &cache_entries, &db_entries).map_err(|e| {
        error!("Template render error on explore: {}", e);
        actix_web::error::ErrorInternalServerError("Failed to render page")
    })?;

    Ok(HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(html))
}

async fn upload_page(data: web::Data<AppState>) -> impl Responder {
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
    mut payload: Multipart,
    data: web::Data<AppState>,
) -> Result<impl Responder, Error> {
    let mut mode = String::new();
    let mut file_bytes: Vec<u8> = Vec::new();
    let mut base64_text = String::new();

    while let Some(mut field) = payload.try_next().await? {
        let name = field.name().unwrap_or("").to_string();
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
                    if base64_text.len() > MAX_UPLOAD_BYTES * 4 / 3 + 4 {
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
            _ => while field.next().await.is_some() {},
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
        if field.name() == Some("base64") {
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

    ensure_schema(&pool).await?;

    let templates = build_template_env();

    let app_state = web::Data::new(AppState {
        images: Mutex::new(LruCache::new(NonZeroUsize::new(CACHE_SIZE).unwrap())),
        db_pool: pool,
        templates,
    });

    info!(
        "Server starting on {}:{}",
        settings.server.hostname, settings.server.port
    );

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