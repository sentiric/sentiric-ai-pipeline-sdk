// File: sentiric-ai-pipeline-sdk/src/orchestrator.rs
use crate::clients::ApiClients;
use crate::config::SdkConfig;
use crate::error::SdkError;
use futures::StreamExt;
use sentiric_contracts::sentiric::dialog::v1::stream_conversation_request::Payload as DialogPayload;
use sentiric_contracts::sentiric::dialog::v1::{ConversationConfig, StreamConversationRequest};
use sentiric_contracts::sentiric::stt::v1::TranscribeStreamRequest;
use sentiric_contracts::sentiric::tts::v1::{AudioConfig, SynthesizeStreamRequest};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

pub struct PipelineOrchestrator {
    config: SdkConfig,
    clients: ApiClients,
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
        tx_audio: mpsc::Sender<Vec<u8>>,
        mut interrupt_rx: mpsc::Receiver<()>, // <--- [YENİ]: Donanımsal VAD Sinyali Kanalı
    ) -> Result<(), SdkError> {
        info!(
            event = "AI_PIPELINE_START",
            trace_id = %trace_id, span_id = %span_id, tenant_id = %tenant_id,
            "🚀 AI Pipeline started."
        );

        // nano modunda çalışırken edge_mode aktif ise, bazı ağır işlemleri atlayarak düşük gecikmeli bir deneyim sağlamak için konfigürasyonu logluyoruz.
        if self.config.edge_mode {
            info!(
                event = "EDGE_MODE_ACTIVE",
                trace_id = %trace_id,
                span_id = %span_id,
                tenant_id = %tenant_id,
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

        // [ARCH-COMPLIANCE]: Asenkron Multiplexing. Hem STT'den gelen metni hem İstemciden gelen donanım kesmesini (VAD) dinler.
        loop {
            tokio::select! {
                // 1. İSTEMCİDEN GELEN DONANIMSAL SÖZ KESME (ZERO-LATENCY BARGE-IN)
                Some(()) = interrupt_rx.recv() => {
                    info!(
                        event = "HARDWARE_BARGE_IN_TRIGGERED",
                        trace_id = %trace_id, span_id = %span_id, tenant_id = %tenant_id,
                        "⚡ Hardware VAD signal received from client. Cancelling Dialog/TTS tasks instantly."
                    );
                    cancel_token.cancel();
                    cancel_token = CancellationToken::new();
                    let _ = tx_audio.try_send(vec![]); // Flush network buffer
                }

                // 2. STT'DEN GELEN TRANSKRİPSİYON AKIŞI
                res_opt = stt_response_stream.next() => {
                    match res_opt {
                        Some(Ok(msg)) => {
                            let text = msg.partial_transcription.trim().to_string();

                            // Yazılımsal Söz Kesme (Fallback)
                            if !msg.is_final {
                                if !text.is_empty() {
                                    info!(event = "SOFTWARE_BARGE_IN_TRIGGERED", trace_id = %trace_id, "⚡ Text-based Barge-in detected.");
                                    cancel_token.cancel();
                                    cancel_token = CancellationToken::new();
                                    let _ = tx_audio.try_send(vec![]);
                                }
                            } else if !text.is_empty() {
                                info!(event = "STT_FINAL_RECEIVED", trace_id = %trace_id, text = %text, "Final transcription received.");

                                let ct = cancel_token.child_token();
                                let clients_clone = self.clients.clone();
                                let config_clone = self.config.clone();
                                let tx_audio_clone = tx_audio.clone();
                                let s_id = session_id.clone();
                                let u_id = user_id.clone();
                                let tr_id = trace_id.clone();
                                let sp_id = span_id.clone();
                                let ten_id = tenant_id.clone();

                                tokio::spawn(async move {
                                    if let Err(e) = Self::handle_dialog_tts_phase(
                                        clients_clone, config_clone, s_id, u_id, tr_id.clone(), sp_id, ten_id, text, tx_audio_clone, ct
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

    // handle_dialog_tts_phase metodu aynı kalıyor... (Önceki snapshot'taki gibi)
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
        tx_audio: mpsc::Sender<Vec<u8>>,
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
                            if let Some(sentiric_contracts::sentiric::dialog::v1::stream_conversation_response::Payload::TextResponse(text_chunk)) = msg.payload {
                                sentence_buffer.push_str(&text_chunk);

                                if sentence_buffer.contains('.') || sentence_buffer.contains('?') || sentence_buffer.contains('!') || sentence_buffer.contains('\n') {
                                    let sentence = sentence_buffer.clone();
                                    sentence_buffer.clear();

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

                                    let req = clients.inject_metadata(tonic::Request::new(tts_req), &trace_id, &span_id, &tenant_id);

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
                                                        if !chunk.is_empty() && tx_audio.send(chunk).await.is_err() {
                                                            return Err(SdkError::Internal("Audio output sender dropped".into()));
                                                        }
                                                    }
                                                    Some(Err(e)) => { warn!(event="TTS_STREAM_ERROR", error=%e, "TTS error"); break; }
                                                    None => break,
                                                }
                                            }
                                        }
                                    }
                                }
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
}
