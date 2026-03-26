//! TLS connector factory for PostgreSQL connections.
//!
//! Builds a [`deadpool_postgres::Pool`] with the appropriate TLS connector
//! based on the configured [`SslMode`].  Uses `native-tls` which delegates
//! to the platform's TLS library (OpenSSL on Linux, Secure Transport on macOS,
//! SChannel on Windows).

use deadpool_postgres::{Pool, Runtime};
use postgres_native_tls::MakeTlsConnector;
use thiserror::Error;
use tokio_postgres::NoTls;

use crate::config::SslMode;

#[derive(Debug, Error)]
pub enum CreatePoolError {
    #[error("{0}")]
    Pool(#[from] deadpool_postgres::CreatePoolError),
    #[error("postgres TLS configuration failed: {0}")]
    TlsConfig(#[from] native_tls::Error),
}

/// Build a native-tls connector using the platform's certificate store.
fn make_tls_connector() -> Result<MakeTlsConnector, native_tls::Error> {
    let tls_connector = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(false)
        .build()?;
    Ok(MakeTlsConnector::new(tls_connector))
}

/// Create a [`deadpool_postgres::Pool`] with the appropriate TLS connector.
///
/// - `Disable` → plain TCP (no TLS)
/// - `Prefer` / `Require` → native-tls with platform certificate store
///
/// **Note:** `Prefer` and `Require` currently behave identically — both
/// provide a TLS connector and will fail if the server rejects the TLS
/// handshake.  True `prefer` semantics (retry without TLS on failure)
/// would require reconnection logic that tokio-postgres does not provide
/// out of the box.  The three-variant enum is kept for forward-compatibility
/// and familiarity with libpq's `sslmode` parameter.
pub fn create_pool(
    config: &deadpool_postgres::Config,
    ssl_mode: SslMode,
) -> Result<Pool, CreatePoolError> {
    match ssl_mode {
        SslMode::Disable => config
            .create_pool(Some(Runtime::Tokio1), NoTls)
            .map_err(CreatePoolError::from),
        SslMode::Prefer | SslMode::Require => {
            let tls = make_tls_connector()?;
            config
                .create_pool(Some(Runtime::Tokio1), tls)
                .map_err(CreatePoolError::from)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_pool_disable_mode() {
        let mut config = deadpool_postgres::Config::new();
        config.url = Some("postgres://localhost/test".to_string());
        let pool = create_pool(&config, SslMode::Disable);
        assert!(pool.is_ok());
    }

    #[test]
    fn create_pool_prefer_mode() {
        let mut config = deadpool_postgres::Config::new();
        config.url = Some("postgres://localhost/test".to_string());
        let pool = create_pool(&config, SslMode::Prefer);
        assert!(pool.is_ok());
    }

    #[test]
    fn create_pool_require_mode() {
        let mut config = deadpool_postgres::Config::new();
        config.url = Some("postgres://localhost/test".to_string());
        let pool = create_pool(&config, SslMode::Require);
        assert!(pool.is_ok());
    }
}
