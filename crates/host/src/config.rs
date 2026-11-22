//! Load the Roc app's route table and settings into host-side configuration.

use std::path::PathBuf;
use std::time::Duration;

use crate::abi;
use crate::types::RocHandler;
use crate::types::{RocSetting, RocSettingView};

const DEFAULT_PORT: u16 = 8000;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq)]
pub enum CorsMode {
    #[default]
    Off,
    Permissive,
    AllowOrigins(Vec<String>),
}

#[derive(Clone, Debug)]
pub struct HostConfig {
    pub port: u16,
    pub timeout: Duration,
    pub max_body_bytes: usize,
    pub cors: CorsMode,
    pub compression: bool,
    pub concurrency_limit: Option<usize>,
    pub serve_files: Option<ServeFilesConfig>,
    pub response_cache: Option<ResponseCacheConfig>,
}

/// The in-memory response cache policy, opted into by the app. `methods` and
/// `bypass_headers` make the cache scope EXPLICIT: only the listed methods are
/// cached, and a request carrying any `bypass_headers` header skips the cache
/// entirely — lookup AND store — so a personalized response is never served
/// from, or written into, a shared entry.
///
/// A credentialed request must be excluded on BOTH sides: an `authorization`
/// request must not be served a cached anonymous body (it would see the wrong
/// personalization), and its own response must not be stored (a later
/// anonymous request would see it). Only a pre-handler, request-shape predicate
/// can gate the lookup side — the handler (and any coeffect it runs) executes
/// AFTER the lookup, so it cannot influence whether a hit is served. That is
/// why cacheability keys off the request here, not off what the handler does.
/// `authorization` is therefore hard-defaulted into the bypass set (see
/// `apply_setting`): an app's list ADDS to it and can never drop it, so a
/// missing entry can't silently leak credentials into the shared cache.
#[derive(Clone, Debug)]
pub struct ResponseCacheConfig {
    pub ttl: Duration,
    pub max_entries: usize,
    pub methods: Vec<http::Method>,
    pub bypass_headers: Vec<http::HeaderName>,
}

#[derive(Clone, Debug)]
pub struct ServeFilesConfig {
    pub url_prefix: String,
    pub dir: PathBuf,
}

pub struct RouteDef {
    pub method: http::Method,
    pub path: String,
    pub handler: RocHandler,
}

pub struct LoadedApp {
    pub config: HostConfig,
    pub routes: Vec<RouteDef>,
}

fn env_port() -> Option<u16> {
    std::env::var("PORT").ok().and_then(|raw| raw.parse().ok())
}

/// Call the app's `main` entrypoint and translate the returned settings and
/// routes. Each route keeps its own reference to its handler, so the handlers
/// live as long as the router does and the rest of the app value is released
/// here.
pub fn load_app() -> Result<LoadedApp, String> {
    let app = abi::roc_app();

    let mut config = HostConfig {
        port: env_port().unwrap_or(DEFAULT_PORT),
        timeout: DEFAULT_TIMEOUT,
        max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        cors: CorsMode::Off,
        compression: false,
        concurrency_limit: None,
        serve_files: None,
        response_cache: None,
    };

    for setting in app.settings.as_slice() {
        apply_setting(setting, &mut config)?;
    }

    let mut routes = Vec::with_capacity(app.routes.len());
    for route in app.routes.as_slice() {
        let raw_method = String::from_utf8_lossy(route.method.as_slice()).into_owned();
        let method: http::Method = raw_method
            .to_ascii_uppercase()
            .parse()
            .map_err(|_| format!("route has invalid HTTP method {raw_method:?}"))?;
        let path = String::from_utf8_lossy(route.path.as_slice()).into_owned();
        if !path.starts_with('/') {
            return Err(format!("route path {path:?} must start with '/'"));
        }
        routes.push(RouteDef {
            method,
            path,
            handler: route.handler.clone(),
        });
    }

    Ok(LoadedApp { config, routes })
}

