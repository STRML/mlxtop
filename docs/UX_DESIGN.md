# mlxtop UX design rules

These rules apply to the terminal UI and to reviews of UI changes. Extend the
existing visual language before introducing a new convention.
The [shared chart specification](CHART_SPEC.md) governs all chart colors,
units, automatic ranges, consolidation and history behavior.

## Naming and typography

| Element | Convention | Examples |
| --- | --- | --- |
| Navigation | Title case | Overview, MLX Top, Journal |
| Operational section heading | Uppercase | SYSINFO |
| Chart title | Lowercase words; preserve acronyms | prompt load, generation, prefill, GPU |
| Status or compact metric label | Uppercase | LIVE, LAST SEEN, CACHE, HISTORY |
| Explanation or suggested action | Sentence case | Inspect added context or large tool results. |
| Keyboard hint | Match the actual key and use a short action | p pause, Home latest |

Charts at the same level must use the same title casing, weight, spacing and
border treatment. A chart does not become an operational section merely because
it spans the full width. Preserve standard units and names: tok/s, GiB, GPU,
oMLX and MLX. Use bold for the primary reading or status, not every line.

## Information hierarchy and space

- Overview answers: what is running, is it healthy, what is limiting it, and
  what should the operator inspect? Put memory pressure and paging first,
  then use throughput to show how the workload is behaving.
- Charts show trends and comparisons. Keep their primary reading, units and
  freshness adjacent. Keep supporting explanations shorter than the chart.
- Use one border per chart. Place a dense full-width SYSINFO strip above the
  histories; do not wrap charts in another throughput box.
  SYSINFO owns hardware identity, cores, RAM, CPU/RSS, thermal and GPU allocation.
  Put a fixed borderless two-row assessment below SYSINFO: finding and fitting
  evidence, then the complete next check or neutral observation. Use `d` for full
  evidence and runtime setup; do not add another diagnosis card.
  Memory/pressure and paging share the first row equally, capped at eight rows.
  Generation, prefill and GPU use 40/35/25 percent widths and receive the
  remaining height. Prompt history takes half of the next row, with Cache and
  Queue taking one quarter each. Paging includes the swap capacity bar.
  OS footprint remains in MLX Top and the static report, without a chart.
  Show measured latency beside Journal when space permits.
- At very short chart heights, use percentage capacity bars and exact readings
  instead of one-row traces with misleading axes. Preserve units on standalone
  narrow rate charts. Keep an accessible help hint and separate chart/view keys.
- Give prompt history nine to thirteen rows on regular terminals, seven on
  short macOS terminals and six with a compact NVIDIA table. Journal gets
  five rows on regular terminals (three event lines) and no compact preview; the full Journal is available with `3`. Wrap summaries to at most
  two rows, align time/state columns, and mark shortened messages.
- Keep a one-row header for views and sampling state, and contextual controls
  in a bottom bar. Show current readings in chart headers, moving detailed
  window statistics into expanded views. Help and quit remain accessible.
- Empty and populated states have the same layout. Keep dimensions stable
  through live pressure, queue spikes and idle transitions; alarms and colors
  provide changing emphasis. The selected chart can always expand for detail.
- Keep exact selected values readable independently of bar height. Give prompt
  history its full allocated width, with a compact token-count label beneath every
  visible bar. Reserve enough width per request to keep these labels readable
  at every zoom level; anchor the newest bar at the right with fixed slot spacing,
  including a single request. Retain older requests through history navigation. Keep a
  single numeric size summary in the bottom border. Do not put long
  request IDs or raw payloads in Overview; use Journal for request detail.
- For NVIDIA devices, put a compact per-card comparison directly below the
  operational summary. Preserve utilization, VRAM used/total and temperature
  on narrow terminals; add load state and a VRAM bar when width permits. Use
  `[` / `]` to select cards and reveal overflow, with the visible range labeled.
  Keep card identities distinct and label the combined utilization `GPU max`.
- At narrower widths, remove secondary detail before clipping primary values.
  Keep status, throughput, request sizes and resource readings available. Avoid wrapping chart
  headers or allowing long identifiers to displace readings.

## Color and emphasis

Use the existing palette; do not introduce per-widget color schemes.

| Role | Treatment |
| --- | --- |
| Healthy or favorable measured condition | Green plus a label or value |
| Condition worth inspecting | Yellow plus an explicit reason |
| Critical measured condition | Red plus an explicit reason |
| Chart identity | Existing metric color; a title color alone is not severity |
| Live request in prompt history | Cyan plus LIVE |
| Historical request in prompt history | Blue plus LAST SEEN or REPORTED and age |
| Prompt size changes | Muted numeric comparison; never warning markers on bars |
| Resident RAM | Cyan shows occupancy including file cache; the separate PRESSURE label carries OS severity |
| GPU utilization | Green/yellow/red configured load bands, with a numeric reading and load label |
| Supporting text, unknown data, historical advice | Muted |

