// State management
let stations = { groups: [] };
let currentState = null;
let currentBand = 'rock';
const BANDS = ['rock', 'cliamp', 'my'];
const PRESET_SLOTS = 7;
let selectedStationIndex = 0;
let isDraggingDial = false;
let isDraggingKnob = false;
let activeKnob = null;
let eventSource = null;
let lastSeenStation = null; // band auto-follow only when this changes
let lastPlayedId = null; // the star's target while nothing is playing
let stationsRevision = 0; // revision of the Stations snapshot on screen; older ones (same boot) are ignored
let stationsBoot = null; // server boot id of that snapshot; a new boot resets the revision
let stationsStale = false; // the event stream dropped: refetch stations once it is back
let pendingFollowStation = null; // station change that arrived mid-drag; applied on drag end
let lcdHoldActive = false;  // an error message currently owns the LCD
let lcdHoldTimer = null;
let powerRequestInFlight = false; // master power POST pending; the button stays disabled
const LOCAL_EDIT_HOLD_MS = 600; // ignore server knob values right after local input

// DOM elements
const elements = {
    nowPlaying: document.getElementById('nowPlaying'),
    signalLamp: document.getElementById('signalLamp'),
    dialNeedle: document.getElementById('dialNeedle'),
    dialScale: document.getElementById('dialScale'),
    dialControl: document.getElementById('dialControl'),
    bandSwitch: document.getElementById('bandSwitch'),
    bandOptions: Array.from(document.querySelectorAll('.band-option')),
    lcdStar: document.getElementById('lcdStar'),
    boombox: document.querySelector('.boombox'),
    searchBtn: document.getElementById('searchBtn'),
    searchBackdrop: document.getElementById('searchBackdrop'),
    searchDrawer: document.getElementById('searchDrawer'),
    searchClose: document.getElementById('searchClose'),
    searchInput: document.getElementById('searchInput'),
    searchChips: document.getElementById('searchChips'),
    searchStatus: document.getElementById('searchStatus'),
    searchResults: document.getElementById('searchResults'),
    presetButtons: document.getElementById('presetButtons'),
    playBtn: document.getElementById('playBtn'),
    stopBtn: document.getElementById('stopBtn'),
    powerBtn: document.getElementById('powerBtn'),
    mainToggle: document.getElementById('mainToggle'),
    zone2Toggle: document.getElementById('zone2Toggle'),
    mainKnob: document.getElementById('mainKnob'),
    zone2Knob: document.getElementById('zone2Knob'),
    mainVolumeLabel: document.getElementById('mainVolumeLabel'),
    zone2VolumeLabel: document.getElementById('zone2VolumeLabel'),
    mainLed: document.getElementById('mainLed'),
    zone2Led: document.getElementById('zone2Led'),
    visCanvas: document.getElementById('visCanvas'),
    speakers: {
        main: document.querySelector('.speaker[data-zone="main"]'),
        zone2: document.querySelector('.speaker[data-zone="zone2"]')
    }
};

// Utility: throttle function calls
function throttle(func, delay) {
    let lastCall = 0;
    let timeout = null;
    return function(...args) {
        const now = Date.now();
        const remaining = delay - (now - lastCall);

        clearTimeout(timeout);

        if (remaining <= 0) {
            lastCall = now;
            func.apply(this, args);
        } else {
            timeout = setTimeout(() => {
                lastCall = Date.now();
                func.apply(this, args);
            }, remaining);
        }
    };
}

// Utility: debounce function calls
function debounce(func, delay) {
    let timeout = null;
    return function(...args) {
        clearTimeout(timeout);
        timeout = setTimeout(() => func.apply(this, args), delay);
    };
}

// API calls
async function apiCall(method, path, body = null) {
    try {
        const options = {
            method,
            headers: body ? { 'Content-Type': 'application/json' } : {}
        };
        if (body) {
            options.body = JSON.stringify(body);
        }

        const response = await fetch(`/api${path}`, options);
        const data = await response.json();

        if (!response.ok) {
            showRequestError(data.detail || data.error || 'Request failed');
            return null;
        }

        return data;
    } catch (error) {
        showRequestError('Network error: ' + error.message);
        return null;
    }
}

/**
 * Report a failed request. The LCD shows it, but the search drawer covers the
 * LCD, so while the drawer is open the drawer's status line says it too.
 * (Only request failures: the receiver's own error is re-sent on every state
 * push and would keep overwriting the search status.)
 * @param {string} message the error text
 */
function showRequestError(message) {
    showError(message);
    if (isSearchOpen()) setSearchStatus(message);
}

// Initialize
async function init() {
    showLoading();

    // Fetch stations
    const stationsData = await apiCall('GET', '/stations');
    if (stationsData) {
        stations = stationsData;
        stationsRevision = stationsData.revision ?? 0;
        stationsBoot = stationsData.boot ?? null;
        renderPresetButtons();
    }
    refreshDial();

    // Fetch initial state
    const state = await apiCall('GET', '/state');
    if (state) {
        updateUI(state);
    }

    // Setup event source for live updates
    setupEventSource();

    // Setup event listeners
    setupEventListeners();
}

// Event source for SSE
function setupEventSource() {
    if (eventSource) {
        eventSource.close();
    }

    eventSource = new EventSource('/api/events');

    eventSource.addEventListener('state', (event) => {
        const state = JSON.parse(event.data);
        updateUI(state);
    });

    // Another browser changed MY: the event carries the whole Stations payload
    eventSource.addEventListener('stations', (event) => {
        applyStations(JSON.parse(event.data));
    });

    eventSource.onerror = () => {
        showError('Connection lost, reconnecting...');
        // EventSource will auto-reconnect; MY events missed meanwhile are refetched then
        stationsStale = true;
    };

    eventSource.onopen = () => {
        if (!stationsStale) return;
        refetchStations();
    };
}

/**
 * Fetch the station registry again and repaint it (after a dropped event stream).
 * The stale flag is cleared only once a refetch succeeded, so a failed one is
 * retried on the next reconnect.
 */
async function refetchStations() {
    const data = await apiCall('GET', '/stations');
    if (!data) return;
    applyStations(data);
    stationsStale = false;
}

/**
 * Adopt a new Stations payload (from a MY change here or elsewhere) and repaint
 * everything that depends on it, keeping the station the dial was on.
 * A snapshot older than the one already shown (by `revision`) is ignored, so
 * out-of-order SSE events, responses and refetches cannot roll the UI back.
 * Revisions are only comparable within one server `boot`: a snapshot from a
 * different boot (the server restarted) is always accepted and resets the
 * remembered revision.
 * @param {?{groups: Array, revision?: number, boot?: string}} data the Stations JSON, or null when a request failed
 */
