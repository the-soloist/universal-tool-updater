use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::domain::{NetworkConfig, Tool};

#[derive(Debug)]
pub struct AppConfig {
    pub app_root: PathBuf,
    pub paths: Paths,
    pub network: NetworkConfig,
    /// Permits plain-HTTP sources end to end; HTTPS remains the only default.
    pub allow_insecure_transports: bool,
    pub tools: BTreeMap<String, Tool>,
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub toolkit_root: PathBuf,
    pub downloads: PathBuf,
    pub staging: PathBuf,
    pub state: PathBuf,
}
