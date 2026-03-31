use thiserror::Error;

#[derive(Error, Debug)]
pub enum SdkError {
    #[error("TLS Configuration Error: {0}")]
    TlsConfigError(String),

    #[error("gRPC Connection Error: {0}")]
    ConnectionError(String),

    #[error("gRPC Request Error: {0}")]
    GrpcError(#[from] tonic::Status),

    #[error("Internal SDK Error: {0}")]
    Internal(String),

    #[error("Invalid Argument: {0}")]
    InvalidArgument(String),
}
