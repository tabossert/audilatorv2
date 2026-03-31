use anyhow::{anyhow, Result};
use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, StreamConfig};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

mod dsp;
use dsp::{Compressor, CompressorConfig};

#[derive(Parser, Debug)]
#[command(name = "audilator", about = "TV volume auto-leveler for Raspberry Pi")]
struct Args {
    /// Windows controller IP address
    #[arg(short, long, default_value = "192.168.1.100")]
    windows_ip: String,

    /// Windows controller port
    #[arg(short, long, default_value_t = 8765)]
    port: u16,

    /// Target loudness in dBFS (calibrate first)
    #[arg(long, default_value_t = -25.0)]
    target: f32,

    /// Dead zone in dB (no adjustment within +/- this of target)
    #[arg(long, default_value_t = 4.0)]
    dead_zone: f32,

    /// Hysteresis in dB (stop adjusting below this threshold)
    #[arg(long, default_value_t = 2.0)]
    hysteresis: f32,

    /// Attack time in ms (how fast to respond to loud sounds)
    #[arg(long, default_value_t = 100.0)]
    attack: f32,

    /// Release time in ms (how slow to recover from loud sounds)
    #[arg(long, default_value_t = 2000.0)]
    release: f32,

    /// Max volume change in dB/sec
    #[arg(long, default_value_t = 30.0)]
    max_slew: f32,

    /// Silence threshold in dBFS
    #[arg(long, default_value_t = -60.0)]
    silence_threshold: f32,

    /// Seconds of silence before freezing volume
    #[arg(long, default_value_t = 5.0)]
    silence_hold: f32,

    /// RMS measurement window in ms
    #[arg(long, default_value_t = 50.0)]
    window: f32,

    /// Minimum volume (0.0-1.0)
    #[arg(long, default_value_t = 0.05)]
    vol_min: f32,

    /// Maximum volume (0.0-1.0)
    #[arg(long, default_value_t = 0.95)]
    vol_max: f32,

    /// Minimum seconds between network sends
    #[arg(long, default_value_t = 0.5)]
    min_interval: f32,

    /// Audio input device name (substring match)
    #[arg(long)]
    device: Option<String>,

    /// List available audio devices and exit
    #[arg(long)]
    list_devices: bool,

    /// Run calibration for N seconds
    #[arg(long, default_value_t = 0.0)]
    calibrate: f32,

    /// Audio sample rate
    #[arg(long, default_value_t = 48000)]
    sample_rate: u32,
}

#[derive(Serialize)]
struct VolumeRequest {
    volume: f32,
}

#[derive(Deserialize)]
struct VolumeResponse {
    volume: Option<f32>,
}

fn list_devices() -> Result<()> {
    let host = cpal::default_host();
    println!("Available input devices:");
    for device in host.input_devices()? {
        if let Ok(name) = device.name() {
            print!("  {name}");
            if let Ok(cfg) = device.default_input_config() {
                print!(
                    " ({}ch, {}Hz, {:?})",
                    cfg.channels(),
                    cfg.sample_rate().0,
                    cfg.sample_format()
                );
            }
            println!();
        }
    }
    Ok(())
}

fn find_device(name_filter: Option<&str>) -> Result<Device> {
    let host = cpal::default_host();
    match name_filter {
        Some(filter) => {
            let filter_lower = filter.to_lowercase();
            host.input_devices()?
                .find(|d| {
                    d.name()
                        .map(|n| n.to_lowercase().contains(&filter_lower))
                        .unwrap_or(false)
                })
                .ok_or_else(|| anyhow!("No input device matching '{filter}'"))
        }
        None => host
            .default_input_device()
            .ok_or_else(|| anyhow!("No default input device")),
    }
}

