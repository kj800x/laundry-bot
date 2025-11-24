use env_logger::Env;
use log::{debug, error, info, warn};
use metrics::{describe_gauge, gauge};
use metrics_exporter_prometheus::PrometheusBuilder;
use metrics_util::MetricKindMask;
use rumqttc::{AsyncClient, Event, MqttOptions, QoS};
use serenity::builder::CreateMessage;
use serenity::http::Http;
use serenity::model::id::ChannelId;
use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time;

// Timing constants for cycle detection
const MAIN_DRY_THRESHOLD: Duration = Duration::from_secs(30);
const FLUFF_DRY_MIN_DURATION: Duration = Duration::from_secs(15);
const FLUFF_DRY_MAX_DURATION: Duration = Duration::from_secs(30);
const FLUFF_WINDOW: Duration = Duration::from_secs(600); // 10 minutes

#[derive(Debug, Clone, PartialEq)]
enum CycleType {
    MainDry,
    FluffDry,
}

#[derive(Debug, Clone, PartialEq)]
enum DrierState {
    Unknown,
    Off,
    On,
}

struct DrierMonitor {
    state: DrierState,
    discord_http: Option<Arc<Http>>,
    discord_channel_id: Option<ChannelId>,
    previous_state: Option<DrierState>,
    cycle_start_time: Option<Instant>,
    last_cycle_end_time: Option<Instant>,
    last_cycle_type: Option<CycleType>,
    main_dry_notified: bool,
}

impl DrierMonitor {
    fn new(discord_http: Option<Arc<Http>>, discord_channel_id: Option<ChannelId>) -> Self {
        Self {
            state: DrierState::Unknown,
            discord_http,
            discord_channel_id,
            previous_state: None,
            cycle_start_time: None,
            last_cycle_end_time: None,
            last_cycle_type: None,
            main_dry_notified: false,
        }
    }

    fn state_to_value(&self, state: &DrierState) -> f64 {
        match state {
            DrierState::Unknown => 0.0,
            DrierState::Off => 1.0,
            DrierState::On => 2.0,
        }
    }

    fn update_metrics(&self, state: &DrierState) {
        let value = self.state_to_value(state);
        gauge!("drier_state").set(value);
    }

    fn is_within_fluff_window(&self) -> bool {
        if let Some(last_end) = self.last_cycle_end_time {
            Instant::now().duration_since(last_end) <= FLUFF_WINDOW
        } else {
            false
        }
    }

    fn classify_cycle(&self, duration: Duration) -> Option<CycleType> {
        if duration > MAIN_DRY_THRESHOLD {
            Some(CycleType::MainDry)
        } else if duration >= FLUFF_DRY_MIN_DURATION
            && duration <= FLUFF_DRY_MAX_DURATION
            && self.is_within_fluff_window()
        {
            Some(CycleType::FluffDry)
        } else {
            None
        }
    }

