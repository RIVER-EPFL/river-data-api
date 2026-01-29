use axum::{
    http::header,
    response::{Html, IntoResponse},
};

pub async fn dashboard() -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "public, max-age=60")],
        Html(DASHBOARD_HTML),
    )
}

const DASHBOARD_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>River Sensor Data</title>
    <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/uplot@1.6.31/dist/uPlot.min.css">
    <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/nouislider@15/dist/nouislider.min.css">
    <style>
        :root {
            --bg: #f8fafc;
            --surface: #ffffff;
            --border: #e2e8f0;
            --text: #1e293b;
            --muted: #64748b;
            --accent: #2563eb;
        }
        * { box-sizing: border-box; margin: 0; padding: 0; }
        body { font-family: system-ui, -apple-system, sans-serif; background: var(--bg); color: var(--text); min-height: 100vh; }

        .container {
            max-width: 1200px;
            margin: 0 auto;
            padding: 1.5rem;
        }

        header {
            display: flex;
            justify-content: space-between;
            align-items: center;
            margin-bottom: 1.5rem;
            flex-wrap: wrap;
            gap: 1rem;
        }
        h1 { font-size: 1.25rem; font-weight: 600; }

        .station-buttons {
            display: flex;
            gap: 0.5rem;
            flex-wrap: wrap;
        }
        .station-btn {
            padding: 0.5rem 1rem;
            border: 1px solid var(--border);
            border-radius: 0.375rem;
            font-size: 0.875rem;
            background: var(--surface);
            cursor: pointer;
            transition: all 0.15s;
        }
        .station-btn:hover {
            border-color: var(--accent);
            color: var(--accent);
        }
        .station-btn.active {
            background: var(--accent);
            border-color: var(--accent);
            color: white;
        }

        .slider-section {
            background: var(--surface);
            border: 1px solid var(--border);
            border-radius: 0.5rem;
            padding: 1.5rem;
            margin-bottom: 1rem;
            user-select: none;
            -webkit-user-select: none;
        }
        .slider-section * {
            user-select: none;
            -webkit-user-select: none;
        }
        .slider-labels {
            display: flex;
            justify-content: space-between;
            font-size: 0.75rem;
            color: var(--muted);
            margin-bottom: 0.5rem;
        }
        /* Timeline legend - thin colored line below slider */
        .timeline-legend {
            display: flex;
            height: 4px;
            border-radius: 2px;
            overflow: visible;
            margin-top: 2rem;  /* Space for pip labels above */
            margin-bottom: 0.5rem;
            margin-left: 0.5rem;
            margin-right: 0.5rem;
        }
        .timeline-zone-history {
            background: #94a3b8;  /* Slate gray for history */
            height: 100%;
            position: relative;
        }
        .timeline-zone-week {
            background: #3b82f6;  /* Blue for last week */
            height: 100%;
            position: relative;
        }
        .timeline-zone-today {
            background: #10b981;  /* Green for today */
            height: 100%;
            position: relative;
        }
        /* Boundary markers between zones */
        .timeline-zone-history::after,
        .timeline-zone-week::after {
            content: '';
            position: absolute;
            right: 0;
            top: -2px;
            height: 8px;
            width: 1px;
            background: var(--text);
            opacity: 0.3;
        }
        /* Reduce pip label clashing */
        .noUi-pips-horizontal {
            padding-top: 8px;
            height: 50px;
        }
        .noUi-value-horizontal {
            transform: translateX(-50%);
            font-size: 0.6rem;
        }
        .noUi-marker-horizontal.noUi-marker-large {
            height: 10px;
        }
        #time-slider {
            margin: 0 0.5rem;
        }
        .slider-info {
            display: flex;
            justify-content: space-between;
            align-items: center;
            margin-top: 0.5rem;
        }
        .window-info {
            font-size: 0.875rem;
            color: var(--muted);
        }
        .resolution-info {
            font-size: 0.75rem;
            color: var(--muted);
            font-style: italic;
        }

        .controls-row {
            display: flex;
            gap: 1rem;
            margin-bottom: 1rem;
            flex-wrap: wrap;
            align-items: center;
        }
        .sensor-toggles {
            background: var(--surface);
            border: 1px solid var(--border);
            border-radius: 0.5rem;
            padding: 0.75rem 1rem;
            display: flex;
            flex-wrap: wrap;
            gap: 1rem;
            min-height: 42px;
            align-items: center;
            flex: 1;
        }
        .sensor-toggle {
            display: flex;
            align-items: center;
            gap: 0.5rem;
            cursor: pointer;
            font-size: 0.875rem;
        }
        .sensor-toggle input {
            width: 1rem;
            height: 1rem;
            accent-color: var(--accent);
        }

        .charts-container {
            display: flex;
            flex-direction: column;
            gap: 0.5rem;
        }
        .sensor-chart {
            background: var(--surface);
            border: 1px solid var(--border);
            border-radius: 0.5rem;
            padding: 0.75rem 1rem;
            padding-top: 1.5rem;  /* Extra space for y-axis top labels */
            position: relative;
            overflow: visible;
        }
        .sensor-chart .chart-label {
            position: absolute;
            top: 0.5rem;
            left: 1rem;
            font-size: 0.75rem;
            font-weight: 600;
            color: var(--muted);
            z-index: 10;
            background: var(--surface);
            padding: 0 0.25rem;
        }
        .sensor-chart .chart-expand {
            position: absolute;
            bottom: 0.5rem;
            right: 0.5rem;
            width: 1.5rem;
            height: 1.5rem;
            border: 1px solid var(--border);
            border-radius: 0.25rem;
            background: var(--surface);
            cursor: pointer;
            font-size: 0.75rem;
            display: flex;
            align-items: center;
            justify-content: center;
            color: var(--muted);
            z-index: 10;
            opacity: 0.6;
            transition: opacity 0.15s;
        }
        .sensor-chart .chart-expand:hover {
            opacity: 1;
            border-color: var(--accent);
            color: var(--accent);
        }
        .sensor-chart .u-wrap {
            cursor: crosshair;
        }
        .sensor-chart .u-over {
            overflow: visible !important;
        }
        .chart-placeholder {
            display: flex;
            align-items: center;
            justify-content: center;
            height: 180px;
            color: var(--muted);
            background: var(--surface);
            border: 1px solid var(--border);
            border-radius: 0.5rem;
        }
        .chart-hint {
            text-align: center;
            font-size: 0.7rem;
            color: var(--muted);
            margin-top: 0.5rem;
        }

        /* Hover tooltip for all sensor values */
        .hover-tooltip {
            position: fixed;
            background: var(--surface);
            border: 1px solid var(--border);
            border-radius: 0.5rem;
            padding: 0.75rem;
            box-shadow: 0 4px 12px rgba(0,0,0,0.15);
            pointer-events: none;
            z-index: 100;
            font-size: 0.8rem;
            min-width: 180px;
            display: none;
        }
        .hover-tooltip.visible {
            display: block;
        }
        .hover-tooltip .tooltip-time {
            font-weight: 600;
            margin-bottom: 0.5rem;
            padding-bottom: 0.5rem;
            border-bottom: 1px solid var(--border);
            color: var(--text);
        }
        .hover-tooltip .tooltip-row {
            display: flex;
            justify-content: space-between;
            gap: 1rem;
            padding: 0.2rem 0;
        }
        .hover-tooltip .tooltip-label {
            color: var(--muted);
        }
        .hover-tooltip .tooltip-value {
            font-weight: 500;
            font-variant-numeric: tabular-nums;
        }

        /* noUiSlider custom styles */
        .noUi-target {
            background: var(--bg);
            border: 1px solid var(--border);
            box-shadow: none;
        }
        .noUi-connect {
            background: var(--accent);
        }
        .noUi-handle {
            border: 2px solid var(--accent);
            background: var(--surface);
            box-shadow: 0 2px 4px rgba(0,0,0,0.1);
        }
        .noUi-handle:before, .noUi-handle:after {
            background: var(--accent);
        }
        .noUi-tooltip {
            font-size: 0.7rem;
            padding: 0.25rem 0.5rem;
            background: var(--text);
            color: white;
            border: none;
        }
        /* Slider pips */
        .noUi-pips {
            color: var(--muted);
            font-size: 0.65rem;
        }
        .noUi-marker-large {
            background: var(--border);
        }
        .noUi-value {
            color: var(--muted);
        }

        .site-footer {
            padding: 1rem 0 0;
            font-size: 0.7rem;
            color: #999;
            opacity: 0.4;
            display: flex;
            align-items: center;
            justify-content: space-between;
            gap: 0.5rem;
            flex-wrap: wrap;
        }
        .site-footer:hover { opacity: 0.7; }
        .site-footer a {
            color: inherit;
            text-decoration: none;
        }
        .site-footer a:hover { text-decoration: underline; }
        .footer-left, .footer-right {
            display: flex;
            align-items: center;
            gap: 0.4rem;
        }
        .footer-separator { margin: 0 0.2rem; }

        .loading-overlay {
            position: absolute;
            inset: 0;
            background: rgba(255,255,255,0.8);
            display: flex;
            align-items: center;
            justify-content: center;
            font-size: 0.875rem;
            color: var(--muted);
            border-radius: 0.5rem;
            z-index: 20;
        }
    </style>
