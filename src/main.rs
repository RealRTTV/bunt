#![feature(let_chains)]

use std::env;
use std::ops::Deref;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context as AnyhowContext, Result};
use chrono::{Datelike, DateTime, Local, Month, Utc};
use parking_lot::RwLock;
use scraper::{ElementRef, Html, Selector};
use serde_json::Value;
use serenity::all::{CreateEmbed, CreateMessage, Message};
use serenity::async_trait;
use serenity::prelude::*;

pub const ATLANTA_BRAVES_TEAM_ID: i64 = 144;
pub const NL_LEAGUE_ID: i64 = 104;
pub const NL_EAST_DIVISION_ID: i64 = 204;

pub fn get_with_sleep(url: &str) -> Result<Value> {
    loop {
        match ureq::get(url).call() {
            Ok(response) => return Ok(response.into_json::<Value>()?),
            Err(_) => std::thread::sleep(Duration::from_millis(1500))
        }
    }
}

struct Handler {
    current_game_id: RwLock<Option<usize>>,
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

    pub async fn exit_velocity(&self, ctx: Context, msg: Message) -> Result<()> {
        let typing_trigger = msg.channel_id.start_typing(&ctx.http);
        let current_braves_game = self.get_today_game().context("Could not get today's game")?;
        if let Some(hit) = current_braves_game["exit_velocity"].as_array().and_then(|ev| ev.last()) {
            let home_name = current_braves_game["scoreboard"]["teams"]["home"]["name"].as_str().context("Could not get home team name")?;
            let away_name = current_braves_game["scoreboard"]["teams"]["away"]["name"].as_str().context("Could not get away team name")?;
            let is_home = current_braves_game["scoreboard"]["teams"]["home"]["id"].as_i64().context("Could not get home team id")? == ATLANTA_BRAVES_TEAM_ID;
            let title = if is_home { format!("{home_name} vs. {away_name}") } else { format!("{home_name} @ {away_name}") };
            let full_desc = hit["des"].as_str().context("Could not get description")?;
            let ab_index = hit["ab_number"].as_u64().context("Could not get at bat number")? - 1;
            let cap_index = hit["cap_index"].as_u64().context("Could not get at bat number")?;
            let wpa = current_braves_game["scoreboard"]["stats"]["wpa"]["gameWpa"].as_array().context("Could not get WPA table")?.iter().rfind(|wpa| wpa["atBatIndex"].as_u64() == Some(ab_index) && wpa["capIndex"].as_u64() == Some(cap_index)).context("WPA must exist")?;
            let hit_speed = hit["hit_speed"].as_str().context("Could not get hit speed")?.to_owned();
            let launch_angle = hit["hit_angle"].as_str().context("Could not get hit angle")?.to_owned();
            let hit_distance = hit["hit_distance"].as_str().context("Could not get hit distance")?.parse::<usize>()?;
            let xba = hit["xba"].as_str().context("Could not get xBA")?;
            let wpa = format!("{:+}", (is_home as usize * 2 - 1) as f64 * wpa["homeTeamWinProbabilityAdded"].as_f64().context("Could not get WPA")?);

            let mut embed = CreateEmbed::new()
                .title(title)
                .description(full_desc.split_once(". ").map(|x| x.0).unwrap_or(full_desc))
                .field("Exit Velocity", hit_speed + "mph", true)
                .field("Launch Angle", launch_angle + "°", true)
                .field("Distance", hit_distance.to_string() + " ft", true)
                .field("xBA", xba, true)
                .field("WPA", wpa, true);
            if hit_distance >= 300 {
                embed = embed.field("Home Run", hit["contextMetrics"]["homeRunBallparks"].as_u64().context("Could not get home run ballpark quantity")?.to_string() + "/30", true);
            }
            typing_trigger.stop();
            msg.channel_id.send_message(&ctx.http, CreateMessage::new().embed(embed)).await?;
        }

        Ok(())
    }

