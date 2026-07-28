use rustls::crypto::CryptoProvider;
use thiserror::Error;

/// Process-wide TLS provider selection for binaries that combine the Crow HTTP,
/// WebSocket, and venue dependency graphs.
#[derive(Debug, Error)]
#[error("could not install the process TLS crypto provider")]
pub struct TlsProviderError;

/// Selects Ring before any TLS client is constructed.
///
/// Some venue dependencies enable AWS-LC while Crow's direct HTTP/WebSocket
/// clients enable Ring. Rustls deliberately refuses to guess when both are
/// present, so every Crow process makes the choice explicitly and idempotently.
pub fn install_tls_crypto_provider() -> Result<(), TlsProviderError> {
    if CryptoProvider::get_default().is_some() {
        return Ok(());
    }
    match rustls::crypto::ring::default_provider().install_default() {
        Ok(()) => Ok(()),
        Err(_) if CryptoProvider::get_default().is_some() => Ok(()),
        Err(_) => Err(TlsProviderError),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_a_process_provider_idempotently() {
        assert!(install_tls_crypto_provider().is_ok());
        assert!(install_tls_crypto_provider().is_ok());
        assert!(CryptoProvider::get_default().is_some());
    }
}
