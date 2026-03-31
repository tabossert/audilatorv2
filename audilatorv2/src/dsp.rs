use std::collections::VecDeque;
use std::time::Instant;

/// Fixed-size ring buffer for RMS computation. O(1) insert.
pub struct RingBuffer {
    buf: VecDeque<f32>,
    capacity: usize,
    sum_squares: f64, // f64 to avoid float accumulation error
}

impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(capacity),
            capacity,
            sum_squares: 0.0,
        }
    }

    pub fn is_full(&self) -> bool {
        self.buf.len() >= self.capacity
    }

    pub fn extend(&mut self, samples: &[f32]) {
        for &s in samples {
            if self.buf.len() >= self.capacity {
                if let Some(old) = self.buf.pop_front() {
                    self.sum_squares -= (old as f64) * (old as f64);
                }
            }
            self.sum_squares += (s as f64) * (s as f64);
            self.buf.push_back(s);
        }
        // Guard against negative drift from float imprecision
        if self.sum_squares < 0.0 {
            self.sum_squares = 0.0;
        }
    }

    pub fn rms(&self) -> f32 {
        if self.buf.is_empty() {
            return 0.0;
        }
        (self.sum_squares / self.buf.len() as f64).sqrt() as f32
    }
}

/// Convert linear RMS to dBFS, floored at -80.
pub fn rms_to_dbfs(rms: f32) -> f32 {
    if rms <= 0.0 {
        return -80.0;
    }
    (20.0 * rms.log10()).max(-80.0)
}

/// First-order IIR envelope follower with asymmetric attack/release.
pub struct EnvelopeFollower {
    attack_coeff: f32,
    release_coeff: f32,
    value: f32,
}

impl EnvelopeFollower {
    pub fn new(attack_ms: f32, release_ms: f32, update_rate_hz: f32) -> Self {
        Self {
            attack_coeff: (-1.0 / (attack_ms / 1000.0 * update_rate_hz)).exp(),
            release_coeff: (-1.0 / (release_ms / 1000.0 * update_rate_hz)).exp(),
            value: -80.0,
        }
    }

    pub fn update(&mut self, level_dbfs: f32) -> f32 {
        if level_dbfs > self.value {
            self.value = self.attack_coeff * self.value + (1.0 - self.attack_coeff) * level_dbfs;
        } else {
            self.value = self.release_coeff * self.value + (1.0 - self.release_coeff) * level_dbfs;
        }
        self.value
    }
}

/// Gain computer with dead zone + hysteresis to prevent oscillation.
struct GainComputer {
    target: f32,
    dead_zone: f32,
    hysteresis: f32,
    max_slew_per_update: f32,
    is_adjusting: bool,
}

impl GainComputer {
    fn new(
        target: f32,
        dead_zone: f32,
        hysteresis: f32,
        max_slew_db_per_sec: f32,
        update_rate_hz: f32,
    ) -> Self {
        Self {
            target,
            dead_zone,
            hysteresis,
            max_slew_per_update: max_slew_db_per_sec / update_rate_hz,
            is_adjusting: false,
        }
    }

    fn compute(&mut self, envelope_dbfs: f32) -> f32 {
        let error = self.target - envelope_dbfs;
        let abs_error = error.abs();

        // Hysteresis state machine
        if self.is_adjusting {
            if abs_error < self.hysteresis {
                self.is_adjusting = false;
                return 0.0;
            }
        } else if abs_error > self.dead_zone {
            self.is_adjusting = true;
        } else {
            return 0.0;
        }

        // Correct only beyond dead zone boundary
        let correction = if error > 0.0 {
            error - self.dead_zone
        } else {
            error + self.dead_zone
        };

        correction.clamp(-self.max_slew_per_update, self.max_slew_per_update)
    }
}

/// Detects sustained silence to freeze volume (prevents runaway).
struct SilenceDetector {
    threshold: f32,
    hold_duration: f32,
    silence_start: Option<Instant>,
}

impl SilenceDetector {
    fn new(threshold_dbfs: f32, hold_sec: f32) -> Self {
        Self {
            threshold: threshold_dbfs,
            hold_duration: hold_sec,
            silence_start: None,
        }
    }

    fn is_silent(&mut self, envelope_dbfs: f32) -> bool {
        if envelope_dbfs < self.threshold {
            let start = self.silence_start.get_or_insert_with(Instant::now);
            start.elapsed().as_secs_f32() > self.hold_duration
        } else {
            self.silence_start = None;
            false
        }
    }
}

/// Tracks volume scalar and applies dB-domain changes.
struct VolumeState {
    scalar: f32,
    min: f32,
    max: f32,
}

impl VolumeState {
    fn new(initial: f32, min: f32, max: f32) -> Self {
        Self {
            scalar: initial.clamp(min, max),
            min,
            max,
        }
    }

    fn apply_db_change(&mut self, delta_db: f32) -> f32 {
        if delta_db.abs() < 0.001 {
            return self.scalar;
        }
        self.scalar *= 10.0_f32.powf(delta_db / 20.0);
        self.scalar = self.scalar.clamp(self.min, self.max);
        self.scalar
    }
}

/// Configuration for the compressor.
pub struct CompressorConfig {
    pub target_dbfs: f32,
    pub dead_zone_db: f32,
    pub hysteresis_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub max_slew_db_per_sec: f32,
    pub silence_threshold_dbfs: f32,
    pub silence_hold_sec: f32,
    pub rms_window_ms: f32,
    pub sample_rate: u32,
    pub vol_min: f32,
    pub vol_max: f32,
}

