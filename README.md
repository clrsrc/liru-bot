# LiRu-Bot

**Li**chess **Ru**st **Bot** — a bridge between the [Lichess Bot API](https://lichess.org/api#tag/Bot)
and chess engines, written in Rust.

LiRu-Bot is a Rust port of the official Python
[lichess-bot](https://github.com/lichess-bot-devs/lichess-bot). It connects a
local UCI engine to a Lichess **BOT** account: it accepts challenges, plays full
games, and can optionally challenge other bots itself.

> **Derivative-work notice (AGPL §5).** LiRu-Bot is a modified work derived from
> lichess-bot © the lichess-bot-devs and contributors, licensed under
> AGPL-3.0-or-later. Every module is a Rust port of the corresponding
> `lib/*.py`; behaviour and configuration mirror the Python original closely.
> First published as a modified version on 2026-05-31. See [License](#license).

## Features

- UCI engine lifecycle (spawn, options, search, ponder, gameover)
- Polyglot opening books
- Syzygy tablebases (local, up to 6-piece) + optional Gaviota (FFI)
- Online move sources: chessdb, Lichess Cloud Analysis, Opening Explorer, Lichess EGTB
- In-game chat handler (`!help`, `!name`, `!eval`, `!queue`, `!wait`) + greetings
- Outbound matchmaking (sending your own challenges)
- PGN recording (per game / opponent / all)
- Concurrency gate, correspondence pickup on start, online blocklist refresh
- **Optional** in-process engine backend (`--features embedded`) for an
  authoritative wall-clock search deadline — see [Embedded engine](#embedded-engine-optional)

## Requirements

| Component   | What you need                                                              |
|-------------|----------------------------------------------------------------------------|
| Bot account | A Lichess account upgraded to BOT via `/api/bot/account/upgrade`            |
| API token   | Scope `bot:play` — create at <https://lichess.org/account/oauth/token>     |
| UCI engine  | An engine binary (Stockfish, lc0, …)                                        |
| Config      | A YAML file with paths, token, and challenge filters                       |
| Rust        | Rust 1.75 or newer                                                         |

## Quick start

```sh
# 1. Build (release). The default build is a self-contained subprocess bot.
cargo build --release

# 2. Configure.
cp config.yml.example config.yml
$EDITOR config.yml          # set your token, engine path, etc.

# 3. Run.
./target/release/liru-bot --config config.yml
```

Build without the Gaviota FFI (no C toolchain required):

```sh
cargo build --release --no-default-features
```

## Usage

```sh
liru-bot --config <path>            # run the bot loop (default subcommand)
liru-bot --config <path> run        # explicit
liru-bot --config <path> list-bots [--min-rating N] [--speed S] [--limit N]
```

Global options:

| Option              | Default                | Description                                   |
|---------------------|------------------------|-----------------------------------------------|
| `-c`, `--config`    | `./config.yml`         | Path to the YAML config                       |
| `-l`, `--log`       | `RUST_LOG` / `info`    | `tracing-subscriber` env-filter, e.g. `liru_bot=debug,info` |

`list-bots` lists online bots filtered by rating/speed — handy for picking
opponents without the web UI.

## Configuration

See [`config.yml.example`](config.yml.example) for an annotated template. The
configuration surface mirrors upstream lichess-bot, so its
[`config.yml.default`](https://github.com/lichess-bot-devs/lichess-bot/blob/master/config.yml.default)
is the exhaustive reference.

> **YAML gotcha:** `selection` belongs on the `polyglot` level, not under
> `book`. Python's parser tolerates the wrong indentation silently; this Rust
> parser reports `expected sequence`.

## Embedded engine (optional)

The default build talks to the engine as a **subprocess** over UCI and does not
depend on any particular engine. There is also an optional in-process backend
for the [clrsrc](https://github.com/clrsrc/clrsrc) engine, enabled with
`--features embedded`, which hands the search a single authoritative absolute
wall-clock deadline instead of the stacked subprocess time approximation. The
embedded API contract is documented in clrsrc's
[`EMBEDDED.md`](https://github.com/clrsrc/clrsrc/blob/v1.1.0/EMBEDDED.md).

> **Combined-work license note.** Building with `--features embedded` links
> clrsrc (GPL-3.0-or-later) into LiRu-Bot (AGPL-3.0-or-later) as a single
> combined work, which is permitted by AGPLv3 §13 / GPLv3 §13. Each part keeps
> its own license — the clrsrc portion remains GPL-3.0-or-later. Operating the
> combined binary as a network service triggers the AGPL §13 obligation to offer
> the Corresponding Source of the whole to its users. The **default**
> (subprocess) build does **not** link clrsrc and ships as a pure AGPL-3.0
> artifact.

The embedded backend is a tag-pinned git dependency on clrsrc `v1.1.0` (no local
checkout needed). For local embedded development without network you can override
it with a sibling `../clrsrc` path checkout (see the comment in `Cargo.toml`).

## License

LiRu-Bot is licensed under the **GNU Affero General Public License v3.0 or
later** (AGPL-3.0-or-later). See [`LICENSE`](LICENSE) for the full text.

As a derivative of lichess-bot (© lichess-bot-devs and contributors, also
AGPL-3.0), LiRu-Bot preserves the original license and authorship; this is a
modified version within the meaning of AGPL §5. If you run a modified LiRu-Bot
as a network service, AGPL §13 requires you to offer your users the
Corresponding Source.

## Credits & references

- Python original: <https://github.com/lichess-bot-devs/lichess-bot>
- Lichess Bot API: <https://lichess.org/api#tag/Bot>
- UCI protocol: <http://wbec-ridderkerk.nl/html/UCIProtocol.html>
- [shakmaty](https://docs.rs/shakmaty) — Rust chess library
- [gaviota-sys](https://docs.rs/gaviota-sys) — Gaviota FFI bindings
