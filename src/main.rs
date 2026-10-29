mod auth;
mod error;
mod listing;
mod paths;
mod upload;

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

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

use crate::auth::Credentials;
use crate::error::Error;
use crate::upload::Uploads;

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

    /// Require this username and a password, which is asked for on startup
    #[arg(long, value_name = "USERNAME")]
    basic_auth: Option<String>,
}

struct Config {
    /// Canonical path of the served directory.
    root: PathBuf,
    uploads: Uploads,
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
    let started = SystemTime::now();

    let root = args
        .directory
        .canonicalize()
        .with_context(|| format!("can't serve {}", args.directory.display()))?;
    let credentials = args.basic_auth.map(Credentials::prompt).transpose()?;
    let listener = tokio::net::TcpListener::bind((args.bind, args.port))
        .await
        .with_context(|| format!("can't listen on {}:{}", args.bind, args.port))?;
    println!(
        "Serving {} at http://{}/",
        root.display(),
        listener.local_addr()?
    );

    // Walking a big tree can take a while, so don't hold up the server for it.
    let sweep_root = root.clone();
    tokio::task::spawn_blocking(move || {
        let removed = upload::remove_stale(&sweep_root, started);
        if removed > 0 {
            tracing::info!("removed {removed} unfinished uploads left over from before");
        }
    });

    let config = Arc::new(Config {
        uploads: Uploads::new(root.clone(), args.allow_replace),
        root,
    });
    let mut app = Router::new()
        .fallback(handle)
        .with_state(config.clone())
        .layer(DefaultBodyLimit::disable());
    if let Some(credentials) = credentials {
        app = app.layer(middleware::from_fn_with_state(
            Arc::new(credentials),
            auth::require,
        ));
    }
    let app = app.layer(middleware::from_fn(log));

    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    );
    // Stop straight away rather than waiting for uploads in progress, which could take hours.
    tokio::select! {
        result = server => result?,
        () = shutdown_signal() => {}
    }
    let removed = config.uploads.remove_unfinished().await;
    if removed > 0 {
        tracing::info!("removed {removed} unfinished uploads");
    }
    Ok(())
}

async fn handle(
    State(config): State<Arc<Config>>,
    Query(params): Query<Params>,
    req: Request,
) -> Result<Response, Error> {
    let target = paths::resolve(&config.root, req.uri().path())?;
    let uploads = &config.uploads;

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
            uploads
                .write_chunk(&target, &id, offset, req.into_body())
                .await?
        }
        (
            &Method::POST,
            Params {
                upload: Some(id),
                size: Some(size),
                mtime,
                ..
            },
        ) => uploads.finish(&target, &id, size, mtime).await?,
        (
            &Method::DELETE,
            Params {
                upload: Some(id), ..
            },
        ) => uploads.abort(&target, &id).await?,
        (&Method::POST, Params { mkdir: Some(_), .. }) => {
            paths::create_dir_within(&config.root, &target).await?
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

/// Resolves on Ctrl-C, or on SIGTERM where there is one.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("can't listen for Ctrl-C");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("can't listen for SIGTERM")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
