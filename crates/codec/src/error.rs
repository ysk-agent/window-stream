//! Error type for the codec crate.

use thiserror::Error;

/// Errors returned by the VP9 encoder / decoder and the IVF helpers.
#[derive(Debug, Error)]
pub enum CodecError {
    /// A dimension or configuration value is not usable (zero, odd where the
    /// 4:2:0 layout forbids it, out of range, ...).
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    /// A frame buffer does not match the declared width / height / strides.
    #[error("invalid frame: {0}")]
    InvalidFrame(String),

    /// libvpx returned a non-OK result code.
    #[error("libvpx error ({code}): {message}")]
    Vpx {
        /// Numeric `vpx_codec_err_t` value.
        code: i32,
        /// Text from `vpx_codec_error` / `vpx_codec_err_to_string`.
        message: String,
    },

    /// The bitstream handed to the decoder is not decodable VP9.
    #[error("decode error: {0}")]
    Decode(String),

    /// An IVF file / stream is malformed.
    #[error("invalid IVF: {0}")]
    InvalidIvf(String),

    /// Underlying I/O failure (IVF writer / reader).
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, CodecError>;
