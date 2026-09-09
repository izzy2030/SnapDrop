//! System-audio capture via WASAPI loopback.
//!
//! Captures the default rendering device's *mix* — i.e. everything the user
//! hears (a playing video, music, a call), not the mic. Windows resamples and
//! converts it to a fixed target we choose (48 kHz / stereo / 16-bit
//! little-endian interleaved PCM), which is exactly the format the
//! [`windows_capture::encoder::VideoEncoder`] audio track expects.
//!
//! The capture runs on its own thread and pushes interleaved PCM packets into
//! an mpsc channel. The video-recording handler drains that channel and feeds
//! it to the encoder via `send_audio_buffer`, whose timestamps are monotonic
//! by sample count, so A/V stays in sync regardless of the 30 fps capture
//! cadence.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;

use windows::Win32::Media::Audio::{
    eConsole, eRender, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_LOOPBACK, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};

/// The encoder's audio settings builder uses these defaults (see
/// `AudioSettingsBuilder::new`): 48 kHz, stereo, 16-bit PCM.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;
pub const BITS_PER_SAMPLE: u16 = 16;
pub const BLOCK_ALIGN: u16 = CHANNELS * BITS_PER_SAMPLE / 8; // 4 bytes/frame

/// Max queued PCM packets (~10 ms each). The video handler only drains audio
/// when a WGC frame arrives; while the screen is static no frames arrive, so
/// without a bound this channel would grow ~192 KB/s indefinitely. At the cap
/// the newest packet is dropped, so a long still-frame recording never
/// accumulates unbounded memory (audio on a static screen is inaudible anyway).
const MAX_PACKETS: usize = 400;

/// A running WASAPI loopback capture session.
pub struct AudioLoopback {
    rx: Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl AudioLoopback {
    /// Begin capturing the default output device's mix. If the audio device
    /// can't be opened (no output device, exclusive-mode app holding it, etc.)
    /// this returns `None` — recording continues silently without audio rather
    /// than failing the whole session.
    pub fn start() -> Option<Self> {
        // Bounded queue: see MAX_PACKETS. `sync_channel` + `try_send` means a
        // never-drained static screen just sheds oldest packets instead of
        // growing memory without limit.
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(MAX_PACKETS);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = std::thread::Builder::new()
            .name("wasapi-loopback".into())
            .spawn(move || run(tx, stop_thread))
            .ok()?;
        Some(Self { rx, stop, handle: Some(handle) })
    }

    /// Non-blocking drain of any PCM packets queued so far.
    pub fn drain(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        while let Ok(pkt) = self.rx.try_recv() {
            out.extend_from_slice(&pkt);
        }
        out
    }

    /// Stop the capture thread and wait for it to exit.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for AudioLoopback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Join if the caller didn't stop() us explicitly (e.g. a finalize that
        // returned early), so the wasapi thread never lingers detached.
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn run(tx: SyncSender<Vec<u8>>, stop: Arc<AtomicBool>) {
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let result = capture_loop(&tx, &stop);
    if let Err(e) = &result {
        // Surface any failure to the debug log; the session just has no audio.
        crate::debuglog::log(&format!("video: audio capture ended: {e}"));
    }
    unsafe { CoUninitialize() };
    let _ = result;
}

fn capture_loop(tx: &SyncSender<Vec<u8>>, stop: &Arc<AtomicBool>) -> windows::core::Result<()> {
    // Enumerate default render device.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole)? };
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };

    // Target format: 48 kHz / stereo / 16-bit PCM. With AUTOCONVERTPCM + the
    // loopback flag, WASAPI resamples + converts the mix to this.
    let fmt = WAVEFORMATEX {
        wFormatTag: 1, // WAVE_FORMAT_PCM
        nChannels: CHANNELS,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE as u32 * BLOCK_ALIGN as u32,
        nBlockAlign: BLOCK_ALIGN,
        wBitsPerSample: BITS_PER_SAMPLE,
        cbSize: 0,
    };

    unsafe {
        // Shared mode, loopback, autoconvert. 0 buffer duration lets WASAPI pick.
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            0,
            0,
            &fmt,
            None,
        )?;
        client.Start()?;
    }

    let capture: IAudioCaptureClient = unsafe { client.GetService()? };

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let packet = unsafe { capture.GetNextPacketSize()? };
        if packet == 0 {
            std::thread::sleep(std::time::Duration::from_millis(5));
            continue;
        }
        let mut data: *mut u8 = std::ptr::null_mut();
        let mut frames: u32 = 0;
        let mut flags: u32 = 0;
        unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)? };
        let bytes = frames as usize * BLOCK_ALIGN as usize;
        let mut pkt = Vec::with_capacity(bytes);
        if !data.is_null() && bytes > 0 {
            pkt.extend_from_slice(unsafe { std::slice::from_raw_parts(data, bytes) });
        }
        unsafe { capture.ReleaseBuffer(frames)? };
        // try_send, non-blocking: when the bounded queue is full (consumer
        // stalled on a static screen) the packet is simply dropped — audio on
        // an unchanged screen is inaudible, and this caps memory. Only a
        // disconnected consumer stops the capture thread.
        match tx.try_send(pkt) {
            Err(mpsc::TrySendError::Disconnected(_)) => break,
            Err(mpsc::TrySendError::Full(_)) => {}
            Ok(()) => {}
        }
    }

    unsafe { client.Stop()? };
    Ok(())
}