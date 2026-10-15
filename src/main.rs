use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use reqwest::blocking::{Client, ClientBuilder, RequestBuilder};
use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::StatusCode;
use serde::Deserialize;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const API_BASE: &str = "https://api.brain.fm";
const TOKEN_ENV: &str = "BRAINFM_TOKEN";
const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:151.0) Gecko/20100101 Firefox/151.0";

static DEBUG: AtomicBool = AtomicBool::new(false);

/// Prints to stderr when --debug is on. Callers must never pass tokens or headers.
fn debug(message: impl AsRef<str>) {
    if DEBUG.load(Ordering::Relaxed) {
        eprintln!("[debug] {}", message.as_ref());
    }
}

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

// ── IDs and JWT ───────────────────────────────────────────────────────────────

/// Ids end up in URL paths, so only accept plain identifier characters.
fn ensure_safe_id(what: &str, id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        bail!("unexpected {what}: {id:?}");
    }
    Ok(())
}

fn user_id_from_jwt(token: &str) -> Result<String> {
    let parts: Vec<&str> = token.splitn(3, '.').collect();
    if parts.len() < 2 {
        bail!("not a JWT");
    }
    let decoded = base64url_decode(parts[1]).context("the token's payload is not valid base64")?;
    let json: serde_json::Value =
        serde_json::from_slice(&decoded).context("the token's payload is not valid JSON")?;
    let id = json["_id"]
        .as_str()
        .map(|s| s.to_owned())
        .ok_or_else(|| anyhow!("no _id in JWT"))?;
    ensure_safe_id("user id in the token", &id)?;
    Ok(id)
}

/// Decodes unpadded or padded URL-safe base64, rejecting anything outside the alphabet.
fn base64url_decode(s: &str) -> Result<Vec<u8>> {
    let s = s.trim_end_matches('=');
    if s.len() % 4 == 1 {
        bail!("invalid base64 length");
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for c in s.bytes() {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => bail!("invalid base64 character {:?}", c as char),
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1u32 << bits) - 1;
        }
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
    let mut auth = HeaderValue::from_str(&format!("Bearer {token}"))
        .context("the token contains characters that cannot be sent in a header")?;
    auth.set_sensitive(true);
    headers.insert(header::AUTHORIZATION, auth);
    ClientBuilder::new()
        .default_headers(headers)
        .timeout(Duration::from_secs(20))
        .build()
        .context("build client")
}

/// A client for downloading audio: no total timeout (a track can take longer than the
/// API's 20 seconds), but a connect timeout so a dead host still fails fast.
fn download_client() -> Result<Client> {
    ClientBuilder::new()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(15))
        .timeout(None)
        .build()
        .context("build download client")
}

/// Sends the request built by `build`, retrying up to three times on timeouts,
/// connection errors, 429 and 5xx, and honouring a numeric Retry-After.
fn send_with_retry(what: &str, build: impl Fn() -> RequestBuilder) -> Result<(StatusCode, String)> {
    const ATTEMPTS: u32 = 3;
    for attempt in 1..=ATTEMPTS {
        debug(format!("{what}: attempt {attempt}"));
        match build().send() {
            Ok(resp) => {
                let status = resp.status();
                debug(format!("{what}: HTTP {status}"));
                let retry_after = resp
                    .headers()
                    .get(header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok());
                let body = resp
                    .text()
                    .with_context(|| format!("{what}: failed to read the response"))?;
                let retryable = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
                if retryable && attempt < ATTEMPTS {
                    let wait = retry_after.unwrap_or(u64::from(attempt)).min(30);
                    debug(format!("{what}: retrying in {wait}s"));
                    std::thread::sleep(Duration::from_secs(wait));
                    continue;
                }
                return Ok((status, body));
            }
            Err(e) if (e.is_timeout() || e.is_connect()) && attempt < ATTEMPTS => {
                debug(format!("{what}: {e}; retrying"));
                std::thread::sleep(Duration::from_secs(u64::from(attempt)));
            }
            Err(e) => return Err(e).with_context(|| format!("{what}: request failed")),
        }
    }
    unreachable!("the loop always returns on its last attempt")
}

