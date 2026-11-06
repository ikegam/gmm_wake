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
const ATTACK_FRAMES: i32 = 4;    // need ~32 ms of continuous speech to open
                                 // (rejects impulsive noise: chip crunch, clicks)
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
    // `target` pins a specific capture source (e.g. the real built-in mic) so
    // we never accidentally record our own virtual mic and loop.
    fn start(target: Option<&str>) -> Mic {
        let mut cmd = Command::new("pw-record");
        cmd.args(["--rate=16000", "--channels=1", "--format=s16", "--raw"]);
        if let Some(t) = target {
            cmd.arg("--target").arg(t);
        }
        cmd.arg("-");
        let child = cmd
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

// raw gated audio -> .wav (for offline verification without a feedback loop)
struct WavOut {
    f: BufWriter<File>,
    n: u64,
}
impl WavOut {
    fn create(path: &str) -> WavOut {
        let mut f = BufWriter::new(File::create(path).expect("create wav"));
        // placeholder header (sizes patched on finish)
        f.write_all(b"RIFF\0\0\0\0WAVEfmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap();              // PCM
        f.write_all(&1u16.to_le_bytes()).unwrap();              // mono
        f.write_all(&(SR as u32).to_le_bytes()).unwrap();
        f.write_all(&((SR * 2) as u32).to_le_bytes()).unwrap(); // byte rate
        f.write_all(&2u16.to_le_bytes()).unwrap();              // block align
        f.write_all(&16u16.to_le_bytes()).unwrap();             // bits
        f.write_all(b"data\0\0\0\0").unwrap();
        WavOut { f, n: 0 }
    }
    fn write_hop(&mut self, samples: &[f32; HOP]) {
        for i in 0..HOP {
            let s = (samples[i].max(-1.0).min(1.0) * 32767.0) as i16;
            self.f.write_all(&s.to_le_bytes()).unwrap();
        }
        self.n += HOP as u64;
    }
    // Patch the RIFF/data sizes in place and flush, so the file is a valid,
    // playable WAV at any moment - even if the process is killed by Ctrl-C
    // before finish() runs. Called periodically and at the end.
    fn checkpoint(&mut self) {
        use std::io::{Seek, SeekFrom};
        let data_len = (self.n * 2) as u32;
        let _ = self.f.seek(SeekFrom::Start(4));
        let _ = self.f.write_all(&(36 + data_len).to_le_bytes());
        let _ = self.f.seek(SeekFrom::Start(40));
        let _ = self.f.write_all(&data_len.to_le_bytes());
        let _ = self.f.seek(SeekFrom::End(0));
        let _ = self.f.flush();
    }
    fn finish(mut self) {
        self.checkpoint();
    }
}

// Ensure a PipeWire virtual mic exists. We create a PLAIN null-sink: that
// gives a sink `<name>` (which we feed with pw-cat --target <name>) and a
// monitor source `<name>.monitor` (which apps pick as their microphone).
// This never touches the speakers. NOTE: do NOT use
// media.class=Audio/Source/Virtual - that makes a source with no feedable
// sink, so pw-cat --target falls back to the real speaker and howls.
fn pactl_out(args: &[&str]) -> String {
    Command::new("pactl").args(args).output().ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

// Build a Chromium/Zoom-visible virtual microphone:
//   null-sink `<name>`  (we feed this)  ->  monitor `<name>.monitor`
//   remap-source `<name>Mic`            (a REAL source apps list as a mic)
// Chromium hides monitor sources, so the remap-source is what makes it appear.
// We snapshot and restore the default sink AND default source, so creating
// these never steals the speakers' output or the app's real-mic capture.
// Returns the real default source name to capture from (so we don't loop).
fn ensure_virtmic(name: &str) -> Option<String> {
    let mic = format!("{name}Mic");
    let mods = pactl_out(&["list", "short", "modules"]);
    let have_sink = mods.contains(&format!("sink_name={name}"));
    let have_mic = mods.contains(&format!("source_name={mic}"));

    // snapshot the real defaults BEFORE we add anything
    let prev_sink = pactl_out(&["get-default-sink"]).trim().to_string();
    let prev_source = pactl_out(&["get-default-source"]).trim().to_string();

    if !have_sink {
        let o = Command::new("pactl").args(["load-module", "module-null-sink",
            &format!("sink_name={name}"),
            &format!("sink_properties=device.description={name}"),
            "channel_map=mono"]).output();
        match o {
            Ok(o) if o.status.success() => eprintln!("[virtmic] created sink '{name}' (module {})", String::from_utf8_lossy(&o.stdout).trim()),
            Ok(o) => eprintln!("[virtmic] null-sink failed: {}", String::from_utf8_lossy(&o.stderr)),
            Err(e) => eprintln!("[virtmic] pactl error: {e}"),
        }
    } else {
        eprintln!("[virtmic] sink '{name}' already present");
    }
    if !have_mic {
        let o = Command::new("pactl").args(["load-module", "module-remap-source",
            &format!("master={name}.monitor"),
            &format!("source_name={mic}"),
            &format!("source_properties=device.description={mic}"),
            "channels=1"]).output();
        match o {
            Ok(o) if o.status.success() => eprintln!("[virtmic] created mic source '{mic}' (module {})", String::from_utf8_lossy(&o.stdout).trim()),
            Ok(o) => eprintln!("[virtmic] remap-source failed: {}", String::from_utf8_lossy(&o.stderr)),
            Err(e) => eprintln!("[virtmic] pactl error: {e}"),
        }
    } else {
        eprintln!("[virtmic] mic '{mic}' already present");
    }

    // restore defaults so YouTube keeps playing on the speakers and nothing
    // else switches its mic to our virtual device
    if !prev_sink.is_empty() && prev_sink != name {
        let _ = Command::new("pactl").args(["set-default-sink", &prev_sink]).status();
        eprintln!("[virtmic] kept default output = {prev_sink}");
    }
    if !prev_source.is_empty() && prev_source != mic {
        let _ = Command::new("pactl").args(["set-default-source", &prev_source]).status();
    }
    eprintln!("[virtmic] >>> in Chromium/Zoom, pick the microphone named '{mic}' <<<");

    // capture from the REAL mic explicitly (never the virtual one)
    if prev_source.is_empty() || prev_source == mic { None } else { Some(prev_source) }
}

// Confirm OUR playback stream is attached to the virtual sink and not the
// speaker, so we can never create a feedback loop. We identify our own stream
// by its unique 16000 Hz / 1-channel format (other apps run at 48000/2ch), and
// ignore every unrelated playback on the machine.
fn verify_routing(name: &str) -> bool {
    // index of our virtual sink, by name
    let mut sink_idx = String::new();
    if let Ok(sk) = Command::new("pactl").args(["list", "short", "sinks"]).output() {
        for line in String::from_utf8_lossy(&sk.stdout).lines() {
            let mut it = line.split_whitespace();
            let idx = it.next().unwrap_or("");
            let nm = it.next().unwrap_or("");
            if nm == name { sink_idx = idx.to_string(); }
        }
    }
    if sink_idx.is_empty() {
        eprintln!("[virtmic] sink '{name}' not found");
        return false;
    }
    // our stream = the one at 16000 Hz
    if let Ok(o) = Command::new("pactl").args(["list", "short", "sink-inputs"]).output() {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            if !line.contains("16000Hz") { continue; }
            let mut it = line.split_whitespace();
            let _i = it.next();
            let tgt = it.next().unwrap_or("");
            if tgt == sink_idx { return true; }
            eprintln!("[virtmic] WARNING: our stream is on sink {tgt}, not '{name}' (idx {sink_idx}).");
            return false;
        }
    }
    eprintln!("[virtmic] our playback stream not visible yet");
    false
}

enum Output {
    VirtMic(String), // feed a virtual source (default): no feedback loop
    Monitor,         // play to default sink (headphones only!)
    Target(String),  // feed a specific node by name
    Wav(String),     // write gated audio to a .wav
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
    let mut mic = Mic::start(None);

    // Phase 1: NON-speech. Capture real interfering sounds (typing, clapping,
    // eating, fan) plus some silence - everything that must NOT open the gate.
    // The noise Gaussian learns them, so later they score as noise, not speech.
    eprintln!("[calibrate] NON-SPEECH sample: DON'T talk. Make your usual noises -");
    eprintln!("[calibrate]   type, clap, eat chips, rustle, let the fan run, then go quiet.");
    let (nz_frames, nz_e) = record_frames(&mut mic, &dsp, 6.0, "noise");
    let noise_floor = {
        let mut v = nz_e.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 5].max(1e-6) // 20th pct: the quiet level, robust to noise bursts
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

fn cmd_run(out: Output) {
    let dsp = Dsp::new();
    let gmm = Gmm::open().expect("open /dev/gmm0");
    let model = GateModel::load(GATE_PATH).expect("load gate.txt (run calibrate first)");

    // set up the chosen output sink
    let mut spk: Option<Speaker> = None;
    let mut wav: Option<WavOut> = None;
    let mut mic_target: Option<String> = None; // explicit real-mic source
    match &out {
        Output::VirtMic(name) => {
            mic_target = ensure_virtmic(name);
            eprintln!("[run] feeding virtual mic '{name}' - select '{name}Mic' as your microphone.");
            spk = Some(Speaker::start(Some(name)));
        }
        Output::Target(name) => {
            eprintln!("[run] feeding node '{name}'");
            spk = Some(Speaker::start(Some(name)));
        }
        Output::Monitor => {
            eprintln!("[run] MONITOR mode -> default speaker. USE HEADPHONES or it will howl!");
            spk = Some(Speaker::start(None));
        }
        Output::Wav(path) => {
            eprintln!("[run] writing gated audio to {path} (ctrl-c to stop, then play it back)");
            wav = Some(WavOut::create(path));
        }
    }
    let mut mic = Mic::start(mic_target.as_deref());

    // Safety: before feeding real audio to a virtual mic, prime with silence
    // and confirm our playback is on the virtual sink, not the speaker. If it
    // isn't, abort - only silence will have been emitted, so no howl.
    if let Output::VirtMic(name) = &out {
        let z = [0f32; HOP];
        let mut ok = false;
        for _ in 0..6 {
            if let Some(s) = spk.as_mut() { for _ in 0..8 { s.write_hop(&z); } }
            std::thread::sleep(std::time::Duration::from_millis(250));
            if verify_routing(name) { ok = true; break; }
        }
        if !ok {
            eprintln!("[run] ABORT: output is not on virtual mic '{name}'. Refusing to risk a speaker feedback loop.");
            eprintln!("[run] (only silence was emitted.) Check `pactl list short sinks` for '{name}'.");
            return;
        }
        eprintln!("[run] routing verified: our stream feeds '{name}' only (no speaker). Safe.");
    }

    let mut ring: Vec<f32> = Vec::with_capacity(FRAME);
    let mut hop = [0f32; HOP];
    let mut gain = 0f32;       // current output gain (smoothed)
    let mut hold: i32 = 0;     // hangover counter
    let mut speech_run: i32 = 0; // consecutive speech-classified frames (attack)
    let mut open_prev = false;
    let mut frames = 0u64;
    let mut open_frames = 0u64;
    eprintln!("[run] chip-gated passthrough. Speak -> open, silence/noise -> muted. (ctrl-c to stop)");
    eprintln!("[run] bias={}", model.bias);

    macro_rules! emit {
        ($buf:expr) => {{
            if let Some(s) = spk.as_mut() { s.write_hop($buf); }
            if let Some(w) = wav.as_mut() { w.write_hop($buf); }
        }};
    }

    while mic.read_hop(&mut hop) {
        ring.extend_from_slice(&hop);
        if ring.len() < FRAME {
            // not enough yet: pass silence
            let z = [0f32; HOP];
            emit!(&z);
            continue;
        }
        let frame: Vec<f32> = ring[..FRAME].to_vec();
        ring.drain(..HOP);

        let qf = quantize(&dsp.feat(&frame));
        let ss = gmm.score(&qf, &model.sp_mean, &model.sp_var) as i32;
        let ns = gmm.score(&qf, &model.nz_mean, &model.nz_var) as i32;
        let is_speech = ss + model.bias < ns;

        // require ATTACK_FRAMES of continuous speech to (re)open: a lone
        // transient (clap, chip crunch, click) never sustains that long.
        if is_speech { speech_run += 1; } else { speech_run = 0; }
        if speech_run >= ATTACK_FRAMES { hold = HANG_FRAMES; } else if hold > 0 { hold -= 1; }
        let gate_open = hold > 0;
        let target_gain = if gate_open { 1.0 } else { 0.0 };

        // output the newest HOP samples with a per-sample fade toward target
        let newest = &frame[FRAME - HOP..];
        let mut og = [0f32; HOP];
        let step = 1.0 / FADE_SAMPLES;
        for i in 0..HOP {
            if gain < target_gain { gain = (gain + step).min(target_gain); }
            else if gain > target_gain { gain = (gain - step).max(target_gain); }
            og[i] = newest[i] * gain;
        }
        emit!(&og);

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
        // keep the wav header valid every ~0.5s so Ctrl-C never leaves it empty
        if frames % 62 == 0 {
            if let Some(w) = wav.as_mut() { w.checkpoint(); }
        }
    }
    if let Some(w) = wav { w.finish(); }
}

// Diagnostic: record the mic straight to a WAV with NO chip and NO gate, so we
// can tell a capture-format problem from a gating problem.
fn cmd_rec(path: &str, secs: f32) {
    let mut mic = Mic::start(None);
    let mut wav = WavOut::create(path);
    let mut hop = [0f32; HOP];
    let total = (secs * SR as f32 / HOP as f32) as u64;
    let mut n = 0u64;
    let mut peak = 0f32;
    eprintln!("[rec] passthrough capture -> {path} for {secs:.0}s (talk now)");
    while n < total && mic.read_hop(&mut hop) {
        for s in hop.iter() { peak = peak.max(s.abs()); }
        wav.write_hop(&hop);
        n += 1;
        if n % 62 == 0 { wav.checkpoint(); }
    }
    wav.finish();
    eprintln!("[rec] wrote {} frames (~{:.1}s), peak level {:.3} (want ~0.1-0.9)",
        n, n as f32 * HOP as f32 / SR as f32, peak);
    eprintln!("[rec] play it: mplayer {path}  (should sound like a clean normal recording)");
}

// read a 16k mono s16 WAV into f32 samples (parses the data chunk)
fn read_wav(path: &str) -> Vec<f32> {
    let mut bytes = Vec::new();
    File::open(path).expect("open wav").read_to_end(&mut bytes).unwrap();
    // find "data" chunk
    let mut i = 12;
    let mut data = &bytes[..0];
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let sz = u32::from_le_bytes([bytes[i+4], bytes[i+5], bytes[i+6], bytes[i+7]]) as usize;
        if id == b"data" {
            let end = (i + 8 + sz).min(bytes.len());
            data = &bytes[i + 8..end];
            break;
        }
        i += 8 + sz + (sz & 1);
    }
    let mut out = Vec::with_capacity(data.len() / 2);
    for c in data.chunks_exact(2) {
        out.push(i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0);
    }
    out
}

// Offline: apply the gate model to in.wav, write gated out.wav, print stats.
// Lets us iterate bias on one fixed recording without re-recording.
fn cmd_gatefile(inp: &str, outp: &str) {
    let dsp = Dsp::new();
    let gmm = Gmm::open().expect("open /dev/gmm0");
    let model = GateModel::load(GATE_PATH).expect("load gate.txt (run calibrate first)");
    let samples = read_wav(inp);
    if samples.is_empty() { eprintln!("[gatefile] no samples in {inp}"); std::process::exit(1); }

    let mut wav = WavOut::create(outp);
    let mut gain = 0f32;
    let mut hold: i32 = 0;
    let mut speech_run: i32 = 0;
    let (mut frames, mut open_frames) = (0u64, 0u64);
    let step = 1.0 / FADE_SAMPLES;
    let mut pos = 0usize;
    while pos + FRAME <= samples.len() {
        let frame = &samples[pos..pos + FRAME];
        let qf = quantize(&dsp.feat(frame));
        let ss = gmm.score(&qf, &model.sp_mean, &model.sp_var) as i32;
        let ns = gmm.score(&qf, &model.nz_mean, &model.nz_var) as i32;
        let is_speech = ss + model.bias < ns;
        if is_speech { speech_run += 1; } else { speech_run = 0; }
        if speech_run >= ATTACK_FRAMES { hold = HANG_FRAMES; } else if hold > 0 { hold -= 1; }
        let gate_open = hold > 0;
        let target_gain = if gate_open { 1.0 } else { 0.0 };
        let newest = &frame[FRAME - HOP..];
        let mut og = [0f32; HOP];
        for i in 0..HOP {
            if gain < target_gain { gain = (gain + step).min(target_gain); }
            else if gain > target_gain { gain = (gain - step).max(target_gain); }
            og[i] = newest[i] * gain;
        }
        wav.write_hop(&og);
        frames += 1;
        if gate_open { open_frames += 1; }
        pos += HOP;
    }
    wav.finish();
    eprintln!("[gatefile] {} frames, gate open {:.0}%  (bias={})",
        frames, 100.0 * open_frames as f32 / frames.max(1) as f32, model.bias);
    eprintln!("[gatefile] wrote {outp} - compare: aplay {inp} ; aplay {outp}");
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
            let get = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
            let out = if args.iter().any(|a| a == "--monitor") {
                Output::Monitor
            } else if let Some(p) = get("--out") {
                Output::Wav(p)
            } else if let Some(t) = get("--target") {
                Output::Target(t)
            } else {
                Output::VirtMic("SessionGate".to_string())
            };
            cmd_run(out);
        }
        "rec" => {
            let path = args.get(2).map(|s| s.as_str()).unwrap_or("/tmp/rec.wav");
            cmd_rec(path, 6.0);
        }
        "gatefile" => {
            let inp = args.get(2).map(|s| s.as_str()).unwrap_or("/tmp/truth.wav");
            let outp = args.get(3).map(|s| s.as_str()).unwrap_or("/tmp/gated.wav");
            cmd_gatefile(inp, outp);
        }
        "unload" => {
            // remove the virtual mic (remap-source first, then its null-sink)
            let _ = Command::new("pactl").args(["unload-module", "module-remap-source"]).status();
            let _ = Command::new("pactl").args(["unload-module", "module-null-sink"]).status();
            eprintln!("[unload] removed SessionGate virtual mic (remap-source + null-sink)");
        }
        "selftest" => cmd_selftest(),
        _ => {
            eprintln!("usage: sessiongate <calibrate | run [--monitor|--out FILE.wav|--target NODE] | unload | selftest>");
            eprintln!("  run with no flag  -> feeds virtual mic 'SessionGate' (no feedback loop)");
            std::process::exit(2);
        }
    }
}