    pub async fn standings(&self, ctx: Context, msg: Message) -> Result<()> {
        use std::fmt::Write;

        let typing_trigger = msg.channel_id.start_typing(&ctx.http);
        let msg_words = msg.content.split_ascii_whitespace().collect::<Vec<_>>();
        let (target_league_id, target_division_id, wild_card) = {
            let american_league = msg_words.iter().any(|word| word.eq_ignore_ascii_case("al") || word.eq_ignore_ascii_case("a") || word.eq_ignore_ascii_case("american"));
            let division = if msg_words.iter().any(|word| word.eq_ignore_ascii_case("west") || word.eq_ignore_ascii_case("w")) { 0 } else if msg_words.iter().any(|word| word.eq_ignore_ascii_case("central") || word.eq_ignore_ascii_case("c")) { 2 } else { 1 };
            let wild_card = msg_words.iter().any(|word| word.eq_ignore_ascii_case("wc") || word.eq_ignore_ascii_case("wild card") || msg.content.starts_with("~wc") || msg.content.starts_with("~wildcard"));

            (if american_league { 103 } else { 104 }, 200 + division + (!american_league) as i64 * 3, wild_card)
        };
        let standings = get_with_sleep(&format!("https://statsapi.mlb.com/api/v1/standings?leagueId={target_league_id}&hydrate=team,division"))?;
        let division = if wild_card { None } else { Some(standings["records"].as_array().context("Could not get standings")?.iter().find(|division| division["division"]["id"].as_i64() == Some(target_division_id)).context("Could not find division")?) };
        let division_name = if let Some(division) = division { division["division"]["nameShort"].as_str().context("Could not get division name")? } else { if target_league_id == 103 { "AL Wild Card" } else { "NL Wild Card" } };
        let selected_teams = if let Some(division) = division {
            division["teamRecords"].as_array().context("Could not get divisions teams")?.iter().collect::<Vec<_>>()
        } else {
            let mut wc = standings["records"].as_array().context("Could not get standings")?.iter().flat_map(|division| division["teamRecords"].as_array().unwrap().iter()).collect::<Vec<_>>();
            wc.sort_by_key(|team| team["wildCardRank"].as_str().unwrap_or("0").parse::<u8>().unwrap_or(0));
            wc
        };
        let timestamp = if let Some(division) = division { division["lastUpdated"].as_str().and_then(|timestamp| DateTime::<Utc>::from_str(timestamp).ok()).context("Could not get last updated timestamp")? } else { Utc::now() };
        let mut table = Vec::new();
        for team in selected_teams {
            // todo
            // let clinched = team["clinched"].as_bool() == Some(true);
            let division_leader = team["divisionLeader"].as_bool() == Some(true);
            let wc_rank = team["wildCardRank"].as_str().unwrap_or("0").parse::<usize>().context("Could not parse wild card rank")?;
            let club_name = team["team"]["clubName"].as_str().context("Could not get club name")?;
            let wpct = team["winningPercentage"].as_str().context("Could not get team's WPCT")?;
            let third_stat = if timestamp.month() >= Month::September.number_from_month() { team["magicNumber"].as_str().context("Could not get magic number")? } else { team[if wild_card { "wildCardGamesBack" } else { "gamesBack" }].as_str().context("Could not get games back")? };
            let fourth_stat = if timestamp.month() >= Month::September.number_from_month() { team["eliminationNumber"].as_str().context("Could not get elimination number")? } else { team["streak"]["streakCode"].as_str().context("Could not get streak")? };
            table.push((if division_leader { "D".to_owned() } else if wc_rank <= 3 { wc_rank.to_string() } else { " ".to_owned() }, format!("{club_name}"), format!("{wpct}"), third_stat, fourth_stat));
        }
        let first_stat = "Team";
        let second_stat = "WPCT";
        let third_stat = if timestamp.month() >= Month::September.number_from_month() { "M#" } else { "GB" };
        let fourth_stat = if timestamp.month() >= Month::September.number_from_month() { "E#" } else { "Streak" };
        let widths = table.iter().fold((first_stat.len(), second_stat.len(), third_stat.len(), fourth_stat.len()), |(m1, m2, m3, m4), (_, a, b, c, d)| (m1.max(a.len()), m2.max(b.len()), m3.max(c.len()), m4.max(d.len())));
        let mut description = String::new();
        writeln!(description, "```")?;
        writeln!(description, "  {first_stat: <a_width$}  {second_stat: <b_width$}  {third_stat: <c_width$}  {fourth_stat: <d_width$}", a_width = widths.0, b_width = widths.1, c_width = widths.2, d_width = widths.3)?;
        for (idx, (prefix, a, b, c, d)) in table.into_iter().enumerate() {
            writeln!(description, "{prefix} {a: <a_width$}  {b: <b_width$}  {c: <c_width$}  {d: <d_width$}", a_width = widths.0, b_width = widths.1, c_width = widths.2, d_width = widths.3)?;
            if idx == 2 && wild_card {
                writeln!(description, "{}", "-".repeat(2 + widths.0 + 2 + widths.1 + 2 + widths.2 + 2 + widths.3))?;
            }
        }
        write!(description, "```")?;
        let embed = CreateEmbed::new().title(format!("{division_name} Standings")).description(description);

        typing_trigger.stop();
        msg.channel_id.send_message(&ctx.http, CreateMessage::new().embed(embed)).await?;
        Ok(())
    }