// ── API ───────────────────────────────────────────────────────────────────────

const TOKEN_HINT: &str =
    "your token may have expired; get a fresh one from the browser and run `brainfm-cli init`";

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
    debug(format!("GET {url}"));
    let (status, body) = send_with_retry(what, || client.get(url))?;
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
    ensure_safe_id("mental state id", state_id)?;
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
    ensure_safe_id("user id", user_id)?;
    let mut payload = serde_json::json!({
        "dynamicActivityId": activity_id,
        "version": 3,
        "neuralEffectLevels": [nel.as_str()],
    });
    if let Some(g) = genre {
        payload["genreNames"] = serde_json::json!([g]);
    }
    let payload = serde_json::to_string(&payload)?;
    let url = format!("{API_BASE}/v3/users/{user_id}/sessions?platform=web");

    debug(format!("POST {url}"));
    let (status, body) =
        send_with_retry("create session", || client.post(&url).body(payload.clone()))?;

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

/// Seconds from now until the next top of the hour (UTC), between 1 and 3600.
fn seconds_until_next_hour() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    3600 - now % 3600
}

/// A player that was stopped by a signal (such as Ctrl-C) did not fail; one that exited
/// with a non-zero code did.
fn check_exit(bin: &str, status: ExitStatus) -> Result<()> {
    if status.success() || status.code().is_none() {
        Ok(())
    } else {
        bail!("{bin} exited with {status}")
    }
}

