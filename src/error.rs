use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum UpdaterError {
    #[error("configuration error in {path}: {message}")]
    Configuration { path: PathBuf, message: String },

    #[error("tool {tool}: release resolution failed: {message}")]
    Resolution { tool: String, message: String },

    #[error("tool {tool}: download failed: {message}")]
    Download { tool: String, message: String },

    #[error("archive operation failed for {path}: {message}")]
    Archive { path: PathBuf, message: String },

    #[error("tool {tool}: installation failed: {message}")]
    Installation { tool: String, message: String },

    #[error("tool {tool}: hook {stage} failed: {message}")]
    Hook {
        tool: String,
        stage: String,
        message: String,
    },
}

impl UpdaterError {
    pub fn config(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::Configuration {
            path: path.into(),
            message: message.into(),
        }
    }
}
