use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use reqwest::blocking::{Client, ClientBuilder};
use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::StatusCode;
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const API_BASE: &str = "https://api.brain.fm";
const TOKEN_ENV: &str = "BRAINFM_TOKEN";
const TOKEN_FILE: &str = ".local/share/brainfm-cli/token";
const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:151.0) Gecko/20100101 Firefox/151.0";

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, ValueEnum)]
pub enum NeuralEffect {
    Low,
    Medium,
    High,
}

impl NeuralEffect {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct MentalState {
    pub id: String,
    #[serde(rename = "displayValue")]
    pub display_value: Option<String>,
    #[serde(rename = "defaultActivity")]
    pub default_activity: Option<Activity>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Activity {
    pub id: String,
    #[serde(rename = "displayValue")]
    pub display_value: Option<String>,
}

// ── JWT ───────────────────────────────────────────────────────────────────────

fn user_id_from_jwt(token: &str) -> Result<String> {
    let parts: Vec<&str> = token.splitn(3, '.').collect();
    if parts.len() < 2 {
        bail!("not a JWT");
    }
    let payload = parts[1];
    let padded = match payload.len() % 4 {
        2 => format!("{payload}=="),
        3 => format!("{payload}="),
        _ => payload.to_owned(),
    };
    let decoded = base64_url_decode(&padded)?;
    let json: serde_json::Value = serde_json::from_slice(&decoded)?;
    json["_id"]
        .as_str()
        .map(|s| s.to_owned())
        .ok_or_else(|| anyhow!("no _id in JWT"))
}

fn base64_url_decode(s: &str) -> Result<Vec<u8>> {
    let std_b64: String = s
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        })
        .collect();
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut table = [0u8; 256];
    for (i, &c) in alphabet.iter().enumerate() {
        table[c as usize] = i as u8;
    }
    let bytes = std_b64.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == b'=' {
            break;
        }
        let n = (table[bytes[i] as usize] as u32) << 18
            | (table[bytes[i + 1] as usize] as u32) << 12
            | (table[bytes[i + 2] as usize] as u32) << 6
            | (table[bytes[i + 3] as usize] as u32);
        out.push(((n >> 16) & 0xff) as u8);
        if bytes[i + 2] != b'=' {
            out.push(((n >> 8) & 0xff) as u8);
        }
        if bytes[i + 3] != b'=' {
            out.push((n & 0xff) as u8);
        }
        i += 4;
    }
    Ok(out)
}

// ── HTTP client ───────────────────────────────────────────────────────────────

fn build_client(token: &str) -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(header::USER_AGENT, HeaderValue::from_static(USER_AGENT));
    headers.insert(header::ACCEPT, HeaderValue::from_static("*/*"));
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        header::ORIGIN,
        HeaderValue::from_static("https://my.brain.fm"),
    );
    headers.insert("Referer", HeaderValue::from_static("https://my.brain.fm/"));
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))?,
    );
    ClientBuilder::new()
        .default_headers(headers)
        .timeout(Duration::from_secs(20))
        .build()
        .context("build client")
}

// ── API ───────────────────────────────────────────────────────────────────────

const TOKEN_HINT: &str = "your token may have expired; get a fresh one from the browser and run `brainfm-cli init --token <token>`";

fn snippet(body: &str) -> String {
    let s: String = body.chars().take(200).collect();
    if body.chars().count() > 200 {
        format!("{s}…")
    } else {
        s
    }
}

/// Extract and deserialize the `result` field of an API response, turning
/// auth failures and empty/null results into actionable errors instead of a
/// bare serde type error.
fn parse_result<T: serde::de::DeserializeOwned>(
    status: StatusCode,
    body: &str,
    what: &str,
) -> Result<T> {
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        bail!("{what}: API returned {status}; {TOKEN_HINT}");
    }
    if !status.is_success() {
        bail!("{what}: API returned {status}: {}", snippet(body));
    }
    let v: serde_json::Value = serde_json::from_str(body)
        .with_context(|| format!("{what}: response was not JSON: {}", snippet(body)))?;
    match v.get("result") {
        None | Some(serde_json::Value::Null) => {
            let detail = ["message", "error"]
                .iter()
                .find_map(|k| v.get(*k).and_then(|m| m.as_str()))
                .map(|m| format!(" ({m})"))
                .unwrap_or_default();
            bail!("{what}: API returned no result{detail}; {TOKEN_HINT}")
        }
        Some(result) => serde_json::from_value(result.clone())
            .with_context(|| format!("{what}: unexpected response shape: {}", snippet(body))),
    }
}