fn build_input_stream(
    device: &Device,
    sample_rate: u32,
    tx: mpsc::UnboundedSender<Vec<f32>>,
) -> Result<cpal::Stream> {
    let supported = device.default_input_config()?;
    let config = StreamConfig {
        channels: 1, // mono is all we need
        sample_rate: cpal::SampleRate(sample_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    let stream = match supported.sample_format() {
        SampleFormat::F32 => device.build_input_stream(
            &config,
            move |data: &[f32], _| {
                let _ = tx.send(data.to_vec());
            },
            |err| eprintln!("Audio error: {err}"),
            None,
        )?,
        SampleFormat::I16 => {
            let tx = tx;
            device.build_input_stream(
                &config,
                move |data: &[i16], _| {
                    let floats: Vec<f32> = data.iter().map(|&s| s as f32 / 32768.0).collect();
                    let _ = tx.send(floats);
                },
                |err| eprintln!("Audio error: {err}"),
                None,
            )?
        }
        fmt => return Err(anyhow!("Unsupported sample format: {fmt:?}")),
    };

    Ok(stream)
}

async fn run_calibration(args: &Args) -> Result<()> {
    let device = find_device(args.device.as_deref())?;
    println!("Device: {}", device.name()?);
    println!(
        "Calibrating for {:.0}s — play dialogue AND loud action scenes.\n",
        args.calibrate
    );

    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<f32>>();
    let stream = build_input_stream(&device, args.sample_rate, tx)?;
    stream.play()?;

    let window_samples = (args.sample_rate as f32 * args.window / 1000.0) as usize;
    let mut ring = dsp::RingBuffer::new(window_samples);
    let update_rate = 1000.0 / args.window;
    let mut envelope = dsp::EnvelopeFollower::new(args.attack, args.release, update_rate);
    let mut levels: Vec<f32> = Vec::new();
    let mut samples_since_rms: usize = 0;

    let deadline = Instant::now() + Duration::from_secs_f32(args.calibrate);

    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            Ok(Some(samples)) => {
                ring.extend(&samples);
                samples_since_rms += samples.len();

                if samples_since_rms >= window_samples && ring.is_full() {
                    samples_since_rms = 0;
                    let rms = ring.rms();
                    let dbfs = dsp::rms_to_dbfs(rms);
                    let env = envelope.update(dbfs);
                    levels.push(env);
                    eprint!(
                        "\r  Envelope: {:+6.1} dBFS  |  Raw: {:+6.1} dBFS",
                        env, dbfs
                    );
                }
            }
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    drop(stream);
    eprintln!();

    if levels.is_empty() {
        println!("No audio detected. Check microphone.");
        return Ok(());
    }

    let non_silent: Vec<f32> = levels
        .iter()
        .copied()
        .filter(|&l| l > args.silence_threshold)
        .collect();

    if non_silent.is_empty() {
        println!("All audio below silence threshold. Check mic placement.");
        return Ok(());
    }

    let mut sorted = non_silent.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let avg: f32 = non_silent.iter().sum::<f32>() / non_silent.len() as f32;
    let p10 = sorted[sorted.len() / 10];
    let p50 = sorted[sorted.len() / 2];
    let p90 = sorted[sorted.len() * 9 / 10];

    println!("\nCalibration Results:");
    println!("  Average:     {:+.1} dBFS", avg);
    println!("  10th pctile: {:+.1} dBFS  (quiet dialogue)", p10);
    println!("  50th pctile: {:+.1} dBFS  (median)", p50);
    println!("  90th pctile: {:+.1} dBFS  (loud moments)", p90);
    println!("  Dynamic range: {:.1} dB", p90 - p10);
    println!("\nSuggested --target {:.1}", p50);
    println!(
        "Suggested --dead-zone {:.1}",
        ((p90 - p10) / 6.0_f32).max(2.0)
    );

    Ok(())
}

async fn run_main_loop(args: &Args) -> Result<()> {
    let device = find_device(args.device.as_deref())?;
    println!("Device: {}", device.name()?);

    let url = format!("http://{}:{}/volume", args.windows_ip, args.port);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;

    // Fetch initial volume
    let initial_vol = match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let data: VolumeResponse = resp.json().await.unwrap_or(VolumeResponse { volume: None });
            let v = data.volume.unwrap_or(0.5);
            println!("Connected. Current volume: {v:.2}");
            v
        }
        Ok(resp) => {
            println!("Controller returned {}. Starting at 0.5", resp.status());
            0.5
        }
        Err(e) => {
            eprintln!("Cannot reach controller at {url}: {e}");
            eprintln!("Start the controller on Windows first.");
            return Err(anyhow!("Controller unreachable"));
        }
    };

    let config = CompressorConfig {
        target_dbfs: args.target,
        dead_zone_db: args.dead_zone,
        hysteresis_db: args.hysteresis,
        attack_ms: args.attack,
        release_ms: args.release,
        max_slew_db_per_sec: args.max_slew,
        silence_threshold_dbfs: args.silence_threshold,
        silence_hold_sec: args.silence_hold,
        rms_window_ms: args.window,
        sample_rate: args.sample_rate,
        vol_min: args.vol_min,
        vol_max: args.vol_max,
    };
    let mut compressor = Compressor::new(config, initial_vol);

    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<f32>>();
    let stream = build_input_stream(&device, args.sample_rate, tx)?;
    stream.play()?;

    println!(
        "Target: {:+.1} dBFS | Dead zone: +/-{:.1} dB | Attack: {:.0}ms | Release: {:.0}ms",
        args.target, args.dead_zone, args.attack, args.release
    );
    println!("Listening... Ctrl+C to stop.\n");

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc_handler(r);

    let min_interval = Duration::from_secs_f32(args.min_interval);
    let mut last_send = Instant::now() - min_interval; // allow immediate first send
    let mut last_sent_vol: Option<f32> = None;

    while running.load(Ordering::Relaxed) {
        match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            Ok(Some(samples)) => {
                if let Some(result) = compressor.process(&samples) {
                    let now = Instant::now();
                    let vol_changed = last_sent_vol
                        .map(|v| (result.volume - v).abs() > 0.005)
                        .unwrap_or(true);

                    if vol_changed && now.duration_since(last_send) >= min_interval {
                        let req = VolumeRequest {
                            volume: result.volume,
                        };
                        match client.post(&url).json(&req).send().await {
                            Ok(resp) if resp.status().is_success() => {
                                last_send = now;
                                last_sent_vol = Some(result.volume);
                            }
                            Ok(resp) => {
                                eprint!("\rController: {}", resp.status());
                            }
                            Err(_) => {} // will retry next cycle
                        }
                    }

                    let status = if result.silent {
                        "SILENT"
                    } else if result.delta_db > 0.01 {
                        "  UP  "
                    } else if result.delta_db < -0.01 {
                        " DOWN "
                    } else {
                        "  OK  "
                    };

                    let sent_marker = if last_sent_vol == Some(result.volume) {
                        "*"
                    } else {
                        " "
                    };

                    eprint!(
                        "\r[{status}] Env: {:+6.1} dBFS | \u{0394}: {:+5.2} dB | Vol: {:.3} {sent_marker}",
                        result.envelope_dbfs, result.delta_db, result.volume
                    );
                }
            }
            Ok(None) => break,
            Err(_) => continue,
        }
    }

    eprintln!("\nStopping.");
    Ok(())
}

fn ctrlc_handler(running: Arc<AtomicBool>) {
    let _ = std::thread::spawn(move || {
        // Simple signal handling without pulling in ctrlc crate
        // The tokio runtime will catch SIGINT and we check the flag
    });
    // Use tokio signal instead
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        running.store(false, Ordering::Relaxed);
    });
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if args.list_devices {
        return list_devices();
    }

    if args.calibrate > 0.0 {
        return run_calibration(&args).await;
    }

    run_main_loop(&args).await
}
