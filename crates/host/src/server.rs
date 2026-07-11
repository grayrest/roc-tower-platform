//! Minimal HTTP server for spike 1: every request is handed to the Roc app's
//! `handle!` entrypoint on the blocking pool; the returned string becomes the
//! response body.

use std::convert::Infallible;
use std::net::SocketAddr;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use tokio::net::TcpListener;

pub type RocHandler = fn(String) -> String;

const DEFAULT_PORT: u16 = 8000;

pub fn run(handler: RocHandler) -> i32 {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("failed to start tokio runtime: {err}");
            return 1;
        }
    };
    runtime.block_on(serve(handler))
}

async fn serve(handler: RocHandler) -> i32 {
    let port = std::env::var("PORT")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = match TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("failed to bind {addr}: {err}");
            return 1;
        }
    };
    tracing::info!("listening on http://{addr}");

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                tracing::warn!("accept error: {err}");
                continue;
            }
        };
        let io = TokioIo::new(stream);
        tokio::spawn(async move {
            let service = service_fn(move |req: Request<Incoming>| async move {
                Ok::<_, Infallible>(respond(handler, req).await)
            });
            if let Err(err) = ConnBuilder::new(TokioExecutor::new())
                .serve_connection(io, service)
                .await
            {
                tracing::debug!("connection error: {err}");
            }
        });
    }
}

async fn respond(handler: RocHandler, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let path = req.uri().path().to_owned();
    match tokio::task::spawn_blocking(move || handler(path)).await {
        Ok(body) => Response::new(Full::new(Bytes::from(body))),
        Err(err) => {
            tracing::error!("handler task failed: {err}");
            let mut response = Response::new(Full::new(Bytes::from_static(b"internal error")));
            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            response
        }
    }
}
