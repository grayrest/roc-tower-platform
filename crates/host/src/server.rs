//! HTTP server: accept loop, per-connection service, graceful shutdown.

use std::net::SocketAddr;

use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;

use http_body_util::BodyExt;
use hyper::body::Incoming;
use tower::ServiceExt;

use crate::middleware::{AppService, ReqBody};

fn box_request(req: hyper::Request<Incoming>) -> hyper::Request<ReqBody> {
    req.map(|body| body.boxed_unsync())
}

const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

pub fn run(
    runtime: tokio::runtime::Runtime,
    service: AppService,
    port: u16,
    route_count: usize,
) -> i32 {
    runtime.block_on(serve(service, port, route_count))
}

async fn serve(service: AppService, port: u16, route_count: usize) -> i32 {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("failed to bind {addr}: {err}");
            return 1;
        }
    };
    tracing::info!("listening on http://{addr} with {route_count} route(s)");

    let graceful = GracefulShutdown::new();
    // Box the streaming body at the hyper boundary so every inner layer is
    // body-agnostic (the cache layer swaps in a buffered body for
    // body-variant lookups — D14).
    let hyper_service =
        TowerToHyperService::new(service.map_request(box_request as fn(_) -> _));

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(err) => {
                        tracing::warn!("accept error: {err}");
                        continue;
                    }
                };
                let io = TokioIo::new(stream);
                let connection_service = hyper_service.clone();
                let builder = ConnBuilder::new(TokioExecutor::new());
                let connection = builder.serve_connection(io, connection_service).into_owned();
                let watched = graceful.watch(connection);
                tokio::spawn(async move {
                    if let Err(err) = watched.await {
                        tracing::debug!("connection error: {err}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutdown signal received, draining connections");
                break;
            }
        }
    }

    tokio::select! {
        _ = graceful.shutdown() => {
            tracing::info!("all connections drained");
        }
        _ = tokio::time::sleep(SHUTDOWN_GRACE) => {
            tracing::warn!("shutdown grace period expired, closing remaining connections");
        }
    }
    0
}
