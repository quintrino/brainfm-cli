# brainfm

An unofficial command-line client for Brain.fm. Plays Focus, Sleep, and Relax
sessions from the terminal.

## Requirements

- Rust: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- A player on your `PATH`: `mpv`, `vlc`, `ffplay`, or `mplayer`

## Build

```bash
cargo build --release
```

The binary is `target/release/brainfm`.

## Setup

1. Open Brain.fm in your browser and log in
2. Open DevTools → Network tab → click any `api.brain.fm` request
3. Copy the `Authorization` header value (everything after `Bearer `)
4. Save it:

```bash
brainfm init --token <token>
```

The token is stored in `~/.local/share/brainfm/token`. You can also pass
`--token` on any command or set `BRAINFM_TOKEN`.

Tokens expire in ~15 minutes. When a command fails with an auth error, grab a
fresh token from the browser and run `init` again.

## Usage

```bash
brainfm ls                          # list mental states
brainfm activities focus            # list activities for a state
brainfm play focus                  # play a state's default activity
brainfm play focus -a <activity>    # play a specific activity
brainfm play sleep --nel low        # neural effect level: low | medium | high
brainfm play relax -g Rain          # filter by genre/sound
brainfm url focus                   # print the signed stream URL
```

Known genres: Thunder, Rain, Wind, Fire, Ocean, River, Underwater, Birds,
Night Sounds, Brown Noise, Pink Noise, White Noise.

`loop` is a helper script that runs `brainfm play focus` continuously,
restarting it at the top of every hour.
