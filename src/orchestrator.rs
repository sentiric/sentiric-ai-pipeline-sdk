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
    ) -> Result<(), SdkError> {
        info!(
            event = "AI_PIPELINE_START",
            trace_id = %trace_id,
            span_id = %span_id,
            tenant_id = %tenant_id,
            "🚀 AI Pipeline started."
        );

        // 1. Gelen sesi STT'ye yönlendirecek stream hazırlığı
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

        // STT Akışını Başlat
        let mut stt_client = self.clients.stt.clone();
        let mut stt_response_stream = match stt_client.transcribe_stream(stt_request).await {
            Ok(res) => res.into_inner(),
            Err(e) => {
                error!(
                    event = "STT_CONNECT_FAIL",
                    trace_id = %trace_id,
                    span_id = %span_id,
                    tenant_id = %tenant_id,
                    error = %e,
                    "Failed to connect to STT Gateway."
                );
                return Err(e.into());
            }
        };

        let mut cancel_token = CancellationToken::new();

        // 2. STT Yanıtlarını Dinle (The Loop)
        while let Some(res) = stt_response_stream.next().await {
            match res {
                Ok(msg) => {
                    let text = msg.partial_transcription.trim().to_string();

                    // [BARGE-IN MANTIĞI] Eğer geçici metin varsa ve boş değilse, konuşma başlamış demektir.
                    if !msg.is_final {
                        if !text.is_empty() {
                            info!(
                                event = "BARGE_IN_TRIGGERED",
                                trace_id = %trace_id,
                                span_id = %span_id,
                                tenant_id = %tenant_id,
                                "⚡ Barge-in detected. Cancelling active Dialog/TTS tasks."
                            );

                            // Mevcut tüm işlem parçacıklarını anında iptal et (Zero-Latency)
                            cancel_token.cancel();
                            cancel_token = CancellationToken::new();

                            // Eski sesin ağda kalmasını önlemek için flush sinyali olarak 0 byte gönder
                            let _ = tx_audio.send(vec![]).await;
                        }
                    } else if !text.is_empty() {
                        info!(
                            event = "STT_FINAL_RECEIVED",
                            trace_id = %trace_id,
                            span_id = %span_id,
                            tenant_id = %tenant_id,
                            text = %text,
                            "Final transcription received. Initiating Dialog & TTS phase."
                        );

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
                                clients_clone,
                                config_clone,
                                s_id,
                                u_id,
                                tr_id.clone(),
                                sp_id.clone(),
                                ten_id.clone(),
                                text,
                                tx_audio_clone,
                                ct,
                            )
                            .await
                            {
                                warn!(
                                    event = "DIALOG_TTS_PHASE_ERROR",
                                    trace_id = %tr_id,
                                    span_id = %sp_id,
                                    tenant_id = %ten_id,
                                    error = %e,
                                    "Error during Dialog->TTS execution."
                                );
                            }
                        });
                    }
                }
                Err(e) => {
                    error!(
                        event = "STT_STREAM_ERROR",
                        trace_id = %trace_id,
                        span_id = %span_id,
                        tenant_id = %tenant_id,
                        error = %e,
                        "STT Stream encountered an error."
                    );
                    return Err(e.into());
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
        tx_audio: mpsc::Sender<Vec<u8>>,
        cancel_token: CancellationToken,
    ) -> Result<(), SdkError> {
        let (dialog_req_tx, dialog_req_rx) = mpsc::channel(10);

        // 1. Config Payload
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

        // 2. Metin Payload
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

        let mut dialog_resp_stream = clients.dialog.stream_conversation(req).await?.into_inner();

        let mut sentence_buffer = String::new();

        while let Some(res) = dialog_resp_stream.next().await {
            // Barge-in check at the dialog stream level
            if cancel_token.is_cancelled() {
                return Ok(());
            }

            let msg = res?;
            if let Some(sentiric_contracts::sentiric::dialog::v1::stream_conversation_response::Payload::TextResponse(text_chunk)) = msg.payload {
                sentence_buffer.push_str(&text_chunk);

                // Noktalama işaretinde cümleyi kes ve TTS'e yolla
                if sentence_buffer.contains('.')
                    || sentence_buffer.contains('?')
                    || sentence_buffer.contains('!')
                    || sentence_buffer.contains('\n')
                {
                    let sentence = sentence_buffer.clone();
                    sentence_buffer.clear();

                    let tts_req = SynthesizeStreamRequest {
                        text: sentence,
                        text_type: 1, // TEXT_TYPE_TEXT
                        voice_id: config.tts_voice_id.clone(),
                        audio_config: Some(AudioConfig {
                            audio_format: 1, // PCM_S16LE
                            sample_rate_hertz: config.tts_sample_rate as i32,
                            volume_gain_db: 0.0,
                        }),
                        preferred_provider: "".to_string(),
                        tuning: None,
                        cloning_audio_data: None,
                    };

                    let req = clients.inject_metadata(
                        tonic::Request::new(tts_req),
                        &trace_id,
                        &span_id,
                        &tenant_id,
                    );

                    let mut tts_stream = clients.tts.synthesize_stream(req).await?.into_inner();

                    // Sesi dışarıya bas, Barge-in gelirse anında kop.
                    while let Some(audio_res) = tts_stream.next().await {
                        tokio::select! {
                            _ = cancel_token.cancelled() => {
                                info!(
                                    event = "TTS_STREAM_ABORTED",
                                    trace_id = %trace_id,
                                    span_id = %span_id,
                                    tenant_id = %tenant_id,
                                    "Barge-in: Dropping ongoing TTS audio stream."
                                );
                                return Ok(());
                            }
                            audio_msg = std::future::ready(audio_res) => {
                                let chunk = audio_msg?.audio_content;
                                // Clippy FIX: Collapsed IF statement
                                if !chunk.is_empty() && tx_audio.send(chunk).await.is_err() {
                                    return Err(SdkError::Internal("Audio output sender dropped".into()));
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
