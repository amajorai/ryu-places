use std::sync::Arc;

use crate::{geocode::Geocoder, host::Model, link::LinkReader, tenancy::Stores};

pub const PLUGIN_ID: &str = "@ryu/places";
pub const DEFAULT_PORT: u16 = 8026;

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub token: Option<String>,
    /// `RYU_MANAGED_NODE`: requests with no verified caller fail closed.
    pub managed_node: bool,
}

impl Config {
    pub fn from_env(port: u16) -> Self {
        Self {
            port,
            token: std::env::var("RYU_EXT_TOKEN")
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty()),
            managed_node: crate::tenancy::is_managed_node(),
        }
    }

    /// A configuration for tests and embedding: a fixed bearer, not managed.
    pub fn for_tests(token: &str) -> Self {
        Self {
            port: 0,
            token: Some(token.to_owned()),
            managed_node: false,
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub stores: Stores,
    pub geocoder: Geocoder,
    /// Core's side model; `None` when running standalone (AI routes then 503).
    pub model: Option<Arc<dyn Model>>,
    pub links: LinkReader,
    pub config: Arc<Config>,
}

impl AppState {
    pub fn new(stores: Stores, geocoder: Geocoder, config: Config) -> Self {
        Self {
            stores,
            geocoder,
            model: crate::host::Host::from_env().map(|host| Arc::new(host) as Arc<dyn Model>),
            links: LinkReader::new(),
            config: Arc::new(config),
        }
    }

    #[must_use]
    pub fn with_model(mut self, model: Option<Arc<dyn Model>>) -> Self {
        self.model = model;
        self
    }
}
