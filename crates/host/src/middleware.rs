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

use bytes::Bytes;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{Full, Limited};

use crate::body::{AppBody, BoxError};
use crate::cache::{self, Key, ResponseCache, Stored};
use crate::config::{CorsMode, HostConfig, ResponseCacheConfig};
use crate::router::RocRouter;

/// The request-body type below the entry adapter: hyper's streaming
/// `Incoming` boxed, OR a re-boxed buffered body when the cache layer had to
/// hash the bytes for a body-variant lookup (D14). Boxing at the top keeps
/// every inner layer body-agnostic.
pub type ReqBody = UnsyncBoxBody<Bytes, hyper::Error>;

type ErringService = BoxCloneSyncService<Request<ReqBody>, Response<AppBody>, BoxError>;
pub type AppService = BoxCloneSyncService<Request<ReqBody>, Response<AppBody>, Infallible>;

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

impl tower::Service<Request<ReqBody>> for CatchError {
    type Response = Response<AppBody>;
    type Error = Infallible;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Readiness (including the concurrency semaphore) is awaited in call.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
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
    /// Bound for buffering a request body ahead of a body-variant lookup
    /// (mirrors the router's own cap, so nothing new is admitted).
    max_body_bytes: usize,
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
fn request_key<B>(req: &Request<B>) -> Key {
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
fn none_match<B>(req: &Request<B>, etag: &str) -> bool {
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

impl tower::Service<Request<ReqBody>> for CacheService {
    type Response = Response<AppBody>;
    type Error = BoxError;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        // Pass straight through when this request isn't in scope.
        if !self.policy.cacheable_request(&req) {
            let clone = self.inner.clone();
            let mut inner = std::mem::replace(&mut self.inner, clone);
            return Box::pin(async move { inner.ready().await?.call(req).await });
        }

        let key = request_key(&req);
        let cache = self.cache.clone();
        let policy = self.policy.clone();
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);

        Box::pin(async move {
            // Hit: resolve the entry's variant from THIS request's values of
            // the recorded dimensions and serve without touching the inner
            // stack (router, Roc, DB all skipped). Cookies are parsed only if
            // the spec asks (`resolve_variant`), and a spec that varies on
            // the body (D14) reports `needs_body` on the first pass instead
            // of paying a second lock up front — plain traffic does ONE lock
            // and touches nothing else.
            let mut needs_body = false;
            if let Some(stored) = cache.lookup(
                &key,
                |spec| match resolve_variant(spec, req.headers(), None) {
                    resolved @ Some(_) => resolved,
                    None => {
                        needs_body = spec.body;
                        None
                    }
                },
                Instant::now(),
            ) {
                let response = if none_match(&req, &stored.etag) {
                    not_modified(&stored.etag)
                } else {
                    serve_stored(&stored)
                };
                return Ok(response);
            }

            // Body-varying entry: buffer the body — bounded exactly like the
            // router would — hash it, rebuild the request, and retry once.
            let req = if needs_body {
                let (parts, body) = req.into_parts();
                let bytes = match Limited::new(body, policy.max_body_bytes).collect().await {
                    Ok(collected) => collected.to_bytes(),
                    Err(err) => {
                        tracing::debug!("request body rejected at cache layer: {err}");
                        let mut response = Response::new(AppBody::full("payload too large"));
                        *response.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
                        return Ok(response);
                    }
                };
                let body_hash = Some(cache::body_hash_of(&bytes));
                let body: ReqBody =
                    UnsyncBoxBody::new(Full::new(bytes).map_err(|never| match never {}));
                let req = Request::from_parts(parts, body);
                if let Some(stored) = cache.lookup(
                    &key,
                    |spec| resolve_variant(spec, req.headers(), body_hash),
                    Instant::now(),
                ) {
                    let response = if none_match(&req, &stored.etag) {
                        not_modified(&stored.etag)
                    } else {
                        serve_stored(&stored)
                    };
                    return Ok(response);
                }
                req
            } else {
                req
            };

            let revalidate_etag = req
                .headers()
                .get(header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            // Snapshot what the store side needs from the request before it
            // moves into the inner stack: `Vary` header values come from
            // here (cookie values and the body hash ride the CacheDeps
            // extension, recorded host-side).
            let req_headers = req.headers().clone();
            // Tell the router this request is store-eligible, so the handler
            // thread records attribution (bypassed traffic skips it).
            let mut req = req;
            req.extensions_mut().insert(cache::CacheEligible);
            let response = inner.ready().await?.call(req).await?;
            Ok(maybe_store(&cache, key, response, revalidate_etag, &req_headers).await)
        })
    }
}

/// Resolve a request's variant id under `spec` from its live headers (cookies
/// parsed only when the spec names any) and an optional pre-computed body
/// hash. `None` = unresolvable (body-varying spec without a hash) or a plain
/// mismatch — both are misses.
fn resolve_variant(
    spec: &cache::VariantSpec,
    headers: &http::HeaderMap,
    body_hash: Option<u64>,
) -> Option<u64> {
    let jar = if spec.cookies.is_empty() {
        Vec::new()
    } else {
        cache::parse_cookies(headers)
    };
    cache::variant_hash(
        spec,
        |name| {
            jar.iter()
                .find(|(candidate, _)| candidate == name)
                .map(|(_, value)| value.clone())
        },
        body_hash,
        |name| header_value_of(headers, name),
    )
}

/// The first value of `name` in `headers`, for `Vary` variant dimensions.
fn header_value_of(headers: &http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Request-header names from the response's `Vary` header(s), lowercased.
fn vary_names(headers: &http::HeaderMap) -> Vec<String> {
    headers
        .get_all(header::VARY)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect()
}

/// RFC 9111 store suppression: `Cache-Control: no-store` or `private` (this
/// is a SHARED cache) blocks storage.
fn cache_control_blocks_store(headers: &http::HeaderMap) -> bool {
    headers
        .get_all(header::CACHE_CONTROL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|directive| directive.trim().to_ascii_lowercase())
        .any(|directive| directive == "no-store" || directive.starts_with("private"))
}

/// After a miss, store the response iff its derived attribution says so
/// (automatic — D10): 2xx, `CacheDeps` present and cacheable, and the
/// response neither opts out (`Cache-Control: no-store`/`private` — this is
/// a shared cache) nor declares un-keyable variance (`Vary: *`). The entry's
/// variant spec is the union of the recorded dimensions (cookies read, body
/// extracted) and the response's `Vary` names; the variant id is computed by
/// the same `variant_hash` lookups use. Attaches the ETag and answers a
/// concurrent `If-None-Match` for the freshly-computed tag with `304`.
async fn maybe_store(
    cache: &ResponseCache,
    key: Key,
    response: Response<AppBody>,
    revalidate_etag: Option<String>,
    req_headers: &http::HeaderMap,
) -> Response<AppBody> {
    let (mut parts, body) = response.into_parts();
    let deps = parts.extensions.remove::<cache::CacheDeps>();

    let vary = vary_names(&parts.headers);
    let storable = parts.status.is_success()
        && deps.as_ref().is_some_and(|deps| deps.cacheable)
        && !cache_control_blocks_store(&parts.headers)
        && !vary.iter().any(|name| name == "*");
    if !storable {
        return Response::from_parts(parts, body);
    }
    let deps = deps.expect("checked by storable");

    let bytes = match body.collect().await {
        Ok(collected) => collected.to_bytes(),
        // On a body error, don't cache; hand back an empty body (the client sees
        // a truncated response either way — buffering just surfaced it here).
        Err(err) => {
            tracing::warn!("response cache: body error, not caching: {err}");
            return Response::from_parts(parts, AppBody::empty());
        }
    };

    let spec = cache::VariantSpec {
        cookies: deps.cookies.iter().map(|(name, _)| name.clone()).collect(),
        body: deps.body_hash.is_some(),
        headers: vary,
    };
    // `spec.body` implies `deps.body_hash` is Some, so this always resolves.
    let variant = cache::variant_hash(
        &spec,
        |name| {
            deps.cookies
                .iter()
                .find(|(candidate, _)| candidate == name)
                .and_then(|(_, value)| value.clone())
        },
        deps.body_hash,
        |name| header_value_of(req_headers, name),
    )
    .unwrap_or(cache::PLAIN_VARIANT);

    let etag = cache::etag_for(&bytes);
    let headers: Vec<(HeaderName, HeaderValue)> = parts
        .headers
        .iter()
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect();
    cache.insert(
        key,
        variant,
        spec,
        Stored {
            status: parts.status.as_u16(),
            headers,
            body: bytes.clone(),
            etag: etag.clone(),
        },
        deps.deps,
        deps.epoch,
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
        service =
            BoxCloneSyncService::new(cache_service(service, cache_cfg, config.max_body_bytes));
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
fn cache_service(
    inner: ErringService,
    cfg: &ResponseCacheConfig,
    max_body_bytes: usize,
) -> CacheService {
    let cache = Arc::new(ResponseCache::new(cfg.ttl, cfg.max_entries));
    cache::install_global(cache.clone());
    CacheService {
        inner,
        cache,
        policy: Arc::new(CachePolicy {
            methods: cfg.methods.clone(),
            bypass_headers: cfg.bypass_headers.clone(),
            max_body_bytes,
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
