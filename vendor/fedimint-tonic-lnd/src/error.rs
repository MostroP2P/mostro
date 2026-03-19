use std::fmt;
use std::path::PathBuf;

/// Error that could happen during connecting to LND
///
/// This error may be returned by the `connect()` function if connecting failed.
/// It is currently opaque because it's unclear how the variants will look long-term.
/// Thus you probably only want to display it.
#[derive(Debug)]
pub struct ConnectError {
    internal: InternalConnectError,
}

impl From<InternalConnectError> for ConnectError {
    fn from(value: InternalConnectError) -> Self {
        ConnectError { internal: value }
    }
}

#[derive(Debug)]
pub(crate) enum InternalConnectError {
    ReadFile {
        file: PathBuf,
        error: std::io::Error,
    },
    ParseCert {
        file: PathBuf,
        error: std::io::Error,
    },
    InvalidAddress {
        address: String,
        error: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use InternalConnectError::*;

        match &self.internal {
            ReadFile { file, .. } => write!(f, "failed to read file {}", file.display()),
            ParseCert { file, .. } => write!(f, "failed to parse certificate {}", file.display()),
            InvalidAddress { address, .. } => write!(f, "invalid address {}", address),
        }
    }
}

impl std::error::Error for ConnectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        use InternalConnectError::*;

        match &self.internal {
            ReadFile { error, .. } => Some(error),
            ParseCert { error, .. } => Some(error),
            InvalidAddress { error, .. } => Some(&**error),
        }
    }
}
