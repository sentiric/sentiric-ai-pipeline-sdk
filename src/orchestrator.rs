// File: sentiric-ai-pipeline-sdk/src/orchestrator.rs
use crate::clients::ApiClients;
use crate::config::SdkConfig;
use crate::error::SdkError;
use crate::{PipelineEvent, TranscriptData, WordData};
use futures::StreamExt;
use sentiric_contracts::sentiric::dialog::v1::stream_conversation_request::Payload as DialogPayload;
use sentiric_contracts::sentiric::dialog::v1::{ConversationConfig, StreamConversationRequest};
use sentiric_contracts::sentiric::stt::v1::TranscribeStreamRequest;
use sentiric_contracts::sentiric::tts::v1::{AudioConfig, SynthesizeStreamRequest};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

pub struct PipelineOrchestrator {
    config: SdkConfig,
    clients: ApiClients,
}

// "Deep Waters" State Takibi İçin Basit Bir Hafıza Yapısı
struct AcousticState {
    last_arousal: f32,
    last_valence: f32,
    current_mood: String,
    speaker_id: String, // [YENİ]: Şizofreni koruması için
}

impl PipelineOrchestrator {
    pub async fn new(config: SdkConfig) -> Result<Self, SdkError> {
        let clients = ApiClients::connect(&config).await?;
        Ok(Self { config, clients })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn run_pipeline(
        &self,
        session_id: String,
        user_id: String,
        trace_id: String,
        span_id: String,
        tenant_id: String,
        mut rx_audio: mpsc::Receiver<Vec<u8>>,
        tx_out: mpsc::Sender<PipelineEvent>,
        mut interrupt_rx: mpsc::Receiver<()>,
    ) -> Result<(), SdkError> {
        info!(
            event = "AI_PIPELINE_START",
            trace_id = %trace_id, span_id = %span_id, tenant_id = %tenant_id,
            listen_only = self.config.listen_only_mode,
            "🚀 AI Pipeline started."
        );

        if self.config.edge_mode {
            info!(
                event = "EDGE_MODE_ACTIVE",
                trace_id = %trace_id, span_id = %span_id, tenant_id = %tenant_id,
                "Edge mode active, disabling heavy telemetry and applying low-latency buffer constraints."
            );
        }

        let (stt_req_tx, stt_req_rx) = mpsc::channel(128);

        tokio::spawn(async move {
            while let Some(chunk) = rx_audio.recv().await {
                let req = TranscribeStreamRequest { audio_chunk: chunk };
                if stt_req_tx.send(req).await.is_err() {
                    break;
                }
            }
        });

        let stt_request = self.clients.inject_metadata(
            tonic::Request::new(ReceiverStream::new(stt_req_rx)),
            &trace_id,
            &span_id,
            &tenant_id,
        );

        let mut stt_client = self.clients.stt.clone();
        let mut stt_response_stream = match stt_client.transcribe_stream(stt_request).await {
            Ok(res) => res.into_inner(),
            Err(e) => {
                error!(event = "STT_CONNECT_FAIL", trace_id = %trace_id, error = %e, "Failed to connect to STT.");
                return Err(e.into());
            }
        };

        let mut cancel_token = CancellationToken::new();

        let mut acoustic_state = AcousticState {
            last_arousal: 0.0,
            last_valence: 0.0,
            current_mood: "neutral".to_string(),
            speaker_id: "".to_string(), // [YENİ]
        };

        loop {
            tokio::select! {
                Some(()) = interrupt_rx.recv() => {
                    info!(
                        event = "HARDWARE_BARGE_IN_TRIGGERED",
                        trace_id = %trace_id, span_id = %span_id, tenant_id = %tenant_id,
                        "⚡ Hardware VAD signal received from client. Cancelling Dialog/TTS tasks instantly."
                    );
                    cancel_token.cancel();
                    cancel_token = CancellationToken::new();
                    let _ = tx_out.try_send(PipelineEvent::ClearBuffer);
                }

                res_opt = stt_response_stream.next() => {
                    match res_opt {
                        Some(Ok(msg)) => {
                            let text = msg.partial_transcription.trim().to_string();

                            // UI için Kelimeleri Map Et
                            let mapped_words: Vec<WordData> = msg.words.into_iter().map(|w| WordData {
                                word: w.word,
                                start: w.start,
                                end: w.end,
                                probability: w.probability,
                            }).collect();

                            let current_arousal = msg.arousal;
                            let current_valence = msg.valence;
                            let speaker_id = msg.speaker_id.clone();

                            // STT'den gelen zengin metni ve duygu durumunu UI için dışarı aktar
                            let _ = tx_out.try_send(PipelineEvent::Transcript(TranscriptData {
                                text: text.clone(),
                                is_final: msg.is_final,
                                sender: "USER".to_string(),
                                emotion: msg.emotion_proxy.clone(),
                                gender: msg.gender_proxy.clone(),
                                arousal: current_arousal,
                                valence: current_valence,
                                speaker_id: speaker_id.clone(),
                                speaker_vec: msg.speaker_vec.clone(),
                                words: mapped_words,
                            }));

                            // -------------------------------------------------------------------------
                            // 🌊 [DEEP WATERS]: Duygu durumunda (Arousal/Tempo) ciddi değişim var mı?
                            // -------------------------------------------------------------------------
                            // Yalnızca "is_final" anında (Cümle bittiğinde) hesaplıyoruz ki
                            // hece hece gereksiz yere event fırlatmayalım.
                            if msg.is_final && current_arousal > 0.0 {
                                let arousal_diff = (current_arousal - acoustic_state.last_arousal).abs();

                                // SADECE AYNI KİŞİ İSE ve DEĞİŞİM BELİRGİNSE (Eşik 0.3'ten 0.15'e çekildi)
                                if acoustic_state.last_arousal > 0.0
                                    && acoustic_state.speaker_id == speaker_id
                                    && (arousal_diff > 0.15 || msg.emotion_proxy != acoustic_state.current_mood) {

                                    debug!(event="DEEP_WATERS_TRIGGERED", trace_id=%trace_id, "Akustik mod değişimi! Fark: {:.2}", arousal_diff);

                                    let _ = tx_out.try_send(PipelineEvent::AcousticMoodShifted {
                                        previous_mood: acoustic_state.current_mood.clone(),
                                        current_mood: msg.emotion_proxy.clone(),
                                        arousal_shift: current_arousal - acoustic_state.last_arousal,
                                        valence_shift: current_valence - acoustic_state.last_valence,
                                        speaker_id: speaker_id.clone()
                                    });
                                }

                                // Hafızayı güncelle
                                acoustic_state.last_arousal = current_arousal;
                                acoustic_state.last_valence = current_valence;
                                acoustic_state.current_mood = msg.emotion_proxy.clone();
                                acoustic_state.speaker_id = speaker_id.clone();
                            }

                            if !msg.is_final {
                                if !text.is_empty() && !self.config.listen_only_mode {
                                    info!(event = "SOFTWARE_BARGE_IN_TRIGGERED", trace_id = %trace_id, "⚡ Text-based Barge-in detected.");
                                    cancel_token.cancel();
                                    cancel_token = CancellationToken::new();
                                    let _ = tx_out.try_send(PipelineEvent::ClearBuffer);
                                }
                            } else if !text.is_empty() {
                                info!(event = "STT_FINAL_RECEIVED", trace_id = %trace_id, text = %text, "Final transcription received.");

                                // [ARCH-COMPLIANCE FIX]: Listen-Only Mode kontrolü. Sadece dinleyici isek yanıt üretme!
                                if self.config.listen_only_mode {
                                    debug!(event = "LISTEN_ONLY_SKIP", trace_id = %trace_id, "Listen-only mode active. Skipping Dialog and TTS generation.");
                                    continue;
                                }

                                let ct = cancel_token.child_token();
                                let clients_clone = self.clients.clone();
                                let config_clone = self.config.clone();
                                let tx_out_clone = tx_out.clone();
                                let s_id = session_id.clone();
                                let u_id = user_id.clone();
                                let tr_id = trace_id.clone();
                                let sp_id = span_id.clone();
                                let ten_id = tenant_id.clone();

                                tokio::spawn(async move {
                                    if let Err(e) = Self::handle_dialog_tts_phase(
                                        clients_clone, config_clone, s_id, u_id, tr_id.clone(), sp_id, ten_id, text, tx_out_clone, ct
                                    ).await {
                                        warn!(event = "DIALOG_TTS_ERROR", trace_id = %tr_id, error = %e, "Error during Dialog execution.");
                                    }
                                });
                            }
                        }
                        Some(Err(e)) => {
                            error!(event = "STT_STREAM_ERROR", trace_id = %trace_id, error = %e, "STT Stream encountered an error.");
                            return Err(e.into());
                        }
                        None => break,
                    }
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_dialog_tts_phase(
        mut clients: ApiClients,
        config: SdkConfig,
        session_id: String,
        user_id: String,
        trace_id: String,
        span_id: String,
        tenant_id: String,
        input_text: String,
        tx_out: mpsc::Sender<PipelineEvent>,
        cancel_token: CancellationToken,
    ) -> Result<(), SdkError> {
        let (dialog_req_tx, dialog_req_rx) = mpsc::channel(10);

        let config_payload = StreamConversationRequest {
            payload: Some(DialogPayload::Config(ConversationConfig {
                session_id,
                user_id,
                language_code: config.language_code.clone(),
                system_prompt_id: config.system_prompt_id.clone(),
            })),
        };
        if dialog_req_tx.send(config_payload).await.is_err() {
            return Err(SdkError::Internal("Dialog channel closed early".into()));
        }

        let text_payload = StreamConversationRequest {
            payload: Some(DialogPayload::TextInput(input_text)),
        };
        if dialog_req_tx.send(text_payload).await.is_err() {
            return Err(SdkError::Internal("Dialog channel closed early".into()));
        }

        let final_trigger_payload = StreamConversationRequest {
            payload: Some(DialogPayload::IsFinalInput(true)),
        };
        if dialog_req_tx.send(final_trigger_payload).await.is_err() {
            return Err(SdkError::Internal("Dialog channel closed early".into()));
        }

        drop(dialog_req_tx);

        let req = clients.inject_metadata(
            tonic::Request::new(ReceiverStream::new(dialog_req_rx)),
            &trace_id,
            &span_id,
            &tenant_id,
        );

        let dialog_resp_stream = tokio::select! {
            _ = cancel_token.cancelled() => return Ok(()),
            res = clients.dialog.stream_conversation(req) => res?.into_inner(),
        };

        let mut dialog_resp_stream = dialog_resp_stream;
        let mut sentence_buffer = String::new();

        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    info!(event = "DIALOG_STREAM_ABORTED", trace_id = %trace_id, "Barge-in: Dropping Dialog task.");
                    return Ok(());
                }
                res_opt = dialog_resp_stream.next() => {
                    match res_opt {
                        Some(Ok(msg)) => {
                            match msg.payload {
                                Some(sentiric_contracts::sentiric::dialog::v1::stream_conversation_response::Payload::TextResponse(text_chunk)) => {
                                    sentence_buffer.push_str(&text_chunk);

                                    if sentence_buffer.contains('.') || sentence_buffer.contains('?') || sentence_buffer.contains('!') || sentence_buffer.contains('\n') {
                                        let sentence = sentence_buffer.clone();
                                        sentence_buffer.clear();
                                        Self::synthesize_and_stream_tts(&mut clients, &config, sentence, &trace_id, &span_id, &tenant_id, &tx_out, &cancel_token).await?;
                                    }
                                }
                                Some(sentiric_contracts::sentiric::dialog::v1::stream_conversation_response::Payload::IsFinalResponse(true)) => {
                                    if !sentence_buffer.trim().is_empty() {
                                        let sentence = sentence_buffer.clone();
                                        sentence_buffer.clear();
                                        Self::synthesize_and_stream_tts(&mut clients, &config, sentence, &trace_id, &span_id, &tenant_id, &tx_out, &cancel_token).await?;
                                    }
                                }
                                _ => {}
                            }
                        }
                        Some(Err(e)) => return Err(SdkError::GrpcError(e)),
                        None => break,
                    }
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn synthesize_and_stream_tts(
        clients: &mut ApiClients,
        config: &SdkConfig,
        sentence: String,
        trace_id: &str,
        span_id: &str,
        tenant_id: &str,
        tx_out: &mpsc::Sender<PipelineEvent>,
        cancel_token: &CancellationToken,
    ) -> Result<(), SdkError> {
        let _ = tx_out.try_send(PipelineEvent::Transcript(TranscriptData {
            text: sentence.clone(),
            is_final: true,
            sender: "AI".to_string(),
            emotion: "neutral".to_string(),
            gender: "neutral".to_string(),
            arousal: 0.0,
            valence: 0.0,
            speaker_id: "AI_1".to_string(),
            speaker_vec: vec![],
            words: vec![],
        }));

        let tts_req = SynthesizeStreamRequest {
            text: sentence,
            text_type: 1,
            voice_id: config.tts_voice_id.clone(),
            audio_config: Some(AudioConfig {
                audio_format: 1,
                sample_rate_hertz: config.tts_sample_rate as i32,
                volume_gain_db: 0.0,
            }),
            preferred_provider: "".to_string(),
            tuning: None,
            cloning_audio_data: None,
        };

        let req =
            clients.inject_metadata(tonic::Request::new(tts_req), trace_id, span_id, tenant_id);

        let tts_stream = tokio::select! {
            _ = cancel_token.cancelled() => return Ok(()),
            res = clients.tts.synthesize_stream(req) => res?.into_inner(),
        };

        let mut tts_stream = tts_stream;

        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    info!(event = "TTS_STREAM_ABORTED", trace_id = %trace_id, "Barge-in: Dropping TTS audio.");
                    return Ok(());
                }
                audio_res_opt = tts_stream.next() => {
                    match audio_res_opt {
                        Some(Ok(audio_msg)) => {
                            let chunk = audio_msg.audio_content;
                            if !chunk.is_empty() && tx_out.send(PipelineEvent::Audio(chunk)).await.is_err() {
                                return Err(SdkError::Internal("Audio output sender dropped".into()));
                            }
                        }
                        Some(Err(e)) => { warn!(event="TTS_STREAM_ERROR", error=%e, "TTS error"); break; }
                        None => break,
                    }
                }
            }
        }
        Ok(())
    }
}
