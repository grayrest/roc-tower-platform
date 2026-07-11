//! Load the Roc app's route table and settings into host-side configuration.

use std::path::PathBuf;
use std::time::Duration;

use crate::abi;
use crate::roc_call::RocHandler;
use crate::types::{RocSetting, RocSettingTag};

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
/// routes. The Roc value is intentionally never released: handlers must stay
/// alive for the lifetime of the server.
pub fn load_app() -> Result<LoadedApp, String> {
    let app = unsafe { abi::roc_app() };

    let mut config = HostConfig {
        port: env_port().unwrap_or(DEFAULT_PORT),
        timeout: DEFAULT_TIMEOUT,
        max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        cors: CorsMode::Off,
        compression: false,
        concurrency_limit: None,
        serve_files: None,
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
            handler: RocHandler(route.handler),
        });
    }

    Ok(LoadedApp { config, routes })
}

fn apply_setting(setting: &RocSetting, config: &mut HostConfig) -> Result<(), String> {
    match setting.tag {
        RocSettingTag::Port => config.port = setting.payload_port(),
        RocSettingTag::TimeoutMs => {
            config.timeout = Duration::from_millis(setting.payload_timeout_ms());
        }
        RocSettingTag::MaxBodyBytes => {
            config.max_body_bytes = usize::try_from(setting.payload_max_body_bytes())
                .map_err(|_| "MaxBodyBytes does not fit in usize".to_string())?;
        }
        RocSettingTag::CorsPermissive => config.cors = CorsMode::Permissive,
        RocSettingTag::CorsAllowOrigins => {
            let origins = setting
                .payload_cors_allow_origins()
                .as_slice()
                .iter()
                .map(|origin| String::from_utf8_lossy(origin.as_slice()).into_owned())
                .collect();
            config.cors = CorsMode::AllowOrigins(origins);
        }
        RocSettingTag::Compression => config.compression = true,
        RocSettingTag::ConcurrencyLimit => {
            let limit = setting.payload_concurrency_limit();
            if limit == 0 {
                return Err("ConcurrencyLimit must be greater than zero".to_string());
            }
            config.concurrency_limit = Some(
                usize::try_from(limit)
                    .map_err(|_| "ConcurrencyLimit does not fit in usize".to_string())?,
            );
        }
        RocSettingTag::ServeFiles => {
            let payload = setting.payload_serve_files();
            let url_prefix = String::from_utf8_lossy(payload.url_prefix.as_slice()).into_owned();
            let dir =
                PathBuf::from(String::from_utf8_lossy(payload.dir.as_slice()).into_owned());
            if !url_prefix.starts_with('/') {
                return Err(format!(
                    "ServeFiles url_prefix {url_prefix:?} must start with '/'"
                ));
            }
            config.serve_files = Some(ServeFilesConfig { url_prefix, dir });
        }
    }
    Ok(())
}
