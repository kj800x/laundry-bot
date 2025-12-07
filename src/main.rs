use env_logger::Env;
use log::{debug, error, info, warn};
use metrics::{describe_gauge, gauge};
use metrics_exporter_prometheus::PrometheusBuilder;
use metrics_util::MetricKindMask;
use rumqttc::{AsyncClient, Event, MqttOptions, QoS};
use serenity::builder::CreateMessage;
use serenity::http::Http;
use serenity::model::id::ChannelId;
use serenity::model::gateway::ActivityType;
use serenity::model::user::OnlineStatus;
use serenity::prelude::*;
use serenity::all::ShardManager;
use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time;

// Timing constants for cycle detection
const MAIN_DRY_THRESHOLD: Duration = Duration::from_secs(30);
const FLUFF_DRY_MIN_DURATION: Duration = Duration::from_secs(15);
const FLUFF_DRY_MAX_DURATION: Duration = Duration::from_secs(30);
const FLUFF_WINDOW: Duration = Duration::from_secs(600); // 10 minutes
const PRESENCE_TIMEOUT: Duration = Duration::from_secs(120); // 2 minutes - if no MQTT messages, appear offline

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
    NoConnection, // No MQTT connection or no updates for 2 minutes
}

struct DrierMonitor {
    state: DrierState,
    discord_http: Option<Arc<Http>>,
    discord_channel_id: Option<ChannelId>,
    discord_shard_manager: Option<Arc<ShardManager>>,
    previous_state: Option<DrierState>,
    cycle_start_time: Option<Instant>,
    last_cycle_end_time: Option<Instant>,
    last_cycle_type: Option<CycleType>,
    current_cycle_type: Option<CycleType>, // Track current cycle type for presence
    main_dry_notified: bool,
    last_mqtt_message: Arc<RwLock<Instant>>, // Track last MQTT message for timeout detection
    mqtt_connected: Arc<RwLock<bool>>, // Track MQTT connection status
}

impl DrierMonitor {
    fn new(
        discord_http: Option<Arc<Http>>,
        discord_channel_id: Option<ChannelId>,
        discord_shard_manager: Option<Arc<ShardManager>>,
        last_mqtt_message: Arc<RwLock<Instant>>,
        mqtt_connected: Arc<RwLock<bool>>,
    ) -> Self {
        Self {
            state: DrierState::Unknown,
            discord_http,
            discord_channel_id,
            discord_shard_manager,
            previous_state: None,
            cycle_start_time: None,
            last_cycle_end_time: None,
            last_cycle_type: None,
            current_cycle_type: None,
            main_dry_notified: false,
            last_mqtt_message,
            mqtt_connected,
        }
    }

    fn state_to_value(&self, state: &DrierState) -> f64 {
        match state {
            DrierState::Unknown => 0.0,
            DrierState::Off => 1.0,
            DrierState::On => 2.0,
            DrierState::NoConnection => 3.0,
        }
    }

    async fn check_mqtt_connection(&self) -> bool {
        let connected = self.mqtt_connected.read().await;
        *connected
    }

    async fn has_recent_mqtt_update(&self) -> bool {
        let last_msg = self.last_mqtt_message.read().await;
        Instant::now().duration_since(*last_msg) <= PRESENCE_TIMEOUT
    }