fn get_result<T: serde::de::DeserializeOwned>(client: &Client, url: &str, what: &str) -> Result<T> {
    let resp = client.get(url).send()?;
    let status = resp.status();
    let body = resp.text()?;
    parse_result(status, &body, what)
}

fn list_mental_states(client: &Client) -> Result<Vec<MentalState>> {
    get_result(
        client,
        &format!("{API_BASE}/v3/mentalStates/dynamic"),
        "list mental states",
    )
}

fn list_activities(client: &Client, state_id: &str) -> Result<Vec<Activity>> {
    get_result(
        client,
        &format!("{API_BASE}/v3/mentalStates/dynamic/{state_id}/activities"),
        "list activities",
    )
}

fn create_session(
    client: &Client,
    user_id: &str,
    activity_id: &str,
    nel: &NeuralEffect,
    genre: Option<&str>,
) -> Result<(String, String)> {
    let mut payload = serde_json::json!({
        "dynamicActivityId": activity_id,
        "version": 3,
        "neuralEffectLevels": [nel.as_str()],
    });
    if let Some(g) = genre {
        payload["genreNames"] = serde_json::json!([g]);
    }

    let resp = client
        .post(format!(
            "{API_BASE}/v3/users/{user_id}/sessions?platform=web"
        ))
        .body(serde_json::to_string(&payload)?)
        .send()?;
    let status = resp.status();
    let body = resp.text()?;

    let result: serde_json::Value = parse_result(status, &body, "create session")?;
    let result = &result;

    let url = ["tokenedUrl", "cdnUrl", "url"]
        .iter()
        .find_map(|key| {
            result["servings"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|s| s["trackVariation"][key].as_str().map(|s| s.to_owned()))
                .or_else(|| result["trackVariation"][key].as_str().map(|s| s.to_owned()))
        })
        .ok_or_else(|| anyhow!("no audio URL in session response"))?;

    let track_name = result["servings"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|s| s["track"]["name"].as_str())
        .unwrap_or("Unknown Track")
        .to_owned();

    Ok((url, track_name))
}

// ── Playback ──────────────────────────────────────────────────────────────────

fn play_url(url: &str) -> Result<()> {
    let players: &[(&str, &[&str])] = &[
        ("mpv", &["--no-video", "--really-quiet"]),
        ("vlc", &["--intf", "dummy", "--play-and-exit"]),
        ("ffplay", &["-nodisp", "-autoexit"]),
        ("mplayer", &["-novideo"]),
    ];
    for (bin, args) in players {
        if which(bin) {
            println!("Playing via {bin} — Ctrl-C to stop");
            Command::new(bin)
                .args(*args)
                .arg(url)
                .status()
                .with_context(|| format!("failed to launch {bin}"))?;
            return Ok(());
        }
    }
    bail!("No player found. Install mpv, or use `brainfm-cli url` and pipe yourself.")
}

// ── Cache ─────────────────────────────────────────────────────────────────────

/// Returns $XDG_DATA_HOME/brainfm-cli or ~/.local/share/brainfm-cli
fn cache_dir() -> PathBuf {
    let base = env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env::var("HOME").unwrap_or_default()).join(".local/share")
        });
    base.join("brainfm-cli")
}

/// If the file is already cached, return its path.
/// Otherwise download it, save it, and return the path.
fn download_or_cache(url: &str, track_name: &str) -> Result<PathBuf> {
    let dir = cache_dir();
    fs::create_dir_all(&dir).with_context(|| format!("failed to create cache dir {dir:?}"))?;

    // Derive a stable filename from the track name — strip query string chars,
    // keep it readable. e.g. "Stormscape Thunder.mp3"
    let safe_name = track_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == ' ' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim()
        .to_owned();
    let path = dir.join(format!("{safe_name}.mp3"));

    if path.exists() {
        println!("  Using cached: {}", path.display());
        return Ok(path);
    }

    println!("  Downloading to cache: {}", path.display());
    // Strip the signed query string from the URL to get the base filename,
    // but download the full signed URL.
    let response = reqwest::blocking::get(url).context("failed to fetch audio")?;
    let bytes = response.bytes().context("failed to read audio")?;
    fs::write(&path, &bytes).with_context(|| format!("failed to write cache file {path:?}"))?;
    println!("  Cached ({:.1} MB)", bytes.len() as f64 / 1_048_576.0);

    Ok(path)
}