fn apply_setting(setting: &RocSetting, config: &mut HostConfig) -> Result<(), String> {
    match setting.view() {
        RocSettingView::Port(port) => config.port = *port,
        RocSettingView::TimeoutMs(ms) => {
            config.timeout = Duration::from_millis(*ms);
        }
        RocSettingView::MaxBodyBytes(bytes) => {
            config.max_body_bytes =
                usize::try_from(*bytes)
                .map_err(|_| "MaxBodyBytes does not fit in usize".to_string())?;
        }
        RocSettingView::CorsPermissive => config.cors = CorsMode::Permissive,
        RocSettingView::CorsAllowOrigins(origins) => {
            let origins = origins
                .as_slice()
                .iter()
                .map(|origin| String::from_utf8_lossy(origin.as_slice()).into_owned())
                .collect();
            config.cors = CorsMode::AllowOrigins(origins);
        }
        RocSettingView::Compression => config.compression = true,
        RocSettingView::ConcurrencyLimit(limit) => {
            let limit = *limit;
            if limit == 0 {
                return Err("ConcurrencyLimit must be greater than zero".to_string());
            }
            config.concurrency_limit = Some(
                usize::try_from(limit)
                    .map_err(|_| "ConcurrencyLimit does not fit in usize".to_string())?,
            );
        }
        RocSettingView::ServeFiles { dir, url_prefix } => {
            let url_prefix = String::from_utf8_lossy(url_prefix.as_slice()).into_owned();
            let dir = PathBuf::from(String::from_utf8_lossy(dir.as_slice()).into_owned());
            if !url_prefix.starts_with('/') {
                return Err(format!(
                    "ServeFiles url_prefix {url_prefix:?} must start with '/'"
                ));
            }
            config.serve_files = Some(ServeFilesConfig { url_prefix, dir });
        }
        RocSettingView::ResponseCache { max_entries, ttl_ms, bypass_headers, methods } => {
            if *ttl_ms == 0 {
                return Err("ResponseCache ttl_ms must be greater than zero".to_string());
            }
            let max_entries = usize::try_from(*max_entries)
                .map_err(|_| "ResponseCache max_entries does not fit in usize".to_string())?;
            if max_entries == 0 {
                return Err("ResponseCache max_entries must be greater than zero".to_string());
            }
            let methods = methods
                .as_slice()
                .iter()
                .map(|m| {
                    let raw = String::from_utf8_lossy(m.as_slice()).into_owned();
                    raw.to_ascii_uppercase()
                        .parse::<http::Method>()
                        .map_err(|_| format!("ResponseCache: invalid method {raw:?}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let app_bypass = bypass_headers
                .as_slice()
                .iter()
                .map(|h| {
                    let raw = String::from_utf8_lossy(h.as_slice()).into_owned();
                    http::HeaderName::from_bytes(raw.to_ascii_lowercase().as_bytes())
                        .map_err(|_| format!("ResponseCache: invalid header name {raw:?}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let bypass_headers = with_authorization_bypass(app_bypass);
            config.response_cache = Some(ResponseCacheConfig {
                ttl: Duration::from_millis(*ttl_ms),
                max_entries,
                methods,
                bypass_headers,
            });
        }
    }
    Ok(())
}

/// Ensure `authorization` is in the bypass set — the hard-default that keeps a
/// credentialed request out of the shared cache on both lookup and store (see
/// [`ResponseCacheConfig`]). The app's list adds to this and can never drop it.
fn with_authorization_bypass(
    mut headers: Vec<http::HeaderName>,
) -> Vec<http::HeaderName> {
    if !headers.contains(&http::header::AUTHORIZATION) {
        headers.push(http::header::AUTHORIZATION);
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    /// authorization is bypassed even when the app declares no bypass headers
    #[test]
    fn authorization_is_hard_defaulted_when_app_list_empty() {
        let bypass = with_authorization_bypass(vec![]);
        assert!(bypass.contains(&http::header::AUTHORIZATION));
    }

    /// the app's own bypass headers are kept, with authorization added
    #[test]
    fn app_bypass_headers_are_preserved_alongside_the_default() {
        let bypass = with_authorization_bypass(vec![http::header::COOKIE]);
        assert!(bypass.contains(&http::header::COOKIE));
        assert!(bypass.contains(&http::header::AUTHORIZATION));
    }

    /// an app that lists authorization itself gets no duplicate
    #[test]
    fn authorization_is_not_duplicated() {
        let bypass = with_authorization_bypass(vec![http::header::AUTHORIZATION]);
        let count =
            bypass.iter().filter(|h| **h == http::header::AUTHORIZATION).count();
        assert_eq!(count, 1);
    }
}