</head>
<body>
    <div class="container">
        <header>
            <h1>River Sensor Data</h1>
            <div class="station-buttons" id="station-buttons">
                <span style="color: var(--muted); font-size: 0.875rem;">Loading stations...</span>
            </div>
        </header>

        <div class="slider-section" id="slider-section" style="display: none;">
            <div class="slider-labels">
                <span id="min-date">--</span>
                <span id="max-date">--</span>
            </div>
            <div id="time-slider"></div>
            <div class="timeline-legend" id="timeline-legend">
                <div class="timeline-zone-history" id="zone-history"></div>
                <div class="timeline-zone-week" id="zone-week"></div>
                <div class="timeline-zone-today" id="zone-today"></div>
            </div>
            <div class="slider-info">
                <div>
                    <span class="window-info" id="window-info">--</span>
                    <span class="resolution-info" id="resolution-info"></span>
                </div>
            </div>
        </div>

        <div class="controls-row">
            <div class="sensor-toggles" id="sensor-toggles">
                <span style="color: var(--muted); font-size: 0.875rem;">Select a station to see sensors</span>
            </div>
        </div>

        <div class="charts-container" id="charts-container">
            <div class="chart-placeholder">Select a station to view data</div>
        </div>
        <div class="chart-hint">Drag to zoom in · Double-click to zoom out</div>

        <footer class="site-footer">
            <div class="footer-left">
                <a href="/docs">API Docs</a>
                <span class="footer-separator">|</span>
                <a href="https://github.com/RIVER-EPFL/river-data-api" target="_blank" rel="noopener">Source</a>
            </div>
            <div class="footer-right">
                <span>Developed by <a href="https://github.com/evanjt" target="_blank" rel="noopener">Evan Thomas</a> at <a href="https://www.epfl.ch/research/domains/alpole/" target="_blank" rel="noopener">ALPOLE</a>, <a href="https://www.epfl.ch/about/campus/fr/valais-fr/" target="_blank" rel="noopener">EPFL Valais</a></span>
            </div>
        </footer>
    </div>

    <div class="hover-tooltip" id="hover-tooltip">
        <div class="tooltip-time" id="tooltip-time">--</div>
        <div id="tooltip-values"></div>
    </div>

    <script src="https://cdn.jsdelivr.net/npm/uplot@1.6.31/dist/uPlot.iife.min.js"></script>
    <script src="https://cdn.jsdelivr.net/npm/nouislider@15/dist/nouislider.min.js"></script>