fn which(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ── Token ─────────────────────────────────────────────────────────────────────

fn token_path() -> Option<PathBuf> {
    env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(TOKEN_FILE))
}
fn save_token(token: &str) -> Result<()> {
    if let Some(path) = token_path() {
        fs::write(&path, token)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}
fn load_token(cli_token: &Option<String>) -> Option<String> {
    cli_token
        .clone()
        .or_else(|| env::var(TOKEN_ENV).ok().filter(|s| !s.is_empty()))
        .or_else(|| {
            token_path()
                .and_then(|p| fs::read_to_string(p).ok())
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        })
}

// ── Resolve helpers ───────────────────────────────────────────────────────────

fn resolve_mental_state<'a>(states: &'a [MentalState], input: &str) -> Option<&'a MentalState> {
    let lower = input.to_lowercase();
    states.iter().find(|s| {
        s.id.to_lowercase() == lower
            || s.display_value
                .as_deref()
                .map(|v| v.to_lowercase() == lower)
                .unwrap_or(false)
    })
}

fn resolve_activity<'a>(acts: &'a [Activity], input: &str) -> Option<&'a Activity> {
    let lower = input.to_lowercase();
    acts.iter().find(|a| {
        a.id.to_lowercase() == lower
            || a.display_value
                .as_deref()
                .map(|v| v.to_lowercase() == lower)
                .unwrap_or(false)
    })
}

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "brainfm-cli", about = "Unofficial Brain.fm CLI")]
struct Cli {
    #[arg(long, env = TOKEN_ENV, global = true, hide_env_values = true)]
    token: Option<String>,
    #[arg(long, global = true)]
    debug: bool,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Save bearer token (from browser Authorization header).
    Init {
        #[arg(long)]
        token: String,
    },

    /// List mental states (focus, sleep, relax).
    Ls,

    /// List activities for a mental state.
    Activities { mental_state: String },

    /// Play a mental state.
    ///
    /// Known genres: Thunder, Rain, Wind, Fire, Ocean, River, Underwater,
    /// Birds, Night Sounds, Brown Noise, Pink Noise, White Noise
    Play {
        mental_state: String,
        #[arg(long, short)]
        activity: Option<String>,
        #[arg(long, value_enum, default_value = "high")]
        nel: NeuralEffect,
        /// Genre/sound filter e.g. --genre Thunder
        #[arg(long, short)]
        genre: Option<String>,
        /// Cache the audio file in $XDG_DATA_HOME/brainfm-cli and reuse on future plays
        #[arg(long)]
        cache: bool,
    },

    /// Print the signed stream URL (pipe to any player).
    Url {
        mental_state: String,
        #[arg(long, short)]
        activity: Option<String>,
        #[arg(long, value_enum, default_value = "high")]
        nel: NeuralEffect,
        #[arg(long, short)]
        genre: Option<String>,
    },
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Cmd::Init { token } => {
            let uid = user_id_from_jwt(&token)?;
            save_token(&token)?;
            println!("Token saved. User ID: {uid}");
        }

        Cmd::Ls => {
            let token = load_token(&cli.token).ok_or_else(|| anyhow!("no token"))?;
            let client = build_client(&token)?;
            for s in list_mental_states(&client)? {
                println!("{:<12} {}", s.id, s.display_value.as_deref().unwrap_or(""));
            }
        }

        Cmd::Activities { mental_state } => {
            let token = load_token(&cli.token).ok_or_else(|| anyhow!("no token"))?;
            let client = build_client(&token)?;
            let states = list_mental_states(&client)?;
            let state = resolve_mental_state(&states, &mental_state)
                .ok_or_else(|| anyhow!("unknown mental state: {mental_state}"))?;
            for a in list_activities(&client, &state.id)? {
                println!("{:<35} {}", a.id, a.display_value.as_deref().unwrap_or(""));
            }
        }

