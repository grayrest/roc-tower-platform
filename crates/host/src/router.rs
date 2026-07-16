//! Router service: matches requests against the Roc route table (matchit),
//! dispatches to boxed Roc handlers on the blocking pool, and falls back to
//! static file serving and 404/405 responses.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use http::{HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use tower::util::ServiceExt;
use tower_http::services::ServeDir;

use crate::body::{AppBody, BoxError};
use crate::config::{LoadedApp, ServeFilesConfig};
use crate::roc_call::{call_handler, RequestParts, RocHandler, RocModel};

pub struct RocRouter {
    inner: Arc<RouterInner>,
}

impl Clone for RocRouter {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

struct RouterInner {
    routers: HashMap<Method, matchit::Router<RocHandler>>,
    model: RocModel,
    max_body_bytes: usize,
    serve_files: Option<ServeFilesMount>,
}

struct ServeFilesMount {
    url_prefix: String,
    service: ServeDir,
}

impl RocRouter {
    pub fn new(app: &LoadedApp, model: RocModel) -> Result<Self, String> {
        let mut routers: HashMap<Method, matchit::Router<RocHandler>> = HashMap::new();
        for route in &app.routes {
            routers
                .entry(route.method.clone())
                .or_default()
                .insert(&route.path, route.handler)
                .map_err(|err| {
                    format!("cannot register route {} {}: {err}", route.method, route.path)
                })?;
        }
        let serve_files = app.config.serve_files.as_ref().map(ServeFilesMount::new);
        Ok(Self {
            inner: Arc::new(RouterInner {
                routers,
                model,
                max_body_bytes: app.config.max_body_bytes,
                serve_files,
            }),
        })
    }
}

impl ServeFilesMount {
    fn new(config: &ServeFilesConfig) -> Self {
        Self {
            url_prefix: config.url_prefix.trim_end_matches('/').to_owned(),
            service: ServeDir::new(&config.dir),
        }
    }

    /// Rewrite the request path relative to the mount prefix, or None if the
    /// request is outside this mount.
    fn strip_prefix(&self, path: &str) -> Option<String> {
        let rest = path.strip_prefix(&self.url_prefix)?;
        if rest.is_empty() {
            return Some("/".to_owned());
        }
        rest.starts_with('/').then(|| rest.to_owned())
    }
}

type RouterFuture = Pin<Box<dyn Future<Output = Result<Response<AppBody>, BoxError>> + Send>>;

impl tower::Service<Request<Incoming>> for RocRouter {
    type Response = Response<AppBody>;
    type Error = BoxError;
    type Future = RouterFuture;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Incoming>) -> Self::Future {
        let inner = self.inner.clone();
        Box::pin(async move { inner.route(req).await })
    }
}

