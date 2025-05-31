use anyhow::{anyhow, Result};
use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, StreamConfig};
use serde::{Deserialize, Serialize};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Windows machine IP address
    #[arg(short, long, default_value = "192.168.1.100")]
    windows_ip: String,
    
    /// Port for Windows volume server
    #[arg(short, long, default_value = "8080")]
    port: u16,
    
    /// Quiet threshold (0.0 to 1.0)
    #[arg(long, default_value = "0.1")]
    quiet_threshold: f32,
    
    /// Loud threshold (0.0 to 1.0)
    #[arg(long, default_value = "0.7")]
    loud_threshold: f32,
}

#[derive(Serialize, Deserialize)]
struct VolumeRequest {
    level: f32,
}

struct VolumeController {
    current_level: f32,
    target_level: f32,
    smoothing_factor: f32,
    quiet_threshold: f32,
    loud_threshold: f32,
    last_adjustment: Instant,
    adjustment_cooldown: Duration,
}

impl VolumeController {
    fn new(quiet_threshold: f32, loud_threshold: f32) -> Self {
        Self {
            current_level: 0.5, // Start at 50% volume
            target_level: 0.5,
            smoothing_factor: 0.1, // Gentle smoothing
            quiet_threshold,
            loud_threshold,
            last_adjustment: Instant::now(),
            adjustment_cooldown: Duration::from_millis(500), // Minimum time between adjustments
        }
    }
    
    fn update(&mut self, audio_level: f32) -> Option<f32> {
        // Only adjust if cooldown period has passed
        if self.last_adjustment.elapsed() < self.adjustment_cooldown {
            return None;
        }
        
        let old_target = self.target_level;
        
        // Determine target volume based on audio level
        if audio_level < self.quiet_threshold {
            // Gradually increase volume for quiet content
            self.target_level = (self.current_level + 0.05).min(0.9);
        } else if audio_level > self.loud_threshold {
            // More aggressively decrease volume for loud content
            self.target_level = (self.current_level - 0.15).max(0.2);
        } else {
            // For normal levels, slowly return to baseline
            let baseline = 0.5;
            if self.current_level != baseline {
                self.target_level = if self.current_level > baseline {
                    (self.current_level - 0.02).max(baseline)
                } else {
                    (self.current_level + 0.02).min(baseline)
                };
            }
        }
        
        // Apply smoothing
        self.current_level += (self.target_level - self.current_level) * self.smoothing_factor;
        
        // Only return a new level if there's a significant change
        if (self.target_level - old_target).abs() > 0.01 {
            self.last_adjustment = Instant::now();
            Some(self.current_level.clamp(0.1, 0.9))
        } else {
            None
        }
    }
}

struct AudioAnalyzer {
    samples: Vec<f32>,
    sample_count: usize,
    rms_window_size: usize,
}

impl AudioAnalyzer {
    fn new(window_size: usize) -> Self {
        Self {
            samples: Vec::with_capacity(window_size),
            sample_count: 0,
            rms_window_size: window_size,
        }
    }
    
    fn add_samples(&mut self, new_samples: &[f32]) -> Option<f32> {
        for &sample in new_samples {
            self.samples.push(sample);
            self.sample_count += 1;
            
            if self.samples.len() > self.rms_window_size {
                self.samples.remove(0);
            }
        }
        
        // Calculate RMS every 1024 samples (roughly 20ms at 48kHz)
        if self.sample_count % 1024 == 0 && self.samples.len() >= self.rms_window_size {
            Some(self.calculate_rms())
        } else {
            None
        }
    }
    
    fn calculate_rms(&self) -> f32 {
        if self.samples.is_empty() {
            return 0.0;
        }
        
        let sum_squares: f32 = self.samples.iter().map(|&x| x * x).sum();
        (sum_squares / self.samples.len() as f32).sqrt()
    }
}