    pub async fn savant(&self, ctx: Context, msg: Message) -> Result<()> {
        use std::fmt::Write;

        let savant_player_id = 'a: {
            if let Some(rest) = msg.content.strip_prefix("~savant ").or(msg.content.strip_prefix("~sav ")) {
                if let Some(id) = rest.parse::<usize>().ok() {
                    break 'a id
                } else {
                    let search = get_with_sleep(&format!("https://baseballsavant.mlb.com/player/search-all?search={rest}"))?;
                    if let Some(id) = search[0]["id"].as_str().and_then(|str| str.parse().ok()) {
                        break 'a id
                    }
                }
            }
            msg.channel_id.say(&ctx.http, "No player ID or name matched the given argument").await?;
            return Ok(())
        };

        let typing_trigger = msg.channel_id.start_typing(&ctx.http);

        #[allow(non_snake_case)]
        struct PercentileRankings {
            year: u16,
            name: String,
            xwOBA: Option<u16>,
            xBA: Option<u16>,
            xSLG: Option<u16>,
            AvgEV: Option<u16>,
            BatSpeed: Option<u16>,
            BarrelPct: Option<u16>,
            HardHitPct: Option<u16>,
            ChasePct: Option<u16>,
            WhiffPct: Option<u16>,
            KPct: Option<u16>,
            BBPct: Option<u16>,
            OAA: Option<u16>,
            ArmStrength: Option<u16>,
            Speed: Option<u16>,
            xERA: Option<u16>,
            FBVelo: Option<u16>,
            FBSpin: Option<u16>,
            CBSpin: Option<u16>,
            Extension: Option<u16>,
        }

        impl PercentileRankings {
            fn hitter(&self) -> bool {
                self.xwOBA.is_some()
            }

            fn fielder(&self) -> bool {
                self.OAA.is_some() | self.ArmStrength.is_some()
            }

            fn runner(&self) -> bool {
                self.Speed.is_some()
            }

            fn pitcher(&self) -> bool {
                self.xERA.is_some()
            }
        }

