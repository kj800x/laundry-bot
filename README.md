# Laundry Bot

A Rust service that monitors drier circuit current via MQTT and detects drier cycle types (main dry vs fluff dry) using a state machine with timing-based classification.

## Features

- Connects to MQTT broker (no authentication required)
- Monitors current readings from a configured MQTT topic
- State machine detects drier cycle states:
  - **Unknown**: Initial state (infers state from first reading)
  - **Off**: Drier is idle (< 1A)
  - **On**: Drier is running (>= 1A)
- Cycle type detection:
  - **Main Dry**: Cycles longer than 30 seconds
  - **Fluff Dry**: Cycles between 15-30 seconds that occur within 10 minutes of a previous cycle
- Optional Discord integration for cycle notifications
- Prometheus metrics endpoint for monitoring
- Structured logging with configurable log levels

## Configuration

The service uses environment variables for configuration:

### MQTT Configuration
- `MQTT_HOST`: MQTT broker hostname (default: `localhost`)
- `MQTT_PORT`: MQTT broker port (default: `1883`)
- `MQTT_TOPIC`: MQTT topic to subscribe to (default: `EnergyMonitor/port14/current`)

### Discord Configuration (Optional)
- `DISCORD_BOT_TOKEN`: Discord bot token (enables Discord notifications)
- `DISCORD_CHANNEL_ID`: Discord channel ID where notifications will be posted

### Logging Configuration
- `RUST_LOG`: Log level filter (default: `info`)
  - Examples: `info`, `debug`, `error`, `laundry_bot=debug`
  - Serenity crate is filtered to `warn` level by default to reduce noise

### Prometheus Configuration
- `METRIC_TIMEOUT_SECS`: Metric idle timeout in seconds (default: `60`)

## Usage

```bash
# Basic usage with defaults
cargo run

# With custom MQTT configuration
MQTT_HOST=mqtt.example.com MQTT_PORT=1883 MQTT_TOPIC=EnergyMonitor/port14/current cargo run

# With Discord integration
DISCORD_BOT_TOKEN=your_token DISCORD_CHANNEL_ID=123456789 MQTT_HOST=mqtt.example.com cargo run

# With debug logging
RUST_LOG=debug cargo run

# Production build
cargo build --release
./target/release/laundry-bot
```

## State Machine Logic

The state machine transitions based on current readings:

1. **Unknown → Off/On**: Infers initial state from first reading
   - >= 1A → On (approximates cycle start time)
   - < 1A → Off
2. **Off → On**: When current exceeds 1A
   - Records cycle start time
   - If not in fluff window: Sends "Main dry started" notification immediately
   - If in fluff window: Waits to confirm cycle duration
3. **On → Off**: When current drops below 1A
   - Calculates cycle duration
   - Classifies cycle type:
     - **Main Dry**: Duration > 30 seconds → Sends "Main dry ended" notification
     - **Fluff Dry**: Duration 15-30 seconds AND within 10 minutes of last cycle → Sends "Fluff dry started" notification
     - **Unknown**: Duration < 15 seconds or not in fluff window → No notification
4. **On (while running)**: Monitors duration
   - If duration > 30 seconds and not yet notified: Sends retroactive "Main dry started" notification

### Cycle Classification Rules

- **Main Dry Threshold**: 30 seconds
  - Any cycle longer than 30 seconds is classified as a main dry cycle
- **Fluff Dry Criteria**:
  - Duration between 15-30 seconds
  - Must occur within 10 minutes of the previous cycle end
- **False Starts**: Cycles shorter than 15 seconds are ignored

## Discord Notifications

When Discord integration is enabled, the following notifications are sent:

- **Main dry cycle started**: Sent when a main dry cycle begins (immediately if not in fluff window, or retroactively if duration exceeds 30s)
- **Main dry cycle complete**: Sent when a main dry cycle ends
- **Fluff dry cycle complete**: Sent when a fluff dry cycle is detected (after cycle completes)

## Prometheus Metrics

The service exposes Prometheus metrics on port 9090 at `/metrics`:

- `drier_state`: Gauge indicating drier state (0=Unknown, 1=Off, 2=On)

Access metrics at: `http://localhost:9090/metrics`

## Logging

The service uses structured logging with the `log` crate and `env_logger`:

- Default log level: `info` (if `RUST_LOG` is not set)
- Log levels: `error`, `warn`, `info`, `debug`
- Timestamps are included in log output
- Serenity crate logs are filtered to `warn` level to reduce noise

Example log output:
```
2024-01-01 12:00:00 INFO Connecting to MQTT broker at localhost:1883
2024-01-01 12:00:01 INFO Cycle ON - Current detected: 5.23A
2024-01-01 12:00:32 INFO Cycle COMPLETED - Duration: 31.2s - Current dropped below threshold: 0.45A
2024-01-01 12:00:32 INFO Cycle classified as MainDry
```

## Building

```bash
cargo build --release
```

## Running the Release Binary

```bash
./target/release/laundry-bot
```
