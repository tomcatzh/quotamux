//! Optional built-in trust store for standalone Linux distributions.

#[cfg(feature = "standalone-tls")]
const ROOTS: &[u8] = include_bytes!("../assets/tls/mozilla-ca-bundle.pem");

pub(crate) fn reqwest_builder() -> Result<reqwest::ClientBuilder, reqwest::Error> {
    let builder = reqwest::Client::builder();
    #[cfg(feature = "standalone-tls")]
    let builder = builder.tls_certs_only(reqwest::Certificate::from_pem_bundle(ROOTS)?);
    Ok(builder)
}

pub(crate) fn aws_client(
    builder: aws_sdk_bedrockruntime::config::Builder,
) -> aws_sdk_bedrockruntime::config::Builder {
    #[cfg(feature = "standalone-tls")]
    let builder = builder.http_client(aws_http_client());
    builder
}

pub(crate) fn aws_loader(loader: aws_config::ConfigLoader) -> aws_config::ConfigLoader {
    #[cfg(feature = "standalone-tls")]
    let loader = loader.http_client(aws_http_client());
    loader
}

#[cfg(feature = "standalone-tls")]
pub(crate) fn aws_http_client() -> aws_sdk_bedrockruntime::config::SharedHttpClient {
    use aws_smithy_http_client::{
        Builder,
        tls::{self, rustls_provider::CryptoMode},
    };
    let context = tls::TlsContext::builder()
        .with_trust_store(tls::TrustStore::empty().with_pem_certificate(ROOTS))
        .build()
        .expect("bundled Mozilla trust store");
    Builder::new()
        .tls_provider(tls::Provider::Rustls(CryptoMode::AwsLc))
        .tls_context(context)
        .build_https()
}

#[cfg(all(test, feature = "standalone-tls"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn standalone_clients_use_a_complete_embedded_trust_store() {
        assert!(reqwest::Certificate::from_pem_bundle(ROOTS).unwrap().len() > 100);
        reqwest_builder().unwrap().build().unwrap();
        let builder = aws_client(aws_sdk_bedrockruntime::config::Builder::new());
        assert!(builder.build().http_client().is_some());
        let _ = aws_http_client();
    }
}