function applyStations(data) {
    if (!data || !Array.isArray(data.groups)) return;
    if (typeof data.revision === 'number') {
        const sameBoot = (data.boot ?? null) === stationsBoot;
        if (sameBoot && data.revision < stationsRevision) return;
        stationsRevision = data.revision;
        stationsBoot = data.boot ?? null;
    }
    const selectedId = getCurrentGroup()?.stations[selectedStationIndex]?.id;
    stations = data;
    const keptIndex = selectedId ? (getCurrentGroup()?.stations.findIndex(s => s.id === selectedId) ?? -1) : -1;
    if (keptIndex !== -1) selectedStationIndex = keptIndex;
    renderPresetButtons();
    refreshDial();
    renderStar();
    refreshSearchStars();
}

/** @returns {boolean} whether the station is in the MY list */
function isInMy(stationId) {
    return stations.groups.find(g => g.id === 'my')?.stations.some(s => s.id === stationId) ?? false;
}

/** Add the station to MY, or remove it when it is already there. */
async function toggleMy(stationId, isSaved) {
    const data = isSaved
        ? await apiCall('DELETE', `/my/${encodeURIComponent(stationId)}`)
        : await apiCall('POST', '/my', { station: stationId });
    applyStations(data);
}

/** @returns {?string} the station the LCD star acts on: the current one, else the last played */
function starTargetId() {
    return currentState?.player.station ?? lastPlayedId;
}

// Paint the LCD star: disabled with no station yet, filled when the station is in MY
function renderStar() {
    const targetId = starTargetId();
    const isSaved = targetId ? isInMy(targetId) : false;
    elements.lcdStar.disabled = !targetId;
    elements.lcdStar.setAttribute('aria-pressed', String(isSaved));
    // The label stays constant; aria-pressed carries the state.
}

// Render the 7 preset keys for the current band using DOM APIs only (no HTML
// strings). A band with fewer than 7 stations leaves the extra keys blank and disabled.
function renderPresetButtons() {
    const presetStations = (getCurrentGroup()?.stations ?? []).slice(0, PRESET_SLOTS);

    const buttons = Array.from({ length: PRESET_SLOTS }, (_, index) => {
        const station = presetStations[index];
        const button = document.createElement('button');
        button.type = 'button';
        button.className = 'preset-btn';
        button.dataset.index = String(index);
        if (!station) {
            button.disabled = true;
            button.setAttribute('aria-label', 'Empty preset');
            return button;
        }
        button.dataset.station = station.id;
        button.textContent = station.short;
        button.title = station.name;
        button.addEventListener('click', () => playStation(station.id));
        return button;
    });
    elements.presetButtons.replaceChildren(...buttons);
    updatePresetSelection(currentState?.player.station ?? null);
}

// Dial label layout (computed in renderDialScale, applied by updateDialLabels)
const DIAL_LABEL_FONT_PX = 9;
const DIAL_LABEL_GAP_PX = 6;
const DIAL_LABEL_MAX_CHARS = 10;
let dialLayout = null;

function dialLabelText(station) {
    const text = station.short;
    return text.length > DIAL_LABEL_MAX_CHARS ? text.slice(0, DIAL_LABEL_MAX_CHARS - 1) + '\u2026' : text;
}

// Render dial scale: one tick per station, labels only where they fit.
// Labels go on one row if every k-th station fits side by side, else on two
// alternating rows; k grows until the measured label widths no longer overlap.
function renderDialScale() {
    const group = getCurrentGroup();
    if (!group || group.stations.length === 0) {
        dialLayout = null;
        const empty = document.createElement('div');
        empty.className = 'dial-empty';
        const isEmptyMyBand = currentBand === 'my';
        empty.classList.toggle('dial-hint', isEmptyMyBand);
        empty.textContent = isEmptyMyBand ? '\u2605 a station to keep it here' : 'NO STATIONS';
        elements.dialScale.replaceChildren(empty);
        return;
    }

    const containerWidth = elements.dialScale.offsetWidth || 300;
    const availableWidth = containerWidth - 20;
    const count = group.stations.length;
    const stepPx = availableWidth / Math.max(count - 1, 1);

    const measureContext = document.createElement('canvas').getContext('2d');
    measureContext.font = `${DIAL_LABEL_FONT_PX}px ${getComputedStyle(elements.dialScale).fontFamily}`;
    const texts = group.stations.map(dialLabelText);
    const widths = texts.map(text => measureContext.measureText(text).width);

    // Smallest k whose labelled stations fit (one row, else two alternating rows)
    let step = count;
    let rows = 1;
    for (let k = 1; k <= count; k++) {
        let widest = 0;
        for (let i = 0; i < count; i += k) widest = Math.max(widest, widths[i]);
        const needed = widest + DIAL_LABEL_GAP_PX;
        if (k * stepPx >= needed) { step = k; rows = 1; break; }
        if (2 * k * stepPx >= needed) { step = k; rows = 2; break; }
    }

    const xs = group.stations.map((_, index) => index * stepPx);
    dialLayout = { groupId: group.id, count, step, rows, xs, widths, glassWidth: containerWidth + 20 };

    const markers = group.stations.map((station, index) => {
        const marker = document.createElement('div');
        marker.className = 'dial-marker';
        marker.style.left = `${xs[index]}px`;
        marker.dataset.index = String(index);

        const label = document.createElement('span');
        label.textContent = texts[index];
        marker.appendChild(label);
        return marker;
    });
    elements.dialScale.replaceChildren(...markers);
    updateDialLabels();
}

// Show labels for the k-th stations plus the selected one (highlighted)
function updateDialLabels() {
    const group = getCurrentGroup();
    if (!dialLayout || !group || dialLayout.groupId !== group.id || dialLayout.count !== group.stations.length) return;

    const { step, rows, xs, widths, glassWidth } = dialLayout;
    const selected = selectedStationIndex;
    const selectedIsLabelled = selected % step === 0;
    const markers = elements.dialScale.querySelectorAll('.dial-marker');

    markers.forEach((marker, index) => {
        const labelled = index % step === 0;
        const isSelected = index === selected;
        const row2 = labelled && rows === 2 && (index / step) % 2 === 1;

        // A labelled neighbour in the same row as an off-grid selected label gets hidden
        let hidden = false;
        if (labelled && !selectedIsLabelled) {
            const sameRowAsSelected = !row2;
            const overlap = Math.abs(xs[index] - xs[selected]) < (widths[index] + widths[selected]) / 2 + DIAL_LABEL_GAP_PX;
            hidden = sameRowAsSelected && overlap;
        }

        marker.classList.toggle('has-label', isSelected || (labelled && !hidden));
        marker.classList.toggle('row-2', row2);
        marker.classList.toggle('selected', isSelected);

        // Keep labels fully inside the glass at both ends
        const span = marker.firstChild;
        const centre = xs[index] + 10;
        const half = widths[index] / 2;
        const shift = Math.max(0, 2 - (centre - half)) - Math.max(0, centre + half - (glassWidth - 2));
        span.style.transform = shift ? `translateX(${shift}px)` : '';
    });
}

