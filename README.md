# 🧠 Sentiric AI Pipeline SDK

**Sentiric İletişim İşletim Sistemi** için Full-Duplex AI Senkronizasyon Motoru. 

Bu kütüphane, STT, Dialog ve TTS servisleri arasındaki akışları (Streaming) kilitlenmesiz (Lock-Free) olarak orkestre eder. VAD tamponlaması ve Sıfır-Gecikmeli Söz Kesme (Zero-Latency Barge-in) mekanizmasını işletir.

## 🏛️ Mimari Kurallar (Constitutional Constraints)
1. **SUTS v4.0 Uyumlu:** SDK, kendi `tracing_subscriber`'ını başlatmaz. Gömüldüğü servisin (ör: `telephony-action-service`) konfigürasyonunu kullanır. Tüm olaylar `x-trace-id`, `x-span-id` ve `x-tenant-id` ile etiketlenir.
2. **mTLS Zorunluluğu:** Uzman motorlara (STT, Dialog, TTS) yapılan gRPC bağlantılarında HTTP fallback YASAKTIR. Tüm iletişim Zero-Trust mTLS ile şifrelenmelidir.
3. **No Panic Policy:** Kütüphane hiçbir koşulda çökmeyecek (panic) şekilde tasarlanmıştır.

## 🚀 Kullanım (Usage)
```rust
use sentiric_ai_pipeline_sdk::config::SdkConfig;
use sentiric_ai_pipeline_sdk::orchestrator::PipelineOrchestrator;
use tokio::sync::mpsc;

let config = SdkConfig { /* ... */ };
let orchestrator = PipelineOrchestrator::new(config).await?;

orchestrator.run_pipeline(
    session_id, user_id, trace_id, span_id, tenant_id, rx_audio, tx_audio
).await?;
```

---