<script>
const api = url => fetch(url).then(r => r.json());

const state = {
    station: null,
    sensors: new Set(),
    sensorsWithData: new Set(),  // Only sensor types that have actual data
    sensorTypeOrder: [],  // Original order of sensor types (for consistent display)
    expandedCharts: new Set(),  // Track which charts are expanded
    start: null,
    end: null,
    charts: {},  // Map of sensor type -> uPlot instance
    chartData: {},  // Map of sensor type -> { sensors, timestamps }
    slider: null,
    data: null,
    loading: false,
};

const CHART_HEIGHT_NORMAL = 180;
const CHART_HEIGHT_EXPANDED = 400;

// Cursor sync key for all charts
const syncKey = uPlot.sync("sensors");

// Tooltip elements
const tooltip = document.getElementById('hover-tooltip');
const tooltipTime = document.getElementById('tooltip-time');
const tooltipValues = document.getElementById('tooltip-values');

// Color palette for sensor types
const colors = ['#2563eb', '#dc2626', '#16a34a', '#ca8a04', '#9333ea', '#0891b2', '#be185d', '#ea580c'];
const sensorColors = {};

// Debounce utility
function debounce(fn, ms) {
    let timeout;
    return (...args) => {
        clearTimeout(timeout);
        timeout = setTimeout(() => fn(...args), ms);
    };
}