// Update UI from state
function updateUI(state) {
    currentState = state;

    // Now playing
    updateNowPlaying(state.player);

    // Signal lamp (STEREO indicator)
    elements.signalLamp.classList.toggle('active', Boolean(state.receiver.airplay_active));

    // Receiver error
    if (!state.receiver.ok && state.receiver.error) {
        showError(state.receiver.error);
    }

    // Follow the playing station's band ONLY when the station changes, so an
    // explicit band-switch selection is never overridden by routine updates.
    const stationId = state.player.station;
    if (stationId !== lastSeenStation) {
        lastSeenStation = stationId;
        if (stationId && isDraggingDial) {
            pendingFollowStation = stationId;
        } else if (stationId) {
            followStation(stationId);
        }
    }

    // Preset button selection and the LCD star
    updatePresetSelection(stationId);
    if (stationId) lastPlayedId = stationId;
    renderStar();

    // Zones
    if (state.zones) {
        updateZone('main', state.zones.main);
        updateZone('zone2', state.zones.zone2);
    }

    renderPowerButton(state);

    // Visualizer feed + zone-linked speakers
    syncPlayback(state);
}

/**
 * Paint the master power button: lit when any zone is on, disabled while the
 * receiver is unreachable (no zones) or a power request is in flight.
 * @param {{zones: ?{main: {power: string}, zone2: {power: string}}}} state
 */
function renderPowerButton(state) {
    const anyZoneOn = Boolean(state.zones) && Object.values(state.zones).some(zone => zone.power === 'on');
    elements.powerBtn.classList.toggle('on', anyZoneOn);
    elements.powerBtn.setAttribute('aria-pressed', String(anyZoneOn));
    elements.powerBtn.disabled = !state.zones || powerRequestInFlight;
}


// ---------------------------------------------------------------------------
// Spectrum visualizer + zone-linked speakers
//
// /api/vis (SSE, `event: vis`, {"bands":[10 floats 0..1]} at ~15 fps) is opened
// only while the player is playing. The canvas is painted from
// requestAnimationFrame with attack/decay easing and falling peak-hold dots; the
// loop sleeps once everything has settled at zero. The left speaker pair
// (MEDIA ROOM) pulses only while that zone plays radio, the right pair (UPSTAIRS)
// likewise, driven by the bass bands when a live feed exists and by a gentle CSS
// pulse otherwise.
// ---------------------------------------------------------------------------
const VIS_COLUMNS = 10;
const VIS_SEGMENTS = 12;
const VIS_ATTACK = 0.55;       // fraction of the gap closed per frame when rising
const VIS_DECAY = 0.12;        // ... when falling
const VIS_PEAK_HOLD_MS = 450;
const VIS_PEAK_FALL_PER_SEC = 0.9;
const VIS_FEED_STALE_MS = 2500; // no frame for this long = treat the feed as absent
const VIS_COLORS = {
    lit: '#39ff14', litEdge: '#9bff7a', warm: '#c6ff1a', hot: '#ffd21a',
    unlit: '#0c2a0c', peak: '#caffb8'
};

const visState = {
    source: null,
    targets: new Array(VIS_COLUMNS).fill(0),
    levels: new Array(VIS_COLUMNS).fill(0),
    peaks: new Array(VIS_COLUMNS).fill(0),
    peakAt: new Array(VIS_COLUMNS).fill(0),
    lastFrameAt: 0,
    lastTick: 0,
    rafId: 0,
    width: 0,
    height: 0,
    dpr: 1,
    context: null,
    bass: 0,            // smoothed bass amplitude 0..1 for the speakers
    retryAfter: 0,      // after a hard stream failure, don't reopen before this time
    speakerActive: { main: false, zone2: false }
};

const reducedMotionQuery = window.matchMedia('(prefers-reduced-motion: reduce)');

function visFeedIsLive(now) {
    return visState.source !== null && now - visState.lastFrameAt < VIS_FEED_STALE_MS;
}

/** Open the vis stream while playing, close it otherwise. */
function setVisStreamOpen(shouldOpen) {
    if (shouldOpen && !visState.source) {
        if (performance.now() < visState.retryAfter) return;
        const source = new EventSource('/api/vis');
        source.addEventListener('vis', (event) => {
            let frame;
            try { frame = JSON.parse(event.data); } catch (_) { return; }
            if (!frame || !Array.isArray(frame.bands)) return;
            for (let i = 0; i < VIS_COLUMNS; i++) {
                const value = Number(frame.bands[i]);
                visState.targets[i] = Number.isFinite(value) ? Math.max(0, Math.min(value, 1)) : 0;
            }
            visState.lastFrameAt = performance.now();
            startVisLoop();
        });
        source.onerror = () => {
            // A hard failure (e.g. endpoint absent) closes the source for good; drop it
            // and back off before a later state update retries. Transient errors auto-reconnect.
            if (source.readyState === EventSource.CLOSED && visState.source === source) {
                visState.source = null;
                visState.targets.fill(0);
                startVisLoop();
                visState.retryAfter = performance.now() + 60000;
            }
        };
        visState.source = source;
        return;
    }
    if (!shouldOpen && visState.source) {
        visState.source.close();
        visState.source = null;
        visState.targets.fill(0); // fall to rest, then the loop sleeps
        startVisLoop();
    }
}

/** React to a state update: stream on/off and which speaker pairs are live. */
function syncPlayback(state) {
    const playing = state.player.state === 'playing';
    setVisStreamOpen(playing);

    const zones = state.zones;
    visState.speakerActive.main = Boolean(playing && zones && zones.main && zones.main.radio);
    visState.speakerActive.zone2 = Boolean(playing && zones && zones.zone2 && zones.zone2.radio);
    for (const zoneId of ['main', 'zone2']) {
        elements.speakers[zoneId].classList.toggle('active', visState.speakerActive[zoneId]);
    }
    startVisLoop();
}

function resizeVisCanvas() {
    const canvas = elements.visCanvas;
    const rect = canvas.getBoundingClientRect();
    if (!rect.width || !rect.height) return;
    visState.dpr = Math.min(window.devicePixelRatio || 1, 2);
    visState.width = rect.width;
    visState.height = rect.height;
    canvas.width = Math.round(rect.width * visState.dpr);
    canvas.height = Math.round(rect.height * visState.dpr);
    visState.context = canvas.getContext('2d');
    visState.context.setTransform(visState.dpr, 0, 0, visState.dpr, 0, 0);
    paintVis();
}

