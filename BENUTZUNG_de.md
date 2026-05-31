---
title: "LiRu-Bot — Benutzungs-Anleitung"
date: "2026"
geometry: "a4paper, margin=2cm"
fontsize: 11pt
---

# Übersicht

`liru-bot` (**Li**chess **Ru**st **Bot**) ist ein Rust-Port des offiziellen
Python-[lichess-bot](https://github.com/lichess-bot-devs/lichess-bot). Er
verbindet einen lokalen UCI-Schachmotor mit deinem Bot-Account auf
**lichess.org**, akzeptiert Challenges, spielt komplette Partien und kann
optional auch von sich aus andere Bots herausfordern.

**Features:**

- UCI-Engine-Lifecycle (spawn, options, search, ponder, gameover)
- Polyglot-Eröffnungsbücher
- Syzygy-Tablebases (lokal, bis 6-Mann) + optional Gaviota (FFI)
- Online-Sources: chessdb, Lichess Cloud Analysis, Opening Explorer, Lichess EGTB
- Chat-Handler (`!help`, `!name`, `!eval`, `!queue`, `!wait`) + Greetings/Goodbyes
- Outbound-Matchmaking (eigene Challenges raussenden)
- PGN-Recording (game/opponent/all-Grouping)
- Concurrency-Gate, Korrespondenz-Pickup beim Start, Online-Blocklist-Refresh
- **Optional** In-Process-Engine-Backend (`--features embedded`) für eine echte
  Wall-Clock-Deadline der Suche — siehe [Embedded-Engine](#embedded-engine-optional)

---

# Voraussetzungen

| Komponente   | Was du brauchst                                                      |
|--------------|----------------------------------------------------------------------|
| Bot-Account  | Ein Lichess-Account, per `/api/bot/account/upgrade` auf BOT umgestuft |
| API-Token    | Mit Scope `bot:play` — unter <https://lichess.org/account/oauth/token> |
| UCI-Engine   | Ausführbare Engine-Binary (Stockfish, lc0, …)                        |
| Config-Datei | YAML-Datei mit Pfaden, Token, Challenge-Filtern                      |
| Rust         | Rust 1.75 oder neuer (nur zum Selbst-Bauen)                          |
| Optional     | Syzygy-Tablebases, Polyglot-`.bin`-Buch                             |

---

# Bot starten

```powershell
liru-bot.exe --config "C:\Pfad\zu\config.yml"
```

Wenn die Config gültig ist und der Token funktioniert, verbindet sich der Bot
mit Lichess, öffnet den Event-Stream und wartet auf Challenges.

Sauberes Herunterfahren via **Ctrl-C** — laufende Spiele werden bis zum
Drain-Timeout abgewartet, dann abgebrochen.

---

# CLI-Optionen

## Globale Optionen (vor jedem Subcommand)

| Option                | Default              | Beschreibung                                |
|-----------------------|----------------------|---------------------------------------------|
| `-c`, `--config PATH` | `./config.yml`       | Pfad zur YAML-Config                        |
| `-l`, `--log FILTER`  | aus `RUST_LOG`/`info`| Log-Filter (`tracing-subscriber`-Syntax)    |

Beispiele für den Log-Filter:

- `info` — Standard, eine Zeile pro wichtigem Ereignis
- `debug` — sehr gesprächig, alles inkl. UCI-Roundtrips
- `liru_bot=debug,info` — Bot-Modul in `debug`, alles andere in `info`
- `liru_bot=debug,reqwest=warn` — Bot-Debug, HTTP-Layer ruhig

## Subcommand: `run` (Default)

```powershell
liru-bot.exe --config <cfg>            # äquivalent
liru-bot.exe --config <cfg> run        # explizit
```

Startet den Bot-Loop. Keine zusätzlichen Argumente — alles steht in der Config.

## Subcommand: `list-bots`

Listet online-Bots, gefiltert nach Rating und/oder Speed.

```powershell
liru-bot.exe --config <cfg> list-bots [OPTIONEN]
```

| Option           | Default | Beschreibung                                              |
|------------------|---------|-----------------------------------------------------------|
| `--min-rating N` | `0`     | Nur Bots mit Rating ≥ N in der gewählten Speed            |
| `--speed S`      | (alle)  | `bullet`, `blitz`, `rapid`, `classical`, `ultraBullet`    |
| `--limit N`      | `300`   | Wie viele Bots Lichess maximal liefern soll               |

```powershell
# Top-Blitz-Bots (ab 2800)
liru-bot.exe --config "C:\Pfad\zu\config.yml" list-bots --min-rating 2800 --speed blitz
```

---

# Config-Datei (Kurzreferenz)

Annotierte Vorlage: siehe [`config.yml.example`](config.yml.example). Volle Doku:
das Upstream-`config.yml.default` von lichess-bot — LiRu-Bot spiegelt dessen
Konfigurations-Oberfläche.

```yaml
token: "lip_..."              # Lichess API-Token (bot:play scope)
url: "https://lichess.org/"   # in der Regel unverändert lassen

abort_time: 20                # Sekunden bis Abbruch einer inaktiven Challenge
move_overhead: 1000           # Millisekunden Puffer für Netzwerk-Latenz

pgn_directory: ""             # leer → kein PGN-Save
pgn_file_grouping: "game"     # game | opponent | all

engine:
  dir: "C:\\Pfad\\zur\\engine"
  name: "engine.exe"
  protocol: "uci"             # uci | xboard | homemade
  uci_ponder: false

  uci_options:
    Threads: 1
    Hash: 256
    # EvalFile: "C:/Pfad/zu/net.nnue"

  polyglot:
    enabled: false
    book:
      standard: []            # z.B. ["C:/Pfad/zu/buch.bin"]
    selection: "weighted_random"   # auf polyglot-Ebene, NICHT unter book
    max_depth: 20

  lichess_bot_tbs:
    syzygy:
      enabled: false
      paths: []               # z.B. ["C:/Pfad/zu/3-4-5-WDL"]
      max_pieces: 6
      move_quality: "best"

challenge:
  concurrency: 1              # max parallele Spiele
  variants: ["standard"]
  time_controls: ["bullet", "blitz", "rapid"]
  modes: ["casual", "rated"]

matchmaking:
  allow_matchmaking: false    # true → Bot challengt selbst
  challenge_initial_time: [180, 300]
  challenge_increment: [0, 2]
  opponent_min_rating: 2400
  opponent_max_rating: 4000
```

> **YAML-Stolperfalle:** `selection` gehört auf die `polyglot`-Ebene, nicht unter
> `book`. Pythons Parser akzeptiert die falsche Einrückung stillschweigend; der
> Rust-Parser meldet `expected sequence`.

---

# Chat-Befehle im Spiel

| Befehl       | Wer darf?              | Antwort                                              |
|--------------|------------------------|------------------------------------------------------|
| `!help`      | jeder                  | Liste der unterstützten Befehle                      |
| `!commands`  | jeder                  | (Alias für `!help`)                                  |
| `!name`      | jeder                  | `BotName running EngineName (liru-bot v…)`           |
| `!eval`      | Bot selbst / Zuschauer | Aktuelle Engine-Stats (depth, score, …)              |
| `!eval`      | Gegner                 | "I don't tell that to my opponent, sorry."           |
| `!wait`      | Gegner (vor 1. Zug)    | Bot zögert länger mit Abort                          |
| `!queue`     | jeder                  | Liste der gerade eingehenden Challenges              |

---

# Embedded-Engine (optional)

Der Default-Build spricht die Engine als **Subprozess** über UCI an und hängt an
keiner bestimmten Engine. Zusätzlich gibt es ein optionales In-Process-Backend für
die [clrsrc](https://github.com/clrsrc/clrsrc)-Engine (`--features embedded`), das
der Suche **eine** autoritative absolute Wall-Clock-Deadline gibt statt der
gestapelten Subprozess-Zeit-Approximation. Vertrag: clrsrcs `EMBEDDED.md`.

> **Lizenz-Hinweis Kombiwerk.** Mit `--features embedded` wird clrsrc
> (GPL-3.0-or-later) in LiRu-Bot (AGPL-3.0-or-later) als ein kombiniertes Werk
> gelinkt (erlaubt nach AGPLv3 §13 / GPLv3 §13). Jeder Teil behält seine Lizenz;
> der clrsrc-Anteil bleibt GPL-3.0-or-later. Betrieb des Kombi-Binaries als
> Netzwerk-Service löst die AGPL-§13-Pflicht aus, den Corresponding Source des
> Ganzen anzubieten. Der **Default-Build** (Subprozess) linkt clrsrc **nicht** und
> ist ein reines AGPL-3.0-Artefakt.

---

# Build (für Entwickler)

```powershell
# Standard Release-Build (mit Gaviota-FFI)
cargo build --release

# Ohne Gaviota (kein C-Toolchain nötig)
cargo build --release --no-default-features

# Mit eingebetteter clrsrc-Engine (nach clrsrc v1.1.0-Release)
cargo build --release --features embedded
```

Die fertige Binary liegt in `target\release\liru-bot.exe`.

---

# Troubleshooting

| Symptom                                              | Ursache / Fix                                                                  |
|------------------------------------------------------|---------------------------------------------------------------------------------|
| `expected sequence`-Fehler beim YAML-Laden            | `selection` falsch eingerückt — gehört auf `polyglot`-Ebene, nicht `book`      |
| `tb_init failed` / Gaviota schweigt                  | Pfade falsch oder Tablebases fehlen; Build ohne `--no-default-features` testen |
| Bot akzeptiert keine Challenge trotz `concurrency: 1` | Ein vorheriges Game noch aktiv → Logs nach `gameStart` ohne `gameFinish` prüfen |
| `event stream ended`                                  | Lichess hat die Verbindung geschlossen — Bot beendet sich; Supervisor neustarten |
| `accept failed: 400 Bad Request` beim Annehmen        | Challenge schon abgelaufen / zurückgezogen; harmlos                            |

---

# Quellen / Referenzen

- Python-Original: <https://github.com/lichess-bot-devs/lichess-bot>
- Lichess Bot-API: <https://lichess.org/api#tag/Bot>
- UCI-Protokoll: <http://wbec-ridderkerk.nl/html/UCIProtocol.html>
- shakmaty (Rust-Schachbibliothek): <https://docs.rs/shakmaty>
