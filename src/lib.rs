// Dosya: src/lib.rs
pub mod clients;
pub mod config;
pub mod error;
pub mod orchestrator;

pub use config::SdkConfig;
pub use error::SdkError;
pub use orchestrator::PipelineOrchestrator;

#[derive(Debug, Clone)]
pub struct WordData {
    pub word: String,
    pub start: f32,
    pub end: f32,
    pub probability: f32,
}

#[derive(Debug, Clone)]
pub struct TranscriptData {
    pub text: String,
    pub is_final: bool,
    pub sender: String,
    pub emotion: String,
    pub gender: String,
    pub arousal: f32,
    pub valence: f32,
    pub speaker_id: String,
    pub speaker_vec: Vec<f32>,
    pub words: Vec<WordData>,
}

// [YENİ]: Artık Pipeline sadece ses değil, metin de alabilir!
#[derive(Debug, Clone)]
pub enum PipelineInputEvent {
    Audio(Vec<u8>),
    Text(String),
}

#[derive(Debug, Clone)]
pub enum PipelineEvent {
    Audio(Vec<u8>),
    Transcript(TranscriptData),
    ClearBuffer,
    AcousticMoodShifted {
        previous_mood: String,
        current_mood: String,
        arousal_shift: f32,
        valence_shift: f32,
        speaker_id: String,
    },
}