// Format date for display
function formatDate(ts) {
    const d = new Date(ts);
    return d.toLocaleDateString('en-US', { month: 'short', year: 'numeric' });
}

function formatDateFull(ts) {
    const d = new Date(ts);
    return d.toLocaleDateString('en-US', { month: 'short', day: 'numeric', year: 'numeric' });
}

function formatDateTimeFull(ts) {
    const d = new Date(ts);
    return d.toLocaleString('en-US', { month: 'short', day: 'numeric', year: 'numeric', hour: '2-digit', minute: '2-digit' });
}

function formatDuration(ms) {
    const days = Math.round(ms / 86400000);
    if (days < 1) return 'Less than 1 day';
    if (days === 1) return '1 day';
    if (days < 7) return `${days} days`;
    if (days < 30) return `${Math.round(days / 7)} week${days >= 14 ? 's' : ''}`;
    if (days < 365) return `${Math.round(days / 30)} month${days >= 60 ? 's' : ''}`;
    return `${(days / 365).toFixed(1)} years`;
}

// Initialize
async function init() {
    const stations = await api('/api/stations');
    const container = document.getElementById('station-buttons');

    container.innerHTML = stations.map(s => `
        <button class="station-btn" data-id="${s.id}">${s.name}</button>
    `).join('');

    container.querySelectorAll('.station-btn').forEach(btn => {
        btn.addEventListener('click', () => {
            container.querySelectorAll('.station-btn').forEach(b => b.classList.remove('active'));
            btn.classList.add('active');
            loadStation(btn.dataset.id);
        });
    });

    // Auto-load first station
    const firstBtn = container.querySelector('.station-btn');
    if (firstBtn) {
        firstBtn.click();
    }
}

