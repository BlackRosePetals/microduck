//! The laptop's voice on Linux: libasound, opened when `udp-pad` starts rather than linked.
//!
//! **Linking it would break the wheel.** `uvx duckctl` installs a manylinux wheel, and
//! libasound is outside the manylinux policy, so the wheel would carry a copy of its own built on
//! AlmaLinux. That copy looks for its plugins where AlmaLinux keeps them, so on Debian or Ubuntu
//! it never finds the PipeWire or PulseAudio plugin that their `default` device is. The result
//! would be a laptop with sound that `duckctl` cannot reach. The system's own libasound, opened at
//! run time, finds its own plugins wherever its distribution put them. A machine without it is a
//! silent laptop, never a `duckctl` that will not start.
//!
//! Eight functions are all this needs, which is why it is not `cpal` here as it is on the other
//! two platforms: `cpal` links libasound on Linux, with no way to turn that off.
//!
//! `default` is the device, so the output is whatever the desktop has selected, and ALSA's own
//! plug layer converts the bank's 48 kHz mono to what that device takes.

use std::ffi::{CStr, c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::sync::{Arc, Mutex};

use libloading::Library;

use super::Mixer;

type Pcm = *mut c_void;

const PLAYBACK: c_int = 0;
const S16_LE: c_int = 2;
const RW_INTERLEAVED: c_int = 3;
const RATE: u32 = sounds::SR;
/// How far ahead of the speaker the writer runs. It is the delay between a trigger and its
/// sound, so short; an underrun costs a click, not the session.
const LATENCY_US: c_uint = 30_000;
/// Frames per write: 5 ms, well inside the latency.
const BLOCK: usize = 240;

/// The writer thread owns the library and the PCM; this is only what keeps the type the same as
/// the other platforms'. The thread ends with the process.
pub struct Output;

struct Alsa {
    _lib: Library,
    writei: unsafe extern "C" fn(Pcm, *const c_void, c_ulong) -> c_long,
    recover: unsafe extern "C" fn(Pcm, c_int, c_int) -> c_int,
}

/// A PCM handle, moved once from the thread that opened it to the one that writes to it.
struct Handle(Pcm);
unsafe impl Send for Handle {}

/// The handler ALSA calls with each of its messages — dropped, since they would land on top of
/// `udp-pad`'s screen. Its real type is variadic; a function that reads none of its arguments
/// can stand in for one where Linux runs.
unsafe extern "C" fn quiet(
    _: *const c_char,
    _: c_int,
    _: *const c_char,
    _: c_int,
    _: *const c_char,
) {
}

pub fn open() -> Result<(Output, u32, Arc<Mutex<Mixer>>), String> {
    // SAFETY: libasound's initialisers are the system's own, run as any program linking it would.
    let lib = unsafe { Library::new("libasound.so.2") }
        .map_err(|_| "libasound.so.2 is not installed".to_owned())?;
    // SAFETY: each symbol is given the signature `alsa/pcm.h` and `alsa/error.h` declare.
    let (pcm, alsa) = unsafe {
        let set_handler = *lib
            .get::<unsafe extern "C" fn(
                Option<
                    unsafe extern "C" fn(*const c_char, c_int, *const c_char, c_int, *const c_char),
                >,
            ) -> c_int>(b"snd_lib_error_set_handler\0")
            .map_err(|e| e.to_string())?;
        let pcm_open = *lib
            .get::<unsafe extern "C" fn(*mut Pcm, *const c_char, c_int, c_int) -> c_int>(
                b"snd_pcm_open\0",
            )
            .map_err(|e| e.to_string())?;
        let set_params = *lib
            .get::<unsafe extern "C" fn(Pcm, c_int, c_int, c_uint, c_uint, c_int, c_uint) -> c_int>(
                b"snd_pcm_set_params\0",
            )
            .map_err(|e| e.to_string())?;
        let close = *lib
            .get::<unsafe extern "C" fn(Pcm) -> c_int>(b"snd_pcm_close\0")
            .map_err(|e| e.to_string())?;
        let strerror = *lib
            .get::<unsafe extern "C" fn(c_int) -> *const c_char>(b"snd_strerror\0")
            .map_err(|e| e.to_string())?;
        let writei = *lib
            .get(b"snd_pcm_writei\0")
            .map_err(|e: libloading::Error| e.to_string())?;
        let recover = *lib
            .get(b"snd_pcm_recover\0")
            .map_err(|e: libloading::Error| e.to_string())?;
        let why = |rc: c_int| CStr::from_ptr(strerror(rc)).to_string_lossy().into_owned();

        set_handler(Some(quiet));
        let mut pcm: Pcm = std::ptr::null_mut();
        let rc = pcm_open(&mut pcm, c"default".as_ptr(), PLAYBACK, 0);
        if rc < 0 {
            return Err(format!("cannot open the default audio output: {}", why(rc)));
        }
        // soft_resample on: the plug layer converts to whatever the device takes.
        let rc = set_params(pcm, S16_LE, RW_INTERLEAVED, 1, RATE, 1, LATENCY_US);
        if rc < 0 {
            close(pcm);
            return Err(format!(
                "the default audio output refuses 48 kHz mono: {}",
                why(rc)
            ));
        }
        (
            Handle(pcm),
            Alsa {
                _lib: lib,
                writei,
                recover,
            },
        )
    };
    let mixer = Arc::new(Mutex::new(Mixer::new(RATE)));
    let fill = mixer.clone();
    std::thread::Builder::new()
        .name("voice".into())
        .spawn(move || write(alsa, pcm, &fill))
        .map_err(|e| e.to_string())?;
    Ok((Output, RATE, mixer))
}

/// Silence between sounds is written too: the device stays open and the next quack starts at
/// once, rather than after a reopen.
fn write(alsa: Alsa, pcm: Handle, mixer: &Mutex<Mixer>) {
    let mut block = [0.0f32; BLOCK];
    let mut pcm16 = [0i16; BLOCK];
    loop {
        match mixer.lock() {
            Ok(mut m) => m.fill(&mut block),
            Err(_) => return,
        }
        for (o, s) in pcm16.iter_mut().zip(&block) {
            *o = (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
        }
        let mut done = 0;
        while done < BLOCK {
            // SAFETY: `pcm` is open, and the pointer and frame count stay inside `pcm16`.
            let n = unsafe {
                (alsa.writei)(
                    pcm.0,
                    pcm16[done..].as_ptr().cast(),
                    (BLOCK - done) as c_ulong,
                )
            };
            if n >= 0 {
                done += n as usize;
                continue;
            }
            // An underrun or a suspend is recovered; anything else ends the voice, not the pad.
            // SAFETY: as above.
            if unsafe { (alsa.recover)(pcm.0, n as c_int, 1) } < 0 {
                return;
            }
        }
    }
}