    fn get_main_dry_start_message(&self) -> &'static str {
        "🔥 **Main dry cycle started!** Getting those clothes nice and toasty! 🌡️"
    }

    fn get_main_dry_end_message(&self) -> &'static str {
        "✅ **Main dry cycle complete!** Time to fold those clothes or start a new load! 🧺"
    }

    fn get_fluff_dry_start_message(&self) -> &'static str {
        "💨 **Fluff dry cycle complete!** Don't forget to unload! 🧺"
    }

    fn get_idle_message(&self, from_running: bool) -> &'static str {
        if from_running {
            "✅ **Drier cycle complete!** Time to fold those clothes or start a new load! 🧺"
        } else {
            "😴 **The drier is IDLE** - Time to start a new load or fold! 🧺"
        }
    }

    async fn send_discord_message(&self, message: &str) {
        if let (Some(http), Some(channel_id)) = (&self.discord_http, &self.discord_channel_id) {
            let builder = CreateMessage::new().content(message);
            if let Err(e) = http.send_message(*channel_id, Vec::new(), &builder).await {
                error!("Failed to send Discord message: {}", e);
            }
        }
    }

    async fn process_current(&mut self, current: f64) {
        debug!("Current: {:.2}A | State: {:?}", current, self.state);

        match self.state {
            DrierState::Unknown => {
                // Infer state from current reading after restart
                if current >= 1.0 {
                    info!("State inferred: ON - Current detected: {:.2}A", current);
                    // Handle service restart mid-cycle
                    self.cycle_start_time = Some(Instant::now()); // Approximation
                    self.main_dry_notified = false; // We don't know if we should have notified
                    self.previous_state = Some(self.state.clone());
                    self.state = DrierState::On;
                    self.update_metrics(&self.state);
                    // Don't send immediate notification (wait to see duration)
                } else {
                    info!(
                        "State inferred: OFF - Low current detected: {:.2}A",
                        current
                    );
                    self.previous_state = Some(self.state.clone());
                    self.state = DrierState::Off;
                    self.update_metrics(&self.state);
                    self.send_discord_message(self.get_idle_message(false))
                        .await;
                }
            }
            DrierState::Off => {
                if current >= 1.0 {
                    info!("Cycle ON - Current detected: {:.2}A", current);
                    // Record cycle start time
                    self.cycle_start_time = Some(Instant::now());

                    // Check if within fluff window
                    if !self.is_within_fluff_window() {
                        // Not in fluff window, likely main dry - notify immediately
                        info!("Main dry cycle started (not in fluff window)");
                        self.send_discord_message(self.get_main_dry_start_message())
                            .await;
                        self.main_dry_notified = true;
                    } else {
                        // Within fluff window, might be fluff dry - don't notify yet
                        debug!("Cycle started within fluff window, waiting to confirm duration");
                        self.main_dry_notified = false;
                    }

                    self.previous_state = Some(self.state.clone());
                    self.state = DrierState::On;
                    self.update_metrics(&self.state);
                }
            }
            DrierState::On => {
                if current < 1.0 {
                    // Drier turned OFF - classify cycle
                    if let Some(start_time) = self.cycle_start_time {
                        let duration = Instant::now().duration_since(start_time);

                        info!(
                            "Cycle COMPLETED - Duration: {:.1}s - Current dropped below threshold: {:.2}A",
                            duration.as_secs_f64(),
                            current
                        );

                        if let Some(cycle_type) = self.classify_cycle(duration) {
                            match cycle_type {
                                CycleType::MainDry => {
                                    // Main dry cycle ended
                                    info!("Cycle classified as MainDry");
                                    self.send_discord_message(self.get_main_dry_end_message())
                                        .await;
                                    self.last_cycle_end_time = Some(Instant::now());
                                    self.last_cycle_type = Some(CycleType::MainDry);
                                    self.main_dry_notified = false;
                                }
                                CycleType::FluffDry => {
                                    // Fluff dry cycle - send notification
                                    info!("Cycle classified as FluffDry");
                                    self.send_discord_message(self.get_fluff_dry_start_message())
                                        .await;
                                    self.last_cycle_end_time = Some(Instant::now());
                                    self.last_cycle_type = Some(CycleType::FluffDry);
                                }
                            }
                        } else {
                            debug!(
                                "Cycle duration {}s did not match any cycle type criteria",
                                duration.as_secs_f64()
                            );
                        }
                        // Reset cycle start time
                        self.cycle_start_time = None;
                    }

                    self.previous_state = Some(self.state.clone());
                    self.state = DrierState::Off;
                    self.update_metrics(&self.state);
                } else {
                    // Still ON - check if we need to send retroactive main dry notification
                    if let Some(start_time) = self.cycle_start_time {
                        let duration = Instant::now().duration_since(start_time);
                        if duration > MAIN_DRY_THRESHOLD && !self.main_dry_notified {
                            // Duration exceeded threshold, send retroactive notification
                            info!(
                                "Sending retroactive main dry start notification (duration: {:.1}s)",
                                duration.as_secs_f64()
                            );
                            self.send_discord_message(self.get_main_dry_start_message())
                                .await;
                            self.main_dry_notified = true;
                        }
                    }
                }
            }
        }
    }
}

