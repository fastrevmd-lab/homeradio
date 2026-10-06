# home-radio API contract (v1)

This is the shared contract between `radio-web` (Rust/axum) and the embedded
frontend. The design decisions:

- **UI style:** the UI is an **80s boom box**, not a 60s console.
- **Zone volumes:** each zone has its own volume knob.
- **Zone toggles:** there are two On/Off zone toggles.
- **PIN:** there is no PIN.

## Receiver volume mapping (verified live 2026-10-05)

Both zones use the same raw range, `0..=161`, with `db = raw * 0.5 - 80.5`. Two
points confirm it: raw 95 is -33.0 dB and raw 161 is 0.0 dB. The receiver's
`max_volume` is 161, so 0.0 dB is the hardware ceiling (161 is not +16.5 dB). The caps are:

| zone    | label      | cap_db | default start_db (only when coming out of standby) |
|---------|------------|--------|-----------------------------------------------------|
| `main`  | Media Room | -15.0  | -35.0                                               |
| `zone2` | Upstairs   | 0.0    | -5.0                                                |

## Zone "radio" semantics

The two toggles are labelled "MEDIA ROOM" and "UPSTAIRS". They reflect `zone.radio`,
which is defined as `power == "on" && input == "airplay"`.

- **Toggle ON:**
  - If the zone is in standby, power it on and set the default start volume.
  - Then `setInput airplay`.
- **Toggle OFF:**
  - If the input is `airplay`, put the zone in standby.
  - If the zone is on a non-network input (e.g. `hdmi1`, someone watching TV), do
    nothing and leave it alone (never kill the TV). The response is still 200 with
    the current state.

## Play policy

AirPlay connect powers on both zones and switches them to `airplay`. `POST /api/play`
works through these steps:

1. **Snapshot.** Before playing, record `{power, input, volume}` for both zones.
2. **Choose zones.**
   - `selected[z]` = `body.zones[z]` if it was given, else `snapshot[z].radio`.
   - If no zone is selected, return 409 with
     `{error:"no_zone", detail:"Turn on Media Room or Upstairs first"}`.
3. **Play.** Run `cliamp open 'cliamp://play?url=<percent-encoded url>'`.
4. **Wait, but only if the player was not already playing.** Poll YXC every 500 ms,
   for at most 10 s, until AirPlay has grabbed the zones. That is, an unselected zone
   whose snapshot was standby or a non-airplay input now shows `on`+`airplay`, or
   `netusb/getPlayInfo.playback == "play"` with input airplay. On a station change
   while already playing, skip the wait.
5. **Apply.** For each zone:
   - **Selected:** make sure it's on and set to `airplay`. If its snapshot was
     standby, set the default start volume.
   - **Not selected, snapshot standby:** set standby.
   - **Not selected, snapshot on a non-network input (TV):** setInput back to the
     snapshot input, keep it on, and restore the snapshot volume.
6. **Re-apply.** Wait 2 s, then re-check and re-apply step 5 once, because the
   receiver can flip late.

Steps 4–6 run in a background task, and the HTTP response returns right after
step 3. The policy is a pure function, `plan(snapshot, selected, current) -> Vec<Action>`,
and is unit-tested. One test must cover "main on hdmi1, upstairs selected → main
restored to hdmi1 and kept on".

Network inputs are `airplay`, `net_radio`, `server`, `spotify`, `pandora`,
`siriusxm`, `napster`, `mc_link`, `bluetooth` and `usb`. Any other input counts as
"someone's using it".

## Endpoints

All responses are JSON. Errors are `{ "error": "<code>", "detail": "<human message>" }`.
If the receiver is unreachable or times out (2 s), the response is **502**
`{error:"receiver_unreachable", detail:"Receiver not responding — is it unplugged?"}`.

Volume and mute calls for a zone in standby are refused with **409**
`{error:"zone_off", detail:"Turn <label> on first"}` without touching the receiver
(it rejects them while the zone is off).

| Method | Path | Body | Response |
|---|---|---|---|
| GET  | `/api/state` | – | `State` |
| GET  | `/api/stations` | – | `Stations` |
| POST | `/api/play` | `{"station":"big100","zones":{"main":false,"zone2":true}}`. `zones` is optional. | `State`. Unknown id → 400 `unknown_station`. No zone → 409 |
| POST | `/api/stop` | – | `State`. Stops cliamp only and leaves the zones alone |
| POST | `/api/power` | `{"on":bool}` | `State`. Master switch. Off stops the radio and puts **both** zones in standby, including a TV on hdmi1. On wakes `main` only and leaves input, volume and selection alone |
| POST | `/api/zone/{main\|zone2}/power` | `{"on":bool}` | `State` (toggle semantics above) |
| POST | `/api/zone/{main\|zone2}/volume` | `{"db":-20.0}` **or** `{"step":2}`. `step` is in raw units of 0.5 dB and may be negative. | `State`. Clamped to `[min_db, cap_db]` and never rejected for being too high |
| POST | `/api/zone/{main\|zone2}/mute` | `{"mute":bool}` | `State` |
| GET  | `/api/events` | – | SSE stream (below) |
| GET  | `/api/vis` | – | SSE spectrum stream (below) |
| GET  | `/healthz` | – | 200 or 503 `{ok, cliamp:{ok,error}, receiver:{ok,error}, airplay:{connected}}` (`airplay.connected` = the RAOP sink unit is active; informational, does not affect `ok`) |

**Zone names.** Any zone name other than `main` or `zone2` is a 404. Station ids
and zone names are validated server-side, and URLs never come from the client.

**Request size.** Bodies are limited to 4 KiB.

### `State`