/// Plays `url` with the first player found. With a `limit` the player is stopped once
/// it has run that long. Returns true if it was stopped at the limit, false if it
/// finished on its own.
fn play_url(url: &str, limit: Option<Duration>) -> Result<bool> {
    let players: &[(&str, &[&str])] = &[
        ("mpv", &["--no-video", "--really-quiet"]),
        ("vlc", &["--intf", "dummy", "--play-and-exit"]),
        ("ffplay", &["-nodisp", "-autoexit"]),
        ("mplayer", &["-novideo"]),
    ];
    for (bin, args) in players {
        // Try to start each player; one that is not installed is simply skipped
        let mut child = match Command::new(bin).args(*args).arg(url).spawn() {
            Ok(child) => child,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("failed to launch {bin}")),
        };
        println!("Playing via {bin} — Ctrl-C to stop");
        let Some(limit) = limit else {
            let status = child
                .wait()
                .with_context(|| format!("failed waiting for {bin}"))?;
            check_exit(bin, status)?;
            return Ok(false);
        };
        let started = Instant::now();
        loop {
            if let Some(status) = child
                .try_wait()
                .with_context(|| format!("failed waiting for {bin}"))?
            {
                check_exit(bin, status)?;
                return Ok(false);
            }
            if started.elapsed() >= limit {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(true);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    bail!("No player found. Install mpv, vlc, ffplay or mplayer, or use `brainfm-cli url` and pipe yourself.")
}

// ── Data directory and cache ──────────────────────────────────────────────────

/// $XDG_DATA_HOME/brainfm-cli, or ~/.local/share/brainfm-cli. Used for both the token
/// and the audio cache.
fn data_dir_from(xdg: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    let base = match xdg.filter(|v| !v.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => {
            let home = home.filter(|v| !v.is_empty()).ok_or_else(|| {
                anyhow!("cannot find your home directory; set HOME (or XDG_DATA_HOME)")
            })?;
            PathBuf::from(home).join(".local/share")
        }
    };
    Ok(base.join("brainfm-cli"))
}

fn data_dir() -> Result<PathBuf> {
    data_dir_from(env::var_os("XDG_DATA_HOME"), env::var_os("HOME"))
}

/// Makes a name safe to use as a file name: anything but letters, digits, spaces and
/// dashes becomes an underscore, so no path separators or dots survive.
fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == ' ' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

/// The cache file for one track. The neural effect level and genre are part of the name
/// because they change the audio you get for the same track name.
fn cache_file_name(track_name: &str, nel: &NeuralEffect, genre: Option<&str>) -> String {
    let mut name = sanitize_name(track_name);
    if name.is_empty() {
        name = "track".to_owned();
    }
    name.push('-');
    name.push_str(&nel.as_str().to_lowercase());
    if let Some(genre) = genre.map(sanitize_name).filter(|g| !g.is_empty()) {
        name.push('-');
        name.push_str(&genre);
    }
    format!("{name}.mp3")
}

/// If the file is already cached, return its path.
/// Otherwise download it, save it, and return the path.
fn download_or_cache(
    url: &str,
    track_name: &str,
    nel: &NeuralEffect,
    genre: Option<&str>,
) -> Result<PathBuf> {
    let dir = data_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("failed to create cache dir {dir:?}"))?;

    let path = dir.join(cache_file_name(track_name, nel, genre));
    if path.exists() {
        println!("  Using cached: {}", path.display());
        return Ok(path);
    }

    println!("  Downloading to cache: {}", path.display());
    // The signed query string is a credential; keep it out of the debug output
    debug(format!("GET {}", url.split('?').next().unwrap_or(url)));

    // Download to a .part file and rename it on success, so an error or Ctrl-C never
    // leaves a truncated or error-page file that later runs would trust.
    let part = path.with_extension("mp3.part");
    let result = (|| -> Result<u64> {
        let mut response = download_client()?
            .get(url)
            .send()
            .context("failed to fetch audio")?
            .error_for_status()
            .context("the audio download was refused")?;
        let is_html = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/"));
        if is_html {
            bail!("the server sent a web page instead of audio");
        }
        let mut file = fs::File::create(&part)
            .with_context(|| format!("failed to create cache file {part:?}"))?;
        let written = io::copy(&mut response, &mut file).context("failed to read audio")?;
        if written == 0 {
            bail!("the server sent an empty file");
        }
        file.flush()?;
        Ok(written)
    })();
    let written = match result {
        Ok(written) => written,
        Err(e) => {
            let _ = fs::remove_file(&part);
            return Err(e);
        }
    };
    fs::rename(&part, &path).with_context(|| format!("failed to write cache file {path:?}"))?;
    println!("  Cached ({:.1} MB)", written as f64 / 1_048_576.0);

    Ok(path)
}

// ── Token ─────────────────────────────────────────────────────────────────────

fn token_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("token"))
}

/// Where older versions always kept the token, whatever XDG_DATA_HOME said.
fn legacy_token_path() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|h| PathBuf::from(h).join(".local/share/brainfm-cli/token"))
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

/// Writes `data` to `path` so that it is never readable by anyone else, not even
/// briefly: the file is created with mode 0600 rather than chmod'd afterwards.
fn write_private_file(path: &Path, data: &[u8]) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to write token to {path:?}"))?;
    #[cfg(unix)]
    {
        // A token file left by an older version may have looser permissions
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(data)
        .with_context(|| format!("failed to write token to {path:?}"))?;
    Ok(())
}

/// Writes the token to `path`, creating its parent directory first: on a fresh
/// machine ~/.local/share/brainfm-cli does not exist yet.
fn write_token(path: &Path, token: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        create_private_dir(dir).with_context(|| format!("failed to create token dir {dir:?}"))?;
    }
    write_private_file(path, token.as_bytes())
}