        Cmd::Play {
            mental_state,
            activity,
            nel,
            genre,
            cache,
        } => {
            let token = load_token(&cli.token).ok_or_else(|| anyhow!("no token"))?;
            let user_id = user_id_from_jwt(&token)?;
            let client = build_client(&token)?;
            let states = list_mental_states(&client)?;
            let state = resolve_mental_state(&states, &mental_state)
                .ok_or_else(|| anyhow!("unknown mental state: {mental_state}"))?;
            let act_id = resolve_act_id(&client, state, activity.as_deref())?;

            if let Some(ref g) = genre {
                println!("Genre: {g}");
            }
            println!("Fetching session…");
            let (url, track_name) =
                create_session(&client, &user_id, &act_id, &nel, genre.as_deref())?;
            println!("▶  {track_name}");
            if cache {
                let path = download_or_cache(&url, &track_name)?;
                play_url(path.to_str().unwrap_or(&url))?;
            } else {
                play_url(&url)?;
            }
        }

        Cmd::Url {
            mental_state,
            activity,
            nel,
            genre,
        } => {
            let token = load_token(&cli.token).ok_or_else(|| anyhow!("no token"))?;
            let user_id = user_id_from_jwt(&token)?;
            let client = build_client(&token)?;
            let states = list_mental_states(&client)?;
            let state = resolve_mental_state(&states, &mental_state)
                .ok_or_else(|| anyhow!("unknown mental state: {mental_state}"))?;
            let act_id = resolve_act_id(&client, state, activity.as_deref())?;
            let (url, _) = create_session(&client, &user_id, &act_id, &nel, genre.as_deref())?;
            println!("{url}");
        }
    }

    Ok(())
}

fn resolve_act_id(client: &Client, state: &MentalState, activity: Option<&str>) -> Result<String> {
    if let Some(a) = activity {
        let acts = list_activities(client, &state.id)?;
        resolve_activity(&acts, a)
            .map(|a| a.id.clone())
            .ok_or_else(|| anyhow!("unknown activity: {a}"))
    } else {
        state
            .default_activity
            .as_ref()
            .map(|a| a.id.clone())
            .ok_or_else(|| anyhow!("no default activity for {}", state.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn states(status: StatusCode, body: &str) -> Result<Vec<MentalState>> {
        parse_result(status, body, "list mental states")
    }

    #[test]
    fn parses_a_normal_result() {
        let body = r#"{"result":[{"id":"focus","displayValue":"Focus"},{"id":"sleep"}]}"#;
        let parsed = states(StatusCode::OK, body).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].id, "focus");
        assert_eq!(parsed[1].display_value, None);
    }

    #[test]
    fn empty_result_list_is_not_an_error() {
        assert!(states(StatusCode::OK, r#"{"result":[]}"#).unwrap().is_empty());
    }

    // Regression: `{"result": null}` used to surface as the cryptic
    // "parse mental states: invalid type: null, expected a sequence".
    #[test]
    fn null_result_gives_actionable_error() {
        let err = states(StatusCode::OK, r#"{"result":null}"#).unwrap_err().to_string();
        assert!(err.contains("no result"), "{err}");
        assert!(err.contains("brainfm-cli init"), "{err}");
        assert!(!err.contains("invalid type"), "{err}");
    }

    #[test]
    fn missing_result_key_gives_actionable_error() {
        let err = states(StatusCode::OK, r#"{"message":"Token expired"}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no result"), "{err}");
        assert!(err.contains("Token expired"), "{err}");
    }

    #[test]
    fn unauthorized_points_at_token_refresh() {
        for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
            let err = states(status, r#"{"result":null}"#).unwrap_err().to_string();
            assert!(err.contains("brainfm-cli init"), "{err}");
        }
    }

    #[test]
    fn server_error_includes_status_and_body() {
        let err = states(StatusCode::INTERNAL_SERVER_ERROR, "boom")
            .unwrap_err()
            .to_string();
        assert!(err.contains("500"), "{err}");
        assert!(err.contains("boom"), "{err}");
    }

    #[test]
    fn non_json_body_is_reported() {
        let err = states(StatusCode::OK, "<html>oops</html>").unwrap_err().to_string();
        assert!(err.contains("not JSON"), "{err}");
    }

    #[test]
    fn wrong_shape_is_reported_with_body() {
        let err = states(StatusCode::OK, r#"{"result":"nope"}"#).unwrap_err().to_string();
        assert!(err.contains("unexpected response shape"), "{err}");
    }
}