    async fn check_and_update_connection_state(&mut self) {
        let connected = self.check_mqtt_connection().await;
        let has_recent_update = self.has_recent_mqtt_update().await;

        if !connected || !has_recent_update {
            // Transition to NoConnection state if not already there
            if self.state != DrierState::NoConnection {
                warn!("MQTT connection lost or no updates for 2+ minutes - entering NoConnection state");
                self.previous_state = Some(self.state.clone());
                self.state = DrierState::NoConnection;
                self.update_metrics(&self.state);
                self.update_presence().await;
            }
        } else if self.state == DrierState::NoConnection {
            // Recovered from NoConnection - transition back to previous state or Unknown
            info!("MQTT connection restored - recovering from NoConnection state");
            if let Some(prev_state) = self.previous_state.take() {
                // Restore previous state
                self.state = prev_state;
            } else {
                // No previous state - go to Unknown to infer from next reading
                self.state = DrierState::Unknown;
            }
            self.update_metrics(&self.state);
            self.update_presence().await;
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

    async fn update_presence(&self) {
        if let Some(shard_manager) = &self.discord_shard_manager {
            // Determine status and activity based on state machine
            let (status, activity_name) = match &self.state {
                DrierState::Unknown => {
                    (OnlineStatus::Online, "Initializing...")
                }
                DrierState::Off => {
                    (OnlineStatus::Idle, "Idle")
                }
                DrierState::On => {
                    // Determine cycle type for activity
                    let activity = if let Some(cycle_type) = &self.current_cycle_type {
                        match cycle_type {
                            CycleType::MainDry => "Main cycle",
                            CycleType::FluffDry => "Fluff cycle",
                        }
                    } else {
                        // Still determining cycle type
                        if let Some(start_time) = self.cycle_start_time {
                            let duration = Instant::now().duration_since(start_time);
                            if duration > MAIN_DRY_THRESHOLD {
                                "Main cycle"
                            } else {
                                "Running..."
                            }
                        } else {
                            "Running..."
                        }
                    };
                    (OnlineStatus::Online, activity)
                }
                DrierState::NoConnection => {
                    // Online but panicking - no MQTT connection or updates
                    (OnlineStatus::Online, "⚠️ No MQTT connection!")
                }
            };

            // Update presence via shard manager using ShardMessenger
            let runners = shard_manager.runners.lock().await;
            for (_shard_id, runner) in runners.iter() {
                // Create activity data
                use serenity::all::ActivityData;
                let activity_data = Some(ActivityData {
                    name: activity_name.to_string(),
                    kind: ActivityType::Playing,
                    url: None,
                    state: None,
                });

                // Update presence using ShardMessenger
                runner.runner_tx.set_presence(activity_data, status);
                debug!("Updated Discord presence: {:?} - {}", status, activity_name);
            }
        }
    }

    async fn process_current(&mut self, current: f64) {
        debug!("Current: {:.2}A | State: {:?}", current, self.state);

        // Update last MQTT message time
        *self.last_mqtt_message.write().await = Instant::now();

        match self.state {
            DrierState::Unknown => {
                // Infer state from current reading after restart
                if current >= 1.0 {
                    info!("State inferred: ON - Current detected: {:.2}A", current);
                    // Handle service restart mid-cycle
                    self.cycle_start_time = Some(Instant::now()); // Approximation
                    self.main_dry_notified = false; // We don't know if we should have notified
                    self.current_cycle_type = None; // Unknown until we see duration
                    self.previous_state = Some(self.state.clone());
                    self.state = DrierState::On;
                    self.update_metrics(&self.state);
                    self.update_presence().await;
                    // Don't send immediate notification (wait to see duration)
                } else {
                    info!(
                        "State inferred: OFF - Low current detected: {:.2}A",
                        current
                    );
                    self.previous_state = Some(self.state.clone());
                    self.state = DrierState::Off;
                    self.current_cycle_type = None;
                    self.update_metrics(&self.state);
                    self.update_presence().await;
                    self.send_discord_message(self.get_idle_message(false))
                        .await;
                }
            }
            DrierState::Off => {
                if current >= 1.0 {
                    info!("Cycle ON - Current detected: {:.2}A", current);
                    // Record cycle start time
                    self.cycle_start_time = Some(Instant::now());
                    self.current_cycle_type = None; // Reset until we determine type

                    // Check if within fluff window
                    if !self.is_within_fluff_window() {
                        // Not in fluff window, likely main dry - notify immediately
                        info!("Main dry cycle started (not in fluff window)");
                        self.send_discord_message(self.get_main_dry_start_message())
                            .await;
                        self.main_dry_notified = true;
                        self.current_cycle_type = Some(CycleType::MainDry);
                    } else {
                        // Within fluff window, might be fluff dry - don't notify yet
                        debug!("Cycle started within fluff window, waiting to confirm duration");
                        self.main_dry_notified = false;
                    }

                    self.previous_state = Some(self.state.clone());
                    self.state = DrierState::On;
                    self.update_metrics(&self.state);
                    self.update_presence().await;
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
                        self.current_cycle_type = None;
                    }

                    self.previous_state = Some(self.state.clone());
                    self.state = DrierState::Off;
                    self.update_metrics(&self.state);
                    self.update_presence().await;
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
                            self.current_cycle_type = Some(CycleType::MainDry);
                            self.update_presence().await;
                        } else if duration > MAIN_DRY_THRESHOLD && self.current_cycle_type.is_none() {
                            // Update cycle type if we've determined it's a main dry
                            self.current_cycle_type = Some(CycleType::MainDry);
                            self.update_presence().await;
                        }
                    }
                }
            }
            DrierState::NoConnection => {
                // If we're processing a current reading while in NoConnection state,
                // it means connection was restored (check_and_update_connection_state was called first)
                // Process the reading based on the recovered state
                // Note: check_and_update_connection_state should have already transitioned us out of NoConnection
                // But handle it here as a safety fallback
                debug!("Received current reading while in NoConnection state - connection likely restored");
                // The connection check should have already handled the state transition,
                // but if we're still here, process as Unknown state
                if current >= 1.0 {
                    info!("State inferred: ON after NoConnection recovery - Current detected: {:.2}A", current);
                    self.cycle_start_time = Some(Instant::now());
                    self.main_dry_notified = false;
                    self.current_cycle_type = None;
                    self.previous_state = Some(self.state.clone());
                    self.state = DrierState::On;
                    self.update_metrics(&self.state);
                    self.update_presence().await;
                } else {
                    info!("State inferred: OFF after NoConnection recovery - Low current detected: {:.2}A", current);
                    self.previous_state = Some(self.state.clone());
                    self.state = DrierState::Off;
                    self.current_cycle_type = None;
                    self.update_metrics(&self.state);
                    self.update_presence().await;
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
        "The state of the drier. 0=Unknown, 1=Off, 2=On, 3=NoConnection"
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

    // Track last MQTT message for timeout detection
    // Initialize to a time in the past so we don't immediately show as connected
    let last_mqtt_message = Arc::new(RwLock::new(Instant::now() - PRESENCE_TIMEOUT - Duration::from_secs(1)));
    // Track MQTT connection status
    let mqtt_connected = Arc::new(RwLock::new(false));

    // Set up Discord Client with Gateway for presence updates
    let (discord_http, discord_shard_manager) = if let Some(token) = &discord_token {
        info!("Discord integration enabled");
        let http = Arc::new(Http::new(&token));

        // Create a minimal client for gateway connection
        let intents = serenity::model::gateway::GatewayIntents::empty();
        let mut client = Client::builder(&token, intents)
            .await
            .expect("Failed to create Discord client");

        let shard_manager = client.shard_manager.clone();

        // Start the client in a background task
        tokio::spawn(async move {
            if let Err(why) = client.start().await {
                error!("Discord client error: {:?}", why);
            }
        });

        // Give the client a moment to connect
        time::sleep(Duration::from_secs(2)).await;

        (Some(http), Some(shard_manager))
    } else {
        warn!("Discord integration disabled (DISCORD_BOT_TOKEN not set)");
        (None, None)
    };

    // Create shareable monitor
    let monitor = Arc::new(RwLock::new(DrierMonitor::new(
        discord_http.clone(),
        discord_channel_id,
        discord_shard_manager.clone(),
        last_mqtt_message.clone(),
        mqtt_connected.clone(),
    )));

    // Background task to periodically check connection state and update presence
    let monitor_for_connection_check = monitor.clone();
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(30)); // Check every 30 seconds
        loop {
            interval.tick().await;
            let mut monitor_guard = monitor_for_connection_check.write().await;
            monitor_guard.check_and_update_connection_state().await;
        }
    });