fn load_token(cli_token: &Option<String>) -> Option<String> {
    let read = |path: PathBuf| {
        fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    cli_token
        .clone()
        .or_else(|| env::var(TOKEN_ENV).ok().filter(|s| !s.is_empty()))
        .or_else(|| token_path().ok().and_then(read))
        .or_else(|| legacy_token_path().and_then(read))
}

fn require_token(cli_token: &Option<String>) -> Result<String> {
    load_token(cli_token).ok_or_else(|| {
        anyhow!("no token found; run `brainfm-cli init` to save one, or set {TOKEN_ENV}")
    })
}

/// The token for `init`: from `--token`, or from standard input (a line from a pipe, or a
/// prompt in a terminal), so it need not appear in shell history or `ps`.
fn read_token_input(arg: Option<String>) -> Result<String> {
    let token = match arg.as_deref() {
        Some(token) if token != "-" => token.trim().to_owned(),
        _ => {
            let stdin = io::stdin();
            if stdin.is_terminal() {
                eprint!("Paste your Brain.fm token: ");
                let _ = io::stderr().flush();
            }
            let mut line = String::new();
            stdin
                .lock()
                .read_line(&mut line)
                .context("failed to read the token from standard input")?;
            line.trim().to_owned()
        }
    };
    if token.is_empty() {
        bail!("no token given");
    }
    Ok(token)
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
#[command(name = "brainfm-cli", about = "Unofficial Brain.fm CLI", version)]
struct Cli {
    #[arg(long, env = TOKEN_ENV, global = true, hide_env_values = true)]
    token: Option<String>,
    /// Print each request's method, URL and status to stderr (never tokens)
    #[arg(long, global = true)]
    debug: bool,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Save the token from the `me` response in the browser.
    ///
    /// Run it with no arguments to be prompted, or pipe the token in (`pbpaste |
    /// brainfm-cli init`). Passing it with --token also works, but then it is visible
    /// in your shell history and in `ps`.
    Init {
        #[arg(long)]
        token: Option<String>,
    },

    /// List mental states (focus, sleep, relax).
    Ls,

    /// List activities for a mental state.
    Activities { mental_state: String },

    /// Play a mental state (add --loop to keep playing).
    ///
    /// With --loop, a new session starts when one ends, and at the top of every hour (UTC)
    /// the player is stopped and a fresh session begins. Press Ctrl-C to stop.
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
        /// Keep playing: fetch a new session when one ends, and restart at the top of every hour
        #[arg(long = "loop")]
        repeat: bool,
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
    DEBUG.store(cli.debug, Ordering::Relaxed);

    match cli.command {
        Cmd::Init { token } => {
            let token = read_token_input(token)?;
            let uid = user_id_from_jwt(&token)?;
            let path = token_path()?;
            write_token(&path, &token)?;
            println!("Token saved to {}. User ID: {uid}", path.display());
        }

        Cmd::Ls => {
            let token = require_token(&cli.token)?;
            let client = build_client(&token)?;
            for s in list_mental_states(&client)? {
                println!("{:<12} {}", s.id, s.display_value.as_deref().unwrap_or(""));
            }
        }

        Cmd::Activities { mental_state } => {
            let token = require_token(&cli.token)?;
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
            repeat,
        } => {
            let token = require_token(&cli.token)?;
            let user_id = user_id_from_jwt(&token)?;
            let client = build_client(&token)?;
            let states = list_mental_states(&client)?;
            let state = resolve_mental_state(&states, &mental_state)
                .ok_or_else(|| anyhow!("unknown mental state: {mental_state}"))?;
            let act_id = resolve_act_id(&client, state, activity.as_deref())?;

            if let Some(ref g) = genre {
                println!("Genre: {g}");
            }
            loop {
                println!("Fetching session…");
                let (url, track_name) =
                    create_session(&client, &user_id, &act_id, &nel, genre.as_deref())?;
                println!("▶  {track_name}");
                let target = if cache {
                    download_or_cache(&url, &track_name, &nel, genre.as_deref())?
                        .to_string_lossy()
                        .into_owned()
                } else {
                    url
                };

                // With --loop, stop at the top of the hour and start a fresh session
                let limit = repeat.then(|| Duration::from_secs(seconds_until_next_hour()));
                let started = Instant::now();
                let hit_limit = play_url(&target, limit)?;
                if !repeat {
                    break;
                }
                // A player that dies at once would otherwise hammer the API in a tight loop
                if !hit_limit && started.elapsed() < Duration::from_secs(3) {
                    bail!("the player exited straight away; stopping --loop");
                }
            }
        }

        Cmd::Url {
            mental_state,
            activity,
            nel,
            genre,
        } => {
            let token = require_token(&cli.token)?;
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

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    // ── time ──

    #[test]
    fn the_next_hour_is_between_one_and_3600_seconds_away() {
        let secs = seconds_until_next_hour();
        assert!((1..=3600).contains(&secs), "{secs}");
    }

    // ── data directory ──

    #[test]
    fn the_data_dir_honours_xdg_data_home() {
        assert_eq!(
            data_dir_from(os("/data"), os("/home/me")).unwrap(),
            PathBuf::from("/data/brainfm-cli")
        );
    }

    #[test]
    fn the_data_dir_falls_back_to_home() {
        for xdg in [None, os("")] {
            assert_eq!(
                data_dir_from(xdg, os("/home/me")).unwrap(),
                PathBuf::from("/home/me/.local/share/brainfm-cli")
            );
        }
    }

    #[test]
    fn no_home_and_no_xdg_is_an_error_not_a_relative_path() {
        for home in [None, os("")] {
            let err = data_dir_from(None, home).unwrap_err().to_string();
            assert!(err.contains("HOME"), "{err}");
        }
    }

    // ── token file ──

    #[test]
    fn write_token_creates_missing_directories_and_saves_the_token() {
        let root = env::temp_dir().join(format!("brainfm-cli-test-{}", std::process::id()));
        let dir = root.join("nested/brainfm-cli");
        let path = dir.join("token");
        let _ = fs::remove_dir_all(&root);

        write_token(&path, "abc123").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "abc123");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        // A second init overwrites the token and the existing directory is fine.
        write_token(&path, "def456").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "def456");
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn write_token_tightens_a_loose_existing_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = env::temp_dir().join(format!("brainfm-cli-test-loose-{}", std::process::id()));
        let path = root.join("token");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        write_token(&path, "new").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = fs::remove_dir_all(&root);
    }

    // ── JWT and base64 ──

    // {"_id":"abc123"}
    const PAYLOAD: &str = "eyJfaWQiOiJhYmMxMjMifQ";

    #[test]
    fn the_user_id_is_read_from_the_token_payload() {
        assert_eq!(
            user_id_from_jwt(&format!("x.{PAYLOAD}.sig")).unwrap(),
            "abc123"
        );
        // padded payloads work too
        assert_eq!(
            user_id_from_jwt(&format!("x.{PAYLOAD}==.sig")).unwrap(),
            "abc123"
        );
    }

    #[test]
    fn a_token_that_is_not_a_jwt_is_rejected() {
        assert!(user_id_from_jwt("nodots").is_err());
        assert!(user_id_from_jwt("a.$$$.c").is_err());
        assert!(user_id_from_jwt("a.eyJmb28iOiJiYXIifQ.c").is_err()); // no _id
    }

    #[test]
    fn an_unsafe_user_id_is_rejected() {
        // {"_id":"../x"}
        let err = user_id_from_jwt("a.eyJfaWQiOiIuLi94In0.c")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unexpected user id"), "{err}");
    }

    #[test]
    fn base64_decoding_is_strict() {
        assert_eq!(base64url_decode("aGk").unwrap(), b"hi");
        assert_eq!(base64url_decode("aGk=").unwrap(), b"hi");
        assert_eq!(base64url_decode("_-8").unwrap(), vec![0xff, 0xef]);
        assert!(base64url_decode("a$b").is_err());
        assert!(
            base64url_decode("a+b/").is_err(),
            "standard-alphabet characters are not URL-safe"
        );
        assert!(
            base64url_decode("a").is_err(),
            "a single leftover character is not valid"
        );
    }

    #[test]
    fn ids_must_be_plain_identifiers() {
        assert!(ensure_safe_id("id", "focus").is_ok());
        assert!(ensure_safe_id("id", "5f3a-B_9").is_ok());
        for bad in ["", "a/b", "../x", "a b", "a?b", "é"] {
            assert!(ensure_safe_id("id", bad).is_err(), "{bad:?}");
        }
        assert!(ensure_safe_id("id", &"a".repeat(129)).is_err());
    }

    // ── cache names ──

    #[test]
    fn cache_names_keep_the_track_name_readable() {
        assert_eq!(
            cache_file_name("Stormscape Thunder", &NeuralEffect::High, None),
            "Stormscape Thunder-high.mp3"
        );
    }

    #[test]
    fn cache_names_differ_by_neural_effect_level_and_genre() {
        let a = cache_file_name("Track", &NeuralEffect::Low, None);
        let b = cache_file_name("Track", &NeuralEffect::High, None);
        let c = cache_file_name("Track", &NeuralEffect::High, Some("Rain"));
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_eq!(c, "Track-high-Rain.mp3");
    }

    #[test]
    fn cache_names_cannot_escape_the_cache_directory() {
        for name in ["../../etc/passwd", "a/b", "..", "x\\y"] {
            let file = cache_file_name(name, &NeuralEffect::High, Some("../x"));
            assert!(
                !file.contains('/') && !file.contains('\\') && !file.contains(".."),
                "{file}"
            );
        }
        assert_eq!(
            cache_file_name("", &NeuralEffect::Medium, None),
            "track-medium.mp3"
        );
    }

    // ── resolving names ──

    fn state(id: &str, display: Option<&str>) -> MentalState {
        MentalState {
            id: id.to_owned(),
            display_value: display.map(str::to_owned),
            default_activity: None,
        }
    }

    #[test]
    fn mental_states_match_by_id_or_display_name_ignoring_case() {
        let states = vec![
            state("focus", Some("Focus")),
            state("sleep", Some("Deep Sleep")),
        ];
        assert_eq!(resolve_mental_state(&states, "FOCUS").unwrap().id, "focus");
        assert_eq!(
            resolve_mental_state(&states, "deep sleep").unwrap().id,
            "sleep"
        );
        assert!(resolve_mental_state(&states, "relax").is_none());
    }

    #[test]
    fn activities_match_by_id_or_display_name_ignoring_case() {
        let acts = vec![
            Activity {
                id: "creative".into(),
                display_value: Some("Creative Work".into()),
            },
            Activity {
                id: "study".into(),
                display_value: None,
            },
        ];
        assert_eq!(
            resolve_activity(&acts, "Creative WORK").unwrap().id,
            "creative"
        );
        assert_eq!(resolve_activity(&acts, "STUDY").unwrap().id, "study");
        assert!(resolve_activity(&acts, "nap").is_none());
    }

    // ── API responses ──

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
        assert!(states(StatusCode::OK, r#"{"result":[]}"#)
            .unwrap()
            .is_empty());
    }

    // Regression: `{"result": null}` used to surface as the cryptic
    // "parse mental states: invalid type: null, expected a sequence".
    #[test]
    fn null_result_gives_actionable_error() {
        let err = states(StatusCode::OK, r#"{"result":null}"#)
            .unwrap_err()
            .to_string();
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
            let err = states(status, r#"{"result":null}"#)
                .unwrap_err()
                .to_string();
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
        let err = states(StatusCode::OK, "<html>oops</html>")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not JSON"), "{err}");
    }

    #[test]
    fn wrong_shape_is_reported_with_body() {
        let err = states(StatusCode::OK, r#"{"result":"nope"}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unexpected response shape"), "{err}");
    }
}
