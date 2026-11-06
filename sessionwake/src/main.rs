// sessionwake - say "session" and the machine goes "pico!".
//
// A wake-word detector that uses the Intel Skylake GMM accelerator (via
// /dev/gmm0) for the acoustic scoring — the chip's original job. Audio capture
// (arecord) and playback (paplay) are external; feature extraction, voice
// activity detection and the app logic are std-only Rust.
//
//   sessionwake mic      # debug: show live VAD / segments
//   sessionwake enroll   # say "session" a few times -> model.txt
//   sessionwake listen   # detect "session", play pico on a match
//
// The GMM score = gconst + Sum_i var[i]*(feat[i]-mean[i])^2. A LOW score means
// the sound matches the enrolled "session" model.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write, BufWriter};
use std::os::unix::io::AsRawFd;
use std::process::{Command, Stdio};

const SR: usize = 16000;
const FRAME: usize = 256;     // 16 ms window
const HOP: usize = 128;       // 8 ms hop
const NBINS: usize = FRAME / 2; // 128 magnitude bins (0..8 kHz)
const NMEL: usize = 24;       // feature dim == GMM vlen
const ENROLL_N: usize = 6;    // utterances to enroll
const MODEL_PATH: &str = "model.txt";

// ---------- /dev/gmm0 ioctl ----------
#[repr(C)]
struct GmmJob {
    vlen: u32,
    gconst: u32,
    feature: [u8; 96],
    mean: [u8; 96],
    var: [u8; 96],
    score: u32,
    hw_status: u32,
}
extern "C" {
    fn ioctl(fd: i32, request: u64, arg: *mut GmmJob) -> i32;
}
fn gmm_compute_num() -> u64 {
    let size = std::mem::size_of::<GmmJob>() as u64; // 304
    (3u64 << 30) | (size << 16) | ((b'G' as u64) << 8) | 1
}

struct Gmm {
    file: File,
    req: u64,
}
impl Gmm {
    fn open() -> std::io::Result<Gmm> {
        let file = OpenOptions::new().read(true).write(true).open("/dev/gmm0")?;
        Ok(Gmm { file, req: gmm_compute_num() })
    }
    // score a 24-dim u8 feature against a model (mean/var u8, gconst)
    fn score(&self, feat: &[u8; NMEL], mean: &[u8; NMEL], var: &[u8; NMEL], gconst: u32) -> u32 {
        let mut job = GmmJob {
            vlen: NMEL as u32,
            gconst,
            feature: [0; 96],
            mean: [0; 96],
            var: [0; 96],
            score: 0,
            hw_status: 0,
        };
        job.feature[..NMEL].copy_from_slice(feat);
        job.mean[..NMEL].copy_from_slice(mean);
        job.var[..NMEL].copy_from_slice(var);
        let rc = unsafe { ioctl(self.file.as_raw_fd(), self.req, &mut job as *mut GmmJob) };
        if rc != 0 {
            eprintln!("ioctl failed (rc={rc})");
            return u32::MAX;
        }
        job.score
    }
}

// ---------- DSP: tables ----------
struct Dsp {
    window: Vec<f32>,            // Hamming
    cos: Vec<Vec<f32>>,          // [NBINS][FRAME]
    sin: Vec<Vec<f32>>,
    mel: Vec<Vec<f32>>,          // [NMEL][NBINS] triangular weights
}
impl Dsp {
    fn new() -> Dsp {
        let window: Vec<f32> = (0..FRAME)
            .map(|n| 0.54 - 0.46 * (2.0 * std::f32::consts::PI * n as f32 / (FRAME as f32 - 1.0)).cos())
            .collect();
        let mut cos = vec![vec![0f32; FRAME]; NBINS];
        let mut sin = vec![vec![0f32; FRAME]; NBINS];
        for k in 0..NBINS {
            for n in 0..FRAME {
                let a = 2.0 * std::f32::consts::PI * k as f32 * n as f32 / FRAME as f32;
                cos[k][n] = a.cos();
                sin[k][n] = a.sin();
            }
        }
        // mel-ish triangular filterbank over bins 1..NBINS
        let hz_to_mel = |f: f32| 2595.0 * (1.0 + f / 700.0).log10();
        let mel_to_hz = |m: f32| 700.0 * (10f32.powf(m / 2595.0) - 1.0);
        let fmax = SR as f32 / 2.0;
        let m0 = hz_to_mel(60.0);
        let m1 = hz_to_mel(fmax);
        let centers: Vec<f32> = (0..NMEL + 2)
            .map(|i| mel_to_hz(m0 + (m1 - m0) * i as f32 / (NMEL as f32 + 1.0)))
            .collect();
        let bin_hz = |b: usize| b as f32 * SR as f32 / FRAME as f32;
        let mut mel = vec![vec![0f32; NBINS]; NMEL];
        for m in 0..NMEL {
            let (lo, ce, hi) = (centers[m], centers[m + 1], centers[m + 2]);
            for b in 1..NBINS {
                let f = bin_hz(b);
                let w = if f >= lo && f <= ce {
                    (f - lo) / (ce - lo + 1e-6)
                } else if f > ce && f <= hi {
                    (hi - f) / (hi - ce + 1e-6)
                } else {
                    0.0
                };
                mel[m][b] = w.max(0.0);
            }
        }
        Dsp { window, cos, sin, mel }
    }