impl RouterInner {
    async fn route(&self, req: Request<Incoming>) -> Result<Response<AppBody>, BoxError> {
        let method = req.method().clone();
        let path = req.uri().path().to_owned();

        if let Some(router) = self.routers.get(&method)
            && let Ok(matched) = router.at(&path)
        {
            let handler = *matched.value;
            let params = matched
                .params
                .iter()
                .map(|(name, value)| (name.to_owned(), value.to_owned()))
                .collect();
            return self.dispatch(handler, params, req).await;
        }

        // Path exists under another method -> 405 with Allow header.
        let allowed: Vec<&Method> = self
            .routers
            .iter()
            .filter(|(candidate, router)| **candidate != method && router.at(&path).is_ok())
            .map(|(candidate, _)| candidate)
            .collect();
        if !allowed.is_empty() {
            let allow = allowed
                .iter()
                .map(|m| m.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let mut response = Response::new(AppBody::full("method not allowed"));
            *response.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
            response
                .headers_mut()
                .insert(http::header::ALLOW, HeaderValue::from_str(&allow)?);
            return Ok(response);
        }

        if let Some(mount) = &self.serve_files
            && let Some(rewritten) = mount.strip_prefix(&path)
        {
            return self.serve_file(mount, rewritten, req).await;
        }

        let mut response = Response::new(AppBody::full("not found"));
        *response.status_mut() = StatusCode::NOT_FOUND;
        Ok(response)
    }

    async fn dispatch(
        &self,
        handler: RocHandler,
        params: Vec<(String, String)>,
        req: Request<Incoming>,
    ) -> Result<Response<AppBody>, BoxError> {
        let dispatch_start = crate::profile::now();
        let (parts, body) = req.into_parts();

        let collected = match Limited::new(body, self.max_body_bytes).collect().await {
            Ok(collected) => collected.to_bytes(),
            Err(err) => {
                // Limited yields an error when the body exceeds the cap.
                tracing::debug!("request body rejected: {err}");
                let mut response = Response::new(AppBody::full("payload too large"));
                *response.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
                return Ok(response);
            }
        };

        // D13: the Cookie header is host-mediated — parse it for the cookie
        // coeffect and STRIP it from the Roc-visible header list, so a handler
        // cannot read cookies invisibly to the response cache's variant key.
        // D14: same for the body — it rides the ctx registry, not the record.
        let cookies = Self::parse_cookies(&parts.headers);
        let request_parts = RequestParts {
            method: parts.method.as_str().to_owned(),
            path: parts.uri.path().to_owned(),
            params,
            query: parts.uri.query().unwrap_or("").to_owned(),
            headers: parts
                .headers
                .iter()
                .filter(|(name, _)| *name != http::header::COOKIE)
                .map(|(name, value)| {
                    (
                        name.as_str().to_owned(),
                        String::from_utf8_lossy(value.as_bytes()).into_owned(),
                    )
                })
                .collect(),
        };

        let model = self.model;
        // Mint the per-request arena and coeffect ctx, then run the handler on
        // the blocking pool. A detached reaper task owns the teardown guard and
        // drops it — releasing memoized thunks AND sweeping any cursor the
        // handler left open on an error path — ONLY after the blocking task has
        // returned. Deferring teardown past the handler is what makes the ctx
        // sweep unable to race a live cursor step, and it survives this
        // dispatch future being cancelled (client disconnect, timeout) as well
        // as a host panic unwinding out of a hosted fn (surfaced as a
        // JoinError). See plan decision 6.
        //
        // The guard also holds a reference to the arena so memo captures
        // allocated in it stay valid through teardown (effects.rs). The blocking
        // closure installs the arena in its thread-local slot for the duration
        // of the handler. Cleanup is explicit (not RAII): a panic unwinding out
        // of a hosted fn skips Rust drops in the intervening frames (roc_call.rs
        // documents this), so a scope guard here is unreliable — catch_unwind,
        // clear the slot on every path, then resume the unwind.
        let arena = crate::arena::RequestArena::new();
        let ctx_guard = crate::effects::mint_ctx(arena.clone(), collected, cookies);
        let ctx = ctx_guard.id();
        let join = tokio::task::spawn_blocking(move || {
            crate::arena::install(arena);
            // Current-ctx for hosted fns that take no ctx argument (poison).
            // Explicit install/clear like the arena: a panic unwinding out of
            // a hosted fn skips intervening drops, so no RAII guard.
            crate::effects::install_current_ctx(ctx);
            let handler_start = crate::profile::now();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                call_handler(handler, model, request_parts, ctx)
            }));
            if let Some(t) = handler_start {
                crate::profile::record_handler(t.elapsed().as_nanos() as u64);
            }
            crate::effects::clear_current_ctx();
            crate::arena::uninstall();
            match result {
                Ok((out, deferred)) => {
                    // Take the cache attribution HERE, on the handler thread:
                    // the reaper below drops the ctx entry before the oneshot
                    // resolves, so reading it router-side would always miss (R9).
                    let cache_deps = crate::effects::take_cache_attribution(ctx);
                    (out, deferred, cache_deps)
                }
                Err(payload) => std::panic::resume_unwind(payload),
            }
        });
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let outcome = join.await;
            drop(ctx_guard);
            let _ = tx.send(outcome);
        });
        let (roc_response, deferred, cache_deps) =
            rx.await.map_err(|_| "handler reaper vanished")??;

        let status = StatusCode::from_u16(roc_response.status).unwrap_or_else(|_| {
            tracing::error!("Roc handler returned invalid status {}", roc_response.status);
            StatusCode::INTERNAL_SERVER_ERROR
        });
        let mut response = Response::new(AppBody::full(roc_response.body.into_bytes()));
        *response.status_mut() = status;
        for (name, value) in &roc_response.headers {
            match (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                (Ok(name), Ok(value)) => {
                    response.headers_mut().append(name, value);
                }
                _ => tracing::warn!("Roc handler returned invalid header {name:?}"),
            }
        }

        // Hand the request's cache attribution to the cache middleware as a
        // response extension (consumed by `maybe_store`).
        if let Some(deps) = cache_deps {
            response.extensions_mut().insert(deps);
        }

        // Fire-and-forget `Opt` effects (B3): run them detached on the blocking
        // pool, concurrent with hyper writing this response's body, so they add
        // no latency to the client. Failures are logged inside `run`; the
        // handle is dropped so a panicking `Opt` cannot affect the response.
        if !deferred.is_empty() {
            tokio::task::spawn_blocking(move || deferred.run());
        }

        if let Some(t) = dispatch_start {
            crate::profile::record_dispatch(t.elapsed().as_nanos() as u64);
        }
        Ok(response)
    }

    fn parse_cookies(headers: &http::HeaderMap) -> Vec<(String, String)> {
        let mut cookies = Vec::new();
        for value in headers.get_all(http::header::COOKIE) {
            let Ok(text) = value.to_str() else { continue };
            for pair in text.split(';') {
                let pair = pair.trim();
                if pair.is_empty() {
                    continue;
                }
                match pair.split_once('=') {
                    Some((name, value)) => {
                        cookies.push((name.trim().to_owned(), value.trim().to_owned()));
                    }
                    None => cookies.push((pair.to_owned(), String::new())),
                }
            }
        }
        cookies
    }

    async fn serve_file(
        &self,
        mount: &ServeFilesMount,
        rewritten_path: String,
        req: Request<Incoming>,
    ) -> Result<Response<AppBody>, BoxError> {
        let (mut parts, body) = req.into_parts();
        let path_and_query = match parts.uri.query() {
            Some(query) => format!("{rewritten_path}?{query}"),
            None => rewritten_path,
        };
        let mut uri_parts = parts.uri.into_parts();
        uri_parts.path_and_query = Some(path_and_query.parse()?);
        parts.uri = http::Uri::from_parts(uri_parts)?;
        let req = Request::from_parts(parts, body);

        let response = mount.service.clone().oneshot(req).await?;
        Ok(response.map(AppBody::new))
    }
}