async function loadStation(stationId) {
    const station = await api(`/api/stations/${stationId}`);
    state.station = station;

    // Clear existing charts
    Object.values(state.charts).forEach(chart => chart.destroy());
    state.charts = {};

    // Build sensor toggles
    const toggles = document.getElementById('sensor-toggles');
    const types = [...new Set((station.sensors || []).map(s => s.sensor_type).filter(Boolean))].sort();

    if (!types.length) {
        toggles.innerHTML = '<span style="color: var(--muted); font-size: 0.875rem;">No sensors configured</span>';
        state.sensors = new Set();
        document.getElementById('charts-container').innerHTML = '<div class="chart-placeholder">No sensors configured</div>';
        return;
    }

    // Assign colors and store original order
    types.forEach((t, i) => sensorColors[t] = colors[i % colors.length]);
    state.sensors = new Set(types);
    state.sensorTypeOrder = types;  // Preserve original order

    toggles.innerHTML = types.map(t => `
        <label class="sensor-toggle">
            <input type="checkbox" value="${t}" checked>
            <span style="color: ${sensorColors[t]}">${t}</span>
        </label>
    `).join('');

    toggles.querySelectorAll('input').forEach(cb => {
        cb.addEventListener('change', () => {
            if (cb.checked) state.sensors.add(cb.value);
            else state.sensors.delete(cb.value);
            updateCharts();
        });
    });

    // Setup slider
    if (!station.data_start || !station.data_end) {
        document.getElementById('slider-section').style.display = 'none';
        document.getElementById('charts-container').innerHTML = '<div class="chart-placeholder">No data available for this station</div>';
        return;
    }

    const minTs = new Date(station.data_start).getTime();
    const maxTs = new Date(station.data_end).getTime();

    document.getElementById('min-date').textContent = formatDate(minTs);
    document.getElementById('max-date').textContent = formatDate(maxTs);
    document.getElementById('slider-section').style.display = 'block';

    // Default to last 1 day
    const defaultWindow = Math.min(1 * 86400000, maxTs - minTs);
    state.start = new Date(maxTs - defaultWindow);
    state.end = new Date(maxTs);

    // Create or update slider
    const sliderEl = document.getElementById('time-slider');
    if (state.slider) {
        state.slider.destroy();
    }

    const rangeDays = (maxTs - minTs) / 86400000;
    const oneDayMs = 86400000;
    const oneWeekMs = 7 * oneDayMs;

    // Telescope timeline with 3 zones:
    // - History: everything before last week (variable %)
    // - Last Week: 7 days before today (15% of slider)
    // - Today: last 24 hours (10% of slider)
    const todayStart = maxTs - oneDayMs;
    const weekStart = maxTs - oneWeekMs;

    let sliderRange;
    let pipsConfig;
    const zoneHistory = document.getElementById('zone-history');
    const zoneWeek = document.getElementById('zone-week');
    const zoneToday = document.getElementById('zone-today');

    // Reset legend bar visibility
    zoneHistory.style.display = '';
    zoneWeek.style.display = '';
    zoneToday.style.display = '';

    if (rangeDays > 8) {
        // 3-zone telescope: 75% history, 15% week, 10% today
        sliderRange = {
            'min': minTs,
            '75%': weekStart,
            '90%': todayStart,
            'max': maxTs
        };
        // Update legend bar widths (no text, just colored line)
        zoneHistory.style.width = '75%';
        zoneWeek.style.width = '15%';
        zoneToday.style.width = '10%';
        zoneHistory.textContent = '';
        zoneWeek.textContent = '';
        zoneToday.textContent = '';

        // Fewer pips to avoid clashing
        pipsConfig = {
            mode: 'positions',
            values: [0, 20, 40, 60, 75, 90, 100],
            density: 100,
            format: {
                to: v => {
                    const d = new Date(v);
                    const hoursFromEnd = (maxTs - v) / 3600000;
                    const daysFromEnd = hoursFromEnd / 24;
                    // Today zone: minimal labels
                    if (hoursFromEnd <= 24) {
                        const h = d.getHours();
                        if (h === 0) return d.toLocaleDateString('en-US', { month: 'short', day: 'numeric' });
                        if (h === 12) return '12:00';
                        return '';
                    }
                    // Week/History: show dates
                    return d.toLocaleDateString('en-US', { month: 'short', day: 'numeric' });
                }
            }
        };
    } else if (rangeDays > 2) {
        // 2-zone: week + today
        sliderRange = {
            'min': minTs,
            '85%': todayStart,
            'max': maxTs
        };
        zoneHistory.style.width = '0%';
        zoneHistory.style.display = 'none';
        zoneWeek.style.width = '85%';
        zoneToday.style.width = '15%';
        zoneWeek.textContent = '';
        zoneToday.textContent = '';

        pipsConfig = {
            mode: 'positions',
            values: [0, 20, 40, 60, 85, 100],
            format: {
                to: v => {
                    const d = new Date(v);
                    const hoursFromEnd = (maxTs - v) / 3600000;
                    if (hoursFromEnd <= 24) {
                        const h = d.getHours();
                        if (h === 0) return d.toLocaleDateString('en-US', { month: 'short', day: 'numeric' });
                        if (h === 12) return '12:00';
                        return '';
                    }
                    return d.toLocaleDateString('en-US', { month: 'short', day: 'numeric' });
                }
            }
        };
    } else {
        // Linear for small ranges
        sliderRange = { min: minTs, max: maxTs };
        zoneHistory.style.display = 'none';
        zoneWeek.style.display = 'none';
        zoneToday.style.width = '100%';
        zoneToday.textContent = '';

        pipsConfig = {
            mode: 'count',
            values: 6,
            format: {
                to: v => {
                    const d = new Date(v);
                    if (rangeDays < 1) {
                        return d.toLocaleTimeString('en-US', { hour: '2-digit', minute: '2-digit' });
                    }
                    return d.toLocaleDateString('en-US', { month: 'short', day: 'numeric' });
                }
            }
        };
    }

    state.slider = noUiSlider.create(sliderEl, {
        start: [state.start.getTime(), state.end.getTime()],
        connect: true,
        range: sliderRange,
        step: 600000,  // 10 minute steps for finer control
        tooltips: [
            { to: v => formatDateTimeFull(v) },
            { to: v => formatDateTimeFull(v) }
        ],
        pips: pipsConfig
    });

    state.slider.on('update', (values) => {
        state.start = new Date(Number(values[0]));
        state.end = new Date(Number(values[1]));
        updateWindowInfo();
        fetchData();
    });

    // Prevent accidental image drag on slider elements
    sliderEl.addEventListener('dragstart', e => e.preventDefault());
    sliderEl.addEventListener('selectstart', e => e.preventDefault());

    updateWindowInfo();
    fetchData();
}

