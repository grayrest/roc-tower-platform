//! Tower middleware stack assembly from the Roc app's settings.
//!
//! Stack, outermost first: request-id -> trace -> catch-error -> CORS ->
//! compression -> concurrency limit -> timeout -> router.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use http::{HeaderValue, Request, Response, StatusCode};
use hyper::body::Incoming;
use tower::limit::GlobalConcurrencyLimitLayer;
use tower::timeout::Timeout;
use tower::util::{BoxCloneSyncService, ServiceExt};
use tower::{Layer, ServiceBuilder};
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::request_id::{
    MakeRequestId, PropagateRequestIdLayer, RequestId, SetRequestIdLayer,
};
use tower_http::trace::TraceLayer;

use crate::body::{AppBody, BoxError};
use crate::config::{CorsMode, HostConfig};
use crate::router::RocRouter;

type ErringService = BoxCloneSyncService<Request<Incoming>, Response<AppBody>, BoxError>;
pub type AppService = BoxCloneSyncService<Request<Incoming>, Response<AppBody>, Infallible>;

/// Sequential request ids; enough to correlate log lines within one process.
#[derive(Clone, Default)]
struct CounterRequestId(std::sync::Arc<AtomicU64>);

impl MakeRequestId for CounterRequestId {
    fn make_request_id<B>(&mut self, _request: &Request<B>) -> Option<RequestId> {
        let id = self.0.fetch_add(1, Ordering::Relaxed);
        HeaderValue::from_str(&id.to_string()).ok().map(RequestId::new)
    }
}

/// Map lower-stack errors (timeouts, handler task failures) to responses so
/// the outer stack and hyper see an infallible service.
#[derive(Clone)]
struct CatchError(ErringService);

impl tower::Service<Request<Incoming>> for CatchError {
    type Response = Response<AppBody>;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Readiness (including the concurrency semaphore) is awaited in call.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Incoming>) -> Self::Future {
        let clone = self.0.clone();
        let mut inner = std::mem::replace(&mut self.0, clone);
        Box::pin(async move {
            let result = async move { inner.ready().await?.call(req).await }.await;
            Ok(result.unwrap_or_else(error_response))
        })
    }
}

fn error_response(err: BoxError) -> Response<AppBody> {
    let status = if err.is::<tower::timeout::error::Elapsed>() {
        StatusCode::REQUEST_TIMEOUT
    } else {
        tracing::error!("request failed: {err}");
        StatusCode::INTERNAL_SERVER_ERROR
    };
    let mut response = Response::new(AppBody::full(status.canonical_reason().unwrap_or("error")));
    *response.status_mut() = status;
    response
}

pub fn build_stack(router: RocRouter, config: &HostConfig) -> Result<AppService, String> {
    let mut service: ErringService = BoxCloneSyncService::new(router);

    service = BoxCloneSyncService::new(Timeout::new(service, config.timeout));

    if let Some(limit) = config.concurrency_limit {
        service =
            BoxCloneSyncService::new(GlobalConcurrencyLimitLayer::new(limit).layer(service));
    }

    if config.compression {
        let compressed = ServiceBuilder::new()
            .map_response(|response: Response<_>| response.map(AppBody::new))
            .layer(CompressionLayer::new())
            .service(service);
        service = BoxCloneSyncService::new(compressed);
    }

    if let Some(cors) = cors_layer(&config.cors)? {
        service = BoxCloneSyncService::new(ServiceBuilder::new().layer(cors).service(service));
    }

    let traced = ServiceBuilder::new()
        .map_response(|response: Response<_>| response.map(AppBody::new))
        .layer(SetRequestIdLayer::x_request_id(CounterRequestId::default()))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(TraceLayer::new_for_http())
        .service(CatchError(service));

    Ok(BoxCloneSyncService::new(traced))
}

fn cors_layer(mode: &CorsMode) -> Result<Option<CorsLayer>, String> {
    match mode {
        CorsMode::Off => Ok(None),
        CorsMode::Permissive => Ok(Some(CorsLayer::permissive())),
        CorsMode::AllowOrigins(origins) => {
            let values: Vec<HeaderValue> = origins
                .iter()
                .map(|origin| {
                    HeaderValue::from_str(origin)
                        .map_err(|_| format!("invalid CORS origin {origin:?}"))
                })
                .collect::<Result<_, _>>()?;
            Ok(Some(
                CorsLayer::new()
                    .allow_origin(AllowOrigin::list(values))
                    .allow_methods(Any)
                    .allow_headers(Any),
            ))
        }
    }
}