function paintVis() {
    const context = visState.context;
    if (!context) return;
    const { width, height, levels, peaks } = visState;
    context.clearRect(0, 0, width, height);

    const padX = 8;
    const padY = 8;
    const columnGap = Math.max(3, width * 0.014);
    const columnWidth = (width - padX * 2 - columnGap * (VIS_COLUMNS - 1)) / VIS_COLUMNS;
    const segmentGap = Math.max(1.5, height * 0.018);
    const segmentHeight = (height - padY * 2 - segmentGap * (VIS_SEGMENTS - 1)) / VIS_SEGMENTS;

    for (let column = 0; column < VIS_COLUMNS; column++) {
        const x = padX + column * (columnWidth + columnGap);
        const litCount = Math.round(levels[column] * VIS_SEGMENTS);
        const peakSegment = Math.min(VIS_SEGMENTS - 1, Math.round(peaks[column] * VIS_SEGMENTS) - 1);

        for (let segment = 0; segment < VIS_SEGMENTS; segment++) {
            const y = height - padY - (segment + 1) * segmentHeight - segment * segmentGap;
            const isLit = segment < litCount;
            const isPeak = segment === peakSegment && peakSegment >= litCount;
            if (!isLit && !isPeak) {
                context.fillStyle = VIS_COLORS.unlit;
                context.fillRect(x, y, columnWidth, segmentHeight);
                continue;
            }
            const colour = segment >= VIS_SEGMENTS - 1 ? VIS_COLORS.hot
                : segment >= VIS_SEGMENTS - 3 ? VIS_COLORS.warm
                : VIS_COLORS.lit;
            context.fillStyle = isPeak ? VIS_COLORS.peak : colour;
            context.shadowColor = isPeak ? VIS_COLORS.lit : colour;
            context.shadowBlur = 7;
            context.fillRect(x, y, columnWidth, segmentHeight);
            context.shadowBlur = 0;
        }
    }
}

function startVisLoop() {
    if (visState.rafId || document.hidden) return;
    visState.lastTick = performance.now();
    visState.rafId = requestAnimationFrame(visTick);
}

function visTick(now) {
    visState.rafId = 0;
    // Paint at ~30 fps at most: plenty for 15 fps data, and gentle on phone batteries
    if (now - visState.lastTick < 30) {
        visState.rafId = requestAnimationFrame(visTick);
        return;
    }
    const elapsedSec = Math.min((now - visState.lastTick) / 1000, 0.1);
    visState.lastTick = now;
    const live = visFeedIsLive(now);
    // Frame-rate independent easing (tuned at 60 fps)
    const frames = elapsedSec * 60;
    const attack = 1 - Math.pow(1 - VIS_ATTACK, frames);
    const decay = 1 - Math.pow(1 - VIS_DECAY, frames);
    let settled = true;

    for (let i = 0; i < VIS_COLUMNS; i++) {
        const target = live ? visState.targets[i] : 0; // no live feed: bars decay to rest
        const level = visState.levels[i];
        const next = level + (target - level) * (target > level ? attack : decay);
        visState.levels[i] = next < 0.004 && target === 0 ? 0 : next;

        if (visState.levels[i] >= visState.peaks[i]) {
            visState.peaks[i] = visState.levels[i];
            visState.peakAt[i] = now;
        } else if (now - visState.peakAt[i] > VIS_PEAK_HOLD_MS) {
            visState.peaks[i] = Math.max(visState.levels[i], visState.peaks[i] - VIS_PEAK_FALL_PER_SEC * elapsedSec);
        }
        if (visState.levels[i] > 0 || visState.peaks[i] > 0.001) settled = false;
    }

    // Bass bands 0-2 drive the speaker cones (CSS --amp, 0..1)
    const bassTarget = live ? Math.max(visState.levels[0], visState.levels[1], visState.levels[2]) : 0;
    visState.bass += (bassTarget - visState.bass) * (bassTarget > visState.bass ? 0.7 : 0.2);
    if (visState.bass < 0.003) visState.bass = 0;
    const driven = live && !reducedMotionQuery.matches;
    for (const zoneId of ['main', 'zone2']) {
        const speaker = elements.speakers[zoneId];
        const amp = driven && visState.speakerActive[zoneId] ? visState.bass : 0;
        speaker.style.setProperty('--amp', amp.toFixed(3));
        speaker.classList.toggle('driven', driven && visState.speakerActive[zoneId]);
    }

    paintVis();

    // Keep ticking while there is a live feed or anything left to animate
    if (!settled || live) {
        visState.rafId = requestAnimationFrame(visTick);
        return;
    }
    // Feed gone: release the speakers back to the CSS fallback pulse
    for (const zoneId of ['main', 'zone2']) {
        elements.speakers[zoneId].classList.remove('driven');
        elements.speakers[zoneId].style.removeProperty('--amp');
    }
    visState.bass = 0;
}

// A stalled feed can't wake the loop by itself, so poll cheaply for staleness
setInterval(() => {
    if (visState.source && !visState.rafId) startVisLoop();
}, 1000);

document.addEventListener('visibilitychange', () => {
    if (!document.hidden) startVisLoop();
});

// Update now-playing display
function updateNowPlaying(player) {
    elements.nowPlaying.classList.remove('loading');
    if (lcdHoldActive) return; // an error message is being shown

    let text = 'READY';

    if (player.error) {
        text = 'PLAYER ERROR: ' + player.error;
        elements.nowPlaying.classList.add('error');
    } else if (player.state === 'playing' || player.state === 'paused') {
        if (player.artist && player.title) {
            text = `${player.artist} – ${player.title}`;
        } else if (player.title) {
            text = player.title;
        } else if (player.station_name) {
            text = player.station_name;
        }
        elements.nowPlaying.classList.remove('error');
    } else if (player.state === 'stopped') {
        // Show "STOPPED · STATION NAME" if a station is known
        text = player.station_name ? `STOPPED · ${player.station_name}` : 'STOPPED';
        elements.nowPlaying.classList.remove('error');
    }

    elements.nowPlaying.textContent = text;

    // Marquee if text is long
    elements.nowPlaying.classList.toggle('marquee', text.length > 40);
}

/** @returns {{id: string, stations: Array}|undefined} the stations group for the current band */
function getCurrentGroup() {
    return stations.groups.find(g => g.id === currentBand);
}

// Position the needle and sync the tuner's ARIA state to selectedStationIndex
function positionNeedle() {
    const group = getCurrentGroup();
    const count = group ? group.stations.length : 0;

    elements.dialControl.setAttribute('aria-valuemin', '0');
    elements.dialControl.setAttribute('aria-valuemax', String(Math.max(count - 1, 0)));

    if (count === 0) {
        elements.dialNeedle.style.display = 'none';
        elements.dialControl.setAttribute('aria-valuenow', '0');
        elements.dialControl.setAttribute('aria-valuetext', 'No stations');
        return;
    }

    selectedStationIndex = Math.max(0, Math.min(selectedStationIndex, count - 1));
    const availableWidth = (elements.dialScale.offsetWidth || 300) - 20;
    const fraction = selectedStationIndex / Math.max(count - 1, 1);
    elements.dialNeedle.style.display = '';
    elements.dialNeedle.style.left = `${fraction * availableWidth + 10}px`;

    elements.dialControl.setAttribute('aria-valuenow', String(selectedStationIndex));
    elements.dialControl.setAttribute('aria-valuetext', group.stations[selectedStationIndex].name);
    updateDialLabels();
}