fn metric_timeout_secs() -> u64 {
    env::var("METRIC_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logger - defaults to info level if RUST_LOG is not set
    env_logger::Builder::from_env(Env::default().default_filter_or("info"))
        .filter_module("serenity", log::LevelFilter::Warn) // Serenity crate spams at info level
        .format_timestamp_secs()
        .format_module_path(false)
        .init();

    // Set up Prometheus metrics exporter
    let builder = PrometheusBuilder::new();
    builder
        .idle_timeout(
            MetricKindMask::ALL,
            Some(Duration::from_secs(metric_timeout_secs())),
        )
        .with_http_listener(([0, 0, 0, 0], 9090))
        .install()
        .expect("Failed to install Prometheus recorder");

    describe_gauge!(
        "drier_state",
        "The state of the drier. 0=Unknown, 1=Off, 2=On"
    );

    // Read configuration from environment variables
    let mqtt_host = env::var("MQTT_HOST").unwrap_or_else(|_| "localhost".to_string());
    let mqtt_port: u16 = env::var("MQTT_PORT")
        .unwrap_or_else(|_| "1883".to_string())
        .parse()
        .map_err(|_| "Invalid MQTT_PORT value")?;
    let mqtt_topic =
        env::var("MQTT_TOPIC").unwrap_or_else(|_| "EnergyMonitor/port14/current".to_string());

    // Discord configuration (optional)
    let discord_token = env::var("DISCORD_BOT_TOKEN").ok();
    let discord_channel_id = env::var("DISCORD_CHANNEL_ID")
        .ok()
        .and_then(|id| id.parse::<u64>().ok())
        .map(ChannelId::new);

    let discord_http = if let Some(token) = &discord_token {
        info!("Discord integration enabled");
        Some(Arc::new(Http::new(&token)))
    } else {
        warn!("Discord integration disabled (DISCORD_BOT_TOKEN not set)");
        None
    };

    info!("Connecting to MQTT broker at {}:{}", mqtt_host, mqtt_port);
    info!("Subscribing to topic: {}", mqtt_topic);

    // Create MQTT client
    let mut mqttoptions = MqttOptions::new("laundry-bot", mqtt_host, mqtt_port);
    mqttoptions.set_keep_alive(Duration::from_secs(60));
    mqttoptions.set_clean_session(true);

    let (client, mut eventloop) = AsyncClient::new(mqttoptions, 10);

    // Subscribe to the topic
    client.subscribe(&mqtt_topic, QoS::AtMostOnce).await?;

    let mut monitor = DrierMonitor::new(discord_http, discord_channel_id);

    info!("Monitoring drier circuit...");
    info!("State machine initialized in UNKNOWN state - will infer state from first reading");
    info!("Prometheus metrics available at http://0.0.0.0:9090/metrics");

    // Process MQTT events
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(rumqttc::Packet::Publish(publish))) => {
                let payload = String::from_utf8_lossy(&publish.payload);

                // Parse the current value
                match payload.trim().parse::<f64>() {
                    Ok(current) => {
                        monitor.process_current(current).await;
                    }
                    Err(e) => {
                        error!("Failed to parse current value '{}': {}", payload, e);
                    }
                }
            }
            Ok(Event::Incoming(packet)) => {
                // Handle other incoming packets if needed
                debug!("Received packet: {:?}", packet);
            }
            Ok(Event::Outgoing(_)) => {
                // Handle outgoing packets if needed
            }
            Err(e) => {
                error!("MQTT error: {}", e);
                // Wait a bit before retrying
                time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}
