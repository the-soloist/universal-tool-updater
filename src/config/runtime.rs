use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::domain::{ExtractionLimits, NetworkConfig, Tool};

#[derive(Debug)]
pub struct AppConfig {
    pub app_root: PathBuf,
    pub paths: Paths,
    pub network: NetworkConfig,
    pub allow_insecure_transports: bool,
    pub extraction_limits: ExtractionLimits,
    pub tools: BTreeMap<String, Tool>,
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub toolkit_root: PathBuf,
    pub downloads: PathBuf,
    pub staging: PathBuf,
    pub state: PathBuf,
}