        fn get_percentile_rankings(savant_player_id: usize) -> Result<Option<PercentileRankings>> {
            fn get_percentile_from_element(row: &[ElementRef], ordinal: usize) -> Option<u16> {
                row.get(ordinal)?.child_elements().next()?.inner_html().parse::<u16>().ok()
            }

            let html = Html::parse_document(&ureq::get(&format!("https://baseballsavant.mlb.com/savant-player/{savant_player_id}?stats=statcast-r-hitting-mlb")).call()?.into_string()?);
            let selector = Selector::parse("table[id=percentileRankings]").unwrap();
            let Some(element) = html.select(&selector).next() else { return Ok(None) };
            let row = element.child_elements().nth(1).unwrap().child_elements().last().unwrap().child_elements().collect::<Vec<_>>();
            let name_selector = Selector::parse(r#"div[class="bio-player-name"]"#).unwrap();
            let name = html.select(&name_selector).next().unwrap().child_elements().next().unwrap().inner_html();
            let mut rankings = PercentileRankings {
                year: row[0].child_elements().next().unwrap().inner_html().parse().unwrap(),
                name,
                xwOBA: None,
                xBA: None,
                xSLG: None,
                AvgEV: None,
                BatSpeed: None,
                BarrelPct: None,
                HardHitPct: None,
                ChasePct: None,
                WhiffPct: None,
                KPct: None,
                BBPct: None,
                OAA: None,
                ArmStrength: None,
                Speed: None,
                xERA: None,
                FBVelo: None,
                FBSpin: None,
                CBSpin: None,
                Extension: None,
            };
            for (idx, child) in element.child_elements().nth(0).unwrap().child_elements().next().unwrap().child_elements().enumerate() {
                let name = child.inner_html().split_ascii_whitespace().collect::<Vec<_>>().join(" ");
                let value = get_percentile_from_element(&row, idx);
                *match &*name {
                    "Year" => continue,
                    "xwOBA" => &mut rankings.xwOBA,
                    "xBA" => &mut rankings.xBA,
                    "xSLG" => &mut rankings.xSLG,
                    "xISO" => continue,
                    "xOBP" => continue,
                    "Brl" => continue,
                    "Brl%" => &mut rankings.BarrelPct,
                    "EV" => &mut rankings.AvgEV,
                    "Max EV" => continue,
                    "Hard <br>Hit%" => &mut rankings.HardHitPct,
                    "K%" => &mut rankings.KPct,
                    "BB%" => &mut rankings.BBPct,
                    "Whiff%" => &mut rankings.WhiffPct,
                    "Chase <br>Rate" => &mut rankings.ChasePct,
                    "Speed" => &mut rankings.Speed,
                    "OAA" => &mut rankings.OAA,
                    "Arm <br> Strength" => &mut rankings.ArmStrength,
                    "Bat <br> Speed" => &mut rankings.BatSpeed,
                    "Swing <br> Length" => continue,
                    "xwOBA / <br>xERA" => &mut rankings.xERA,
                    "FB <br>Velo" => &mut rankings.FBVelo,
                    "FB <br>Spin" => &mut rankings.FBSpin,
                    "CB <br>Spin" => &mut rankings.CBSpin,
                    "Extension" => &mut rankings.Extension,
                    name => {
                        println!("Unknown percentile statistic: {name}");
                        continue
                    },
                } = value;
            }
            drop(html);
            Ok(Some(rankings))
        }

        fn format_percentile_ranking(name: &str, ranking: Option<u16>) -> String {
            const PERCENTILE_WIDTH: usize = 15;

            if let Some(percentile) = ranking {
                let percentile_surroundings = if percentile >= 95 { "***" } else if percentile >= 90 { "**" } else { "" };
                format!("\n`{percentile: >3}% / [{percentile_line: <PERCENTILE_WIDTH$}]` {percentile_surroundings}{name}{percentile_surroundings}", percentile_line = "-".repeat((percentile as usize * PERCENTILE_WIDTH + 50) / 100))
            } else {
                String::new()
            }
        }

        let Some(percentile_rankings) = get_percentile_rankings(savant_player_id)? else { return Ok(()) };
        let message = CreateMessage::new()
            .embed(CreateEmbed::new().title(format!("{} ({})", percentile_rankings.name, percentile_rankings.year)).thumbnail(format!("https://content.mlb.com/images/headshots/current/60x60/{savant_player_id}@3x.png")).description({
                let mut description = String::new();
                if percentile_rankings.hitter() {
                    write!(description, "{}", ":cricket_game: Batting".to_owned()
                        + &format_percentile_ranking("xwOBA", percentile_rankings.xwOBA)
                        + &format_percentile_ranking("xBA", percentile_rankings.xBA)
                        + &format_percentile_ranking("xSLG", percentile_rankings.xSLG)
                        + &format_percentile_ranking("Avg EV", percentile_rankings.AvgEV)
                        + &format_percentile_ranking("Bat Speed", percentile_rankings.BatSpeed)
                        + &format_percentile_ranking("Barrel %", percentile_rankings.BarrelPct)
                        + &format_percentile_ranking("Hard-Hit %", percentile_rankings.HardHitPct)
                        + &format_percentile_ranking("Chase %", percentile_rankings.ChasePct)
                        + &format_percentile_ranking("Whiff %", percentile_rankings.WhiffPct)
                        + &format_percentile_ranking("K %", percentile_rankings.KPct)
                        + &format_percentile_ranking("BB %", percentile_rankings.BBPct))?;
                }
                if percentile_rankings.fielder() {
                    write!(description, "{}", "\n:gloves: Fielding".to_owned()
                        + &format_percentile_ranking("Range (OAA)", percentile_rankings.OAA)
                        + &format_percentile_ranking("Arm Strength", percentile_rankings.ArmStrength))?
                }
                if percentile_rankings.runner() {
                    write!(description, "{}", "\n:athletic_shoe: Baserunning".to_owned()
                        + &format_percentile_ranking("Sprint Speed", percentile_rankings.Speed))?
                }
                if percentile_rankings.pitcher() {
                    write!(description, "{}", ":baseball: Pitching".to_owned()
                        + &format_percentile_ranking("xERA", percentile_rankings.xERA)
                        + &format_percentile_ranking("xBA", percentile_rankings.xBA)
                        + &format_percentile_ranking("Fastball Velo", percentile_rankings.FBVelo)
                        + &format_percentile_ranking("Avg EV", percentile_rankings.AvgEV)
                        + &format_percentile_ranking("Chase %", percentile_rankings.ChasePct)
                        + &format_percentile_ranking("Whiff %", percentile_rankings.WhiffPct)
                        + &format_percentile_ranking("K %", percentile_rankings.KPct)
                        + &format_percentile_ranking("BB %", percentile_rankings.BBPct)
                        + &format_percentile_ranking("Barrel %", percentile_rankings.BarrelPct)
                        + &format_percentile_ranking("Hard-Hit %", percentile_rankings.HardHitPct)
                        + &format_percentile_ranking("Extension", percentile_rankings.Extension))?;
                }
                description
            }));
        typing_trigger.stop();
        msg.channel_id.send_message(&ctx.http, message).await?;
        Ok(())
    }