// Re-render the dial scale and needle for the current band
function refreshDial() {
    renderDialScale();
    positionNeedle();
}

/**
 * Make `band` the current one and repaint the switch (thumb position, aria-checked,
 * roving tabindex). Does not touch the presets or the dial.
 * @param {string} band one of BANDS
 * @param {boolean} [focus] move keyboard focus to the newly checked segment
 */
function paintBandSwitch(band, focus = false) {
    currentBand = band;
    elements.bandSwitch.dataset.band = band;
    for (const option of elements.bandOptions) {
        const isChecked = option.dataset.band === band;
        option.setAttribute('aria-checked', String(isChecked));
        option.tabIndex = isChecked ? 0 : -1;
        if (isChecked && focus) option.focus();
    }
}

/**
 * Switch band by the user's choice: the presets and dial follow it, and the dial
 * starts on the playing station when that station is in the band.
 * @param {string} band one of BANDS
 * @param {{focus?: boolean}} [options]
 */
function setBand(band, { focus = false } = {}) {
    if (!BANDS.includes(band)) return;
    paintBandSwitch(band, focus);
    const playingId = currentState?.player.station;
    const group = getCurrentGroup();
    const playingIndex = playingId && group ? group.stations.findIndex(s => s.id === playingId) : -1;
    selectedStationIndex = playingIndex === -1 ? 0 : playingIndex;
    renderPresetButtons();
    refreshDial();
}

// Move the dial to a newly playing station, switching band if it lives in another one.
// The current band wins when the station is in several (a MY entry that is also a preset).
function followStation(stationId) {
    const searchOrder = [currentBand, ...BANDS.filter(band => band !== currentBand)];
    for (const band of searchOrder) {
        const group = stations.groups.find(g => g.id === band);
        const index = group ? group.stations.findIndex(s => s.id === stationId) : -1;
        if (index === -1) continue;
        if (band === currentBand) {
            selectedStationIndex = index;
            positionNeedle();
            return;
        }
        paintBandSwitch(band);
        renderPresetButtons();
        selectedStationIndex = index;
        refreshDial();
        return;
    }
}

// Update preset button selection
function updatePresetSelection(stationId) {
    elements.presetButtons.querySelectorAll('.preset-btn').forEach(btn => {
        btn.classList.toggle('selected', btn.dataset.station === stationId);
    });
}

// Volume knob mapping. The sweep is non-linear (squared taper): position =
// normalized^2, so dB-per-degree SHRINKS toward the cap. The loud end gets the
// finest control (a small drag near the cap can never leap to maximum) while
// the knob's real minimum (zone.min_db) is still reachable. Values are never
// clamped to a "nicer" floor, so the knob never misreports the actual level.
const KNOB_CURVE = 2;
// Half a turn: the indicator rests at 3 o'clock at the minimum and travels
// clockwise, round the bottom, to 9 o'clock at the cap.
const KNOB_SWEEP_DEGREES = 180;
const KNOB_REST_DEGREES = 90;
// One drag gesture may move the level at most this many dB per pixel of travel
// (6 dB per 20 px), whatever the taper says.
const KNOB_MAX_DB_PER_PIXEL = 6 / 20;

function dbToPosition(db, minDb, capDb) {
    const normalized = Math.max(0, Math.min((db - minDb) / (capDb - minDb), 1));
    return Math.pow(normalized, KNOB_CURVE);
}

function positionToDb(position, minDb, capDb) {
    const clamped = Math.max(0, Math.min(position, 1));
    return minDb + Math.pow(clamped, 1 / KNOB_CURVE) * (capDb - minDb);
}

function positionToAngle(position) {
    return KNOB_REST_DEGREES + position * KNOB_SWEEP_DEGREES;
}

function isKnobDisabled(knob) {
    return knob.getAttribute('aria-disabled') === 'true';
}

// Paint a knob (indicator, label, ARIA) for a given dB value
function renderKnob(zoneId, db) {
    const knob = zoneId === 'main' ? elements.mainKnob : elements.zone2Knob;
    const label = zoneId === 'main' ? elements.mainVolumeLabel : elements.zone2VolumeLabel;
    const minDb = parseFloat(knob.dataset.minDb);
    const capDb = parseFloat(knob.dataset.capDb);

    const angle = positionToAngle(dbToPosition(db, minDb, capDb));
    knob.querySelector('.knob-indicator').style.transform = `translateX(-50%) rotate(${angle}deg)`;

    knob.setAttribute('aria-valuenow', db.toFixed(1));
    knob.setAttribute('aria-valuetext', `${db.toFixed(1)} dB`);
    label.textContent = isKnobDisabled(knob) ? 'STANDBY' : `${db.toFixed(1)} dB`;
}

// Update zone UI
function updateZone(zoneId, zone) {
    const toggle = zoneId === 'main' ? elements.mainToggle : elements.zone2Toggle;
    const led = zoneId === 'main' ? elements.mainLed : elements.zone2Led;
    const knob = zoneId === 'main' ? elements.mainKnob : elements.zone2Knob;

    // Toggle reflects "radio" (on + airplay); the pilot LED reflects power only
    toggle.checked = zone.radio;
    led.classList.toggle('on', zone.power === 'on');

    // Knob range and enabled state come straight from the zone
    knob.dataset.minDb = zone.min_db;
    knob.dataset.capDb = zone.cap_db;
    knob.dataset.stepDb = zone.step_db || 0.5;
    knob.setAttribute('aria-valuemin', zone.min_db.toFixed(1));
    knob.setAttribute('aria-valuemax', zone.cap_db.toFixed(1));

    const standby = zone.power !== 'on';
    knob.classList.toggle('disabled', standby);
    if (standby) {
        knob.setAttribute('aria-disabled', 'true');
    } else {
        knob.removeAttribute('aria-disabled');
    }

    // Don't fight the user's own in-flight input (drag, or recent wheel/keys)
    const userIsAdjusting = (isDraggingKnob && activeKnob === knob) ||
        Date.now() - Number(knob.dataset.localAt || 0) < LOCAL_EDIT_HOLD_MS;
    if (userIsAdjusting) return;

    knob.dataset.db = zone.db;
    renderKnob(zoneId, zone.db);
}

// Play station
async function playStation(stationId) {
    const state = await apiCall('POST', '/play', { station: stationId });
    if (state) {
        updateUI(state);
    }
}

