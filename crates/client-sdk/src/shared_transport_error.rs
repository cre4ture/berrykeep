use std::sync::Arc;

/// A clonable failure that retains typed causes for concurrent transport consumers.
#[derive(Clone, Debug)]
pub(crate) struct SharedTransportError(Arc<anyhow::Error>);

impl From<anyhow::Error> for SharedTransportError {
    fn from(error: anyhow::Error) -> Self {
        Self(Arc::new(error))
    }
}

impl std::fmt::Display for SharedTransportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:#}", self.0)
    }
}

impl std::error::Error for SharedTransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}
