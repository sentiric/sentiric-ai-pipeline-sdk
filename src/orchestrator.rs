// File: src/orchestrator.rs
use crate::clients::ApiClients;
use crate::config::SdkConfig;
use crate::error::SdkError;
use crate::{PipelineEvent, PipelineInputEvent, TranscriptData, WordData};
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

struct AcousticState {
    last_arousal: f32,
    last_valence: f32,
    current_mood: String,
    speaker_id: String,
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
        mut rx_input: mpsc::Receiver<PipelineInputEvent>,
        tx_out: mpsc::Sender<PipelineEvent>,
        mut interrupt_rx: mpsc::Receiver<()>,
    ) -> Result<(), SdkError> {
        info!(
            event = "AI_PIPELINE_START",
            trace_id = %trace_id, span_id = %span_id, tenant_id = %tenant_id,
            listen_only = self.config.listen_only_mode,
            speak_only = self.config.speak_only_mode,
            chat_only = self.config.chat_only_mode,
            "🚀 AI Pipeline started."
        );

        if self.config.edge_mode {
            debug!(
                event = "EDGE_MODE_ACTIVE",
                trace_id = %trace_id, span_id = %span_id, tenant_id = %tenant_id,
                "Edge mode active, disabling heavy telemetry and applying low-latency buffer constraints."
            );
        }

        let (stt_req_tx, stt_req_rx) = mpsc::channel(128);
        let (text_trigger_tx, mut text_trigger_rx) = mpsc::channel::<String>(128);

        let is_speak_only = self.config.speak_only_mode;
        let is_chat_only = self.config.chat_only_mode;

        // [ARCH-COMPLIANCE FIX]: "Smart Media, Dumb Logic" kuralı gereği SDK içindeki
        // Hardcoded Türkçe karşılama metni silinmiştir.
        // Onun yerine LLM'i uyandırmak için dil-bağımsız evrensel bir "System Event" fırlatılır.
        // LLM, kendi sistem promptundaki dile (İngilizce/Almanca/Türkçe) göre doğal bir selam verecektir.
        if !is_speak_only && !self.config.listen_only_mode {
            let init_tx = text_trigger_tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                let _ = init_tx
                    .send("<SYSTEM_EVENT: CALL_CONNECTED>".to_string())
                    .await;
            });
        }

        // 1. Giriş Yönlendiricisi (Router)
        tokio::spawn(async move {
            while let Some(input) = rx_input.recv().await {
                match input {
                    PipelineInputEvent::Audio(chunk) => {
                        if !is_speak_only && !is_chat_only {
                            let req = TranscribeStreamRequest { audio_chunk: chunk };
                            if stt_req_tx.send(req).await.is_err() {
                                break;
                            }
                        }
                    }
                    PipelineInputEvent::Text(text) => {
                        if text_trigger_tx.send(text).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        // 2. STT Bağlantısı
        let mut stt_response_stream = None;
        if !self.config.speak_only_mode && !self.config.chat_only_mode {
            let stt_request = self.clients.inject_metadata(
                tonic::Request::new(ReceiverStream::new(stt_req_rx)),
                &trace_id,
                &span_id,
                &tenant_id,
            );
            let mut stt_client = self.clients.stt.clone();
            match stt_client.transcribe_stream(stt_request).await {
                Ok(res) => stt_response_stream = Some(res.into_inner()),
                Err(e) => {
                    error!(event = "STT_CONNECT_FAIL", trace_id = %trace_id, error = %e, "Failed to connect to STT.");
                    return Err(e.into());
                }
            }
        }

        let mut cancel_token = CancellationToken::new();
        let mut acoustic_state = AcousticState {
            last_arousal: 0.0,
            last_valence: 0.0,
            current_mood: "neutral".to_string(),
            speaker_id: "".to_string(),
        };

        loop {
            tokio::select! {
                Some(()) = interrupt_rx.recv() => {
                    info!(event = "HARDWARE_BARGE_IN_TRIGGERED", trace_id = %trace_id, "⚡ VAD signal received. Cancelling Tasks.");
                    cancel_token.cancel();
                    cancel_token = CancellationToken::new();
                    let _ = tx_out.try_send(PipelineEvent::ClearBuffer);
                }

                Some(text) = text_trigger_rx.recv() => {
                    info!(event = "TEXT_INPUT_RECEIVED", trace_id = %trace_id, text = %text, "Direct text input or Wake-up trigger received.");
                    cancel_token.cancel();
                    cancel_token = CancellationToken::new();
                    let _ = tx_out.try_send(PipelineEvent::ClearBuffer);

                    let ct = cancel_token.child_token();
                    let mut clients_clone = self.clients.clone();
                    let config_clone = self.config.clone();
                    let tx_out_clone = tx_out.clone();
                    let s_id = session_id.clone();
                    let u_id = user_id.clone();
                    let tr_id = trace_id.clone();
                    let sp_id = span_id.clone();
                    let ten_id = tenant_id.clone();

                    tokio::spawn(async move {
                        if config_clone.speak_only_mode {
                            let _ = Self::synthesize_and_stream_tts(&mut clients_clone, &config_clone, text, &tr_id, &sp_id, &ten_id, &tx_out_clone, &ct).await;
                        } else {
                            if let Err(e) = Self::handle_dialog_tts_phase(
                                clients_clone, config_clone, s_id, u_id, tr_id.clone(), sp_id, ten_id, text, tx_out_clone, ct
                            ).await {
                                warn!(event = "DIALOG_TTS_ERROR", trace_id = %tr_id, error = %e, "Error during Dialog execution.");
                            }
                        }
                    });
                }

                res_opt = async {
                    if let Some(stream) = stt_response_stream.as_mut() { stream.next().await }
                    else { futures::future::pending().await }
                } => {
                    match res_opt {
                        Some(Ok(msg)) => {
                            let text = msg.partial_transcription.trim().to_string();

                            // [HALÜSİNASYON FİLTRESİ]: Çok kısa, sadece "Iıı", "Eee" içeren nefes/gürültü çıktılarını atla.
                            if msg.is_final && !text.is_empty() {
                                let lower = text.to_lowercase();
                                if text.len() < 10 && (lower.contains("ıı") || lower.contains("ee") || lower.contains("mm") || lower.contains("hm")) {
                                    debug!(event = "STT_FILTERED_HALLUCINATION", trace_id = %trace_id, text = %text, "Filler word/Noise filtered.");
                                    continue;
                                }
                            }

                            let mapped_words: Vec<WordData> = msg.words.into_iter().map(|w| WordData { word: w.word, start: w.start, end: w.end, probability: w.probability }).collect();
                            let current_arousal = msg.arousal; let current_valence = msg.valence; let speaker_id = msg.speaker_id.clone();

                            let _ = tx_out.try_send(PipelineEvent::Transcript(TranscriptData {
                                text: text.clone(), is_final: msg.is_final, sender: "USER".to_string(), emotion: msg.emotion_proxy.clone(), gender: msg.gender_proxy.clone(),
                                arousal: current_arousal, valence: current_valence, speaker_id: speaker_id.clone(), speaker_vec: msg.speaker_vec.clone(), words: mapped_words,
                            }));

                            if msg.is_final && current_arousal > 0.0 {
                                let arousal_diff = (current_arousal - acoustic_state.last_arousal).abs();
                                if acoustic_state.last_arousal > 0.0 && acoustic_state.speaker_id == speaker_id && (arousal_diff > 0.15 || msg.emotion_proxy != acoustic_state.current_mood) {
                                    let _ = tx_out.try_send(PipelineEvent::AcousticMoodShifted {
                                        session_id: session_id.clone(),
                                        previous_mood: acoustic_state.current_mood.clone(),
                                        current_mood: msg.emotion_proxy.clone(),
                                        arousal_shift: current_arousal - acoustic_state.last_arousal,
                                        valence_shift: current_valence - acoustic_state.last_arousal,
                                        speaker_id: speaker_id.clone(),
                                        speaker_vec: msg.speaker_vec.clone() // [ARCH-COMPLIANCE FIX]
                                    });
                                }
                                acoustic_state.last_arousal = current_arousal;
                                acoustic_state.last_valence = current_valence;
                                acoustic_state.current_mood = msg.emotion_proxy.clone();
                                acoustic_state.speaker_id = speaker_id.clone();
                            }

                            if !msg.is_final {
                                if !text.is_empty() && !self.config.listen_only_mode {
                                    info!(event = "SOFTWARE_BARGE_IN_TRIGGERED", trace_id = %trace_id, "⚡ Text-based Barge-in detected.");
                                    cancel_token.cancel(); cancel_token = CancellationToken::new();
                                    let _ = tx_out.try_send(PipelineEvent::ClearBuffer);
                                }
                            } else if !text.is_empty() {
                                if self.config.listen_only_mode { continue; }

                                let ct = cancel_token.child_token();
                                let clients_clone = self.clients.clone();
                                let config_clone = self.config.clone();
                                let tx_out_clone = tx_out.clone();
                                let s_id = session_id.clone(); let u_id = user_id.clone(); let tr_id = trace_id.clone(); let sp_id = span_id.clone(); let ten_id = tenant_id.clone();

                                tokio::spawn(async move {
                                    if let Err(e) = Self::handle_dialog_tts_phase(clients_clone, config_clone, s_id, u_id, tr_id.clone(), sp_id, ten_id, text, tx_out_clone, ct).await {
                                        warn!(event = "DIALOG_TTS_ERROR", trace_id = %tr_id, error = %e, "Error during Dialog execution.");
                                    }
                                });
                            }
                        }
                        Some(Err(e)) => { error!(event = "STT_STREAM_ERROR", trace_id = %trace_id, error = %e, "STT Stream error."); return Err(e.into()); }
                        None => {}
                    }
                }
            }
        }
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
        let mut full_chat_text = String::new();

        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    info!(event = "DIALOG_STREAM_ABORTED", trace_id = %trace_id, "⚡ Barge-in: Dropping Dialog task. Clearing buffers.");
                    sentence_buffer.clear();
                    full_chat_text.clear();
                    return Ok(());
                }
                res_opt = dialog_resp_stream.next() => {
                    match res_opt {
                        Some(Ok(msg)) => {
                            match msg.payload {
                                Some(sentiric_contracts::sentiric::dialog::v1::stream_conversation_response::Payload::TextResponse(text_chunk)) => {
                                    if config.chat_only_mode {
                                        full_chat_text.push_str(&text_chunk);
                                        let _ = tx_out.try_send(PipelineEvent::Transcript(crate::TranscriptData {
                                            text: full_chat_text.clone(),
                                            is_final: false,
                                            sender: "AI".to_string(),
                                            emotion: "neutral".to_string(),
                                            gender: "neutral".to_string(),
                                            arousal: 0.0, valence: 0.0, speaker_id: "AI_1".to_string(), speaker_vec: vec![], words: vec![],
                                        }));
                                    } else {
                                        sentence_buffer.push_str(&text_chunk);

                                        // [KUSURSUZ TONLAMA DÜZELTMESİ - UX YÜKSELTMESİ]:
                                        // Virgül, ara noktalar veya kelime ortasındaki noktalar (Örn: "Dr.") tetiklememeli.
                                        // Cümle sadece ve sadece asıl bitiş işaretleriyle bitiyor SA (boşluklar trimlenerek) bölünür.
                                        let trimmed = sentence_buffer.trim_end();
                                        let ends_with_major_punct = trimmed.ends_with('.') || trimmed.ends_with('?') || trimmed.ends_with('!');
                                        let ends_with_newline = trimmed.ends_with('\n');
                                        let is_too_long = sentence_buffer.len() > 160 && sentence_buffer.ends_with(' ');

                                        if (ends_with_major_punct && sentence_buffer.ends_with(' ')) || ends_with_newline || is_too_long {
                                            let sentence = sentence_buffer.clone();
                                            sentence_buffer.clear();
                                            Self::synthesize_and_stream_tts(&mut clients, &config, sentence, &trace_id, &span_id, &tenant_id, &tx_out, &cancel_token).await?;
                                        }
                                    }
                                }
                                Some(sentiric_contracts::sentiric::dialog::v1::stream_conversation_response::Payload::IsFinalResponse(true)) => {
                                    if !config.chat_only_mode && !sentence_buffer.trim().is_empty() {
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
