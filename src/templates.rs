use minijinja::{context, path_loader, Environment};
use serde::Serialize;

const CACHE_SIZE: usize = 10_000;

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

fn render_notice(message: Option<&str>, level: &str) -> String {
    match message {
        Some(message) => format!(
            "<div class=\"notice {}\">{}</div>",
            level,
            minijinja::value::Value::from(message)
        ),
        None => String::new(),
    }
}

pub fn render_index_page(
    env: &Environment<'static>,
    cache_count: usize,
    db_count: i64,
) -> Result<String, minijinja::Error> {
    let body = env.get_template("index.html")?.render(context! {
        cache_count => cache_count,
        db_count => db_count,
        cache_capacity => CACHE_SIZE,
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
) -> Result<String, minijinja::Error> {
    let body = env.get_template("explore.html")?.render(context! {
        cache_entries => cache_entries,
        db_entries => db_entries,
        cache_count => cache_entries.len(),
        db_count => db_entries.len(),
    })?;

    env.get_template("base.html")?.render(context! {
        title => "Explore Images",
        body => body,
    })
}