pub mod clients;
pub mod config;
pub mod error;
pub mod orchestrator;

pub use config::SdkConfig;
pub use error::SdkError;
pub use orchestrator::PipelineOrchestrator;

// [YENİ] UI ve Sistemler arası zengin veri akışı için eklendi
#[derive(Debug, Clone)]
pub struct TranscriptData {
    pub text: String,
    pub is_final: bool,
    pub sender: String,
    pub emotion: String,
    pub gender: String,
}

#[derive(Debug, Clone)]
pub enum PipelineEvent {
    Audio(Vec<u8>),
    Transcript(TranscriptData),
    ClearBuffer,
}
