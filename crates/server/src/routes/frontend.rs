use axum::response::IntoResponse;
use reqwest::StatusCode;

#[cfg(not(debug_assertions))]
use axum::{body::Body, http::HeaderValue, response::Response};
#[cfg(not(debug_assertions))]
use reqwest::header;
#[cfg(not(debug_assertions))]
use rust_embed::RustEmbed;

#[cfg(not(debug_assertions))]
#[derive(RustEmbed)]
#[folder = "../../frontend/dist"]
pub struct Assets;

#[cfg(debug_assertions)]
pub async fn serve_development_frontend_hint() -> impl IntoResponse {
    let frontend_port = std::env::var("FRONTEND_PORT").unwrap_or_else(|_| "3000".to_string());
    (
        StatusCode::NOT_FOUND,
        format!(
            "Development backend serves API only. Open http://localhost:{frontend_port} instead."
        ),
    )
}

#[cfg(not(debug_assertions))]
pub async fn serve_frontend(uri: axum::extract::Path<String>) -> impl IntoResponse {
    let path = uri.trim_start_matches('/');
    serve_file(path).await
}

#[cfg(not(debug_assertions))]
pub async fn serve_frontend_root() -> impl IntoResponse {
    serve_file("index.html").await
}

#[cfg(not(debug_assertions))]
async fn serve_file(path: &str) -> impl IntoResponse + use<> {
    let file = Assets::get(path);

    match file {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();

            Response::builder()
                .status(StatusCode::OK)
                .header(
                    header::CONTENT_TYPE,
                    HeaderValue::from_str(mime.as_ref()).unwrap(),
                )
                .body(Body::from(content.data.into_owned()))
                .unwrap()
        }
        None => {
            // For SPA routing, serve index.html for unknown routes
            if let Some(index) = Assets::get("index.html") {
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, HeaderValue::from_static("text/html"))
                    .body(Body::from(index.data.into_owned()))
                    .unwrap()
            } else {
                Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::from("404 Not Found"))
                    .unwrap()
            }
        }
    }
}