// Stop playback
async function stop() {
    const state = await apiCall('POST', '/stop');
    if (state) {
        updateUI(state);
    }
}

// Zone power toggle
async function setZonePower(zoneId, on) {
    const state = await apiCall('POST', `/zone/${zoneId}/power`, { on });
    if (state) {
        updateUI(state);
        return;
    }
    // Failed: snap the toggle back to the receiver's real state
    await resyncState();
}

/** Master power: off stops the radio and puts both zones in standby; on wakes the Media Room. */
async function toggleMasterPower() {
    if (powerRequestInFlight || !currentState?.zones) return;
    const turnOn = !elements.powerBtn.classList.contains('on');
    powerRequestInFlight = true;
    elements.powerBtn.disabled = true;
    const state = await apiCall('POST', '/power', { on: turnOn });
    powerRequestInFlight = false;
    // A failed request leaves the button as the last known state says
    updateUI(state || currentState);
}

// Fetch the authoritative state and repaint everything, abandoning any local edit
async function resyncState() {
    const state = await apiCall('GET', '/state');
    if (!state) return;
    for (const knob of [elements.mainKnob, elements.zone2Knob]) {
        knob.dataset.localAt = '0';
    }
    isDraggingKnob = false;
    activeKnob = null;
    updateUI(state);
}

// Zone volume, throttled per zone so the two knobs never cancel each other
const volumeSenders = {
    main: createVolumeSender('main'),
    zone2: createVolumeSender('zone2')
};

function createVolumeSender(zoneId) {
    return throttle(async (db) => {
        const state = await apiCall('POST', `/zone/${zoneId}/volume`, { db });
        if (state) {
            updateUI(state);
            return;
        }
        // Refused (e.g. 409 zone_off): the LCD shows the detail, snap the knob back
        await resyncState();
    }, 150);
}

// Show error in LCD (held for a few seconds so live updates don't wipe it)
function showError(message) {
    elements.nowPlaying.classList.remove('loading', 'marquee');
    elements.nowPlaying.textContent = message.toUpperCase();
    elements.nowPlaying.classList.add('error');
    lcdHoldActive = true;
    clearTimeout(lcdHoldTimer);
    lcdHoldTimer = setTimeout(() => {
        lcdHoldActive = false;
        elements.nowPlaying.classList.remove('error');
        if (currentState) {
            updateNowPlaying(currentState.player);
        }
    }, 5000);
}

// Show loading
function showLoading() {
    elements.nowPlaying.textContent = 'LOADING';
    elements.nowPlaying.classList.add('loading');
}

// Event listeners
function setupEventListeners() {
    // Band switch: an explicit choice sticks until the playing station changes
    elements.bandSwitch.addEventListener('click', (event) => {
        const option = event.target.closest('.band-option');
        // Re-clicking the current band must not reset the tuned dial
        if (option && option.dataset.band !== currentBand) setBand(option.dataset.band);
    });
    // Radiogroup keys: arrows move to the neighbouring band, wrapping round
    elements.bandSwitch.addEventListener('keydown', (event) => {
        // Alt/Ctrl/Meta+Arrow belongs to the browser (Back/Forward, word jumps)
        if (event.altKey || event.ctrlKey || event.metaKey) return;
        const step = { ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 }[event.key];
        if (step === undefined) return;
        event.preventDefault();
        const nextIndex = (BANDS.indexOf(currentBand) + step + BANDS.length) % BANDS.length;
        setBand(BANDS[nextIndex], { focus: true });
    });

    // LCD star: keep the current station in MY, or take it out again
    elements.lcdStar.addEventListener('click', () => {
        const targetId = starTargetId();
        if (targetId) toggleMy(targetId, isInMy(targetId));
    });

    // Play button
    elements.playBtn.addEventListener('click', () => {
        const group = getCurrentGroup();
        if (!group || group.stations.length === 0) return;
        const station = group.stations[selectedStationIndex] || group.stations[0];
        playStation(station.id);
    });

    // Stop button
    elements.stopBtn.addEventListener('click', stop);

    // Master power button
    elements.powerBtn.addEventListener('click', toggleMasterPower);

    // Zone toggles
    elements.mainToggle.addEventListener('change', (e) => {
        setZonePower('main', e.target.checked);
    });

    elements.zone2Toggle.addEventListener('change', (e) => {
        setZonePower('zone2', e.target.checked);
    });

    // Dial control
    setupDialControl();

    // Search drawer
    setupSearch();

    // Volume knobs
    setupVolumeKnob('main', elements.mainKnob);
    setupVolumeKnob('zone2', elements.zone2Knob);

    // Re-render dial scale on window resize
    window.addEventListener('resize', debounce(() => { refreshDial(); resizeVisCanvas(); }, 250));
    if (typeof ResizeObserver === 'function') new ResizeObserver(debounce(resizeVisCanvas, 100)).observe(elements.visCanvas);
    resizeVisCanvas();
}

// ---------------------------------------------------------------------------
// Search drawer: open/close, focus handling
// ---------------------------------------------------------------------------

/** @returns {boolean} whether the search drawer is showing */
function isSearchOpen() {
    return !elements.searchDrawer.hidden;
}

/** Show the drawer, make the boom box inert behind it and focus the search box. */
function openSearch() {
    if (isSearchOpen()) return;
    elements.searchDrawer.hidden = false;
    elements.searchBackdrop.hidden = false;
    elements.boombox.inert = true;
    elements.searchBtn.setAttribute('aria-expanded', 'true');
    elements.searchInput.focus();
}

/** Hide the drawer and hand focus back to the 🔍 button. */
function closeSearch() {
    if (!isSearchOpen()) return;
    elements.searchDrawer.hidden = true;
    elements.searchBackdrop.hidden = true;
    elements.boombox.inert = false;
    elements.searchBtn.setAttribute('aria-expanded', 'false');
    elements.searchBtn.focus();
}

/** Keep Tab and Shift+Tab inside the open drawer. */
function trapSearchFocus(event) {
    const focusable = Array.from(
        elements.searchDrawer.querySelectorAll('button:not(:disabled), input:not(:disabled)')
    );
    if (focusable.length === 0) return;
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
    }
}

// ---------------------------------------------------------------------------
// Search drawer: genre chips, debounced search, result rows
//
// Everything Radio Browser sends us is untrusted text, so rows are built with
// createElement and textContent only, never HTML strings.
// ---------------------------------------------------------------------------
const SEARCH_GENRES = ['Rock', 'Classic Rock', 'Alt', 'Jazz', 'Blues', 'Country', 'Oldies', 'Classical', 'Lofi', 'News', 'Talk'];
const SEARCH_MIN_CHARS = 2;
const SEARCH_DEBOUNCE_MS = 300;
const SEARCH_IDLE_HINT = 'Type a name or pick a genre.';
let searchGenre = null; // the selected chip, or null
let searchRequestId = 0; // a response only counts while its request is the latest

