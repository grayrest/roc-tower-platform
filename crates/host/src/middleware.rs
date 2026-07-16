//! Tower middleware stack assembly from the Roc app's settings.
//!
//! Stack, outermost first: request-id -> trace -> catch-error -> CORS ->
//! compression -> concurrency limit -> timeout -> router.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use http::{header, HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::BodyExt;
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
use crate::cache::{self, Key, ResponseCache, Stored};
use crate::config::{CorsMode, HostConfig, ResponseCacheConfig};
use crate::router::RocRouter;

/// Internal header a handler sets (via `Res.cacheable`) to declare a response
/// cacheable and name the tables its reads touched. The cache layer consumes it
/// and strips it before the response leaves the process.
const CACHE_TABLES_HEADER: HeaderName = HeaderName::from_static("x-tp-cache-tables");

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

/// Which requests this cache applies to, made explicit by config: only these
/// methods, and never a request carrying a bypass header (e.g. `authorization`,
/// so a personalized response never lands in a shared entry).
struct CachePolicy {
    methods: Vec<Method>,
    bypass_headers: Vec<HeaderName>,
}

impl CachePolicy {
    fn cacheable_request<B>(&self, req: &Request<B>) -> bool {
        self.methods.contains(req.method())
            && !self
                .bypass_headers
                .iter()
                .any(|h| req.headers().contains_key(h))
    }
}

/// The outermost caching layer. It sits OUTSIDE compression, so a stored entry
/// is the already-encoded body — a hit returns finished bytes and re-runs
/// nothing below (router, Roc handler, DB all skipped). Keyed on
/// `(method, path+query, Accept-Encoding)`; entries carry the tables their reads
/// touched so the write path can purge precisely.
#[derive(Clone)]
struct CacheService {
    inner: ErringService,
    cache: Arc<ResponseCache>,
    policy: Arc<CachePolicy>,
}

/// The cache key for a request (the negotiated encoding partitions gzip vs br
/// vs identity, since the stored body is already compressed).
fn request_key(req: &Request<Incoming>) -> Key {
    let target = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or_else(|| req.uri().path())
        .to_string();
    let encoding = req
        .headers()
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    Key {
        method: req.method().as_str().to_string(),
        target,
        encoding,
    }
}

/// True when `If-None-Match` matches the entry's ETag (exact or `*`).
fn none_match(req: &Request<Incoming>, etag: &str) -> bool {
    req.headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|inm| inm == "*" || inm.split(',').any(|t| t.trim() == etag))
}

fn not_modified(etag: &str) -> Response<AppBody> {
    let mut resp = Response::new(AppBody::empty());
    *resp.status_mut() = StatusCode::NOT_MODIFIED;
    if let Ok(v) = HeaderValue::from_str(etag) {
        resp.headers_mut().insert(header::ETAG, v);
    }
    resp
}

/// Replay a stored entry as a fresh response (body is a cheap `Bytes` clone).
fn serve_stored(stored: &Stored) -> Response<AppBody> {
    let mut resp = Response::new(AppBody::full(stored.body.clone()));
    *resp.status_mut() = StatusCode::from_u16(stored.status).unwrap_or(StatusCode::OK);
    let headers = resp.headers_mut();
    for (name, value) in &stored.headers {
        headers.insert(name.clone(), value.clone());
    }
    if let Ok(v) = HeaderValue::from_str(&stored.etag) {
        headers.insert(header::ETAG, v);
    }
    resp
}

