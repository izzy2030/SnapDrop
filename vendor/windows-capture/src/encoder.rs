use std::path::Path;

use windows::Foundation::TimeSpan;
use windows::Graphics::DirectX::Direct3D11::IDirect3DSurface;
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapEncoder, BitmapPixelFormat};
use windows::Media::MediaProperties::{
    AudioEncodingProperties, ContainerEncodingProperties, VideoEncodingProperties,
};
use windows::Storage::Streams::{DataReader, IRandomAccessStream, InMemoryRandomAccessStream};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    ID3D11Device, ID3D11RenderTargetView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGISurface;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11SurfaceFromDXGISurface, IDirect3DDxgiInterfaceAccess,
};
use windows::core::{HSTRING, Interface};

use crate::d3d11::SendDirectX;
use crate::frame::Frame;
use crate::settings::ColorFormat;

#[derive(thiserror::Error, Debug)]
/// Errors that can occur when encoding raw buffers to images via [`ImageEncoder`].
pub enum ImageEncoderError {
    /// The provided source pixel format is not supported for image encoding.
    ///
    /// This occurs for formats such as [`crate::settings::ColorFormat::Rgba16F`].
    #[error("This color format is not supported for saving as an image")]
    UnsupportedFormat,
    /// An I/O error occurred while writing the image to disk.
    ///
    /// Wraps [`std::io::Error`].
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),
    /// An integer conversion failed during buffer sizing or Windows API calls.
    ///
    /// Wraps [`std::num::TryFromIntError`].
    #[error("Integer conversion error: {0}")]
    IntConversionError(#[from] std::num::TryFromIntError),
    /// A Windows Runtime/Win32 API call failed.
    ///
    /// Wraps [`windows::core::Error`].
    #[error("Windows API error: {0}")]
    WindowsError(#[from] windows::core::Error),
}

#[derive(Eq, PartialEq, Clone, Copy, Debug)]
/// Supported output image formats for [`crate::encoder::ImageEncoder`].
pub enum ImageFormat {
    /// JPEG (lossy).
    Jpeg,
    /// PNG (lossless).
    Png,
    /// GIF (palette-based).
    Gif,
    /// TIFF (Tagged Image File Format).
    Tiff,
    /// BMP (Bitmap).
    Bmp,
    /// JPEG XR (HD Photo).
    JpegXr,
}

/// Pixel formats supported by the Windows API for image encoding.
#[derive(Eq, PartialEq, Clone, Copy, Debug)]
pub enum ImageEncoderPixelFormat {
    /// 16-bit floating-point RGBA format.
    Rgb16F,
    /// 8-bit unsigned integer BGRA format.
    Bgra8,
    /// 8-bit unsigned integer RGBA format.
    Rgba8,
}

/// Encodes raw image buffers into encoded bytes for common formats.
///
/// Supports saving as PNG, JPEG, GIF, TIFF, BMP, and JPEG XR when the input
/// color format is compatible.
///
/// # Example
/// ```no_run
/// use windows_capture::encoder::{ImageEncoder, ImageEncoderPixelFormat, ImageFormat};
///
/// let width = 320u32;
/// let height = 240u32;
/// // BGRA8 buffer (e.g., from a frame)
/// let bgra = vec![0u8; (width * height * 4) as usize];
///
/// let png_bytes = ImageEncoder::new(ImageFormat::Png, ImageEncoderPixelFormat::Bgra8)
///     .unwrap()
///     .encode(&bgra, width, height)
///     .unwrap();
///
/// std::fs::write("example.png", png_bytes).unwrap();
/// ```
pub struct ImageEncoder {
    encoder: windows::core::GUID,
    pixel_format: BitmapPixelFormat,
}

impl ImageEncoder {
    /// Constructs a new [`ImageEncoder`].
    #[inline]
    pub fn new(format: ImageFormat, pixel_format: ImageEncoderPixelFormat) -> Result<Self, ImageEncoderError> {
        let encoder = match format {
            ImageFormat::Jpeg => BitmapEncoder::JpegEncoderId()?,
            ImageFormat::Png => BitmapEncoder::PngEncoderId()?,
            ImageFormat::Gif => BitmapEncoder::GifEncoderId()?,
            ImageFormat::Tiff => BitmapEncoder::TiffEncoderId()?,
            ImageFormat::Bmp => BitmapEncoder::BmpEncoderId()?,
            ImageFormat::JpegXr => BitmapEncoder::JpegXREncoderId()?,
        };

        let pixel_format = match pixel_format {
            ImageEncoderPixelFormat::Bgra8 => BitmapPixelFormat::Bgra8,
            ImageEncoderPixelFormat::Rgba8 => BitmapPixelFormat::Rgba8,
            ImageEncoderPixelFormat::Rgb16F => BitmapPixelFormat::Rgba16,
        };

        Ok(Self { pixel_format, encoder })
    }

    /// Encodes the provided pixel buffer into the configured output [`ImageFormat`].
    ///
    /// The input buffer must match the specified source [`crate::settings::ColorFormat`]
    /// and dimensions. For packed 8-bit formats (e.g., [`crate::settings::ColorFormat::Bgra8`]),
    /// the buffer length should be `width * height * 4`.
    ///
    /// # Errors
    ///
    /// - [`ImageEncoderError::UnsupportedFormat`] when the source format is unsupported for images
    ///   (e.g., [`crate::settings::ColorFormat::Rgba16F`])
    /// - [`ImageEncoderError::WindowsError`] when Windows Imaging API calls fail
    /// - [`ImageEncoderError::IntConversionError`] on integer conversion failures
    #[inline]
    pub fn encode(&self, image_buffer: &[u8], width: u32, height: u32) -> Result<Vec<u8>, ImageEncoderError> {
        let stream = InMemoryRandomAccessStream::new()?;

        let encoder = BitmapEncoder::CreateAsync(self.encoder, &stream)?.join()?;

        encoder.SetPixelData(
            self.pixel_format,
            BitmapAlphaMode::Premultiplied,
            width,
            height,
            1.0,
            1.0,
            image_buffer,
        )?;
        encoder.FlushAsync()?.join()?;

        let size = stream.Size()?;
        let input = stream.GetInputStreamAt(0)?;
        let reader = DataReader::CreateDataReader(&input)?;
        reader.LoadAsync(size as u32)?.join()?;

        let mut bytes = vec![0u8; size as usize];
        reader.ReadBytes(&mut bytes)?;

        Ok(bytes)
    }
}

#[derive(thiserror::Error, Debug)]
/// Errors emitted by [`VideoEncoder`] during configuration, streaming, or finalization.
pub enum VideoEncoderError {
    /// A Windows Runtime/Win32 API call failed.
    ///
    /// Wraps [`windows::core::Error`].
    #[error("Windows API error: {0}")]
    WindowsError(#[from] windows::core::Error),
    /// Video encoding was disabled via [`VideoSettingsBuilder::disabled`].
    #[error("Video encoding is disabled")]
    VideoDisabled,
    /// Audio encoding was disabled via [`AudioSettingsBuilder::disabled`].
    #[error("Audio encoding is disabled")]
    AudioDisabled,
    /// An I/O error occurred during file creation or writing.
    ///
    /// Wraps [`std::io::Error`].
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),
    /// The provided frame color format is unsupported by the encoder path.
    ///
    /// See [`crate::settings::ColorFormat`].
    #[error("Unsupported frame color format: {0:?}")]
    UnsupportedFrameFormat(ColorFormat),
}

unsafe impl Send for VideoEncoderError {}
unsafe impl Sync for VideoEncoderError {}

/// Video sources used by [`VideoEncoder`].
///
/// - For [`VideoEncoderSource::DirectX`], the COM surface pointer is ref-counted; holding the
///   pointer is sufficient.
/// - For [`VideoEncoderSource::Buffer`], the encoder takes ownership of the bytes, allowing callers
///   to return immediately.
pub enum VideoEncoderSource {
    /// A Direct3D surface sample.
    DirectX(SendDirectX<IDirect3DSurface>),
    /// A raw BGRA sample buffer.
    Buffer(Vec<u8>),
}

/// Audio sources used by [`VideoEncoder`]. The encoder takes ownership of the bytes.
pub enum AudioEncoderSource {
    /// Interleaved PCM bytes.
    Buffer(Vec<u8>),
}

struct CachedSurface {
    width: u32,
    height: u32,
    format: ColorFormat,
    texture: SendDirectX<ID3D11Texture2D>,
    surface: SendDirectX<IDirect3DSurface>,
    render_target_view: Option<SendDirectX<ID3D11RenderTargetView>>,
}

/// Builder for configuring video encoder settings.
pub struct VideoSettingsBuilder {
    sub_type: VideoSettingsSubType,
    bitrate: u32,
    width: u32,
    height: u32,
    frame_rate: u32,
    pixel_aspect_ratio: (u32, u32),
    disabled: bool,
}

impl VideoSettingsBuilder {
    /// Constructs a new [`VideoSettingsBuilder`] with required geometry.
    ///
    /// Defaults:
    /// - Subtype: [`VideoSettingsSubType::HEVC`]
    /// - Bitrate: 15 Mbps
    /// - Frame rate: 60 fps
    /// - Pixel aspect ratio: 1:1
    /// - Disabled: false
    pub const fn new(width: u32, height: u32) -> Self {
        Self {
            bitrate: 15_000_000,
            frame_rate: 60,
            pixel_aspect_ratio: (1, 1),
            sub_type: VideoSettingsSubType::HEVC,
            width,
            height,
            disabled: false,
        }
    }

    /// Sets the video codec/subtype (e.g., [`VideoSettingsSubType::HEVC`]).
    pub const fn sub_type(mut self, sub_type: VideoSettingsSubType) -> Self {
        self.sub_type = sub_type;
        self
    }

    /// Sets target bitrate in bits per second.
    pub const fn bitrate(mut self, bitrate: u32) -> Self {
        self.bitrate = bitrate;
        self
    }

    /// Sets target frame width in pixels.
    pub const fn width(mut self, width: u32) -> Self {
        self.width = width;
        self
    }

    /// Sets target frame height in pixels.
    pub const fn height(mut self, height: u32) -> Self {
        self.height = height;
        self
    }

    /// Sets target frame rate (numerator; denominator is fixed to 1).
    pub const fn frame_rate(mut self, frame_rate: u32) -> Self {
        self.frame_rate = frame_rate;
        self
    }

    /// Sets pixel aspect ratio as (numerator, denominator).
    pub const fn pixel_aspect_ratio(mut self, par: (u32, u32)) -> Self {
        self.pixel_aspect_ratio = par;
        self
    }
    /// Disables or enables video encoding.
    ///
    /// When `true`, calls to send frames still succeed but produce no video samples.
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    fn build(self) -> Result<(VideoEncodingProperties, bool), VideoEncoderError> {
        let properties = VideoEncodingProperties::new()?;
        properties.SetSubtype(&self.sub_type.to_hstring())?;
        properties.SetBitrate(self.bitrate)?;
        properties.SetWidth(self.width)?;
        properties.SetHeight(self.height)?;
        properties.FrameRate()?.SetNumerator(self.frame_rate)?;
        properties.FrameRate()?.SetDenominator(1)?;
        properties.PixelAspectRatio()?.SetNumerator(self.pixel_aspect_ratio.0)?;
        properties.PixelAspectRatio()?.SetDenominator(self.pixel_aspect_ratio.1)?;
        Ok((properties, self.disabled))
    }
}

/// Builder for configuring audio encoder settings.
pub struct AudioSettingsBuilder {
    bitrate: u32,
    channel_count: u32,
    sample_rate: u32,
    bit_per_sample: u32,
    sub_type: AudioSettingsSubType,
    disabled: bool,
}

impl AudioSettingsBuilder {
    /// Constructs a new [`AudioSettingsBuilder`] with common defaults.
    ///
    /// Defaults:
    /// - Bitrate: 192 kbps
    /// - Channels: 2
    /// - Sample rate: 48 kHz
    /// - Bits per sample: 16
    /// - Subtype: [`AudioSettingsSubType::AAC`]
    /// - Disabled: false
    pub const fn new() -> Self {
        Self {
            bitrate: 192_000,
            channel_count: 2,
            sample_rate: 48_000,
            bit_per_sample: 16,
            sub_type: AudioSettingsSubType::AAC,
            disabled: false,
        }
    }
    /// Sets audio bitrate in bits per second.
    pub const fn bitrate(mut self, bitrate: u32) -> Self {
        self.bitrate = bitrate;
        self
    }
    /// Sets number of interleaved channels.
    pub const fn channel_count(mut self, channel_count: u32) -> Self {
        self.channel_count = channel_count;
        self
    }
    /// Sets sample rate in Hz.
    pub const fn sample_rate(mut self, sample_rate: u32) -> Self {
        self.sample_rate = sample_rate;
        self
    }
    /// Sets bits per sample.
    pub const fn bit_per_sample(mut self, bit_per_sample: u32) -> Self {
        self.bit_per_sample = bit_per_sample;
        self
    }
    /// Sets audio codec/subtype (e.g., [`AudioSettingsSubType::AAC`]).
    pub const fn sub_type(mut self, sub_type: AudioSettingsSubType) -> Self {
        self.sub_type = sub_type;
        self
    }
    /// Disables or enables audio encoding.
    pub const fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    fn build(self) -> Result<(AudioEncodingProperties, bool), VideoEncoderError> {
        let properties = AudioEncodingProperties::new()?;
        properties.SetBitrate(self.bitrate)?;
        properties.SetChannelCount(self.channel_count)?;
        properties.SetSampleRate(self.sample_rate)?;
        properties.SetBitsPerSample(self.bit_per_sample)?;
        properties.SetSubtype(&self.sub_type.to_hstring())?;
        Ok((properties, self.disabled))
    }
}

impl Default for AudioSettingsBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Builder for configuring container settings.
pub struct ContainerSettingsBuilder {
    sub_type: ContainerSettingsSubType,
}
impl ContainerSettingsBuilder {
    /// Constructs a new [`ContainerSettingsBuilder`].
    ///
    /// Default subtype: [`ContainerSettingsSubType::MPEG4`].
    pub const fn new() -> Self {
        Self { sub_type: ContainerSettingsSubType::MPEG4 }
    }
    /// Sets the container subtype (e.g., [`ContainerSettingsSubType::MPEG4`]).
    pub const fn sub_type(mut self, sub_type: ContainerSettingsSubType) -> Self {
        self.sub_type = sub_type;
        self
    }
    fn build(self) -> Result<ContainerEncodingProperties, VideoEncoderError> {
        let properties = ContainerEncodingProperties::new()?;
        properties.SetSubtype(&self.sub_type.to_hstring())?;
        Ok(properties)
    }
}
impl Default for ContainerSettingsBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Video encoder subtypes.
#[derive(Eq, PartialEq, Clone, Copy, Debug)]
pub enum VideoSettingsSubType {
    /// Uncompressed 32-bit ARGB (8:8:8:8).
    ARGB32,
    /// Uncompressed 32-bit BGRA (8:8:8:8).
    BGRA8,
    /// 16-bit depth format.
    D16,
    /// H.263 video.
    H263,
    /// H.264/AVC video.
    H264,
    /// H.264 elementary stream.
    H264ES,
    /// H.265/HEVC video.
    HEVC,
    /// H.265/HEVC elementary stream.
    HEVCES,
    /// Planar YUV 4:2:0 (IYUV).
    IYUV,
    /// 8-bit luminance (grayscale).
    L8,
    /// 16-bit luminance (grayscale).
    L16,
    /// Motion JPEG.
    MJPG,
    /// NV12 YUV 4:2:0 (semi-planar).
    NV12,
    /// MPEG-1 video.
    MPEG1,
    /// MPEG-2 video.
    MPEG2,
    /// 24-bit RGB.
    RGB24,
    /// 32-bit RGB.
    RGB32,
    /// Windows Media Video 9 (WMV3).
    WMV3,
    /// Windows Media Video Advanced Profile (VC-1).
    WVC1,
    /// VP9 video.
    VP9,
    /// Packed YUY2 4:2:2.
    YUY2,
    /// Planar YV12 4:2:0.
    YV12,
}
impl VideoSettingsSubType {
    /// Returns the Windows Media subtype identifier string for this [`VideoSettingsSubType`].
    pub fn to_hstring(&self) -> HSTRING {
        let s = match self {
            Self::ARGB32 => "ARGB32",
            Self::BGRA8 => "BGRA8",
            Self::D16 => "D16",
            Self::H263 => "H263",
            Self::H264 => "H264",
            Self::H264ES => "H264ES",
            Self::HEVC => "HEVC",
            Self::HEVCES => "HEVCES",
            Self::IYUV => "IYUV",
            Self::L8 => "L8",
            Self::L16 => "L16",
            Self::MJPG => "MJPG",
            Self::NV12 => "NV12",
            Self::MPEG1 => "MPEG1",
            Self::MPEG2 => "MPEG2",
            Self::RGB24 => "RGB24",
            Self::RGB32 => "RGB32",
            Self::WMV3 => "WMV3",
            Self::WVC1 => "WVC1",
            Self::VP9 => "VP9",
            Self::YUY2 => "YUY2",
            Self::YV12 => "YV12",
        };
        HSTRING::from(s)
    }
}

/// Audio encoder subtypes.
#[derive(Eq, PartialEq, Clone, Copy, Debug)]
pub enum AudioSettingsSubType {
    /// Advanced Audio Coding (AAC).
    AAC,
    /// Dolby Digital (AC-3).
    AC3,
    /// AAC framed with ADTS headers.
    AACADTS,
    /// AAC with HDCP protection.
    AACHDCP,
    /// AC-3 over S/PDIF.
    AC3SPDIF,
    /// AC-3 with HDCP protection.
    AC3HDCP,
    /// ADTS (Audio Data Transport Stream).
    ADTS,
    /// Apple Lossless Audio Codec (ALAC).
    ALAC,
    /// Adaptive Multi-Rate Narrowband (AMR-NB).
    AMRNB,
    /// Adaptive Multi-Rate Wideband (AMR-WB).
    AWRWB,
    /// DTS audio.
    DTS,
    /// Enhanced AC-3 (E-AC-3).
    EAC3,
    /// Free Lossless Audio Codec (FLAC).
    FLAC,
    /// 32-bit floating-point PCM.
    Float,
    /// MPEG-1/2 Layer III (MP3).
    MP3,
    /// Generic MPEG audio.
    MPEG,
    /// Opus audio.
    OPUS,
    /// Pulse-code modulation (PCM).
    PCM,
    /// Windows Media Audio 8.
    WMA8,
    /// Windows Media Audio 9.
    WMA9,
    /// Vorbis audio.
    Vorbis,
}
impl AudioSettingsSubType {
    /// Returns the Windows Media subtype identifier string for this [`AudioSettingsSubType`].
    pub fn to_hstring(&self) -> HSTRING {
        let s = match self {
            Self::AAC => "AAC",
            Self::AC3 => "AC3",
            Self::AACADTS => "AACADTS",
            Self::AACHDCP => "AACHDCP",
            Self::AC3SPDIF => "AC3SPDIF",
            Self::AC3HDCP => "AC3HDCP",
            Self::ADTS => "ADTS",
            Self::ALAC => "ALAC",
            Self::AMRNB => "AMRNB",
            Self::AWRWB => "AWRWB",
            Self::DTS => "DTS",
            Self::EAC3 => "EAC3",
            Self::FLAC => "FLAC",
            Self::Float => "Float",
            Self::MP3 => "MP3",
            Self::MPEG => "MPEG",
            Self::OPUS => "OPUS",
            Self::PCM => "PCM",
            Self::WMA8 => "WMA8",
            Self::WMA9 => "WMA9",
            Self::Vorbis => "Vorbis",
        };
        HSTRING::from(s)
    }
}

/// Container subtypes.
#[derive(Eq, PartialEq, Clone, Copy, Debug)]
pub enum ContainerSettingsSubType {
    /// Advanced Systems Format (ASF).
    ASF,
    /// Raw MP3 container.
    MP3,
    /// MPEG-4 container (e.g., MP4).
    MPEG4,
    /// Audio Video Interleave (AVI).
    AVI,
    /// MPEG-2 container.
    MPEG2,
    /// WAVE (WAV) container.
    WAVE,
    /// AAC ADTS stream.
    AACADTS,
    /// ADTS container.
    ADTS,
    /// 3GP container.
    GP3,
    /// AMR container.
    AMR,
    /// FLAC container.
    FLAC,
}
impl ContainerSettingsSubType {
    /// Returns the Windows Media container subtype identifier string for this
    /// [`ContainerSettingsSubType`].
    pub fn to_hstring(&self) -> HSTRING {
        match self {
            Self::ASF => HSTRING::from("ASF"),
            Self::MP3 => HSTRING::from("MP3"),
            Self::MPEG4 => HSTRING::from("MPEG4"),
            Self::AVI => HSTRING::from("AVI"),
            Self::MPEG2 => HSTRING::from("MPEG2"),
            Self::WAVE => HSTRING::from("WAVE"),
            Self::AACADTS => HSTRING::from("AACADTS"),
            Self::ADTS => HSTRING::from("ADTS"),
            Self::GP3 => HSTRING::from("3GP"),
            Self::AMR => HSTRING::from("AMR"),
            Self::FLAC => HSTRING::from("FLAC"),
        }
    }
}

/// Encodes video frames (and optional audio) and writes them to a file or stream.
///
/// Uses `IMFSinkWriter` for VFR (Variable Frame Rate) output: each frame is
/// written at its exact timestamp without CFR padding or duplication.
pub struct VideoEncoder {
    first_timestamp: Option<TimeSpan>,
    sink_writer: IMFSinkWriter,
    video_stream_index: u32,
    audio_stream_index: u32,
    is_video_disabled: bool,
    is_audio_disabled: bool,
    audio_sample_rate: u32,
    audio_block_align: u32,
    audio_samples_sent: u64,
    target_width: u32,
    target_height: u32,
    target_color_format: ColorFormat,
    cached_surface: Option<CachedSurface>,
    dropped_frames: u64,
}

impl VideoEncoder {
    fn create_cached_surface(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        format: ColorFormat,
    ) -> Result<CachedSurface, VideoEncoderError> {
        let texture_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT(format as i32),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };

        let mut texture = None;
        unsafe {
            device.CreateTexture2D(&texture_desc, None, Some(&mut texture))?;
        }
        let texture = texture.expect("CreateTexture2D returned None");

        let mut render_target = None;
        unsafe {
            device.CreateRenderTargetView(&texture, None, Some(&mut render_target))?;
        }
        let render_target_view = render_target.map(SendDirectX::new);

        let dxgi_surface: IDXGISurface = texture.cast()?;
        let inspectable = unsafe { CreateDirect3D11SurfaceFromDXGISurface(&dxgi_surface)? };
        let surface: IDirect3DSurface = inspectable.cast()?;

        Ok(CachedSurface {
            width,
            height,
            format,
            texture: SendDirectX::new(texture),
            surface: SendDirectX::new(surface),
            render_target_view,
        })
    }

    /// Constructs a new `VideoEncoder` that writes to a file path using
    /// `IMFSinkWriter` for VFR output (no CFR padding).
    #[inline]
    pub fn new<P: AsRef<Path>>(
        video_settings: VideoSettingsBuilder,
        audio_settings: AudioSettingsBuilder,
        _container_settings: ContainerSettingsBuilder,
        path: P,
    ) -> Result<Self, VideoEncoderError> {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL)? };

        let (video_cfg, is_video_disabled) = video_settings.build()?;
        let (audio_cfg, is_audio_disabled) = audio_settings.build()?;

        let target_width = video_cfg.Width()?;
        let target_height = video_cfg.Height()?;
        let target_fps = video_cfg.FrameRate()?.Numerator()?;
        let video_bitrate = video_cfg.Bitrate()?;

        let audio_sr = audio_cfg.SampleRate()?;
        let audio_ch = audio_cfg.ChannelCount()?;
        let audio_bps = audio_cfg.BitsPerSample()?;
        let audio_block_align = (audio_bps / 8) * audio_ch;

        let path = path.as_ref();
        let path_hstring = HSTRING::from(path.as_os_str());

        let attributes = unsafe {
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 1)?;
            let attrs = attrs.unwrap();
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            attrs
        };

        let sink_writer: IMFSinkWriter = unsafe {
            MFCreateSinkWriterFromURL(&path_hstring, None, Some(&attributes))?
        };

        // ---- Video output type (H.264) ----
        let video_out = unsafe { MFCreateMediaType()? };
        unsafe {
            video_out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            video_out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            video_out.SetUINT32(&MF_MT_AVG_BITRATE, video_bitrate)?;
            video_out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            video_out.SetUINT64(&MF_MT_FRAME_SIZE, ((target_width as u64) << 32) | target_height as u64)?;
            video_out.SetUINT64(&MF_MT_FRAME_RATE, ((target_fps as u64) << 32) | 1)?;
            video_out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            video_out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
        }

        let video_stream_index = unsafe { sink_writer.AddStream(&video_out)? };

        // ---- Video input type (uncompressed BGRA) ----
        let video_in = unsafe { MFCreateMediaType()? };
        unsafe {
            video_in.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            video_in.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_ARGB32)?;
            video_in.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            video_in.SetUINT64(&MF_MT_FRAME_SIZE, ((target_width as u64) << 32) | target_height as u64)?;
            video_in.SetUINT64(&MF_MT_FRAME_RATE, ((target_fps as u64) << 32) | 1)?;
            video_in.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            video_in.SetUINT32(&MF_MT_DEFAULT_STRIDE, (target_width * 4) as u32)?;
        }

        unsafe {
            sink_writer.SetInputMediaType(video_stream_index, &video_in, None)?;
        }

        // ---- Audio output type (AAC) ----
        let audio_out = unsafe { MFCreateMediaType()? };
        unsafe {
            audio_out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            audio_out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            audio_out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, audio_sr)?;
            audio_out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, audio_ch as u32)?;
            audio_out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, audio_bps as u32)?;
        }

        let audio_stream_index = unsafe { sink_writer.AddStream(&audio_out)? };

        // ---- Audio input type (PCM) ----
        let audio_in = unsafe { MFCreateMediaType()? };
        unsafe {
            audio_in.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            audio_in.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            audio_in.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, audio_sr)?;
            audio_in.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, audio_ch as u32)?;
            audio_in.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, audio_bps as u32)?;
            audio_in.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, audio_block_align)?;
            audio_in.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, audio_sr * audio_block_align)?;
        }

        unsafe {
            sink_writer.SetInputMediaType(audio_stream_index, &audio_in, None)?;
        }

        // Start writing.
        unsafe {
            sink_writer.BeginWriting()?;
        }

        Ok(Self {
            sink_writer,
            video_stream_index,
            audio_stream_index,
            first_timestamp: None,
            is_video_disabled,
            is_audio_disabled,
            target_width,
            target_height,
            target_color_format: ColorFormat::Bgra8,
            cached_surface: None,
            dropped_frames: 0,
            audio_sample_rate: audio_sr,
            audio_block_align,
            audio_samples_sent: 0,
        })
    }

    /// Constructs a new `VideoEncoder` that writes to the given stream.
    ///
    /// **Not implemented for IMFSinkWriter.** Use [`VideoEncoder::new`] instead.
    #[inline]
    pub fn new_from_stream(
        _video_settings: VideoSettingsBuilder,
        _audio_settings: AudioSettingsBuilder,
        _container_settings: ContainerSettingsBuilder,
        _stream: IRandomAccessStream,
    ) -> Result<Self, VideoEncoderError> {
        Err(VideoEncoderError::WindowsError(windows::core::Error::from_hresult(
            windows::Win32::Foundation::E_NOTIMPL,
        )))
    }

    fn build_padded_surface(&mut self, frame: &Frame) -> Result<SendDirectX<IDirect3DSurface>, VideoEncoderError> {
        let frame_format = frame.color_format();
        let needs_recreate = self.cached_surface.as_ref().is_none_or(|cache| {
            cache.format != frame_format || cache.width != self.target_width || cache.height != self.target_height
        });

        if needs_recreate {
            let surface =
                Self::create_cached_surface(frame.device(), self.target_width, self.target_height, frame_format)?;
            self.cached_surface = Some(surface);
            self.target_color_format = frame_format;
        }

        let cache = self.cached_surface.as_mut().expect("cached_surface must be populated before use");
        let context = frame.device_context();

        if let Some(rtv) = &cache.render_target_view {
            let clear_color = [0.0f32, 0.0, 0.0, 1.0];
            unsafe {
                context.ClearRenderTargetView(&rtv.0, &clear_color);
            }
        }

        let copy_width = self.target_width.min(frame.width());
        let copy_height = self.target_height.min(frame.height());

        if copy_width > 0 && copy_height > 0 {
            let source_box = D3D11_BOX { left: 0, top: 0, front: 0, right: copy_width, bottom: copy_height, back: 1 };
            unsafe {
                context.CopySubresourceRegion(
                    &cache.texture.0,
                    0,
                    0,
                    0,
                    0,
                    frame.as_raw_texture(),
                    0,
                    Some(&source_box),
                );
            }
        }

        unsafe {
            context.Flush();
        }

        Ok(SendDirectX::new(cache.surface.0.clone()))
    }

    /// Crops `frame` to the encoder's target size **on the GPU** and returns the
    /// resulting surface.
    ///
    /// Unlike [`Self::build_padded_surface`] this honours an arbitrary origin,
    /// so a screen *region* can be cut out of a monitor-sized capture texture
    /// without ever touching the CPU. The destination texture is cached and
    /// reused, so there is no per-frame allocation either.
    fn build_cropped_surface(
        &mut self,
        frame: &Frame,
        origin_x: u32,
        origin_y: u32,
    ) -> Result<SendDirectX<IDirect3DSurface>, VideoEncoderError> {
        let frame_format = frame.color_format();
        let needs_recreate = self.cached_surface.as_ref().is_none_or(|cache| {
            cache.format != frame_format
                || cache.width != self.target_width
                || cache.height != self.target_height
        });

        if needs_recreate {
            let surface =
                Self::create_cached_surface(frame.device(), self.target_width, self.target_height, frame_format)?;
            self.cached_surface = Some(surface);
            self.target_color_format = frame_format;
        }

        let cache = self.cached_surface.as_mut().expect("cached_surface must be populated before use");
        let context = frame.device_context();

        // Clamp: a region overhanging the monitor edge (multi-monitor
        // selections all land on one monitor's texture) must not read out of
        // bounds.
        let copy_width = self.target_width.min(frame.width().saturating_sub(origin_x));
        let copy_height = self.target_height.min(frame.height().saturating_sub(origin_y));
        let partial = copy_width != self.target_width || copy_height != self.target_height;

        // Clear first, but only when the copy leaves part of the target
        // untouched — otherwise stale pixels leak in from the previous frame.
        // The common (fully in-bounds) case skips this entirely.
        if partial && let Some(rtv) = &cache.render_target_view {
            let clear_color = [0.0f32, 0.0, 0.0, 1.0];
            unsafe { context.ClearRenderTargetView(&rtv.0, &clear_color) };
        }

        if copy_width > 0 && copy_height > 0 {
            let source_box = D3D11_BOX {
                left: origin_x,
                top: origin_y,
                front: 0,
                right: origin_x + copy_width,
                bottom: origin_y + copy_height,
                back: 1,
            };
            unsafe {
                context.CopySubresourceRegion(
                    &cache.texture.0,
                    0,
                    0,
                    0,
                    0,
                    frame.as_raw_texture(),
                    0,
                    Some(&source_box),
                );
            }
        }

        unsafe {
            context.Flush();
        }

        Ok(SendDirectX::new(cache.surface.0.clone()))
    }

    /// Writes a video sample to the sink writer.
    fn write_video_sample(
        &mut self,
        source: VideoEncoderSource,
        timestamp: TimeSpan,
    ) -> Result<(), VideoEncoderError> {
        let sample = unsafe { MFCreateSample()? };

        match source {
            VideoEncoderSource::DirectX(surface) => {
                let dxgi_interface: IDirect3DDxgiInterfaceAccess = surface.0.cast()?;
                let texture: ID3D11Texture2D = unsafe { dxgi_interface.GetInterface::<ID3D11Texture2D>()? };
                let media_buffer = unsafe {
                    MFCreateDXGISurfaceBuffer(
                        &ID3D11Texture2D::IID,
                        &texture,
                        0,
                        false,
                    )?
                };
                let two_d: IMF2DBuffer = media_buffer.cast()?;
                unsafe {
                    let length = two_d.GetContiguousLength()?;
                    media_buffer.SetCurrentLength(length)?;
                    sample.AddBuffer(&media_buffer)?;
                }
            }
            VideoEncoderSource::Buffer(bytes) => {
                let media_buffer = unsafe {
                    let buf = MFCreateMemoryBuffer(bytes.len() as u32)?;
                    let mut data: *mut u8 = std::ptr::null_mut();
                    let mut max_len: u32 = 0;
                    buf.Lock(&mut data, Some(&mut max_len), None)?;
                    if !data.is_null() {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
                    }
                    buf.Unlock()?;
                    buf.SetCurrentLength(bytes.len() as u32)?;
                    buf
                };
                unsafe {
                    sample.AddBuffer(&media_buffer)?;
                }
            }
        }

        unsafe {
            sample.SetSampleTime(timestamp.Duration)?;
            sample.SetSampleDuration(1)?;
            self.sink_writer.WriteSample(self.video_stream_index, &sample)?;
        }

        Ok(())
    }

    /// Writes an audio sample to the sink writer.
    fn write_audio_sample(
        &mut self,
        buffer: &[u8],
        timestamp: TimeSpan,
        duration: i64,
    ) -> Result<(), VideoEncoderError> {
        let media_buffer = unsafe {
            let buf = MFCreateMemoryBuffer(buffer.len() as u32)?;
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut max_len: u32 = 0;
            buf.Lock(&mut data, Some(&mut max_len), None)?;
            if !data.is_null() {
                std::ptr::copy_nonoverlapping(buffer.as_ptr(), data, buffer.len());
            }
            buf.Unlock()?;
            buf.SetCurrentLength(buffer.len() as u32)?;
            buf
        };

        let sample = unsafe { MFCreateSample()? };
        unsafe {
            sample.AddBuffer(&media_buffer)?;
            sample.SetSampleTime(timestamp.Duration)?;
            sample.SetSampleDuration(duration)?;
            self.sink_writer.WriteSample(self.audio_stream_index, &sample)?;
        }

        Ok(())
    }

    /// Sends a region of `frame`, cropped on the GPU with zero CPU copies.
    #[inline]
    pub fn send_frame_region(
        &mut self,
        frame: &Frame,
        origin_x: u32,
        origin_y: u32,
        timestamp: i64,
    ) -> Result<(), VideoEncoderError> {
        if self.is_video_disabled {
            return Err(VideoEncoderError::VideoDisabled);
        }

        let surface = self.build_cropped_surface(frame, origin_x, origin_y)?;

        let timestamp = match self.first_timestamp {
            Some(t0) => TimeSpan { Duration: timestamp - t0.Duration },
            None => {
                self.first_timestamp = Some(TimeSpan { Duration: timestamp });
                TimeSpan { Duration: 0 }
            }
        };

        self.write_video_sample(VideoEncoderSource::DirectX(surface), timestamp)
    }

    /// Samples shed because the encoder could not keep up. Diagnostic only.
    #[inline]
    pub const fn dropped_frames(&self) -> u64 {
        self.dropped_frames
    }

    /// Sends a video frame (DirectX). Returns immediately.
    #[inline]
    pub fn send_frame(&mut self, frame: &Frame) -> Result<(), VideoEncoderError> {
        if self.is_video_disabled {
            return Err(VideoEncoderError::VideoDisabled);
        }

        let timestamp = match self.first_timestamp {
            Some(t0) => TimeSpan { Duration: frame.timestamp()?.Duration - t0.Duration },
            None => {
                let ts = frame.timestamp()?;
                self.first_timestamp = Some(ts);
                TimeSpan { Duration: 0 }
            }
        };

        let surface = if frame.width() == self.target_width && frame.height() == self.target_height {
            SendDirectX::new(frame.as_raw_surface().clone())
        } else {
            self.build_padded_surface(frame)?
        };

        self.write_video_sample(VideoEncoderSource::DirectX(surface), timestamp)
    }

    /// Sends a video frame and an audio buffer (owned). Returns immediately.
    /// Audio timestamp is derived from total samples sent so far (monotonic).
    #[inline]
    pub fn send_frame_with_audio(&mut self, frame: &mut Frame, audio_buffer: &[u8]) -> Result<(), VideoEncoderError> {
        if self.is_video_disabled {
            return Err(VideoEncoderError::VideoDisabled);
        }
        if self.is_audio_disabled {
            return Err(VideoEncoderError::AudioDisabled);
        }

        let video_ts = match self.first_timestamp {
            Some(t0) => TimeSpan { Duration: frame.timestamp()?.Duration - t0.Duration },
            None => {
                let ts = frame.timestamp()?;
                self.first_timestamp = Some(ts);
                TimeSpan { Duration: 0 }
            }
        };

        let surface = if frame.width() == self.target_width && frame.height() == self.target_height {
            SendDirectX::new(frame.as_raw_surface().clone())
        } else {
            self.build_padded_surface(frame)?
        };

        let _ = self.write_video_sample(VideoEncoderSource::DirectX(surface), video_ts);

        let frames_in_buf = (audio_buffer.len() as u32) / self.audio_block_align;
        let audio_ts_ticks = ((self.audio_samples_sent as i128) * 10_000_000i128) / (self.audio_sample_rate as i128);
        let audio_ts = TimeSpan { Duration: audio_ts_ticks as i64 };
        let duration = (frames_in_buf as i64) * 10_000_000 / (self.audio_sample_rate as i64);

        self.audio_samples_sent = self.audio_samples_sent.saturating_add(frames_in_buf as u64);

        self.write_audio_sample(audio_buffer, audio_ts, duration)
    }

    /// Sends a raw frame buffer (owned inside). Returns immediately.
    #[inline]
    pub fn send_frame_buffer(&mut self, buffer: &[u8], timestamp: i64) -> Result<(), VideoEncoderError> {
        if self.is_video_disabled {
            return Err(VideoEncoderError::VideoDisabled);
        }

        let timestamp = match self.first_timestamp {
            Some(t0) => TimeSpan { Duration: timestamp - t0.Duration },
            None => {
                self.first_timestamp = Some(TimeSpan { Duration: timestamp });
                TimeSpan { Duration: 0 }
            }
        };

        self.write_video_sample(VideoEncoderSource::Buffer(buffer.to_vec()), timestamp)
    }

    /// Sends an audio buffer (owned inside). Returns immediately.
    /// NOTE: The provided `timestamp` is ignored; we use a monotonic audio clock.
    #[inline]
    pub fn send_audio_buffer(
        &mut self,
        buffer: &[u8],
        _timestamp: i64,
    ) -> Result<(), VideoEncoderError> {
        if self.is_audio_disabled {
            return Err(VideoEncoderError::AudioDisabled);
        }

        let frames_in_buf = (buffer.len() as u32) / self.audio_block_align;
        let audio_ts_ticks = ((self.audio_samples_sent as i128) * 10_000_000i128) / (self.audio_sample_rate as i128);
        let timestamp = TimeSpan { Duration: audio_ts_ticks as i64 };
        let duration = (frames_in_buf as i64) * 10_000_000 / (self.audio_sample_rate as i64);

        self.audio_samples_sent = self.audio_samples_sent.saturating_add(frames_in_buf as u64);

        self.write_audio_sample(buffer, timestamp, duration)
    }

    /// Finishes the encoding and performs any necessary cleanup.
    #[inline]
    pub fn finish(self) -> Result<(), VideoEncoderError> {
        unsafe {
            self.sink_writer.Finalize()?;
            MFShutdown()?;
        }
        Ok(())
    }
}

impl Drop for VideoEncoder {
    #[inline]
    fn drop(&mut self) {
        let _ = unsafe { self.sink_writer.Finalize() };
        let _ = unsafe { MFShutdown() };
    }
}

#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl Send for VideoEncoder {}