/** Show one line of status text under the chips (empty clears it). */
function setSearchStatus(text) {
    elements.searchStatus.textContent = text;
}

/** Build the genre chips: tapping one selects it, tapping it again clears it. */
function buildGenreChips() {
    const chips = SEARCH_GENRES.map((genre) => {
        const chip = document.createElement('button');
        chip.type = 'button';
        chip.className = 'search-chip';
        chip.textContent = genre;
        chip.setAttribute('aria-pressed', 'false');
        chip.addEventListener('click', () => {
            searchGenre = searchGenre === genre ? null : genre;
            for (const other of elements.searchChips.children) {
                other.setAttribute('aria-pressed', String(other.textContent === searchGenre));
            }
            runSearch();
        });
        return chip;
    });
    elements.searchChips.replaceChildren(...chips);
}

/**
 * One result row: name, a "genre · country · bitrate" line, then ▶ and ★.
 * @param {{id: string, name: string, genre: string, country: string, bitrate: number}} result
 * @returns {HTMLLIElement}
 */
function createResultRow(result) {
    const row = document.createElement('li');
    row.className = 'search-row';

    const info = document.createElement('div');
    info.className = 'search-info';
    const name = document.createElement('span');
    name.className = 'search-name';
    name.textContent = result.name;
    const meta = document.createElement('span');
    meta.className = 'search-meta';
    meta.textContent = [result.genre, result.country, result.bitrate ? `${result.bitrate} kbps` : '']
        .filter(Boolean).join(' · ');
    info.append(name, meta);

    const play = document.createElement('button');
    play.type = 'button';
    play.className = 'search-play';
    play.textContent = '▶';
    play.setAttribute('aria-label', `Play ${result.name}`);
    play.addEventListener('click', () => playStation(result.id));

    const star = document.createElement('button');
    star.type = 'button';
    star.className = 'search-star';
    star.textContent = '★';
    star.dataset.id = result.id;
    star.setAttribute('aria-label', `Keep ${result.name} in MY`);
    star.addEventListener('click', () => toggleMy(result.id, isInMy(result.id)));

    row.append(info, play, star);
    return row;
}

/** Repaint each result's ★ from the MY list, in place so keyboard focus stays put. */
function refreshSearchStars() {
    for (const star of elements.searchResults.querySelectorAll('.search-star')) {
        star.setAttribute('aria-pressed', String(isInMy(star.dataset.id)));
    }
}

/** Replace the result list. */
function renderSearchResults(results) {
    elements.searchResults.replaceChildren(...results.map(createResultRow));
    refreshSearchStars();
}

/** Run the search for the box text and the selected chip, and show the outcome. */
async function runSearch() {
    const requestId = ++searchRequestId;
    const query = elements.searchInput.value.trim();
    const useQuery = query.length >= SEARCH_MIN_CHARS;
    if (!useQuery && !searchGenre) {
        elements.searchResults.removeAttribute('aria-busy');
        renderSearchResults([]);
        setSearchStatus(SEARCH_IDLE_HINT);
        return;
    }

    const params = new URLSearchParams();
    if (useQuery) params.set('q', query);
    if (searchGenre) params.set('genre', searchGenre);
    setSearchStatus('Searching…');
    elements.searchResults.setAttribute('aria-busy', 'true');

    let results = null;
    try {
        const response = await fetch(`/api/search?${params}`);
        if (response.ok) results = (await response.json()).results;
    } catch (error) {
        results = null;
    }
    if (requestId !== searchRequestId) return; // a newer search took over
    elements.searchResults.removeAttribute('aria-busy');

    if (!Array.isArray(results)) {
        renderSearchResults([]);
        setSearchStatus('Search unavailable');
        return;
    }
    renderSearchResults(results);
    setSearchStatus(results.length === 0 ? 'No stations found' : '');
}

const scheduleSearch = debounce(runSearch, SEARCH_DEBOUNCE_MS);

/** Wire the drawer: 🔍 opens it; Escape, the close button or the backdrop close it. */
function setupSearch() {
    buildGenreChips();
    setSearchStatus(SEARCH_IDLE_HINT);
    elements.searchInput.addEventListener('input', () => {
        searchRequestId++; // whatever is in flight is for text that is gone
        scheduleSearch();
    });
    elements.searchBtn.addEventListener('click', openSearch);
    elements.searchClose.addEventListener('click', closeSearch);
    elements.searchBackdrop.addEventListener('click', closeSearch);
    document.addEventListener('keydown', (event) => {
        if (!isSearchOpen()) return;
        if (event.key === 'Escape') {
            event.preventDefault();
            closeSearch();
        } else if (event.key === 'Tab') {
            trapSearchFocus(event);
        }
    });
}

// Dial control setup
function setupDialControl() {
    let startX = 0;
    let startIndex = 0;

    const hasStations = () => (getCurrentGroup()?.stations.length ?? 0) > 0;

    const updateDialPosition = (index) => {
        if (!hasStations()) return;
        selectedStationIndex = index;
        positionNeedle(); // clamps the index
    };

    let playScheduled = false;
    const playSelected = debounce(() => {
        playScheduled = false;
        const station = getCurrentGroup()?.stations[selectedStationIndex];
        if (station) {
            playStation(station.id);
        }
    }, 400);
    const schedulePlay = () => {
        playScheduled = true;
        playSelected();
    };

    const endDrag = (e) => {
        if (!isDraggingDial) return;
        isDraggingDial = false;
        if (elements.dialControl.hasPointerCapture(e.pointerId)) {
            elements.dialControl.releasePointerCapture(e.pointerId);
        }
        // A station change that arrived mid-drag is applied now, unless the
        // user's own pick is about to play (that pick wins and will be echoed back).
        const pending = pendingFollowStation;
        pendingFollowStation = null;
        if (pending && !playScheduled) followStation(pending);
    };

    // Pointer events
    elements.dialControl.addEventListener('pointerdown', (e) => {
        if (!hasStations()) return;
        isDraggingDial = true;
        startX = e.clientX;
        startIndex = selectedStationIndex;
        elements.dialControl.setPointerCapture(e.pointerId);
        e.preventDefault();
    });

    elements.dialControl.addEventListener('pointermove', (e) => {
        if (!isDraggingDial || !hasStations()) return;

        const sensitivity = 20;
        const indexDelta = Math.round((e.clientX - startX) / sensitivity);
        updateDialPosition(startIndex + indexDelta);
        schedulePlay();
    });

    elements.dialControl.addEventListener('pointerup', endDrag);
    elements.dialControl.addEventListener('pointercancel', endDrag);

    // Wheel events
    elements.dialControl.addEventListener('wheel', (e) => {
        if (!hasStations()) return;
        e.preventDefault();
        updateDialPosition(selectedStationIndex + (e.deltaY > 0 ? 1 : -1));
        schedulePlay();
    }, { passive: false });

    // Keyboard events
    elements.dialControl.addEventListener('keydown', (e) => {
        const group = getCurrentGroup();
        if (!group || group.stations.length === 0) return;

        let handled = true;

        if (e.key === 'ArrowLeft' || e.key === 'ArrowDown') {
            updateDialPosition(selectedStationIndex - 1);
            schedulePlay();
        } else if (e.key === 'ArrowRight' || e.key === 'ArrowUp') {
            updateDialPosition(selectedStationIndex + 1);
            schedulePlay();
        } else if (e.key === 'Home') {
            updateDialPosition(0);
            schedulePlay();
        } else if (e.key === 'End') {
            updateDialPosition(group.stations.length - 1);
            schedulePlay();
        } else if (e.key === 'Enter' || e.key === ' ') {
            const station = group.stations[selectedStationIndex];
            if (station) playStation(station.id);
        } else {
            handled = false;
        }

        if (handled) {
            e.preventDefault();
        }
    });
}

