use anyhow::{Context, Result};
use feed_rs::parser;
use matrix_sdk::{
    config::SyncSettings,
    ruma::{
        events::room::message::RoomMessageEventContent,
        OwnedRoomId, RoomId,
    },
    Client,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    env,
    path::Path,
};
use tokio::signal;
use tracing::{error, info, warn};

#[derive(Debug, Deserialize)]
struct Config {
    matrix: MatrixConfig,
    settings: Settings,
    feeds: Vec<FeedConfig>,
}

#[derive(Debug, Deserialize)]
struct MatrixConfig {
    homeserver: String,
    username: String,
    password: String,
    room_id: String,
}

#[derive(Debug, Deserialize)]
struct Settings {
    poll_interval_secs: u64,
    max_items_per_feed: usize,
    state_file: String,
}

#[derive(Debug, Deserialize, Clone)]
struct FeedConfig {
    name: String,
    url: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct State {
    /// Map of feed URL -> set of seen item IDs/links
    seen: HashMap<String, HashSet<String>>,
}

impl State {
    fn load(path: &str) -> Result<Self> {
        if Path::new(path).exists() {
            let data = std::fs::read_to_string(path)?;
            Ok(serde_json::from_str(&data)?)
        } else {
            Ok(Self::default())
        }
    }

    fn save(&self, path: &str) -> Result<()> {
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(path, data)?;
        Ok(())
    }
}

fn load_config() -> Result<Config> {
    let config_path = env::var("CONFIG_PATH").unwrap_or_else(|_| "./config.toml".to_string());
    let data = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config from {}", config_path))?;
    toml::from_str(&data).context("Failed to parse config.toml")
}

async fn fetch_feed(url: &str) -> Result<feed_rs::model::Feed> {
    let client = reqwest::Client::builder()
        .user_agent("rss-matrix-bot/0.1")
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let bytes = client.get(url).send().await?.bytes().await?;
    parser::parse(bytes.as_ref()).context("Failed to parse feed")
}

fn item_id(entry: &feed_rs::model::Entry) -> String {
    if !entry.id.is_empty() {
        return entry.id.clone();
    }
    // Fallback: use first link
    entry
        .links
        .first()
        .map(|l| l.href.clone())
        .unwrap_or_default()
}

fn item_url(entry: &feed_rs::model::Entry) -> String {
    entry
        .links
        .first()
        .map(|l| l.href.clone())
        .unwrap_or_else(|| "(no url)".to_string())
}

fn item_title(entry: &feed_rs::model::Entry) -> String {
    entry
        .title
        .as_ref()
        .map(|t| t.content.clone())
        .unwrap_or_else(|| "(no title)".to_string())
}

async fn poll_feeds(
    client: &Client,
    room_id: &OwnedRoomId,
    feeds: &[FeedConfig],
    state: &mut State,
    max_items: usize,
) -> Result<()> {
    for feed_cfg in feeds {
        info!("Polling feed: {} ({})", feed_cfg.name, feed_cfg.url);
        let feed = match fetch_feed(&feed_cfg.url).await {
            Ok(f) => f,
            Err(e) => {
                error!("Failed to fetch feed {}: {}", feed_cfg.url, e);
                continue;
            }
        };

        let seen = state.seen.entry(feed_cfg.url.clone()).or_default();
        let is_first_run = seen.is_empty();

        let mut new_items: Vec<&feed_rs::model::Entry> = feed
            .entries
            .iter()
            .filter(|e| !seen.contains(&item_id(e)))
            .collect();

        // Mark all as seen regardless
        for entry in &feed.entries {
            seen.insert(item_id(entry));
        }

        if is_first_run {
            info!(
                "First run for feed '{}': seeding {} items silently",
                feed_cfg.name,
                feed.entries.len()
            );
            continue;
        }

        if new_items.is_empty() {
            info!("No new items for feed '{}'", feed_cfg.name);
            continue;
        }

        // Limit items, newest first (feeds are typically newest-first)
        new_items.truncate(max_items);

        let room = match client.get_room(room_id) {
            Some(r) => r,
            None => {
                error!("Room {} not found", room_id);
                continue;
            }
        };

        for entry in new_items {
            let title = item_title(entry);
            let url = item_url(entry);
            let plain = format!("{}\n{}\n{}", feed_cfg.name, title, url);
            // Use text_html with a simple HTML rendering (bold for feed name)
            let html = format!("<strong>{}</strong><br/>{}<br/>{}", feed_cfg.name, title, url);
            let content = RoomMessageEventContent::text_html(plain.clone(), html);
            match room.send(content).await {
                Ok(_) => info!("Sent: {}", plain),
                Err(e) => error!("Failed to send message: {}", e),
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = load_config()?;
    info!("Loaded config with {} feeds", config.feeds.len());

    let client = Client::builder()
        .homeserver_url(&config.matrix.homeserver)
        .build()
        .await
        .context("Failed to build Matrix client")?;

    info!("Logging in as {}", config.matrix.username);
    client
        .matrix_auth()
        .login_username(&config.matrix.username, &config.matrix.password)
        .initial_device_display_name("RSS Bot")
        .send()
        .await
        .context("Failed to login to Matrix")?;
    info!("Logged in successfully");

    // Set display name
    if let Err(e) = client
        .account()
        .set_display_name(Some("RSS Bot"))
        .await
    {
        warn!("Failed to set display name: {}", e);
    }

    let room_id: OwnedRoomId = RoomId::parse(&config.matrix.room_id)
        .with_context(|| format!("Invalid room ID: {}", config.matrix.room_id))?;

    // Join the room if needed
    match client.join_room_by_id(&room_id).await {
        Ok(_) => info!("Joined room {}", room_id),
        Err(e) => warn!("Could not join room (may already be in it): {}", e),
    }

    // Do an initial sync so the client knows about rooms
    info!("Performing initial sync...");
    client.sync_once(SyncSettings::default()).await?;
    info!("Initial sync complete");

    let mut state = State::load(&config.settings.state_file)?;
    let feeds = config.feeds.clone();
    let max_items = config.settings.max_items_per_feed;
    let state_file = config.settings.state_file.clone();
    let poll_interval = std::time::Duration::from_secs(config.settings.poll_interval_secs);

    // First poll
    if let Err(e) = poll_feeds(&client, &room_id, &feeds, &mut state, max_items).await {
        error!("Poll error: {}", e);
    }
    state.save(&state_file)?;

    info!("Entering poll loop (interval: {}s)", config.settings.poll_interval_secs);

    loop {
        tokio::select! {
            _ = tokio::time::sleep(poll_interval) => {
                if let Err(e) = poll_feeds(&client, &room_id, &feeds, &mut state, max_items).await {
                    error!("Poll error: {}", e);
                }
                if let Err(e) = state.save(&state_file) {
                    error!("Failed to save state: {}", e);
                }
            }
            _ = signal::ctrl_c() => {
                info!("Received shutdown signal, exiting");
                break;
            }
        }
    }

    Ok(())
}
