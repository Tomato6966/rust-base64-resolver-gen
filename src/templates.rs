use minijinja::{context, path_loader, Environment};
use serde::Serialize;

#[derive(Debug, Serialize, Clone)]
pub struct ExploreCache {
    pub id: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct ExploreDb {
    pub hash: String,
    pub meta_name: String,
    pub saved_at: String,
}

pub fn build_template_env() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_loader(path_loader("src/templates"));
    env
}

pub fn render_index_page(
    env: &Environment<'static>,
    cache_count: usize,
    db_count: i64,
    cache_capacity: usize,
) -> Result<String, minijinja::Error> {
    let body = env.get_template("index.html")?.render(context! {
        cache_count => cache_count,
        db_count => db_count,
        cache_capacity => cache_capacity,
    })?;

    env.get_template("base.html")?.render(context! {
        title => "Base64 Image Service",
        body => body,
    })
}

pub fn render_upload_page(
    env: &Environment<'static>,
    result: Option<&str>,
    error: Option<&str>,
) -> Result<String, minijinja::Error> {
    let result_html = match result {
        Some(url) => format!(
            "<div class=\"result-box\" style=\"margin-bottom:18px\"><strong>Image stored!</strong> Retrieve it at: <a href=\"{0}\">{0}</a></div>",
            url
        ),
        None => String::new(),
    };

    let error_html = match error {
        Some(msg) => format!("<div class=\"notice error\">{}</div>", msg),
        None => String::new(),
    };

    let body = env.get_template("upload.html")?.render(context! {
        result_html => result_html,
        error_html => error_html,
    })?;

    env.get_template("base.html")?.render(context! {
        title => "Upload Image",
        body => body,
    })
}

pub fn render_explore_page(
    env: &Environment<'static>,
    cache_entries: &[ExploreCache],
    db_entries: &[ExploreDb],
    page: usize,
    total_pages: usize,
    db_total: i64,
    source: &str,
    legacy_enabled: bool,
    ref_enabled: bool,
) -> Result<String, minijinja::Error> {
    let body = env.get_template("explore.html")?.render(context! {
        cache_entries => cache_entries,
        db_entries => db_entries,
        cache_count => cache_entries.len(),
        db_count => db_total,
        source => source,
        legacy_enabled => legacy_enabled,
        ref_enabled => ref_enabled,
        page => page,
        total_pages => total_pages,
        has_prev => page > 1,
        has_next => page < total_pages,
        prev_page => if page > 1 { page - 1 } else { 1 },
        next_page => if page < total_pages { page + 1 } else { total_pages },
    })?;

    env.get_template("base.html")?.render(context! {
        title => "Explore Images",
        body => body,
    })
}