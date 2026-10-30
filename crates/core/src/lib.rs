//! Simple Setup engine. The CLI and the desktop app are thin shells over this crate:
//! every scan, snapshot build, verification and restore runs here.

pub mod adapters;
pub mod build;
pub mod catalog;
pub mod crypt;
pub mod discovery;
pub mod fetch;
pub mod inspect;
pub mod installers;
pub mod manifest;
pub mod paths;
pub mod platform;
pub mod probe;
pub mod registry;
pub mod resolve;
pub mod restore;
pub mod selection;
pub mod sensitive;
pub mod snapshot;
pub mod store;
pub mod transfer;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("The operation was cancelled.")]
    Cancelled,
    #[error("This snapshot is encrypted. Enter its password or recovery key.")]
    Locked,
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn invalid(msg: impl Into<String>) -> Error {
    Error::Invalid(msg.into())
}

pub(crate) trait IoContext<T> {
    fn ctx(self, what: impl FnOnce() -> String) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn ctx(self, what: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|source| Error::Io {
            context: what(),
            source,
        })
    }
}

/// RFC 3339 UTC timestamp, second precision.
pub fn now() -> String {
    humantime::format_rfc3339_seconds(std::time::SystemTime::now()).to_string()
}