function updateWindowInfo() {
    const duration = state.end - state.start;
    document.getElementById('window-info').textContent = `Showing: ${formatDuration(duration)}`;
}

function zoom(factor) {
    if (!state.slider || !state.station) return;

    const minTs = new Date(state.station.data_start).getTime();
    const maxTs = new Date(state.station.data_end).getTime();

    const center = (state.start.getTime() + state.end.getTime()) / 2;
    const currentSpan = state.end - state.start;
    const newSpan = currentSpan * factor;

    const clampedSpan = Math.max(3600000, Math.min(newSpan, maxTs - minTs));

    let newStart = center - clampedSpan / 2;
    let newEnd = center + clampedSpan / 2;

    if (newStart < minTs) {
        newStart = minTs;
        newEnd = Math.min(minTs + clampedSpan, maxTs);
    }
    if (newEnd > maxTs) {
        newEnd = maxTs;
        newStart = Math.max(maxTs - clampedSpan, minTs);
    }

    state.slider.set([newStart, newEnd]);
}

// Fetch data with improved day-based resolution thresholds for stacked charts
const fetchData = debounce(async () => {
    if (!state.station || !state.start || !state.end) return;

    const days = (state.end - state.start) / 86400000;
    let endpoint, resolution;

    // Improved thresholds for stacked charts (each chart shows ~1 sensor):
    // - Raw: up to 14 days (144 pts/day × 14 = 2016 pts max per chart)
    // - Hourly: 14-120 days (24 pts/day × 120 = 2880 pts max)
    // - Daily: 120-365 days
    // - Weekly: beyond 1 year
    if (days <= 14) {
        endpoint = 'readings';
        resolution = '10-min raw';
    } else if (days <= 120) {
        endpoint = 'aggregates/hourly';
        resolution = 'hourly avg';
    } else if (days <= 365) {
        endpoint = 'aggregates/daily';
        resolution = 'daily avg';
    } else {
        endpoint = 'aggregates/weekly';
        resolution = 'weekly avg';
    }

    const url = `/api/stations/${state.station.id}/${endpoint}?start=${state.start.toISOString()}&end=${state.end.toISOString()}`;

    showLoading();

    try {
        let data = await api(url);

        // Fallback to raw readings if aggregates return empty
        if (!data.times?.length && endpoint !== 'readings') {
            const fallbackUrl = `/api/stations/${state.station.id}/readings?start=${state.start.toISOString()}&end=${state.end.toISOString()}`;
            data = await api(fallbackUrl);
            resolution = '10-min raw (fallback)';
        }

        state.data = data;
        document.getElementById('resolution-info').textContent = `(${resolution})`;
        updateCharts();
    } catch (e) {
        console.error('Failed to fetch data:', e);
        document.getElementById('charts-container').innerHTML = '<div class="chart-placeholder">Error loading data</div>';
    } finally {
        hideLoading();
    }
}, 50);

function showLoading() {
    state.loading = true;
    const container = document.getElementById('charts-container');
    let overlay = container.querySelector('.loading-overlay');
    if (!overlay) {
        overlay = document.createElement('div');
        overlay.className = 'loading-overlay';
        overlay.textContent = 'Loading...';
        overlay.style.position = 'fixed';
        overlay.style.top = '50%';
        overlay.style.left = '50%';
        overlay.style.transform = 'translate(-50%, -50%)';
        container.style.position = 'relative';
        container.appendChild(overlay);
    }
}

function hideLoading() {
    state.loading = false;
    const overlay = document.getElementById('charts-container').querySelector('.loading-overlay');
    if (overlay) overlay.remove();
}

// Check if a sensor has any non-null data
function hasData(sensor) {
    const values = sensor.values || sensor.avg || [];
    return values.some(v => v != null);
}