    // one frame of f32 samples -> 24 log-mel energies (f32)
    fn feat(&self, frame: &[f32]) -> [f32; NMEL] {
        let mut w = [0f32; FRAME];
        for n in 0..FRAME {
            w[n] = frame[n] * self.window[n];
        }
        let mut mag = [0f32; NBINS];
        for k in 0..NBINS {
            let (ck, sk) = (&self.cos[k], &self.sin[k]);
            let mut re = 0f32;
            let mut im = 0f32;
            for n in 0..FRAME {
                re += w[n] * ck[n];
                im -= w[n] * sk[n];
            }
            mag[k] = (re * re + im * im).sqrt();
        }
        let mut out = [0f32; NMEL];
        for m in 0..NMEL {
            let mut e = 0f32;
            for b in 1..NBINS {
                e += self.mel[m][b] * mag[b];
            }
            out[m] = (1.0 + e).ln();
        }
        out
    }
}

// loudness-invariant quantization of a 24-dim log-mel vector to u8
fn quantize(v: &[f32; NMEL]) -> [u8; NMEL] {
    let mean: f32 = v.iter().sum::<f32>() / NMEL as f32;
    let mut out = [0u8; NMEL];
    for i in 0..NMEL {
        // center on 128, ~24 counts per log unit, clamp
        let x = 128.0 + (v[i] - mean) * 24.0;
        out[i] = x.max(0.0).min(255.0) as u8;
    }
    out
}

