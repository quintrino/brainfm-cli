# brainfm-cli

An unofficial command-line client for Brain.fm. Plays Focus, Sleep, and Relax
sessions from the terminal.

## Requirements

A player on your `PATH`: `mpv`, `vlc`, `ffplay`, or `mplayer`.

## Install

```bash
brew install quintrino/tap/brainfm-cli
```

This installs a prebuilt binary with no extra dependencies. You still need an
audio player on your `PATH` (see Requirements), e.g. `brew install mpv`.

## Build

To build it yourself you need Rust 1.70 or newer
(`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`):

```bash
cargo build --release
```

The binary is `target/release/brainfm-cli`.

## Setup

1. Open Brain.fm in your browser and log in
2. Open the web console (DevTools) → Network tab
3. Find the request to `https://api.brain.fm` for the file `me`. The token is
   labeled `token` in that request
4. Copy the `token` value and save it:

```bash
brainfm-cli init                  # prompts for the token
pbpaste | brainfm-cli init        # or pipe it in (macOS clipboard)
```

`init --token <token>` also works, but then the token is visible in your shell
history and in `ps`, so prefer one of the forms above.

The token is stored in `brainfm-cli/token` under `$XDG_DATA_HOME`, which is
`~/.local/share` unless you set it. The directory and file are created
readable only by you. You can also pass `--token` on any command or set
`BRAINFM_TOKEN`.

Tokens expire in ~15 minutes. When a command fails with an auth error, grab a
fresh token from the browser and run `init` again.

## Usage

```bash
brainfm-cli ls                          # list mental states
brainfm-cli activities focus            # list activities for a state
brainfm-cli play focus                  # play a state's default activity
brainfm-cli play focus -a <activity>    # play a specific activity
brainfm-cli play sleep --nel low        # neural effect level: low | medium | high
brainfm-cli play relax -g Rain          # filter by genre/sound
brainfm-cli play focus --cache          # download the audio once and reuse it
brainfm-cli play focus --loop           # keep playing, new session at the top of each hour
brainfm-cli url focus                   # print the signed stream URL
brainfm-cli --debug play focus          # print each request's URL and status to stderr
brainfm-cli --version
```

Known genres: Thunder, Rain, Wind, Fire, Ocean, River, Underwater, Birds,
Night Sounds, Brown Noise, Pink Noise, White Noise.

`--loop` keeps playing: when a session ends it fetches a new one, and at the top
of every hour (UTC) it stops the player and starts a fresh session. Press Ctrl-C
to stop. It also stops if the player exits straight away. Tokens last about 15
minutes, so a long loop will end with an auth error once the token expires; run
`init` with a fresh token and start it again.

Requests that time out or get a 429 or 5xx response are retried up to three
times. `--debug` shows each attempt, and never prints the token.

## Caveats

This is an unofficial tool. It talks to Brain.fm's undocumented web API with a
token copied from your browser session, and it sends browser-style request
headers because the API expects them. Brain.fm may change the API without
notice, and using it this way may be against their terms of service. Use it at
your own risk.