// Update tooltip with values at cursor index
function updateTooltip(idx, mouseX, mouseY) {
    if (idx == null || !state.data?.times?.length) {
        tooltip.classList.remove('visible');
        return;
    }

    const time = new Date(state.data.times[idx]);
    tooltipTime.textContent = time.toLocaleString('en-US', {
        month: 'short', day: 'numeric', year: 'numeric',
        hour: '2-digit', minute: '2-digit'
    });

    let html = '';
    // Use original sensor type order for consistent display
    state.sensorTypeOrder.forEach(type => {
        if (!state.sensors.has(type) || !state.chartData[type]) return;
        const { sensors } = state.chartData[type];
        sensors.forEach(sensor => {
            const values = sensor.values || sensor.avg || [];
            const val = values[idx];
            const color = sensorColors[type] || '#666';
            html += `<div class="tooltip-row">
                <span class="tooltip-label" style="color: ${color}">${sensor.name}</span>
                <span class="tooltip-value">${val != null ? val.toFixed(2) : '--'} ${sensor.units || ''}</span>
            </div>`;
        });
    });

    tooltipValues.innerHTML = html;
    tooltip.classList.add('visible');

    // Position tooltip below and to the right of cursor
    const rect = tooltip.getBoundingClientRect();
    let left = mouseX + 20;
    let top = mouseY + 20;

    // Keep on screen - prefer bottom-right, fallback to other positions
    if (left + rect.width > window.innerWidth - 10) {
        left = mouseX - rect.width - 20;  // Move to left side
    }
    if (top + rect.height > window.innerHeight - 10) {
        top = mouseY - rect.height - 20;  // Move above cursor
    }
    if (left < 10) left = 10;
    if (top < 10) top = 10;

    tooltip.style.left = left + 'px';
    tooltip.style.top = top + 'px';
}

// Hide tooltip when cursor leaves charts
function hideTooltip() {
    tooltip.classList.remove('visible');
}