Never require color alone to interpret severity, freshness or selection. Use
text, symbols and values alongside it. Preserve captured severity
colors in time-series history instead of recoloring old samples on refresh.

## Metric truth and operator advice

- Keep implementation scope within mlxtop. Use existing provider interfaces
  and operating-system metrics; do not patch, inject into, reconfigure or
  restart serving runtimes to obtain counters. Omit unavailable-only dashboard
  summaries rather than filling operator space with implementation limitations.
- Unknown is `—`, not zero. Distinguish idle, stale, live and reported data.
- Show observation age for retained request values. Redrawing must not refresh
  their timestamps. Label historical assessments HISTORY.
- State chart units and scales. Only percentage charts use 0–100; numeric
  axes automatically fit visible observations in their actual units. These
  display ranges are not model or hardware limits. Retain exact selected
  values. Keep time-series gaps disconnected and never drop captured samples to fit a plot.
- Compare like-for-like provider/model observations. Say previous observed
  request; do not assume conversation membership or a tool loop.
- Show cache reuse only when reported for that request. Do not substitute an
  aggregate cache metric or infer latency from token counts alone.
- The prompt-load headline is the selected request's exact input-token count,
  including cached tokens. Keep that total prominent when browsing history or
  zooming; cached and uncached segments partition the same total.
- Stacked prompt bars use green for reported cached tokens, and cyan/live or
  blue/historical for the remainder. Unknown cache reuse stays unsplit. Prompt
  changes appear numerically; do not mark growth as an alarm. `↑` marks scale
  overflow. Keep the selected `▲` beside its bar's size label. Compact labels
  use `k` for thousands of tokens and `M` for millions, never KB. The selected
  observation's UTC timestamp stays in the headline when it fits; output speed
  takes priority on narrow panels, with full metadata in the expanded view. Horizontal spacing
  represents request order.
  Cache availability must not alter bar height. Partial cells that cannot fit
  both segment colors and empty space use the dominant segment; the selected
  readout retains the precise count and reported cache reuse.
- Keep OS process memory in MLX Top and the static report, attributed to its
  PID and source. Keep footprint, RSS and allocator counters distinct.
- Suggestions must follow visible evidence. Prompt history emphasizes actual
  token counts, recent median and range. GPU utilization alone establishes
  neither a slowdown nor a compute bottleneck. Diagnostics should pair a
  measured throughput drop with a supporting signal, label correlation and
  confidence, and give a short testable next step. Wrap advice instead of
  truncating it. Avoid generic tuning advice when no problem is established.
- Ring the terminal bell once when critical memory pressure or severe paging
  begins. Keep a visible, acknowledgeable banner. High GPU utilization alone
  must never trigger an alarm. Missing counters do not establish recovery.

## Interaction and review

- Preserve the three views: Overview, MLX Top and Journal. Place request load
  in Overview, with contextual history controls and a clear selected marker.
- Tab/Shift-Tab switch views globally, including from an expanded chart or
  an active process filter. In Overview, arrow keys select charts. Mouse clicks select,
  the wheel and +/− zoom the selected history, and Enter/Esc enlarge/restore.
  Time-series charts share their window and zoom in Overview, widening the
  same observations to fit different panel widths. Prompt and latency bars
  retain independent ordinal zoom. Sampling cadence remains unchanged.
  Show selection with border shape as well as color.
- MLX Top prioritizes the process table and selected-process details. Show
  filtered aggregate CPU/RSS and full commands. OS readings must belong to
  the selected PID; label provider-wide telemetry as runtime data rather than
  assigning a model/state to every process. Do not repeat Overview charts.
- Preserve 1/2/3 for views and p for pause. `d` opens read-only Diagnostics
  outside text entry; Esc/d closes it, and view switching dismisses it. Keep
  quit and alarm acknowledgment available and label retained paused observations. Shift-↑↓ and Home/End inspect prompt
  history. Use { / } for sampling cadence. Show contextual controls.
- Review at 80×24, a medium terminal, and a wide terminal. Check empty, live,
  idle/historical, unavailable and large-value states when affected.
- For layout changes, inspect an actual terminal rendering. For data semantics,
  use focused tests that verify freshness, comparisons and missing data. Follow
  the build and validation checks in [CONTRIBUTING.md](../CONTRIBUTING.md).
