# Station discovery: MY band, ★ and search

**Status:** approved design, 2026-10-06.

## Goal

Let anyone in the household find stations beyond the curated ROCK list and
keep the ones they like, without accounts or setup. It has to be simple: one
tap to keep what is playing, one drawer to search.

## User experience

- **Band switch, three positions:** `ROCK | CLIAMP | MY`. Presets (first 7) and
  the tuning dial follow the selected band, exactly as ROCK and CLIAMP do today.
- **★ on the LCD**, right edge. Filled when the current station is in MY.
  Tapping toggles add/remove. Disabled (dim) when nothing has been played yet.
  Works for ROCK, CLIAMP and Radio Browser stations alike.
- **🔍 button** next to the band switch opens the **search drawer**: a bottom
  sheet on phones, a panel over the control panel on desktop. Contents:
  - a search box (debounced, 300 ms; at least 2 characters);
  - genre chips: Rock, Classic Rock, Alt, Jazz, Blues, Country, Oldies,
    Classical, Lofi, News, Talk. A chip searches by tag; the server maps each
    label to a Radio Browser tag (lowercase label, except Alt → `alternative`), and typing searches by name, narrowed by the selected chip, if any;
  - up to 30 results, each row showing name, genre (first tag), country and
    bitrate, with **▶** (play now, using the current zone selection, exactly
    like a preset) and **★** (add to / remove from MY);
  - states: idle hint, loading, "no stations found", "search unavailable".
  - Escape, the close button or a tap outside closes it; focus returns to 🔍.
- **Empty MY band:** the dial shows "★ a station to keep it here" and the
  presets are blank.

## Shared MY list

- One household list. Anyone can add or remove; there are no accounts.
- Stored at `<cache_dir>/my-stations.json` (default
  `/var/lib/home-radio/my-stations.json`), outside the bundle, so it survives
  redeploys.
- Each entry is either a **reference** to an existing ROCK/CLIAMP id
  (`{"ref":"big100"}`) or a **Radio Browser station**
  (`{"id":"rb-<uuid>","name":…,"short":…,"genre":…,"url":…}`), the URL kept
  server-side only.
- Order: insertion order, newest last. Cap: **50** entries; adding a 51st
  returns 409 `my_full`. Adding a station already in MY is a no-op. Removing one
  not in MY is a no-op (idempotent).
- Writes are atomic: write a temp file in the same directory, fsync, rename.
  A corrupt or missing file loads as an empty list and is logged, never fatal.
- A reference whose ROCK/CLIAMP station later disappears is hidden from the MY
  group (not deleted) and reappears if the station comes back.

## Server

### Invariant

The browser never sends a stream URL. It only sends ids. The server resolves
every id to a URL itself: ROCK/CLIAMP from the registry, MY Radio Browser
entries from the stored list, and unsaved search results from Radio Browser by
uuid. Nobody can make the receiver play an arbitrary URL.

### Radio Browser client (`src/radiobrowser.rs`, behind a trait for tests)

- Base: `https://all.api.radio-browser.info` resolved to one server at startup
  (fall back to `https://de1.api.radio-browser.info`), with a `User-Agent:
  homeradio/<version>` header as Radio Browser asks.
- `search(name, tag)` → `GET /json/stations/search?name=…&tag=…&limit=60&
  hidebroken=true&order=clickcount&reverse=true`. Filtered server-side, then
  truncated to 30:
  - `lastcheckok == 1`, `hls == 0`;
  - codec MP3 or AAC/AAC+ (case-insensitive);
  - `url_resolved` is `http`/`https`;
  - de-duplicated by name + `url_resolved`.
- `by_uuid(uuid)` → `GET /json/stations/byuuid/<uuid>`, same filters.
- Timeouts: 4 s per request. Any error → the caller's "search unavailable"
  path; nothing else in the app depends on Radio Browser.
- Text from Radio Browser is untrusted: trimmed, control characters stripped,
  names capped at 80 characters and tags at 40. The UI renders it with
  `textContent` only.

### Ids

- ROCK/CLIAMP ids are unchanged.
- Radio Browser ids are `rb-` followed by a lowercase hyphenated UUID; anything
  else is rejected with 400 `bad_station`.

### API

| Method | Path | Body | Response |
|---|---|---|---|
| GET | `/api/stations` | – | Adds a `my` group to the existing registry. Each station has `id`, `name`, `short`, `genre`, and `in_my` |
| GET | `/api/search?q=…&genre=…` | – | `{"results":[{"id":"rb-…","name","genre","country","bitrate","in_my"}]}`. Needs `q` (≥2 chars) or `genre` (one of the chip names) → else 400 `bad_query`. Radio Browser down → 503 `search_unavailable` |
| POST | `/api/my` | `{"station":"<id>"}` | `Stations`. Unknown id → 400 `unknown_station`; full → 409 `my_full` |
| DELETE | `/api/my/{id}` | – | `Stations` |
| POST | `/api/play` | unchanged | Also accepts `rb-` ids: MY first, then a short-lived cache of recent search results (10 min, 200 entries), then `by_uuid` |

The SSE stream gains a `stations` event after MY changes, so other open
browsers refresh their MY band.

`State.player.station_name` for an `rb-` station comes from the MY entry or
the search cache, so the LCD shows a real name.

## Web UI

- `web/index.html`: three-way band switch, ★ on the LCD, 🔍 button, drawer
  markup (hidden by default, `role="dialog"`, `aria-modal`, labelled).
- `web/app.js`: band state becomes `rock | cliamp | my`; MY rendering; ★ toggle
  for the current station; drawer open/close, debounced search, chips, results
  with ▶ and ★; refresh on the `stations` SSE event.
- `web/app.css`: segmented switch, star, drawer (bottom sheet ≤ 900 px, side
  panel above), in the existing boom box style and tokens.

## Out of scope

Reordering MY, per-person lists, accounts, "near me", radio roulette, station
logos (Radio Browser favicons would be third-party requests from the page).

## Testing

- **Rust unit tests:** Radio Browser filter (codec, hls, lastcheckok, scheme,
  de-dup, truncation, text sanitising), id validation, MY store (add, dedupe,
  cap, remove, atomic save, corrupt-file load, hidden dangling references).
- **Integration tests** (fake Radio Browser): search success, bad query, Radio
  Browser down → 503; add/remove via the API; `my` group in `/api/stations`;
  playing an `rb-` id resolves the URL server-side from MY, the cache, and
  `by_uuid`; a client can never supply a URL; MY survives an app restart.
- **Browser check:** the drawer on phone and desktop widths, the ★ toggle, the
  three-way switch, an empty MY band, and "search unavailable".
