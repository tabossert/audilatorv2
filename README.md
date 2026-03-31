# Audilator v2

TV volume auto-leveler. Keeps volume consistent by dynamically adjusting it — turns down explosions, turns up quiet dialogue.

## Architecture

Two components talking over HTTP:

- **Listener** (Rust, runs on Raspberry Pi / Linux) — captures audio from a USB mic near the TV, analyzes loudness with a real-time dynamic range compressor, sends volume commands to the controller
- **Controller** (C# .NET 8, runs on Windows 11) — receives HTTP commands and sets Windows system volume via Core Audio API (NAudio)

## Quick Start

### Windows (Controller)

Requires [.NET 8 SDK](https://dotnet.microsoft.com/download/dotnet/8.0).

```bash
cd controller
dotnet run
# or: dotnet run 8765  (custom port)
```

To allow network access, run once as Administrator:
```powershell
netsh http add urlacl url=http://+:8765/ user=Everyone
netsh advfirewall firewall add rule name="Audilator" dir=in action=allow protocol=tcp localport=8765
```

To publish a standalone exe:
```bash
dotnet publish -c Release -r win-x64 --self-contained
```

### Raspberry Pi (Listener)

Requires Rust toolchain (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`) and ALSA dev headers:

```bash
sudo apt install libasound2-dev
cd audilatorv2
cargo build --release
```

First, calibrate with your mic and TV:
```bash
./target/release/audilator --calibrate 30
# Play some dialogue and some loud action scenes for 30 seconds
# It will suggest --target and --dead-zone values
```

Then run:
```bash
./target/release/audilator --windows-ip 192.168.1.100 --target -25
```

Cross-compile from Mac (optional):
```bash
rustup target add aarch64-unknown-linux-gnu
# Requires a linker for ARM Linux (e.g., via cross or zig)
cargo build --release --target aarch64-unknown-linux-gnu
```

### Options

```
--windows-ip    Windows controller IP (default: 192.168.1.100)
--port          Controller port (default: 8765)
--target        Target loudness in dBFS (default: -25.0)
--dead-zone     No-adjust zone in dB (default: 4.0)
--hysteresis    Hysteresis in dB (default: 2.0)
--attack        Attack time in ms (default: 100)
--release       Release time in ms (default: 2000)
--max-slew      Max volume change dB/sec (default: 30)
--device        Audio input device name (substring match)
--list-devices  List available audio devices
--calibrate N   Listen for N seconds and suggest settings
--sample-rate   Audio sample rate (default: 48000)
```

## How It Works

1. USB mic near TV captures audio via ALSA
2. RMS computed over 50ms windows, converted to dBFS (logarithmic, matches human hearing)
3. Attack/release envelope follower smooths the signal (100ms attack catches explosions, 2s release prevents pumping)
4. Gain computer checks if envelope is outside the dead zone around the target
5. Hysteresis prevents oscillation at dead zone boundaries
6. Silence detector freezes volume during quiet pauses (prevents runaway to max)
7. Volume changes are slew-rate limited for smooth transitions (max 30 dB/sec)
8. Absolute volume commands sent to Windows controller over HTTP
