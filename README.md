# brainfm-cli

An unofficial command-line client for Brain.fm. Plays Focus, Sleep, and Relax
sessions from the terminal.

## Requirements

- Rust: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- A player on your `PATH`: `mpv`, `vlc`, `ffplay`, or `mplayer`

## Install

```bash
brew install quintrino/tap/brainfm-cli
```

This installs a prebuilt binary with no extra dependencies. You still need an
audio player on your `PATH` (see Requirements), e.g. `brew install mpv`. Or
build it yourself:

## Build

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
brainfm-cli init --token <token>
```

The token is stored in `~/.local/share/brainfm-cli/token`. You can also pass
`--token` on any command or set `BRAINFM_TOKEN`.

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
brainfm-cli url focus                   # print the signed stream URL
```

Known genres: Thunder, Rain, Wind, Fire, Ocean, River, Underwater, Birds,
Night Sounds, Brown Noise, Pink Noise, White Noise.

`loop` is a helper script that runs `brainfm-cli play focus` continuously,
restarting it at the top of every hour.
