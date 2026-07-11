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

        if let Some(router) = self.routers.get(&method) {
            if let Ok(matched) = router.at(&path) {
                let handler = *matched.value;
                let params = matched
                    .params
                    .iter()
                    .map(|(name, value)| (name.to_owned(), value.to_owned()))
                    .collect();
                return self.dispatch(handler, params, req).await;
            }
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

        if let Some(mount) = &self.serve_files {
            if let Some(rewritten) = mount.strip_prefix(&path) {
                return self.serve_file(mount, rewritten, req).await;
            }
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

        let request_parts = RequestParts {
            method: parts.method.as_str().to_owned(),
            path: parts.uri.path().to_owned(),
            params,
            query: parts.uri.query().unwrap_or("").to_owned(),
            headers: parts
                .headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_owned(),
                        String::from_utf8_lossy(value.as_bytes()).into_owned(),
                    )
                })
                .collect(),
            body: collected,
        };

        let model = self.model;
        let roc_response =
            tokio::task::spawn_blocking(move || call_handler(handler, model, request_parts))
                .await?;

        let status = StatusCode::from_u16(roc_response.status).unwrap_or_else(|_| {
            tracing::error!("Roc handler returned invalid status {}", roc_response.status);
            StatusCode::INTERNAL_SERVER_ERROR
        });
        let mut response = Response::new(AppBody::full(roc_response.body));
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
        Ok(response)
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
