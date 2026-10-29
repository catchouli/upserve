mod error;
mod listing;
mod paths;
mod upload;

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Query, Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use clap::Parser;
use serde::Deserialize;
use tokio::fs;
use tower_http::services::ServeFile;

use crate::error::Error;

/// Share a directory over HTTP, like `python -m http.server`, with file and folder uploads.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Port to listen on
    #[arg(default_value_t = 8000)]
    port: u16,

    /// Address to listen on
    #[arg(short, long, default_value = "0.0.0.0")]
    bind: IpAddr,

    /// Directory to serve
    #[arg(short, long, default_value = ".")]
    directory: PathBuf,

    /// Let uploads replace existing files (folders are never replaced)
    #[arg(long)]
    allow_replace: bool,
}

struct Config {
    /// Canonical path of the served directory.
    root: PathBuf,
    allow_replace: bool,
}

/// Query parameters used by uploads; see the `upload` module.
#[derive(Deserialize)]
struct Params {
    upload: Option<String>,
    offset: Option<u64>,
    size: Option<u64>,
    mtime: Option<u64>,
    mkdir: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_target(false).init();
    let args = Args::parse();

    let root = args
        .directory
        .canonicalize()
        .with_context(|| format!("can't serve {}", args.directory.display()))?;
    let listener = tokio::net::TcpListener::bind((args.bind, args.port))
        .await
        .with_context(|| format!("can't listen on {}:{}", args.bind, args.port))?;
    println!(
        "Serving {} at http://{}/",
        root.display(),
        listener.local_addr()?
    );

    let config = Arc::new(Config {
        root,
        allow_replace: args.allow_replace,
    });
    let app = Router::new()
        .fallback(handle)
        .with_state(config)
        .layer(DefaultBodyLimit::disable())
        .layer(middleware::from_fn(log));
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

async fn handle(
    State(config): State<Arc<Config>>,
    Query(params): Query<Params>,
    req: Request,
) -> Result<Response, Error> {
    let root = &config.root;
    let target = paths::resolve(root, req.uri().path())?;
    let allow_replace = config.allow_replace;

    match (req.method(), params) {
        (&Method::GET | &Method::HEAD, _) => return serve(&target, req).await,
        (
            &Method::PUT,
            Params {
                upload: Some(id),
                offset: Some(offset),
                ..
            },
        ) => {
            upload::write_chunk(root, &target, &id, offset, req.into_body(), allow_replace).await?
        }
        (
            &Method::POST,
            Params {
                upload: Some(id),
                size: Some(size),
                mtime,
                ..
            },
        ) => upload::finish(root, &target, &id, size, mtime, allow_replace).await?,
        (
            &Method::DELETE,
            Params {
                upload: Some(id), ..
            },
        ) => upload::abort(root, &target, &id).await?,
        (&Method::POST, Params { mkdir: Some(_), .. }) => {
            paths::create_dir_within(root, &target).await?
        }
        _ => {
            return Err(Error::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "unsupported request",
            ));
        }
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Serves a file, or a listing for a directory.
async fn serve(path: &Path, req: Request) -> Result<Response, Error> {
    if fs::metadata(path).await?.is_dir() {
        let url_path = req.uri().path();
        if !url_path.ends_with('/') {
            return Ok(Redirect::permanent(&format!("{url_path}/")).into_response());
        }
        return Ok(listing::render(path, url_path).await?.into_response());
    }
    Ok(ServeFile::new(path).try_call(req).await?.map(Body::new))
}

/// Logs each request like `http.server` does, apart from successful upload chunks, which would
/// drown everything else out.
async fn log(ConnectInfo(addr): ConnectInfo<SocketAddr>, req: Request, next: Next) -> Response {
    let (method, uri) = (req.method().clone(), req.uri().clone());
    let response = next.run(req).await;
    if method != Method::PUT || !response.status().is_success() {
        tracing::info!(
            "{} \"{} {}\" {}",
            addr.ip(),
            method,
            uri,
            response.status().as_u16()
        );
    }
    response
}
