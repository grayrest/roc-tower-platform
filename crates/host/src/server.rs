//! HTTP server: accept loop, per-connection service, graceful shutdown.

use std::net::SocketAddr;

use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ConnBuilder;
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;

use crate::middleware::AppService;

const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

pub fn run(service: AppService, port: u16, route_count: usize) -> i32 {
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("failed to start tokio runtime: {err}");
            return 1;
        }
    };
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
    let hyper_service = TowerToHyperService::new(service);

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