function updateCharts() {
    const container = document.getElementById('charts-container');

    if (!state.data || !state.data.times?.length) {
        container.innerHTML = '<div class="chart-placeholder">No data for selected range</div>';
        Object.values(state.charts).forEach(chart => chart.destroy());
        state.charts = {};
        state.chartData = {};
        return;
    }

    const { times, sensors } = state.data;

    // Prepare timestamps once
    const timestamps = times.map(t => new Date(t).getTime() / 1000);

    // Group sensors by type AND filter out those with no data
    const sensorsByType = {};
    state.sensorsWithData.clear();

    sensors.forEach(sensor => {
        if (!hasData(sensor)) return;  // Skip sensors with all null values
        if (!sensorsByType[sensor.type]) sensorsByType[sensor.type] = [];
        sensorsByType[sensor.type].push(sensor);
        state.sensorsWithData.add(sensor.type);
    });

    // Update sensor toggles to only show sensors with data
    const toggles = document.getElementById('sensor-toggles');
    const allTypes = [...new Set(sensors.map(s => s.type))].sort();
    toggles.innerHTML = allTypes.map(t => {
        const hasAnyData = state.sensorsWithData.has(t);
        const checked = state.sensors.has(t) && hasAnyData;
        return `<label class="sensor-toggle" ${!hasAnyData ? 'style="opacity: 0.4"' : ''}>
            <input type="checkbox" value="${t}" ${checked ? 'checked' : ''} ${!hasAnyData ? 'disabled' : ''}>
            <span style="color: ${sensorColors[t]}">${t}${!hasAnyData ? ' (no data)' : ''}</span>
        </label>`;
    }).join('');

    toggles.querySelectorAll('input:not(:disabled)').forEach(cb => {
        cb.addEventListener('change', () => {
            if (cb.checked) state.sensors.add(cb.value);
            else state.sensors.delete(cb.value);
            updateCharts();
        });
    });

    // Only show enabled types that have data
    const enabledTypes = [...state.sensors].filter(t => state.sensorsWithData.has(t)).sort();

    if (!enabledTypes.length) {
        container.innerHTML = '<div class="chart-placeholder">No data available for selected sensors</div>';
        Object.values(state.charts).forEach(chart => chart.destroy());
        state.charts = {};
        state.chartData = {};
        return;
    }

    // Remove charts for disabled/empty types
    Object.keys(state.charts).forEach(type => {
        if (!enabledTypes.includes(type)) {
            state.charts[type].destroy();
            delete state.charts[type];
            delete state.chartData[type];
            const el = document.getElementById(`chart-${type}`);
            if (el) el.remove();
        }
    });

    // Create/update charts for enabled types with data
    enabledTypes.forEach(type => {
        const typeSensors = sensorsByType[type] || [];
        if (!typeSensors.length) return;

        // Store for tooltip
        state.chartData[type] = { sensors: typeSensors, timestamps };

        let chartDiv = document.getElementById(`chart-${type}`);
        const isExpanded = state.expandedCharts.has(type);
        const chartHeight = isExpanded ? CHART_HEIGHT_EXPANDED : CHART_HEIGHT_NORMAL;

        if (!chartDiv) {
            chartDiv = document.createElement('div');
            chartDiv.id = `chart-${type}`;
            chartDiv.className = 'sensor-chart';
            chartDiv.innerHTML = `
                <div class="chart-label" style="color: ${sensorColors[type]}">${type} (${typeSensors[0]?.units || ''})</div>
                <div class="chart-area"></div>
                <button class="chart-expand" data-type="${type}" title="Expand/collapse chart">⤢</button>
            `;
            container.appendChild(chartDiv);

            // Add expand button handler
            chartDiv.querySelector('.chart-expand').addEventListener('click', (e) => {
                const t = e.target.dataset.type;
                if (state.expandedCharts.has(t)) {
                    state.expandedCharts.delete(t);
                } else {
                    state.expandedCharts.add(t);
                }
                updateCharts();
            });
        }

        const chartArea = chartDiv.querySelector('.chart-area');
        const expandBtn = chartDiv.querySelector('.chart-expand');
        expandBtn.textContent = isExpanded ? '⤡' : '⤢';
        expandBtn.title = isExpanded ? 'Collapse chart' : 'Expand chart';

        // Build series data for this type
        const seriesData = [timestamps];
        const seriesOpts = [{}];

        typeSensors.forEach(sensor => {
            const values = sensor.values || sensor.avg || [];
            seriesData.push(values);
            seriesOpts.push({
                label: sensor.name,
                stroke: sensorColors[type] || '#666',
                width: 1.5,
                value: (u, v) => v == null ? '--' : v.toFixed(2) + (sensor.units ? ' ' + sensor.units : ''),
            });
        });

        const opts = {
            width: container.clientWidth - 32,
            height: chartHeight,
            scales: { x: { time: true }, y: { auto: true } },
            axes: [
                { stroke: '#64748b', grid: { stroke: '#e2e8f0' }, size: 40 },
                { stroke: sensorColors[type], grid: { stroke: '#e2e8f0' }, size: 50, values: (u, vals) => vals.map(v => v == null ? '' : v.toFixed(1)) }
            ],
            series: seriesOpts,
            cursor: {
                sync: {
                    key: syncKey.key,
                    setSeries: true,
                },
                drag: { x: true, y: false },
            },
            hooks: {
                setCursor: [
                    (u) => {
                        const idx = u.cursor.idx;
                        if (idx != null) {
                            const bbox = u.root.getBoundingClientRect();
                            const cx = u.cursor.left + bbox.left;
                            const cy = u.cursor.top + bbox.top;
                            updateTooltip(idx, cx, cy);
                        } else {
                            hideTooltip();
                        }
                    }
                ],
                setSelect: [
                    (u) => {
                        if (u.select.width > 0) {
                            const left = u.posToVal(u.select.left, 'x');
                            const right = u.posToVal(u.select.left + u.select.width, 'x');
                            state.slider.set([left * 1000, right * 1000]);
                            u.setSelect({ width: 0, height: 0 });
                        }
                    }
                ]
            },
            legend: { show: false },
        };

        // Destroy old chart if exists
        if (state.charts[type]) {
            state.charts[type].destroy();
        }

        chartArea.innerHTML = '';
        state.charts[type] = new uPlot(opts, seriesData, chartArea);

        // Double-click to zoom out
        chartArea.addEventListener('dblclick', () => zoom(2));
    });

    // Remove placeholder if we have charts
    const placeholder = container.querySelector('.chart-placeholder');
    if (placeholder && enabledTypes.length) placeholder.remove();
}

// Hide tooltip when mouse leaves charts container
document.getElementById('charts-container').addEventListener('mouseleave', hideTooltip);

// Handle resize
window.addEventListener('resize', debounce(() => {
    const container = document.getElementById('charts-container');
    const width = container.clientWidth - 32;
    Object.entries(state.charts).forEach(([type, chart]) => {
        const height = state.expandedCharts.has(type) ? CHART_HEIGHT_EXPANDED : CHART_HEIGHT_NORMAL;
        chart.setSize({ width, height });
    });
}, 100));

init();
</script>
</body>
</html>
"##;