    pub async fn help(&self, ctx: Context, msg: Message) -> Result<()> {
        msg.channel_id.send_message(&ctx.http, CreateMessage::new().embed(CreateEmbed::new()
            .title("Bunt Commands")
            .field("~ev", "Gets the statcast data from the most recent ball put in play in the active braves game.", false)
            .field("~st / ~standings", "Gets the standings in the NL East (specify AL, West/Central, and even WC) to get other stats", false)
            .field("~savant / ~sav", "Gets the baseball savant percentile rankings data of the most likely specified player", false)
        )).await?;
        Ok(())
    }

    async fn on_message(&self, ctx: Context, msg: Message) -> Result<()> {
        if msg.content.starts_with("~ev") {
            return self.exit_velocity(ctx, msg).await;
        } else if msg.content.starts_with("~st") || msg.content.starts_with("~standings") || msg.content.starts_with("~wc") || msg.content.starts_with("~wildcard") {
            return self.standings(ctx, msg).await;
        } else if msg.content.starts_with("~sav") || msg.content.starts_with("~savant") {
            return self.savant(ctx, msg).await;
        } else if msg.content == "~h" || msg.content == "~help" {
            return self.help(ctx, msg).await;
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
    let intents = GatewayIntents::GUILD_MESSAGES | GatewayIntents::MESSAGE_CONTENT | GatewayIntents::GUILD_MESSAGE_TYPING;

    let token = env::var("BUNT_DISCORD_TOKEN").expect("Expected a token to be in the environment variables");
    let mut client = Client::builder(&token, intents).event_handler(Handler { current_game_id: RwLock::new(None) }).await.expect("Error creating client");

    std::fs::write("download.html", ureq::get(&format!("https://baseballsavant.mlb.com/savant-player/660271?stats=statcast-r-hitting-mlb")).call().unwrap().into_string().unwrap()).unwrap();

    if let Err(e) = client.start().await {
        println!("Error running client: {e}");
    }
}
