mod completion;
mod download;
mod http;
mod partial;
mod recovery;
pub(crate) mod transfer;

pub use download::Downloader;

#[cfg(test)]
pub(super) use completion::DownloadCompletion;

#[cfg(test)]
mod tests;