fn list_audio_devices() -> Result<()> {
    let host = cpal::default_host();
    println!("Available audio devices:");
    
    for device in host.input_devices()? {
        let name = device.name()?;
        println!("  Input: {}", name);
        
        if let Ok(config) = device.default_input_config() {
            println!("    Default config: {:?}", config);
        }
    }
    
    Ok(())
}

fn setup_audio_stream(device: &Device) -> Result<(cpal::Stream, mpsc::Receiver<Vec<f32>>)> {
    let config = device.default_input_config()?;
    println!("Using audio config: {:?}", config);
    
    let (tx, rx) = mpsc::channel();
    
    let stream = match config.sample_format() {
        SampleFormat::F32 => build_stream::<f32>(&device, &config.into(), tx)?,
        SampleFormat::I16 => build_stream::<i16>(&device, &config.into(), tx)?,
        SampleFormat::U16 => build_stream::<u16>(&device, &config.into(), tx)?,
        _ => return Err(anyhow!("Unsupported sample format: {:?}", config.sample_format())),
    };
    
    Ok((stream, rx))
}

fn build_stream<T>(
    device: &Device,
    config: &StreamConfig,
    tx: mpsc::Sender<Vec<f32>>,
) -> Result<cpal::Stream>
where
    T: cpal::Sample + cpal::SizedSample + Into<f32>,
{
    let channels = config.channels as usize;
    
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            // Convert samples to f32 and take only the first channel for mono analysis
            let samples: Vec<f32> = data
                .chunks(channels)
                .map(|frame| frame[0].into()) // Take first channel only
                .collect();
            
            if tx.send(samples).is_err() {
                eprintln!("Failed to send audio data");
            }
        },
        |err| eprintln!("Audio stream error: {}", err),
        None,
    )?;
    
    Ok(stream)
}

async fn send_volume_request(url: &str, level: f32) -> Result<()> {
    let client = reqwest::Client::new();
    let request = VolumeRequest { level };
    
    let response = client
        .post(url)
        .json(&request)
        .timeout(Duration::from_secs(2))
        .send()
        .await?;
    
    if response.status().is_success() {
        println!("Volume set to {:.2}", level);
    } else {
        eprintln!("Failed to set volume: {}", response.status());
    }
    
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    
    println!("TV Volume Controller Starting...");
    println!("Target Windows machine: {}:{}", args.windows_ip, args.port);
    
    // List available audio devices
    list_audio_devices()?;
    
    // Get the default input device (your USB microphone)
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("No input device available"))?;
    
    println!("Using device: {}", device.name()?);
    
    // Set up audio stream
    let (stream, audio_rx) = setup_audio_stream(&device)?;
    stream.play()?;
    
    // Initialize components
    let mut analyzer = AudioAnalyzer::new(2048); // ~40ms window at 48kHz
    let mut controller = VolumeController::new(args.quiet_threshold, args.loud_threshold);
    let volume_url = format!("http://{}:{}/volume", args.windows_ip, args.port);
    
    println!("Listening for audio... Press Ctrl+C to stop");
    
    // Main processing loop
    loop {
        // Process audio samples
        if let Ok(samples) = audio_rx.try_recv() {
            if let Some(rms_level) = analyzer.add_samples(&samples) {
                // Print current audio level for debugging
                let db = 20.0 * rms_level.log10();
                print!("\rAudio level: {:.3} RMS ({:.1} dB) | Volume: {:.2}", 
                       rms_level, db, controller.current_level);
                
                // Update volume controller
                if let Some(new_volume) = controller.update(rms_level) {
                    println!("\nAdjusting volume to {:.2}", new_volume);
                    
                    // Send volume adjustment request
                    if let Err(e) = send_volume_request(&volume_url, new_volume).await {
                        eprintln!("Failed to send volume request: {}", e);
                    }
                }
            }
        }
        
        // Small delay to prevent busy waiting
        sleep(Duration::from_millis(10)).await;
    }
}
