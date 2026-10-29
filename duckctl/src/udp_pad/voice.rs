//! The quack and the wheee, from this laptop's speakers as well as from the duck.
//!
//! `udp-pad` reads the triggers the way the robot's mapping does — RT quacks on crossing
//! [`pad_map::SOUND_THRESHOLD`], LT rides the wheee while past it — and plays them here by
//! `robotd`'s rules (`robotd/src/sound.rs`): a quack cuts the one still playing, a quack during
//! a ride is dropped, and the ride is its start, then its loop for as long as the trigger is
//! down, cut when it is let go. Nothing is sent for it: the duck hears the triggers in the pad
//! state it is already sent.
//!
//! **The laptop has a voice of its own**, not the duck's. A duck's voice is seeded from its SoC
//! serial, which nothing reaches from here; a laptop's is a random seed drawn on the first run
//! and kept in `duckctl/voice-seed` under the user's configuration directory, so it is the same
//! voice every time. `--voice N` picks another, `--mute` none. The bank is rendered by the
//! robot's own synth (`sounds`) on a thread of its own, so the pad drives at once and the
//! laptop starts quacking a moment later.
//!
//! The sound goes to the system's default output, whatever this machine has selected. On Linux
//! that is libasound opened when it is needed rather than linked (`voice/alsa.rs`, which says
//! why); on macOS and Windows it is `cpal` (`voice/cpal_out.rs`).

use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pad_map::SOUND_THRESHOLD;

#[cfg(target_os = "linux")]
mod alsa;
#[cfg(target_os = "linux")]
use alsa as out;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod cpal_out;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use cpal_out as out;

/// What the triggers asked for since they were last read.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Cues {
    pub quack: bool,
    /// `Some(true)` while LT is down, `Some(false)` once on its release.
    pub wheee: Option<bool>,
}

/// The triggers' edges, read as `pad_map::Mapper` reads them on the robot.
#[derive(Default)]
pub struct Triggers {
    lt: f64,
    rt: f64,
}

impl Triggers {
    pub fn read(&mut self, lt: f64, rt: f64) -> Cues {
        let cues = Cues {
            quack: self.rt < SOUND_THRESHOLD && rt >= SOUND_THRESHOLD,
            wheee: if lt >= SOUND_THRESHOLD {
                Some(true)
            } else if self.lt >= SOUND_THRESHOLD {
                Some(false)
            } else {
                None
            },
        };
        (self.lt, self.rt) = (lt, rt);
        cues
    }
}

/// Every sound the laptop plays, rendered once at the output's rate.
pub struct Bank {
    quacks: Vec<Vec<f32>>,
    /// Each wheee variant's start and loop. The end segment is the ride *landing*, which on the
    /// robot is only a pad that went quiet mid-ride; a release cuts, here as there.
    rides: Vec<(Vec<f32>, Vec<f32>)>,
}

impl Bank {
    pub fn render(seed: u32, rate: u32) -> Self {
        let p = sounds::Personality::from_seed(seed);
        let fit = |b: Vec<f32>| resample(&b, sounds::SR, rate);
        Self {
            quacks: (0..sounds::variant_count("chirp"))
                .map(|v| fit(sounds::voices::chirp(&p, v)))
                .collect(),
            rides: (0..sounds::variant_count("wheee"))
                .map(|v| {
                    let (start, loop_seg, _) = sounds::voices::wheee_segments(&p, v);
                    (fit(start), fit(loop_seg))
                })
                .collect(),
        }
    }
}