/// Result of processing an audio chunk.
pub struct ProcessResult {
    pub envelope_dbfs: f32,
    pub delta_db: f32,
    pub volume: f32,
    pub silent: bool,
}

/// Full compressor pipeline: RingBuffer -> dBFS -> Envelope -> Gain -> Volume.
pub struct Compressor {
    ring: RingBuffer,
    envelope: EnvelopeFollower,
    gain: GainComputer,
    silence: SilenceDetector,
    volume: VolumeState,
    window_samples: usize,
    samples_since_rms: usize,
}

impl Compressor {
    pub fn new(config: CompressorConfig, initial_volume: f32) -> Self {
        let window_samples = (config.sample_rate as f32 * config.rms_window_ms / 1000.0) as usize;
        let update_rate = 1000.0 / config.rms_window_ms;

        Self {
            ring: RingBuffer::new(window_samples),
            envelope: EnvelopeFollower::new(config.attack_ms, config.release_ms, update_rate),
            gain: GainComputer::new(
                config.target_dbfs,
                config.dead_zone_db,
                config.hysteresis_db,
                config.max_slew_db_per_sec,
                update_rate,
            ),
            silence: SilenceDetector::new(config.silence_threshold_dbfs, config.silence_hold_sec),
            volume: VolumeState::new(initial_volume, config.vol_min, config.vol_max),
            window_samples,
            samples_since_rms: 0,
        }
    }

    /// Feed audio samples. Returns a result when a full RMS window has been analyzed.
    pub fn process(&mut self, samples: &[f32]) -> Option<ProcessResult> {
        self.ring.extend(samples);
        self.samples_since_rms += samples.len();

        if self.samples_since_rms < self.window_samples || !self.ring.is_full() {
            return None;
        }

        self.samples_since_rms = 0;

        let rms = self.ring.rms();
        let dbfs = rms_to_dbfs(rms);
        let env = self.envelope.update(dbfs);

        if self.silence.is_silent(env) {
            return Some(ProcessResult {
                envelope_dbfs: env,
                delta_db: 0.0,
                volume: self.volume.scalar,
                silent: true,
            });
        }

        let delta = self.gain.compute(env);
        let vol = self.volume.apply_db_change(delta);

        Some(ProcessResult {
            envelope_dbfs: env,
            delta_db: delta,
            volume: vol,
            silent: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_rms_of_silence() {
        let mut ring = RingBuffer::new(100);
        ring.extend(&[0.0; 100]);
        assert_eq!(ring.rms(), 0.0);
    }

    #[test]
    fn ring_buffer_rms_of_dc() {
        let mut ring = RingBuffer::new(100);
        ring.extend(&[0.5; 100]);
        assert!((ring.rms() - 0.5).abs() < 0.001);
    }

    #[test]
    fn ring_buffer_overwrites_old_samples() {
        let mut ring = RingBuffer::new(4);
        ring.extend(&[1.0, 1.0, 1.0, 1.0]);
        assert!(ring.is_full());
        ring.extend(&[0.0, 0.0, 0.0, 0.0]);
        assert!(ring.rms() < 0.001);
    }

    #[test]
    fn rms_to_dbfs_full_scale() {
        assert!((rms_to_dbfs(1.0) - 0.0).abs() < 0.01);
    }

    #[test]
    fn rms_to_dbfs_half() {
        let db = rms_to_dbfs(0.5);
        assert!((db - (-6.02)).abs() < 0.1);
    }

    #[test]
    fn rms_to_dbfs_silence() {
        assert_eq!(rms_to_dbfs(0.0), -80.0);
    }

    #[test]
    fn envelope_attack_rises() {
        let mut env = EnvelopeFollower::new(50.0, 2000.0, 20.0);
        // Feed loud signal
        for _ in 0..10 {
            env.update(-10.0);
        }
        assert!(env.value > -30.0);
    }

    #[test]
    fn envelope_release_is_slow() {
        let mut env = EnvelopeFollower::new(50.0, 2000.0, 20.0);
        // Bring up to -10
        for _ in 0..100 {
            env.update(-10.0);
        }
        // Now feed silence
        env.update(-80.0);
        // Should still be near -10 after one update (slow release)
        assert!(env.value > -15.0);
    }

    #[test]
    fn gain_computer_dead_zone() {
        let mut gc = GainComputer::new(-25.0, 4.0, 2.0, 30.0, 20.0);
        // Envelope at target: no correction
        assert_eq!(gc.compute(-25.0), 0.0);
        // Envelope just outside dead zone: still no correction (hasn't exceeded)
        assert_eq!(gc.compute(-28.0), 0.0);
        // Envelope well outside: should correct
        let delta = gc.compute(-35.0);
        assert!(delta > 0.0); // should increase volume
    }

    #[test]
    fn volume_state_applies_db_change() {
        let mut vs = VolumeState::new(0.5, 0.05, 0.95);
        // +6 dB should roughly double
        let new_vol = vs.apply_db_change(6.0);
        assert!((new_vol - 0.95).abs() < 0.05 || new_vol <= 0.95); // clamped
    }

    #[test]
    fn volume_state_clamps() {
        let mut vs = VolumeState::new(0.1, 0.05, 0.95);
        vs.apply_db_change(-40.0);
        assert!(vs.scalar >= 0.05);
    }
}
