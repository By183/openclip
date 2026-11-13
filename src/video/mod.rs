//! Video pipeline: raw frame types, pixel conversion, scaling, encoders, preview.

pub mod convert;
#[cfg(windows)]
pub mod d3d11_vp;
pub mod encoder;
#[cfg(windows)]
pub mod mf;
pub mod mouse_fx;
pub mod openh264;
pub mod preview;
pub mod scale;
pub mod thumbnail;
pub mod watermark;

pub use convert::{Converter, PixelFormat, RawFrame};
pub use encoder::{
    available_encoders, create_video_encoder, refresh_encoders, CodecParams, EncodeStageTiming, EncodedFrame,
    EncoderInfo, EncoderRequest, FrameInput, InputLayout, Vendor, VideoEncoder,
};
#[cfg(windows)]
pub use encoder::create_video_encoder_with_d3d;
pub use openh264::H264Encoder;
pub use scale::Scaler;
pub use thumbnail::MediaInfo;
pub use watermark::{Watermark, WatermarkRenderer};