impl tower::Service<Request<Incoming>> for CacheService {
    type Response = Response<AppBody>;
    type Error = BoxError;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Incoming>) -> Self::Future {
        // Pass straight through when this request isn't in scope.
        if !self.policy.cacheable_request(&req) {
            let clone = self.inner.clone();
            let mut inner = std::mem::replace(&mut self.inner, clone);
            return Box::pin(async move { inner.ready().await?.call(req).await });
        }

        let key = request_key(&req);
        let cache = self.cache.clone();

        // Hit: serve the stored bytes (or 304) without touching the inner stack.
        // Transitional (DC4 adds variant resolution): match only plain entries.
        if let Some(stored) = cache.lookup(
            &key,
            |spec| spec.is_plain().then_some(cache::PLAIN_VARIANT),
            Instant::now(),
        ) {
            let response = if none_match(&req, &stored.etag) {
                not_modified(&stored.etag)
            } else {
                serve_stored(&stored)
            };
            return Box::pin(async move { Ok(response) });
        }

        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move {
            let revalidate_etag = req
                .headers()
                .get(header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let response = inner.ready().await?.call(req).await?;
            Ok(maybe_store(&cache, key, response, revalidate_etag).await)
        })
    }
}

/// After a miss, store the response iff it opted in (`x-tp-cache-tables`) and
/// succeeded; strip the internal header, attach the ETag, and answer a
/// concurrent `If-None-Match` for the freshly-computed tag with `304`.
async fn maybe_store(
    cache: &ResponseCache,
    key: Key,
    response: Response<AppBody>,
    revalidate_etag: Option<String>,
) -> Response<AppBody> {
    let is_cacheable = response.status().is_success()
        && response.headers().contains_key(&CACHE_TABLES_HEADER);
    if !is_cacheable {
        return response;
    }

    let (mut parts, body) = response.into_parts();
    let tables: Vec<Box<str>> = parts
        .headers
        .get(&CACHE_TABLES_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(Box::from)
                .collect()
        })
        .unwrap_or_default();
    parts.headers.remove(&CACHE_TABLES_HEADER);

    let bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        // On a body error, don't cache; hand back an empty body (the client sees
        // a truncated response either way — buffering just surfaced it here).
        Err(err) => {
            tracing::warn!("response cache: body error, not caching: {err}");
            return Response::from_parts(parts, AppBody::empty());
        }
    };

    let etag = cache::etag_for(&bytes);
    let headers: Vec<(HeaderName, HeaderValue)> = parts
        .headers
        .iter()
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect();
    // Transitional (DC4 replaces the header path with the CacheDeps
    // extension): header-declared tables at full-column width, plain
    // variant, generations sampled here (the pre-DC4 race window, unchanged).
    let deps = tables
        .into_iter()
        .map(|table: Box<str>| {
            let generation = cache.generation(&table);
            (table, crate::shadow::Cols::All, generation)
        })
        .collect();
    cache.insert(
        key,
        cache::PLAIN_VARIANT,
        cache::VariantSpec::default(),
        Stored {
            status: parts.status.as_u16(),
            headers,
            body: bytes.clone(),
            etag: etag.clone(),
        },
        deps,
        cache.epoch(),
        Instant::now(),
    );

    if revalidate_etag.is_some_and(|inm| inm == "*" || inm.split(',').any(|t| t.trim() == etag)) {
        return not_modified(&etag);
    }

    if let Ok(v) = HeaderValue::from_str(&etag) {
        parts.headers.insert(header::ETAG, v);
    }
    Response::from_parts(parts, AppBody::full(bytes))
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

    // Outside compression so entries store the already-encoded body: a hit
    // returns finished bytes and never reaches compression, the router, Roc, or
    // the DB. Installed as the process-global handle so the write path can purge.
    if let Some(cache_cfg) = &config.response_cache {
        service = BoxCloneSyncService::new(cache_service(service, cache_cfg));
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

/// Build the cache service and install its handle as the process-global so the
/// write path ([`crate::effect`]) can purge the same store.
fn cache_service(inner: ErringService, cfg: &ResponseCacheConfig) -> CacheService {
    let cache = Arc::new(ResponseCache::new(cfg.ttl, cfg.max_entries));
    cache::install_global(cache.clone());
    CacheService {
        inner,
        cache,
        policy: Arc::new(CachePolicy {
            methods: cfg.methods.clone(),
            bypass_headers: cfg.bypass_headers.clone(),
        }),
    }
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