```json
{
  "player": {
    "state": "playing",            // "playing" | "paused" | "stopped" | "unknown"
    "station": "big100",           // registry id or null
    "station_name": "BIG 100.3 – DC Classic Rock (WBIG)",
    "artist": "Billy Idol",        // parsed, may be null
    "title": "Rebel Yell",         // parsed, may be null
    "error": null                  // string when cliamp IPC is down
  },
  "receiver": {
    "ok": true,
    "error": null,                 // "Receiver not responding — is it unplugged?"
    "playback": "play",            // netusb playback, or null
    "airplay_active": true         // playback=="play" && some zone is on+airplay  → signal lamp
  },
  "zones": {
    "main":  { "label": "Media Room", "power": "standby", "radio": false, "input": "airplay",
               "volume": 95, "db": -33.0, "mute": false,
               "cap_db": -15.0, "min_db": -80.5, "step_db": 0.5 },
    "zone2": { "label": "Upstairs",   "power": "on",      "radio": true,  "input": "airplay",
               "volume": 151, "db": -5.0, "mute": false,
               "cap_db": 0.0,  "min_db": -80.5, "step_db": 0.5 }
  },
  "updated_ms": 1791240043000
}
```

When the receiver is unreachable, `zones` is `null`.

### `Stations`

```json
{
  "groups": [
    { "id": "rock",   "label": "ROCK",
      "stations": [ { "id": "big100", "name": "BIG 100.3 – DC Classic Rock (WBIG)", "short": "BIG 100", "genre": "Classic Rock" } ] },
    { "id": "cliamp", "label": "CLIAMP",
      "stations": [ { "id": "lofi", "name": "Lofi", "short": "Lofi", "genre": "Lofi" } ] }
  ]
}
```

- The `rock` group comes from `stations.toml`, in file order. The first 7 stations
  are the preset buttons.
- The `cliamp` group is fetched from `https://radio.cliamp.stream/stations` at
  startup and every 6 h, and cached on disk. Ids that collide with rock ids are
  dropped, and streams must be `https://` or `http://`.

### `GET /api/events` (SSE)

- **Updates.** Each state change is sent as `event: state` with `data: <State JSON>`.
  The first event goes out immediately on connect.
- **Keepalive.** A `: ping` comment is sent every 15 s.
- **Sources.** Changes come from these places:
  - `cliamp remote events runtime.state`, an NDJSON child process that is restarted
    if it dies
  - a YXC poll every 3 s
  - every mutating API call

### GET /api/vis (SSE)

A live spectrum feed for an LED visualizer.

- **Frames.** Each frame is sent as `event: vis` with `data: {"bands":[...]}`.
  `bands` holds 10 values in `0..1`, passed through exactly as cliamp emits them
  (cliamp has already clamped them).
- **Keepalive.** A `: ping` comment is sent every 15 s.
- **Rate.** At most 15 frames per second per client. If frames arrive faster, the
  newest one wins.
- **Playing.** While the player state is `playing`, frames are relayed from the
  source as they arrive. Nothing is sent on connect until the first frame.
- **Not playing.** While the player is not `playing`, each client gets a single
  all-zero frame, `{"bands":[0,0,0,0,0,0,0,0,0,0]}`, when it connects and again
  whenever the player stops. After that, nothing except keepalives is sent.
- **Source.** One shared child process, `cliamp visstream --fps 15`, feeds every
  client through an in-process broadcast channel. It inherits the same
  `CLIAMP_CONFIG_DIR` as the other cliamp calls and prints NDJSON lines like
  `{"ok":true,"visualizer":"Bars","bands":[0.32,0.47,0.33,0.35,0.34,0.28,0.16,0,0,0]}`.
  - **Lifecycle.** It runs only while at least one client is connected AND the
    player is `playing`. The conditions are checked about every 1 s, and when
    either stops holding the child is killed. If it dies while they still hold, it
    is restarted with a backoff of 1 s, doubling up to 30 s.
  - **Bad lines.** A line that is not valid JSON, has `"ok":false`, or has no
    `bands` array of exactly 10 numbers is skipped.
  - **Slow clients.** A client that falls behind skips ahead to the newest frames.

## cliamp facts (v2.3.0, verified)

- **State.** `cliamp remote state` →
  `{"ok":true,"snapshot":{"state":"stopped|playing|paused","logical_track":{"title":"…","path":"<url>","stream":true}, …}}`
- **Events.** `cliamp remote events runtime.state` streams NDJSON lines like
  `{"event":"runtime.state","seq":1,"data":{<same as snapshot>}}`.
- **Spectrum.** `cliamp visstream --fps <1-60>` prints NDJSON lines like
  `{"ok":true,"visualizer":"Bars","bands":[0.32,0.47,0.33,0.35,0.34,0.28,0.16,0,0,0]}`.
  It needs the same `CLIAMP_CONFIG_DIR` as the other cliamp calls.
- **Play and stop.** Play with `cliamp open 'cliamp://play?url=<url>'` and stop with
  `cliamp stop`.
- **Config directory.** `CLIAMP_CONFIG_DIR` sets the config dir, and the socket is
  `$CLIAMP_CONFIG_DIR/cliamp.sock`. Its path must stay under 108 characters.
- **Now playing.** The now-playing text is `snapshot.logical_track.title`. It may be
  an iHeart blob like `title="Rebel Yell",artist="BILLY IDOL",url="song_spot=..."` or
  `text="Spiderwebs" song_spot="M" ...`, or a plain `Artist - Title` ICY string.
- **Station id.** Map the station by `logical_track.path` matching a registry URL,
  falling back to the last id that radio-web played.