// ---------- audio capture (arecord -> raw i16) ----------
struct Mic {
    child: std::process::Child,
}
impl Mic {
    fn start() -> Mic {
        let child = Command::new("arecord")
            .args(["-q", "-f", "S16_LE", "-r", "16000", "-c", "1", "-t", "raw"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("failed to start arecord (is alsa-utils installed?)");
        Mic { child }
    }
    // read exactly HOP samples; returns false on EOF
    fn read_hop(&mut self, out: &mut [f32; HOP]) -> bool {
        let mut buf = [0u8; HOP * 2];
        let so = self.child.stdout.as_mut().unwrap();
        if so.read_exact(&mut buf).is_err() {
            return false;
        }
        for i in 0..HOP {
            let s = i16::from_le_bytes([buf[2 * i], buf[2 * i + 1]]);
            out[i] = s as f32 / 32768.0;
        }
        true
    }
}
impl Drop for Mic {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

// ---------- VAD / segmentation ----------
// Streams frames, emits a 24-dim feature per detected speech segment (mean of
// voiced frames). Calls `on_segment` for each.
struct Segmenter {
    dsp: Dsp,
    ring: Vec<f32>,      // sample ring for framing
    noise: f32,          // running noise-energy floor
    voiced: bool,
    sil: usize,
    acc: [f32; NMEL],
    nframes: usize,
}
impl Segmenter {
    fn new() -> Segmenter {
        Segmenter {
            dsp: Dsp::new(),
            ring: Vec::with_capacity(FRAME),
            noise: 1e-4,
            voiced: false,
            sil: 0,
            acc: [0f32; NMEL],
            nframes: 0,
        }
    }
    fn push_hop(&mut self, hop: &[f32; HOP], debug: bool) -> Option<[u8; NMEL]> {
        self.ring.extend_from_slice(hop);
        if self.ring.len() < FRAME {
            return None;
        }
        // take one frame from the front, slide by HOP
        let frame: Vec<f32> = self.ring[..FRAME].to_vec();
        self.ring.drain(..HOP);

        let energy: f32 = frame.iter().map(|x| x * x).sum::<f32>() / FRAME as f32;
        // adapt noise floor slowly when not voiced
        if !self.voiced {
            self.noise = 0.995 * self.noise + 0.005 * energy;
        }
        let thresh = (self.noise * 8.0).max(3e-4);
        let is_voice = energy > thresh;

        let feat = self.dsp.feat(&frame);
        let mut emit = None;

        if is_voice {
            if !self.voiced {
                self.voiced = true;
                self.acc = [0f32; NMEL];
                self.nframes = 0;
            }
            for i in 0..NMEL {
                self.acc[i] += feat[i];
            }
            self.nframes += 1;
            self.sil = 0;
        } else if self.voiced {
            self.sil += 1;
            if self.sil > 12 {
                // end of segment (~96 ms of silence)
                self.voiced = false;
                let dur_ms = self.nframes * HOP * 1000 / SR;
                if self.nframes >= 18 && self.nframes <= 180 {
                    let mut mean = [0f32; NMEL];
                    for i in 0..NMEL {
                        mean[i] = self.acc[i] / self.nframes as f32;
                    }
                    if debug {
                        eprintln!("segment: {} frames (~{} ms)", self.nframes, dur_ms);
                    }
                    emit = Some(quantize(&mean));
                } else if debug {
                    eprintln!("segment rejected: {} frames (~{} ms)", self.nframes, dur_ms);
                }
            }
        }
        emit
    }
}

// ---------- model IO ----------
struct Model {
    mean: [u8; NMEL],
    var: [u8; NMEL],
    gconst: u32,
    threshold: u32,
}
impl Model {
    fn save(&self, path: &str) -> std::io::Result<()> {
        let mut f = BufWriter::new(File::create(path)?);
        write!(f, "gconst {}\nthreshold {}\nmean", self.gconst, self.threshold)?;
        for v in &self.mean { write!(f, " {}", v)?; }
        write!(f, "\nvar")?;
        for v in &self.var { write!(f, " {}", v)?; }
        writeln!(f)?;
        Ok(())
    }
    fn load(path: &str) -> std::io::Result<Model> {
        let mut s = String::new();
        File::open(path)?.read_to_string(&mut s)?;
        let mut mean = [0u8; NMEL];
        let mut var = [0u8; NMEL];
        let mut gconst = 0u32;
        let mut threshold = 0u32;
        for line in s.lines() {
            let mut it = line.split_whitespace();
            match it.next() {
                Some("gconst") => gconst = it.next().unwrap().parse().unwrap(),
                Some("threshold") => threshold = it.next().unwrap().parse().unwrap(),
                Some("mean") => for (i, t) in it.enumerate() { mean[i] = t.parse().unwrap(); },
                Some("var") => for (i, t) in it.enumerate() { var[i] = t.parse().unwrap(); },
                _ => {}
            }
        }
        Ok(Model { mean, var, gconst, threshold })
    }
}

// ---------- pico sound ----------
fn write_pico_wav(path: &str) {
    // two short rising blips ("pi-co")
    let sr = 16000u32;
    let mut samples: Vec<i16> = Vec::new();
    let blip = |freq: f32, ms: usize, out: &mut Vec<i16>| {
        let n = sr as usize * ms / 1000;
        for i in 0..n {
            let t = i as f32 / sr as f32;
            let env = (std::f32::consts::PI * i as f32 / n as f32).sin(); // fade in/out
            let s = (2.0 * std::f32::consts::PI * freq * t).sin() * env * 0.5;
            out.push((s * 32767.0) as i16);
        }
    };
    blip(1200.0, 70, &mut samples);
    blip(1800.0, 90, &mut samples);
    let data_len = (samples.len() * 2) as u32;
    let mut f = BufWriter::new(File::create(path).unwrap());
    f.write_all(b"RIFF").unwrap();
    f.write_all(&(36 + data_len).to_le_bytes()).unwrap();
    f.write_all(b"WAVEfmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();      // PCM
    f.write_all(&1u16.to_le_bytes()).unwrap();      // mono
    f.write_all(&sr.to_le_bytes()).unwrap();
    f.write_all(&(sr * 2).to_le_bytes()).unwrap();  // byte rate
    f.write_all(&2u16.to_le_bytes()).unwrap();      // block align
    f.write_all(&16u16.to_le_bytes()).unwrap();     // bits
    f.write_all(b"data").unwrap();
    f.write_all(&data_len.to_le_bytes()).unwrap();
    for s in samples {
        f.write_all(&s.to_le_bytes()).unwrap();
    }
}
fn play_pico(path: &str) {
    let _ = Command::new("paplay").arg(path).status();
}

// ---------- commands ----------
fn cmd_mic() {
    let mut mic = Mic::start();
    let mut seg = Segmenter::new();
    let mut hop = [0f32; HOP];
    eprintln!("[mic] listening; speak to see segments (ctrl-c to stop)");
    while mic.read_hop(&mut hop) {
        if let Some(f) = seg.push_hop(&hop, true) {
            eprintln!("  feature: {:?}", &f[..8]);
        }
    }
}

fn cmd_enroll() {
    let gmm = Gmm::open().expect("open /dev/gmm0");
    let mut mic = Mic::start();
    let mut seg = Segmenter::new();
    let mut hop = [0f32; HOP];
    let mut samples: Vec<[u8; NMEL]> = Vec::new();
    eprintln!("[enroll] say \"session\" clearly, {ENROLL_N} times. Pause between.");
    while mic.read_hop(&mut hop) {
        if let Some(f) = seg.push_hop(&hop, false) {
            samples.push(f);
            eprintln!("  captured {}/{}", samples.len(), ENROLL_N);
            if samples.len() >= ENROLL_N {
                break;
            }
        }
    }
    drop(mic);

    // mean + variance across samples -> model
    let n = samples.len() as f32;
    let mut mean = [0u8; NMEL];
    let mut var = [0u8; NMEL];
    for i in 0..NMEL {
        let m: f32 = samples.iter().map(|s| s[i] as f32).sum::<f32>() / n;
        let v: f32 = samples.iter().map(|s| (s[i] as f32 - m).powi(2)).sum::<f32>() / n;
        mean[i] = m.round().max(0.0).min(255.0) as u8;
        // precision = inverse variance, scaled & clamped (consistent features -> high weight)
        let prec = (600.0 / (v + 2.0)).round().max(1.0).min(255.0);
        var[i] = prec as u8;
    }
    // threshold from enrollment self-scores
    let mut model = Model { mean, var, gconst: 0, threshold: u32::MAX };
    let scores: Vec<u32> = samples.iter().map(|s| gmm.score(s, &model.mean, &model.var, 0)).collect();
    let maxs = *scores.iter().max().unwrap();
    model.threshold = ((maxs as f32) * 1.8) as u32 + 1;
    eprintln!("[enroll] self-scores: {:?}", scores);
    eprintln!("[enroll] threshold set to {}", model.threshold);
    model.save(MODEL_PATH).expect("save model");
    eprintln!("[enroll] saved {MODEL_PATH}. Run: sessionwake listen");
}

fn cmd_listen() {
    let gmm = Gmm::open().expect("open /dev/gmm0");
    let model = Model::load(MODEL_PATH).expect("load model.txt (run enroll first)");
    let pico = "/tmp/sessionwake_pico.wav";
    write_pico_wav(pico);
    let mut mic = Mic::start();
    let mut seg = Segmenter::new();
    let mut hop = [0f32; HOP];
    eprintln!("[listen] say \"session\"! (threshold {})", model.threshold);
    while mic.read_hop(&mut hop) {
        if let Some(f) = seg.push_hop(&hop, false) {
            let score = gmm.score(&f, &model.mean, &model.var, model.gconst);
            let hit = score <= model.threshold;
            eprintln!("  score={} {}", score, if hit { "<= MATCH -> pico!" } else { "" });
            if hit {
                play_pico(pico);
            }
        }
    }
}

// verify the Rust -> /dev/gmm0 path against a CPU reference (no audio needed)
fn cmd_selftest() {
    let gmm = Gmm::open().expect("open /dev/gmm0");
    let mut ok = true;
    for t in 0..3u32 {
        let mut feat = [0u8; NMEL];
        let mut mean = [0u8; NMEL];
        let mut var = [0u8; NMEL];
        for i in 0..NMEL {
            feat[i] = (7 * i as u32 + 3 + t * 13) as u8;
            mean[i] = (3 * i as u32 + 11) as u8;
            var[i] = (1 + (i & 3)) as u8;
        }
        let gconst = 1000 + t * 500;
        let hw = gmm.score(&feat, &mean, &var, gconst);
        let mut exp = gconst;
        for i in 0..NMEL {
            let d = feat[i] as i32 - mean[i] as i32;
            let sq = ((d * d) as u32) & 0xffff;
            exp += sq * var[i] as u32;
        }
        let pass = hw == exp;
        ok &= pass;
        println!("selftest {t}: HW={hw} CPU={exp} -> {}", if pass { "PASS" } else { "FAIL" });
    }
    println!("{}", if ok { "ALL PASS - Rust drives the GMM chip" } else { "FAILED" });
    std::process::exit(if ok { 0 } else { 1 });
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    match arg.as_str() {
        "mic" => cmd_mic(),
        "enroll" => cmd_enroll(),
        "listen" => cmd_listen(),
        "selftest" => cmd_selftest(),
        _ => {
            eprintln!("usage: sessionwake <mic|enroll|listen|selftest>");
            std::process::exit(2);
        }
    }
}
