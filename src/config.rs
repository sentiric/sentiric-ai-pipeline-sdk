#[derive(Debug, Clone)]
pub struct SdkConfig {
    pub stt_gateway_url: String,
    pub dialog_service_url: String,
    pub tts_gateway_url: String,

    pub tls_ca_path: String,
    pub tls_cert_path: String,
    pub tls_key_path: String,

    pub language_code: String,
    pub system_prompt_id: String,

    pub tts_voice_id: String,
    pub tts_sample_rate: u32,
}
