// sessiongate - a chip-driven noise gate for PipeWire.
//
// The Intel Skylake GMM accelerator (via /dev/gmm0) scores each audio frame
// against two Gaussian models - "speech" and "noise" - and the one it is
// closer to decides whether the gate is open. Speech passes through; noise is
// muted. The chip does the per-frame classification (its original job); the CPU
// only does the FFT/mel front-end and the sample-level fade.
//
//   sessiongate calibrate   # record room noise, then speech -> gate.txt
//   sessiongate run         # mic -> chip gate -> speaker (hear it work)
//   sessiongate selftest    # verify the Rust -> /dev/gmm0 path (no audio)
//
// Audio I/O is PipeWire: pw-record for capture, pw-cat for playback.
// std-only Rust, no external crates.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write, BufWriter};
use std::os::unix::io::AsRawFd;
use std::process::{Command, Stdio, Child, ChildStdin};

const SR: usize = 16000;
const FRAME: usize = 256;        // 16 ms window
const HOP: usize = 128;          // 8 ms hop -> 125 frames/s
const NBINS: usize = FRAME / 2;  // 128 magnitude bins
const NMEL: usize = 24;          // feature dim == GMM vlen
const GATE_PATH: &str = "gate.txt";

// gate dynamics
const HANG_FRAMES: i32 = 20;     // keep gate open ~160 ms after last speech
const FADE_SAMPLES: f32 = 96.0;  // ~6 ms sample-level fade (anti-click)

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
    fn score(&self, feat: &[u8; NMEL], mean: &[u8; NMEL], var: &[u8; NMEL]) -> u32 {
        let mut job = GmmJob {
            vlen: NMEL as u32,
            gconst: 0,
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

// ---------- DSP ----------
struct Dsp {
    window: Vec<f32>,
    cos: Vec<Vec<f32>>,
    sin: Vec<Vec<f32>>,
    mel: Vec<Vec<f32>>,
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
// loudness-invariant quantization to u8
fn quantize(v: &[f32; NMEL]) -> [u8; NMEL] {
    let mean: f32 = v.iter().sum::<f32>() / NMEL as f32;
    let mut out = [0u8; NMEL];
    for i in 0..NMEL {
        let x = 128.0 + (v[i] - mean) * 24.0;
        out[i] = x.max(0.0).min(255.0) as u8;
    }
    out
}

// ---------- PipeWire capture / playback ----------
struct Mic {
    child: Child,
}
impl Mic {
    fn start() -> Mic {
        let child = Command::new("pw-record")
            .args(["--rate=16000", "--channels=1", "--format=s16", "--raw", "-"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("failed to start pw-record (is pipewire installed?)");
        Mic { child }
    }
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

// playback sink: pw-cat --playback reading raw s16 from stdin.
// `target` optionally pins a specific sink/virtual-source node.
struct Speaker {
    child: Child,
    stdin: ChildStdin,
}
impl Speaker {
    fn start(target: Option<&str>) -> Speaker {
        let mut cmd = Command::new("pw-cat");
        cmd.args(["--playback", "--rate=16000", "--channels=1", "--format=s16", "--raw"]);
        if let Some(t) = target {
            cmd.arg("--target").arg(t);
        }
        cmd.arg("-");
        let mut child = cmd
            .stdin(Stdio::piped())
            .spawn()
            .expect("failed to start pw-cat");
        let stdin = child.stdin.take().unwrap();
        Speaker { child, stdin }
    }
    fn write_hop(&mut self, samples: &[f32; HOP]) {
        let mut buf = [0u8; HOP * 2];
        for i in 0..HOP {
            let s = (samples[i].max(-1.0).min(1.0) * 32767.0) as i16;
            let b = s.to_le_bytes();
            buf[2 * i] = b[0];
            buf[2 * i + 1] = b[1];
        }
        let _ = self.stdin.write_all(&buf);
    }
}
impl Drop for Speaker {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

// ---------- gate model ----------
// Two Gaussian classes. Each stored as mean + precision (u8), scored on-chip.
struct GateModel {
    sp_mean: [u8; NMEL],
    sp_var: [u8; NMEL],
    nz_mean: [u8; NMEL],
    nz_var: [u8; NMEL],
    bias: i32, // open if sp_score + bias < nz_score  (>0 = favour opening)
}
impl GateModel {
    fn save(&self, path: &str) -> std::io::Result<()> {
        let mut f = BufWriter::new(File::create(path)?);
        writeln!(f, "bias {}", self.bias)?;
        let row = |f: &mut BufWriter<File>, name: &str, a: &[u8; NMEL]| -> std::io::Result<()> {
            write!(f, "{name}")?;
            for v in a { write!(f, " {}", v)?; }
            writeln!(f)
        };
        row(&mut f, "sp_mean", &self.sp_mean)?;
        row(&mut f, "sp_var", &self.sp_var)?;
        row(&mut f, "nz_mean", &self.nz_mean)?;
        row(&mut f, "nz_var", &self.nz_var)?;
        Ok(())
    }
    fn load(path: &str) -> std::io::Result<GateModel> {
        let mut s = String::new();
        File::open(path)?.read_to_string(&mut s)?;
        let mut m = GateModel {
            sp_mean: [0; NMEL], sp_var: [1; NMEL],
            nz_mean: [0; NMEL], nz_var: [1; NMEL], bias: 0,
        };
        for line in s.lines() {
            let mut it = line.split_whitespace();
            let key = it.next().unwrap_or("");
            let mut fill = |dst: &mut [u8; NMEL]| {
                for (i, t) in it.by_ref().enumerate() { if i < NMEL { dst[i] = t.parse().unwrap_or(0); } }
            };
            match key {
                "bias" => m.bias = it.next().and_then(|t| t.parse().ok()).unwrap_or(0),
                "sp_mean" => fill(&mut m.sp_mean),
                "sp_var" => fill(&mut m.sp_var),
                "nz_mean" => fill(&mut m.nz_mean),
                "nz_var" => fill(&mut m.nz_var),
                _ => {}
            }
        }
        Ok(m)
    }
}

// build mean + precision (u8) from a set of quantized frames
fn build_class(frames: &[[u8; NMEL]]) -> ([u8; NMEL], [u8; NMEL]) {
    let n = frames.len().max(1) as f32;
    let mut mean = [0u8; NMEL];
    let mut var = [0u8; NMEL];
    for i in 0..NMEL {
        let m: f32 = frames.iter().map(|s| s[i] as f32).sum::<f32>() / n;
        let v: f32 = frames.iter().map(|s| (s[i] as f32 - m).powi(2)).sum::<f32>() / n;
        mean[i] = m.round().max(0.0).min(255.0) as u8;
        let prec = (600.0 / (v + 2.0)).round().max(1.0).min(255.0);
        var[i] = prec as u8;
    }
    (mean, var)
}

// ---------- commands ----------
// record `secs` of audio, returning (quantized frames, energy per frame)
fn record_frames(mic: &mut Mic, dsp: &Dsp, secs: f32, label: &str) -> (Vec<[u8; NMEL]>, Vec<f32>) {
    let total_hops = (secs * SR as f32 / HOP as f32) as usize;
    let mut ring: Vec<f32> = Vec::with_capacity(FRAME);
    let mut hop = [0f32; HOP];
    let mut feats = Vec::new();
    let mut energies = Vec::new();
    let mut done = 0usize;
    eprintln!("[calibrate] {label}: recording {secs:.0}s ...");
    while done < total_hops && mic.read_hop(&mut hop) {
        ring.extend_from_slice(&hop);
        done += 1;
        if ring.len() < FRAME { continue; }
        let frame: Vec<f32> = ring[..FRAME].to_vec();
        ring.drain(..HOP);
        let e: f32 = frame.iter().map(|x| x * x).sum::<f32>() / FRAME as f32;
        feats.push(quantize(&dsp.feat(&frame)));
        energies.push(e);
    }
    (feats, energies)
}

fn cmd_calibrate() {
    let dsp = Dsp::new();
    let gmm = Gmm::open().expect("open /dev/gmm0");
    let mut mic = Mic::start();

    // Phase 1: room noise - stay quiet.
    eprintln!("[calibrate] STAY QUIET for the noise sample ...");
    let (nz_frames, nz_e) = record_frames(&mut mic, &dsp, 3.0, "noise");
    let noise_floor = {
        let mut v = nz_e.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2].max(1e-6) // median
    };
    let (nz_mean, nz_var) = build_class(&nz_frames);

    // Phase 2: speech - keep talking.
    eprintln!("[calibrate] now KEEP TALKING (count, read a sentence) ...");
    let (sp_all, sp_e) = record_frames(&mut mic, &dsp, 5.0, "speech");
    // keep only frames clearly above the noise floor (actual voiced frames)
    let sp_thresh = noise_floor * 8.0;
    let sp_frames: Vec<[u8; NMEL]> = sp_all.iter().zip(sp_e.iter())
        .filter(|(_, &e)| e > sp_thresh).map(|(f, _)| *f).collect();
    drop(mic);
    if sp_frames.len() < 20 {
        eprintln!("[calibrate] too little speech captured ({} voiced frames). Talk louder/longer.", sp_frames.len());
        std::process::exit(1);
    }
    let (sp_mean, sp_var) = build_class(&sp_frames);
    eprintln!("[calibrate] noise frames: {}, voiced speech frames: {}", nz_frames.len(), sp_frames.len());

    // choose a bias: look at the margin (nz_score - sp_score) on the speech
    // frames; set bias so most voiced frames open the gate with headroom.
    let model0 = GateModel { sp_mean, sp_var, nz_mean, nz_var, bias: 0 };
    let margins: Vec<i32> = sp_frames.iter().map(|f| {
        let ss = gmm.score(f, &model0.sp_mean, &model0.sp_var) as i32;
        let ns = gmm.score(f, &model0.nz_mean, &model0.nz_var) as i32;
        ns - ss // positive = correctly favours speech
    }).collect();
    let mut sorted = margins.clone();
    sorted.sort();
    let p10 = sorted[sorted.len() / 10]; // 10th percentile margin on speech
    // open if sp + bias < nz  <=>  (nz - sp) > bias ; pick just below p10 so
    // ~90% of voiced frames open, with a small floor so noise stays shut.
    let bias = (p10 - 1).max(-2000).min(20000);
    let model = GateModel { bias, ..model0 };
    model.save(GATE_PATH).expect("save gate.txt");
    eprintln!("[calibrate] speech-margin p10={p10}, bias set to {bias}");
    eprintln!("[calibrate] saved {GATE_PATH}. Run: sessiongate run");
}

fn cmd_run(target: Option<&str>) {
    let dsp = Dsp::new();
    let gmm = Gmm::open().expect("open /dev/gmm0");
    let model = GateModel::load(GATE_PATH).expect("load gate.txt (run calibrate first)");
    let mut mic = Mic::start();
    let mut spk = Speaker::start(target);

    let mut ring: Vec<f32> = Vec::with_capacity(FRAME);
    let mut hop = [0f32; HOP];
    let mut gain = 0f32;       // current output gain (smoothed)
    let mut hold: i32 = 0;     // hangover counter
    let mut open_prev = false;
    let mut frames = 0u64;
    let mut open_frames = 0u64;
    eprintln!("[run] chip-gated passthrough. Speak -> open, silence/noise -> muted. (ctrl-c to stop)");
    eprintln!("[run] bias={}", model.bias);

    while mic.read_hop(&mut hop) {
        ring.extend_from_slice(&hop);
        if ring.len() < FRAME {
            // not enough yet: pass silence
            let z = [0f32; HOP];
            spk.write_hop(&z);
            continue;
        }
        let frame: Vec<f32> = ring[..FRAME].to_vec();
        ring.drain(..HOP);

        let qf = quantize(&dsp.feat(&frame));
        let ss = gmm.score(&qf, &model.sp_mean, &model.sp_var) as i32;
        let ns = gmm.score(&qf, &model.nz_mean, &model.nz_var) as i32;
        let is_speech = ss + model.bias < ns;

        if is_speech { hold = HANG_FRAMES; } else if hold > 0 { hold -= 1; }
        let gate_open = hold > 0;
        let target_gain = if gate_open { 1.0 } else { 0.0 };

        // output the newest HOP samples with a per-sample fade toward target
        let newest = &frame[FRAME - HOP..];
        let mut out = [0f32; HOP];
        let step = 1.0 / FADE_SAMPLES;
        for i in 0..HOP {
            if gain < target_gain { gain = (gain + step).min(target_gain); }
            else if gain > target_gain { gain = (gain - step).max(target_gain); }
            out[i] = newest[i] * gain;
        }
        spk.write_hop(&out);

        frames += 1;
        if gate_open { open_frames += 1; }
        if gate_open != open_prev {
            eprintln!("  {} (sp={} nz={} margin={})",
                if gate_open { "OPEN  " } else { "close " }, ss, ns, ns - ss);
            open_prev = gate_open;
        }
        if frames % 625 == 0 {
            eprintln!("  [{}s] gate open {:.0}%", frames / 125, 100.0 * open_frames as f32 / frames as f32);
        }
    }
}

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
        let hw = gmm.score(&feat, &mean, &var);
        let mut exp = 0u32;
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
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("");
    match cmd {
        "calibrate" => cmd_calibrate(),
        "run" => {
            // optional: sessiongate run --target <node>
            let target = args.iter().position(|a| a == "--target").and_then(|i| args.get(i + 1)).map(|s| s.as_str());
            cmd_run(target);
        }
        "selftest" => cmd_selftest(),
        _ => {
            eprintln!("usage: sessiongate <calibrate|run [--target NODE]|selftest>");
            std::process::exit(2);
        }
    }
}
