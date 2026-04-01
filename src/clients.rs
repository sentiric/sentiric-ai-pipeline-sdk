// File: sentiric-ai-pipeline-sdk/src/clients.rs
use crate::config::SdkConfig;
use crate::error::SdkError;
use sentiric_contracts::sentiric::dialog::v1::dialog_service_client::DialogServiceClient;
use sentiric_contracts::sentiric::stt::v1::stt_gateway_service_client::SttGatewayServiceClient;
use sentiric_contracts::sentiric::tts::v1::tts_gateway_service_client::TtsGatewayServiceClient;
use std::str::FromStr;
use tonic::metadata::MetadataValue;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};

#[derive(Clone)]
pub struct ApiClients {
    pub stt: SttGatewayServiceClient<Channel>,
    pub dialog: DialogServiceClient<Channel>,
    pub tts: TtsGatewayServiceClient<Channel>,
}

impl ApiClients {
    pub async fn connect(config: &SdkConfig) -> Result<Self, SdkError> {
        // [ARCH-COMPLIANCE] Insecure bağlantı engellemesi (mTLS Strict Policy)
        if config.stt_gateway_url.starts_with("http://")
            || config.dialog_service_url.starts_with("http://")
            || config.tts_gateway_url.starts_with("http://")
        {
            return Err(SdkError::InvalidArgument(
                "Architectural Violation: Insecure HTTP channels are strictly forbidden. Use https:// and mTLS.".into(),
            ));
        }

        let tls_config = Self::load_tls(config).await?;

        // [ARCH-COMPLIANCE] Senkron .connect().await yerine .connect_lazy() yapıldı.
        let stt_channel = Endpoint::from_shared(config.stt_gateway_url.clone())
            .map_err(|e| SdkError::ConnectionError(e.to_string()))?
            .tls_config(tls_config.clone())
            .map_err(|e| SdkError::TlsConfigError(e.to_string()))?
            .connect_lazy();

        let dialog_channel = Endpoint::from_shared(config.dialog_service_url.clone())
            .map_err(|e| SdkError::ConnectionError(e.to_string()))?
            .tls_config(tls_config.clone())
            .map_err(|e| SdkError::TlsConfigError(e.to_string()))?
            .connect_lazy();

        let tts_channel = Endpoint::from_shared(config.tts_gateway_url.clone())
            .map_err(|e| SdkError::ConnectionError(e.to_string()))?
            .tls_config(tls_config)
            .map_err(|e| SdkError::TlsConfigError(e.to_string()))?
            .connect_lazy();

        Ok(Self {
            stt: SttGatewayServiceClient::new(stt_channel),
            dialog: DialogServiceClient::new(dialog_channel),
            tts: TtsGatewayServiceClient::new(tts_channel),
        })
    }

    async fn load_tls(config: &SdkConfig) -> Result<ClientTlsConfig, SdkError> {
        let ca_cert = tokio::fs::read(&config.tls_ca_path).await.map_err(|e| {
            tracing::error!(event="SDK_TLS_ERROR", path=%config.tls_ca_path, error=%e, "CA Cert okunamadı. İzinleri kontrol edin.");
            SdkError::TlsConfigError(format!("CA read error ({}): {}", config.tls_ca_path, e))
        })?;
        let ca = Certificate::from_pem(ca_cert);

        let cert = tokio::fs::read(&config.tls_cert_path).await.map_err(|e| {
            tracing::error!(event="SDK_TLS_ERROR", path=%config.tls_cert_path, error=%e, "Cert okunamadı. İzinleri kontrol edin.");
            SdkError::TlsConfigError(format!("Cert read error ({}): {}", config.tls_cert_path, e))
        })?;
        let key = tokio::fs::read(&config.tls_key_path).await.map_err(|e| {
            tracing::error!(event="SDK_TLS_ERROR", path=%config.tls_key_path, error=%e, "Key okunamadı. İzinleri kontrol edin.");
            SdkError::TlsConfigError(format!("Key read error ({}): {}", config.tls_key_path, e))
        })?;
        let identity = Identity::from_pem(cert, key);

        Ok(ClientTlsConfig::new()
            .domain_name("sentiric.cloud")
            .ca_certificate(ca)
            .identity(identity))
    }

    pub fn inject_metadata<T>(
        &self,
        mut req: tonic::Request<T>,
        trace_id: &str,
        span_id: &str,
        tenant_id: &str,
    ) -> tonic::Request<T> {
        if let Ok(val) = MetadataValue::from_str(trace_id) {
            req.metadata_mut().insert("x-trace-id", val);
        }
        if let Ok(val) = MetadataValue::from_str(span_id) {
            req.metadata_mut().insert("x-span-id", val);
        }
        if let Ok(val) = MetadataValue::from_str(tenant_id) {
            req.metadata_mut().insert("x-tenant-id", val);
        }
        req
    }
}
