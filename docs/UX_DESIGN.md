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
  The first and largest history row holds system memory/pressure, process
  footprint and paging at 30/30/40 percent widths. Without process footprint,
  memory/paging use 40/60. Give this row at least twice the height of the rate
  row. Generation, prefill and GPU share a supporting row at 50/30/20 percent
  widths, capped at six rows. Prefill stays selectable and expandable.
  Below these rows, use half the width for compact prompt history and one
  quarter each for Cache and Queue. Preserve separate scales and selectable
  charts. Paging combines traffic history with a swap capacity bar.
  Show measured latency beside the Journal when space permits.
- At very short chart heights, use percentage capacity bars and exact readings
  instead of one-row traces with misleading axes. Preserve units on standalone
  narrow rate charts. Keep an accessible help hint and separate chart/view keys.
- Cap prompt history at eight rows in Overview (seven on short macOS terminals,
  six with a compact NVIDIA table). Give the Journal about one quarter of the
  height, bounded to seven–twelve rows on regular terminals. It shows eight
  event rows at 170×42. A short macOS terminal retains one recent event; full
  Journal is always available with `3`. Wrap summaries to at most two rows,
  align time/state columns, and mark shortened messages with an ellipsis.
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
| Resident RAM | Height shows occupancy including file cache; color follows captured pressure, with an explicit PRESSURE label |
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
  values. Keep time-series gaps disconnected and one column per sample at 1×.
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
- Label OS process memory with its PID and source. Put the current byte value
  above a trace with labeled lower/middle/upper ticks. Fit the range to visible
  values; it may start above zero and never implies a process memory limit.
  Keep peak and complete growth fields in the footer when space permits. Keep process footprint,
  RSS and allocator counters distinct. Lifetime peak must not be confused with
  a peak observed only during monitoring. Growth requires consecutive readings
  of the same process instance; positive growth alone does not imply severity.
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
  Keep each chart's zoom independent and sampling cadence unchanged. At 1×,
  time-series charts use one column per observation, while prompt bars reserve
  space for each size label. 2×/4×/8× widen observations without inventing
  intermediate readings. Show selection with border shape as well as color.
- MLX Top prioritizes the process table and selected-process details. Show
  filtered aggregate CPU/RSS and full commands. OS readings must belong to
  the selected PID; label provider-wide telemetry as runtime data rather than
  assigning a model/state to every process. Do not repeat Overview charts.
- Preserve 1/2/3 for views and p for pause. Shift-↑↓ and Home/End inspect prompt
  history. Use { / } for sampling cadence. Show contextual controls.
- Review at 80×24, a medium terminal, and a wide terminal. Check empty, live,
  idle/historical, unavailable and large-value states when affected.
- For layout changes, inspect an actual terminal rendering. For data semantics,
  use focused tests that verify freshness, comparisons and missing data. Follow
  the build and validation checks in [CONTRIBUTING.md](../CONTRIBUTING.md).