// Volume knob setup
function setupVolumeKnob(zoneId, knob) {
    const DEGREES_PER_PIXEL = 1;
    let startY = 0;
    let startPosition = 0;
    let startDb = 0;

    const knobRange = () => ({
        minDb: parseFloat(knob.dataset.minDb),
        capDb: parseFloat(knob.dataset.capDb),
        stepDb: parseFloat(knob.dataset.stepDb) || 0.5
    });

    const isUsable = () => !isKnobDisabled(knob) && !Number.isNaN(knobRange().minDb);

    // Clamp to [min, cap], remember the value locally, repaint and send it
    const updateKnob = (db) => {
        const { minDb, capDb } = knobRange();
        const clampedDb = Math.max(minDb, Math.min(db, capDb));

        knob.dataset.db = clampedDb; // so rapid wheel/key input accumulates
        knob.dataset.localAt = String(Date.now());
        renderKnob(zoneId, clampedDb);
        volumeSenders[zoneId](clampedDb);
    };

    const endDrag = (e) => {
        if (!isDraggingKnob || activeKnob !== knob) return;
        isDraggingKnob = false;
        activeKnob = null;
        if (knob.hasPointerCapture(e.pointerId)) {
            knob.releasePointerCapture(e.pointerId);
        }
    };

    // Pointer events
    knob.addEventListener('pointerdown', (e) => {
        if (!isUsable()) return;
        const { minDb, capDb } = knobRange();
        isDraggingKnob = true;
        activeKnob = knob;
        startY = e.clientY;
        startDb = parseFloat(knob.dataset.db);
        startPosition = dbToPosition(startDb, minDb, capDb);
        knob.setPointerCapture(e.pointerId);
        e.preventDefault();
    });

    knob.addEventListener('pointermove', (e) => {
        if (!isDraggingKnob || activeKnob !== knob) return;

        const { minDb, capDb, stepDb } = knobRange();
        // Pulling down turns the knob clockwise (louder), like dragging its right edge
        const positionDelta = ((e.clientY - startY) * DEGREES_PER_PIXEL) / KNOB_SWEEP_DEGREES;
        const rawDb = positionToDb(startPosition + positionDelta, minDb, capDb);
        // Never travel faster than KNOB_MAX_DB_PER_PIXEL from where the drag began
        const maxTravelDb = Math.abs(startY - e.clientY) * KNOB_MAX_DB_PER_PIXEL;
        const limitedDb = Math.max(startDb - maxTravelDb, Math.min(rawDb, startDb + maxTravelDb));
        // Snap to the receiver's step grid, anchored at the minimum (updateKnob clamps to the cap)
        updateKnob(minDb + Math.round((limitedDb - minDb) / stepDb) * stepDb);
    });

    knob.addEventListener('pointerup', endDrag);
    knob.addEventListener('pointercancel', endDrag);

    // Wheel events
    knob.addEventListener('wheel', (e) => {
        if (!isUsable()) return; // inert: let the page scroll
        e.preventDefault();
        const { stepDb } = knobRange();
        updateKnob(parseFloat(knob.dataset.db) + (e.deltaY > 0 ? -stepDb : stepDb));
    }, { passive: false });

    // Keyboard events
    knob.addEventListener('keydown', (e) => {
        const keys = ['ArrowUp', 'ArrowRight', 'ArrowDown', 'ArrowLeft', 'PageUp', 'PageDown', 'Home', 'End'];
        if (!keys.includes(e.key)) return;
        if (!isUsable()) return; // inert: leave the key (page scroll etc.) alone
        e.preventDefault();

        const { minDb, capDb, stepDb } = knobRange();
        const currentDb = parseFloat(knob.dataset.db);

        if (e.key === 'ArrowUp' || e.key === 'ArrowRight') updateKnob(currentDb + stepDb);
        else if (e.key === 'ArrowDown' || e.key === 'ArrowLeft') updateKnob(currentDb - stepDb);
        else if (e.key === 'PageUp') updateKnob(currentDb + 5);
        else if (e.key === 'PageDown') updateKnob(currentDb - 5);
        else if (e.key === 'Home') updateKnob(minDb);
        else if (e.key === 'End') updateKnob(capDb);
    });
}

// --- mechub edge: pointer-tracked gradient border + spotlight ---------------
// Writes --mx/--my (spotlight, px) and --edge-angle (conic start angle) on the
// key under the pointer. Delegated so re-rendered preset buttons keep working.
// Skipped under reduced motion: the border then stays at its static angle.
const EDGE_SELECTOR = '.preset-btn, .transport-btn, .band-switch-slider, .toggle-slider';
let edgeFrame = 0;
let edgeEvent = null;

function paintEdge() {
    edgeFrame = 0;
    const event = edgeEvent;
    edgeEvent = null;
    if (!event || !(event.target instanceof Element)) return;
    const target = event.target.closest(EDGE_SELECTOR);
    if (!target) return;
    const rect = target.getBoundingClientRect();
    const x = event.clientX - rect.left;
    const y = event.clientY - rect.top;
    const angle = Math.atan2(y - rect.height / 2, x - rect.width / 2) * (180 / Math.PI);
    target.style.setProperty('--mx', `${x}px`);
    target.style.setProperty('--my', `${y}px`);
    target.style.setProperty('--edge-angle', `${angle}deg`);
}

function trackEdge(event) {
    if (reducedMotionQuery.matches) return;
    edgeEvent = event;
    if (!edgeFrame) edgeFrame = requestAnimationFrame(paintEdge);
}

document.addEventListener('pointermove', trackEdge, { passive: true });
document.addEventListener('pointerdown', trackEdge, { passive: true });
// iOS Safari only applies :active (the pressed edge) when a touchstart listener exists.
document.addEventListener('touchstart', () => {}, { passive: true });

// Start the app
init();
