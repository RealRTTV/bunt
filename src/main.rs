#![feature(let_chains)]

use std::env;
use std::ops::Deref;
use std::time::Duration;

use anyhow::{Context as AnyhowContext, Result};
use chrono::{Datelike, Local};
use parking_lot::RwLock;
use serde_json::Value;
use serenity::all::{CreateEmbed, CreateMessage, Message};
use serenity::async_trait;
use serenity::prelude::*;

pub const ATLANTA_BRAVES_TEAM_ID: i64 = 144;

pub fn get_with_sleep(url: &str) -> Result<Value> {
    loop {
        match ureq::get(url).call() {
            Ok(response) => return Ok(response.into_json::<Value>()?),
            Err(_) => std::thread::sleep(Duration::from_millis(1500))
        }
    }
}

struct Handler {
    current_game_id: RwLock<Option<usize>>
}

impl Handler {
    fn get_today_game(&self) -> Option<Value> {
        let mut current_game_id = self.current_game_id.write();

        let game_pk = if let Some(game_pk) = current_game_id.deref().clone() {
            Some(game_pk)
        } else {
            let all_games_root = get_with_sleep(&format!("https://statsapi.mlb.com/api/v1/schedule/games/?sportId=1&startDate={year}-01-01&endDate={year}-12-31&hydrate=venue(timezone)", year = Local::now().date_naive().year())).ok()?;
            let next_game = all_games_root["dates"]
                .as_array()?
                .iter()
                .flat_map(|date| date["games"].as_array().expect("Date has games").iter())
                .filter(|game| game["teams"]["home"]["team"]["id"].as_i64() == Some(ATLANTA_BRAVES_TEAM_ID) || game["teams"]["away"]["team"]["id"].as_i64() == Some(ATLANTA_BRAVES_TEAM_ID))
                .filter(|game| game["status"]["abstractGameState"].as_str() != Some("Final"))
                .map(|game| game["gamePk"].as_i64().expect("Game ID exists") as usize)
                .next();
            *current_game_id = next_game;
            next_game
        }?/*747123*/;

        let response = get_with_sleep(&format!("https://baseballsavant.mlb.com/gf?game_pk={game_pk}")).ok()?;
        if response["scoreboard"]["status"]["abstractGameState"].as_str() == Some("Final") {
            *current_game_id = None;
            self.get_today_game()
        } else {
            Some(response)
        }
    }

    async fn on_message(&self, ctx: Context, msg: Message) -> Result<()> {
        if msg.content == "~ev" {
            let typing_trigger = msg.channel_id.start_typing(&ctx.http);
            let current_braves_game = self.get_today_game().context("Could not get today's game")?;
            typing_trigger.stop();
            if let Some(hit) = current_braves_game["exit_velocity"].as_array().context("Could not get exit velocity table")?.first() {
                let home_name = current_braves_game["scoreboard"]["teams"]["home"]["name"].as_str().context("Could not get home team name")?;
                let away_name = current_braves_game["scoreboard"]["teams"]["away"]["name"].as_str().context("Could not get away team name")?;
                let title = if current_braves_game["scoreboard"]["teams"]["home"]["id"].as_i64().context("Could not get home team id")? == ATLANTA_BRAVES_TEAM_ID { format!("{home_name} vs. {away_name}") } else { format!("{home_name} @ {away_name}") };
                let full_desc = hit["des"].as_str().context("Could not get description")?;

                let mut embed = CreateEmbed::new()
                    .title(title)
                    .description(full_desc.split_once(". ").map(|x| x.0).unwrap_or(full_desc))
                    .field("Exit Velocity", hit["hit_speed"].as_str().context("Could not get hit speed")?.to_owned() + "mph", true)
                    .field("Launch Angle", hit["hit_angle"].as_str().context("Could not get hit angle")?.to_owned() + "°", true)
                    .field("Distance", hit["hit_distance"].as_str().context("Could not get hit distance")?.to_owned() + " ft", true)
                    .field("xBA", hit["xba"].as_str().context("Could not get xBA")?, true);
                if hit["contextMetrics"]["homeRunBallparks"].as_u64().is_some_and(|x| x > 0) {
                    embed = embed.field("Home Run", hit["contextMetrics"]["homeRunBallparks"].as_u64().context("Could not get home run ballpark quantity")?.to_string() + "/30", true);
                }
                msg.channel_id.send_message(&ctx.http, CreateMessage::new().embed(embed)).await?;
            }
        }

        Ok(())
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        if let Err(e) = self.on_message(ctx, msg).await {
            println!("Error sending message: {e}")
        }
    }
}

#[tokio::main]
async fn main() {
    let intents = GatewayIntents::GUILD_MESSAGES | GatewayIntents::MESSAGE_CONTENT;

    let token = env::var("BUNT_DISCORD_TOKEN").expect("Expected a token to be in the environment variables");
    let mut client = Client::builder(&token, intents).event_handler(Handler { current_game_id: RwLock::new(None) }).await.expect("Error creating client");

    if let Err(e) = client.start().await {
        println!("Error running client: {e}");
    }
}