/// Linear interpolation: the bank is a few seconds of voice, played once in a while, so the
/// cheapest resampler that does not alias audibly is enough. Most outputs run at the synth's
/// own 48 kHz and skip it.
fn resample(b: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || b.is_empty() {
        return b.to_vec();
    }
    let step = f64::from(from) / f64::from(to);
    let n = (b.len() as u64 * u64::from(to) / u64::from(from)) as usize;
    (0..n)
        .map(|i| {
            let x = i as f64 * step;
            let (j, f) = (x as usize, (x.fract()) as f32);
            let a = b[j];
            let c = b.get(j + 1).copied().unwrap_or(a);
            a + (c - a) * f
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clip {
    Quack(usize),
    Start(usize),
    Loop(usize),
}

#[derive(Debug, Clone, Copy)]
struct Play {
    clip: Clip,
    pos: usize,
    /// Samples left of a fade-out: a cut sound is faded rather than stopped, which would click.
    fade: Option<usize>,
}

/// What plays, sample by sample: the sound now, and the one it cut fading out under it.
pub struct Mixer {
    bank: Option<Arc<Bank>>,
    now: Option<Play>,
    cut: Option<Play>,
    fade: usize,
    rng: u64,
}

/// How long a cut sound takes to fade.
const FADE: Duration = Duration::from_millis(8);

/// Below the bank's level, which is normalised for a duck's small speaker: a laptop's is next
/// to whoever is driving.
const GAIN: f32 = 0.6;

impl Mixer {
    pub fn new(rate: u32) -> Self {
        Self {
            bank: None,
            now: None,
            cut: None,
            fade: (FADE.as_secs_f64() * f64::from(rate)).max(1.0) as usize,
            rng: std::collections::hash_map::RandomState::new()
                .build_hasher()
                .finish()
                | 1,
        }
    }

    pub fn load(&mut self, bank: Bank) {
        self.bank = Some(Arc::new(bank));
    }

    pub fn cue(&mut self, cues: Cues) {
        let Some(bank) = self.bank.clone() else {
            return;
        };
        let riding =
            matches!(self.now, Some(p) if matches!(p.clip, Clip::Start(_) | Clip::Loop(_)));
        match cues.wheee {
            Some(true) if !riding => {
                let v = self.pick(bank.rides.len());
                self.play(Clip::Start(v));
            }
            Some(false) if riding => self.stop(),
            _ => {}
        }
        // Dropped during a ride, as on the robot: holding both triggers is how they are used,
        // and a quack must not restart the ride.
        let riding =
            matches!(self.now, Some(p) if matches!(p.clip, Clip::Start(_) | Clip::Loop(_)));
        if cues.quack && !riding {
            let v = self.pick(bank.quacks.len());
            self.play(Clip::Quack(v));
        }
    }

    fn play(&mut self, clip: Clip) {
        self.stop();
        self.now = Some(Play {
            clip,
            pos: 0,
            fade: None,
        });
    }

    fn stop(&mut self) {
        if let Some(mut p) = self.now.take() {
            p.fade = Some(self.fade);
            self.cut = Some(p);
        }
    }

    /// xorshift: which variant plays is all it decides.
    fn pick(&mut self, n: usize) -> usize {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng % n.max(1) as u64) as usize
    }

    /// Mono samples, for an output to copy to its channels.
    pub fn fill(&mut self, out: &mut [f32]) {
        let Some(bank) = self.bank.clone() else {
            out.fill(0.0);
            return;
        };
        for s in out {
            let a = next(&bank, &mut self.now, self.fade);
            let b = next(&bank, &mut self.cut, self.fade);
            *s = GAIN * (a + b);
        }
    }
}

fn next(bank: &Bank, slot: &mut Option<Play>, fade: usize) -> f32 {
    let Some(p) = slot else {
        return 0.0;
    };
    let clip = |c: Clip| match c {
        Clip::Quack(v) => &bank.quacks[v],
        Clip::Start(v) => &bank.rides[v].0,
        Clip::Loop(v) => &bank.rides[v].1,
    };
    if p.pos >= clip(p.clip).len() {
        match p.clip {
            Clip::Start(v) => p.clip = Clip::Loop(v),
            Clip::Loop(_) => {}
            Clip::Quack(_) => {
                *slot = None;
                return 0.0;
            }
        }
        p.pos = 0;
        if clip(p.clip).is_empty() {
            *slot = None;
            return 0.0;
        }
    }
    let mut s = clip(p.clip)[p.pos];
    p.pos += 1;
    if let Some(left) = &mut p.fade {
        if *left == 0 {
            *slot = None;
            return 0.0;
        }
        s *= *left as f32 / fade as f32;
        *left -= 1;
    }
    s
}

/// The laptop's voice, playing while it lives.
pub struct Voice {
    mixer: Arc<Mutex<Mixer>>,
    _out: out::Output,
}

impl Voice {
    /// Opens the default output and starts rendering the bank for `seed`. An error is a laptop
    /// that cannot play — no device, no libasound — and `udp-pad` drives on without it.
    pub fn open(seed: u32) -> Result<Self, String> {
        let (out, rate, mixer) = out::open()?;
        let render = mixer.clone();
        std::thread::Builder::new()
            .name("voice-bank".into())
            .spawn(move || {
                let bank = Bank::render(seed, rate);
                if let Ok(mut m) = render.lock() {
                    m.load(bank);
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { mixer, _out: out })
    }

    pub fn cue(&self, cues: Cues) {
        if cues == Cues::default() {
            return;
        }
        if let Ok(mut m) = self.mixer.lock() {
            m.cue(cues);
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod out {
    pub struct Output;
    pub fn open() -> Result<(Output, u32, super::Arc<super::Mutex<super::Mixer>>), String> {
        Err("no audio output on this platform".to_owned())
    }
}

/// Where the laptop's seed is kept: `duckctl/voice-seed` under `%APPDATA%` on Windows, and
/// `$XDG_CONFIG_HOME` or `~/.config` elsewhere — macOS included, where a dotfile directory is
/// what a command-line tool's user looks in.
fn seed_file() -> Option<PathBuf> {
    let set = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let dir = if cfg!(windows) {
        set("APPDATA")
    } else {
        set("XDG_CONFIG_HOME").or_else(|| set("HOME").map(|h| h.join(".config")))
    };
    Some(dir?.join("duckctl").join("voice-seed"))
}

/// This laptop's voice: the kept seed, or a new one drawn and kept. A seed that cannot be kept
/// is still a voice, for this run.
pub fn seed() -> u32 {
    let file = seed_file();
    if let Some(seed) = file
        .as_ref()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|s| s.trim().parse().ok())
    {
        return seed;
    }
    let seed = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish() as u32;
    if let Some(f) = file {
        let _ = f
            .parent()
            .map(std::fs::create_dir_all)
            .and_then(Result::ok)
            .map(|()| std::fs::write(&f, format!("{seed}\n")));
    }
    seed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rt_quacks_on_its_rising_edge_only() {
        let mut t = Triggers::default();
        assert!(!t.read(0.0, 0.2).quack);
        assert!(t.read(0.0, 0.5).quack, "crossing");
        assert!(!t.read(0.0, 1.0).quack, "held");
        assert!(!t.read(0.0, 0.1).quack, "released");
        assert!(t.read(0.0, 0.3).quack, "again, at the threshold");
    }

    #[test]
    fn lt_holds_the_wheee_and_lets_go_once() {
        let mut t = Triggers::default();
        assert_eq!(t.read(0.1, 0.0).wheee, None);
        assert_eq!(t.read(0.4, 0.0).wheee, Some(true));
        assert_eq!(
            t.read(0.9, 0.0).wheee,
            Some(true),
            "a level, as padd sends it"
        );
        assert_eq!(t.read(0.0, 0.0).wheee, Some(false));
        assert_eq!(t.read(0.0, 0.0).wheee, None);
    }

    fn mixer() -> Mixer {
        let mut m = Mixer::new(1000);
        m.load(Bank {
            quacks: vec![vec![1.0; 50]],
            rides: vec![(vec![0.5; 20], vec![0.25; 10])],
        });
        m
    }

    const QUACK: Cues = Cues {
        quack: true,
        wheee: None,
    };
    const HOLD: Cues = Cues {
        quack: false,
        wheee: Some(true),
    };
    const RELEASE: Cues = Cues {
        quack: false,
        wheee: Some(false),
    };

    #[test]
    fn a_quack_plays_once_and_stops() {
        let mut m = mixer();
        m.cue(QUACK);
        let mut out = [0.0; 80];
        m.fill(&mut out);
        assert!(out[..50].iter().all(|&s| s == GAIN));
        assert!(out[50..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn the_ride_loops_while_held_and_fades_when_let_go() {
        let mut m = mixer();
        m.cue(HOLD);
        let mut out = [0.0; 100];
        m.fill(&mut out);
        assert_eq!(out[19], 0.5 * GAIN, "start");
        assert_eq!(out[20], 0.25 * GAIN, "loop");
        assert_eq!(out[99], 0.25 * GAIN, "still looping");
        m.cue(HOLD);
        m.cue(RELEASE);
        m.fill(&mut out);
        assert!(out[1] < out[0] && out[1] > 0.0, "fading");
        assert!(out[20..].iter().all(|&s| s == 0.0), "gone");
    }

    #[test]
    fn a_quack_during_a_ride_is_dropped() {
        let mut m = mixer();
        m.cue(HOLD);
        m.cue(Cues {
            quack: true,
            wheee: Some(true),
        });
        let mut out = [0.0; 30];
        m.fill(&mut out);
        assert!(out.iter().all(|&s| s <= 0.5 * GAIN), "no quack in it");
    }

    #[test]
    fn nothing_plays_before_the_bank_is_rendered() {
        let mut m = Mixer::new(1000);
        m.cue(QUACK);
        let mut out = [1.0; 10];
        m.fill(&mut out);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    /// By ear, on the machine running it: two quacks, then a two-second ride.
    /// `cargo test -p duckctl --release -- --ignored plays_here --nocapture`.
    #[test]
    #[ignore = "plays sound on this machine's default output"]
    fn plays_here() {
        let t = std::time::Instant::now();
        let v = Voice::open(seed()).expect("an audio output");
        while v.mixer.lock().unwrap().bank.is_none() {
            std::thread::sleep(Duration::from_millis(10));
        }
        eprintln!("bank rendered in {:?}", t.elapsed());
        for _ in 0..2 {
            v.cue(QUACK);
            std::thread::sleep(Duration::from_millis(600));
        }
        v.cue(HOLD);
        std::thread::sleep(Duration::from_secs(2));
        v.cue(RELEASE);
        std::thread::sleep(Duration::from_millis(300));
    }

    #[test]
    fn resampling_keeps_the_duration() {
        let b = vec![0.0; 48_000];
        assert_eq!(resample(&b, 48_000, 44_100).len(), 44_100);
        assert_eq!(resample(&b, 48_000, 48_000).len(), 48_000);
    }
}