    info!("Monitoring drier circuit...");
    info!("State machine initialized in UNKNOWN state - will infer state from first reading");
    info!("Prometheus metrics available at http://0.0.0.0:9090/metrics");

    // Reconnection loop - recreate client and eventloop on connection errors
    loop {
        info!("Connecting to MQTT broker at {}:{}", mqtt_host, mqtt_port);

        // Create MQTT client
        let mut mqttoptions = MqttOptions::new("laundry-bot", mqtt_host.clone(), mqtt_port);
        mqttoptions.set_keep_alive(Duration::from_secs(60));
        mqttoptions.set_clean_session(true);

        let (client, mut eventloop) = AsyncClient::new(mqttoptions, 10);

        // Subscribe to the topic
        match client.subscribe(&mqtt_topic, QoS::AtMostOnce).await {
            Ok(_) => {
                info!("Successfully subscribed to topic: {}", mqtt_topic);
                *mqtt_connected.write().await = true;
            }
            Err(e) => {
                error!("Failed to subscribe to topic: {}", e);
                *mqtt_connected.write().await = false;
                warn!("Retrying connection in 5 seconds...");
                time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        }

        // Process MQTT events until connection error
        let mut connection_ok = true;
        while connection_ok {
            match eventloop.poll().await {
                Ok(Event::Incoming(rumqttc::Packet::Publish(publish))) => {
                    let payload = String::from_utf8_lossy(&publish.payload);

                    // Parse the current value
                    match payload.trim().parse::<f64>() {
                        Ok(current) => {
                            // Update last message time
                            *last_mqtt_message.write().await = Instant::now();
                            // Ensure connection is marked as connected
                            *mqtt_connected.write().await = true;

                            let mut monitor_guard = monitor.write().await;
                            // Check connection state before processing
                            monitor_guard.check_and_update_connection_state().await;
                            monitor_guard.process_current(current).await;
                        }
                        Err(e) => {
                            error!("Failed to parse current value '{}': {}", payload, e);
                        }
                    }
                }
                Ok(Event::Incoming(rumqttc::Packet::ConnAck(_))) => {
                    info!("MQTT connection established");
                    *mqtt_connected.write().await = true;
                    // Update last message time to prevent immediate NoConnection state
                    *last_mqtt_message.write().await = Instant::now();
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
                    connection_ok = false;
                    *mqtt_connected.write().await = false;
                    warn!("Connection lost, will attempt to reconnect in 5 seconds...");
                    time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }
}
