# Chart specification

This is the shared contract for **all charts**, including compact, expanded,
macOS and Linux layouts. New charts and redesigns must follow it. See
[UX design rules](UX_DESIGN.md) for the surrounding interface.

## Panels and consolidation

Overview uses flat sibling panels with **one border per chart**, never a
bordered group around already bordered charts. Size panels by their importance
to monitoring local inference and memory pressure, rather than giving every
metric an equal rectangle. SYSINFO is a dense full-width strip, not a tall card.

| Row | Space allocation |
| --- | --- |
| Context | Three-row SYSINFO; compact selectable NVIDIA device table when present |
| Resources | First and largest history row: memory/pressure 30%, process memory 30%, paging 40%; without process memory use memory 40% and paging 60% |
| Supporting rates | Generation 50%, prefill 30%, GPU 20%; cap the row at six rows |
| Requests | Prompt load takes half of an eight-row row; separate Cache and Queue take one quarter each |
| Recent events | Journal gets about one quarter of terminal height, seven to twelve rows including borders |

After the fixed context, request and Journal rows, give resources at least twice
the height of the supporting rate row. At 170×42, resources get thirteen rows
and supporting rates get six. Each memory/paging panel has at least twice the
area of prefill at supported macOS sizes. Resource history appears before the
rate and request rows; GPU belongs with the supporting rate charts.
Prompt load never grows beyond eight rows in Overview. It uses seven rows on
short macOS terminals and six with a compact NVIDIA table. Expanding a chart
makes its full history available.
At 170×42 the Journal has eight event rows. At 80×24 it retains the newest event;
a compact NVIDIA table may consume this preview budget, with Journal available
through `3`. Journal messages may wrap to a second line and are explicitly
shortened if needed. Show newest events first, with aligned time and state
columns. Measured latency may take 30% of a wide Journal row; it must not
replace the Journal.

An empty state explains the absence of samples without fabricated bars or axes.
Request arrival, idle transitions, pressure and queue spikes must not rearrange
panels. Use the existing critical banner and semantic colors to emphasize pressure.
Keep system and process memory adjacent with independent percent/byte scales.
NVIDIA identity and per-device readings retain a selectable table budget.

| Panel | Contents |
| --- | --- |
| SYSINFO | Model/state, source/age, device/cores, RAM, LLM CPU/RSS, thermal and GPU allocation |
| generation / prefill | Separate tok/s histories with independent axes; no outer throughput box |
| prompt load | One labeled bar per observed request, cache split when reported |
| memory / process memory | Adjacent percent/byte histories with independent axes; no outer memory box |
| GPU | Utilization history and load state; hardware metadata belongs in SYSINFO |
| paging / I/O | Paging-rate history, IN/OUT rates and a horizontal SWAP used/total capacity bar |
| cache | Interval history, explicitly labeled average fallback and prefix hit reading |
| queue | Independent active/waiting request history and counts |

Cache and Queue must remain separate, independently selectable panels; never
merge them under a shared card. Consolidate cache readings only with Cache.
Do not add summary cards repeating these charts. Reserve space for recent Journal events; show explicitly measured first-token
latency alongside them when width permits. There is no
dedicated Diagnostics card; existing alarms and Journal findings remain.

On short terminals, show percentage capacity bars and exact readings rather
than misleading one-row traces with 0/100 axes. Queue retains both counts even
when history cannot fit. Enter expands a selected compact panel into history.
Swap's capacity bar stays visible independently of paging-rate availability.
Cache may show an **TOTAL** gauge when only aggregate reuse is available; this
must never become interval history. A visible history window containing only
gaps must explain the missing samples, even if older off-screen data exists.

## Units and automatic ranges

**Only percentage charts use a 0–100 axis. Every numeric chart automatically
fits its visible observations and labels the axis in the measured unit.**

| Measurement | Axis |
| --- | --- |
| Generation and prefill | tok/s; independent ranges with readable rounded ticks and headroom |
| Prompt size | tokens; zero-based automatic ceiling, size label on every visible bar |
| Process memory | B/KiB/MiB/GiB; fitted lower and upper bounds with labeled ticks, distinct from system RAM percentage |
| Paging / I/O | B/s, KiB/s or MiB/s; automatic ceiling, never a normalized 0–100 score |
| Queue | requests; zero-based automatic ceiling shared by active and waiting |
| First-token latency | measured milliseconds; zero-based automatic ceiling |
| System memory, GPU utilization, cache percentage | 0–100%, explicitly identified as percentages |

Use the visible history after horizontal zoom/scroll to compute numeric ranges.
An old off-screen spike must not flatten the current trace. Generation around
35 tok/s should get an axis near that workload, not a hard-coded 100 tok/s
ceiling. A different provider/model starts a new throughput history. Handle
empty, zero, constant, very small and very large observations without division
by zero, clipping a valid visible peak or inventing observations.

Rescaling changes screen coordinates, never recorded values, sample order,
timestamps or captured colors. Numeric display ceilings are not context,
hardware or service limits. Changing horizontal zoom recalculates the numeric
vertical range; zooming does not change the sampling interval.

## Color contract

All charts use the same semantic palette. **Green, yellow and red encode
defined value bands or reported memory-pressure states; a series' identity
color must never replace its measured severity.** Match the current reading, trace and related gauge to the same rule.

| Band | Meaning |
| --- | --- |
| Green | Within the metric's normal/favorable band |
| Yellow | Elevated load or a measured condition to inspect |
| Red | Highest defined load band or a measured critical condition |
| Muted | Missing/unavailable data; never a healthy zero |

The current configured defaults are:

| Metric | Green | Yellow | Red |
| --- | --- | --- | --- |
| Resident RAM (color follows pressure) | normal pressure | warning pressure | critical pressure |
| GPU utilization | below 75% | 75% to below 90% | 90% or more |
| Paging rate | below 1 MiB/s | 1 MiB/s to below 16 MiB/s | 16 MiB/s or more |

RAM percentage and bytes still measure physical occupancy including file cache.
Its current value, compact gauge and trace all use captured pressure severity,
with muted color for unknown pressure. Native macOS pressure is authoritative.
Linux derives pressure from unavailable memory (`100 - MemAvailable%`) using
configured 70/85 defaults, with full PSI stalls of 1%/5% escalating to watch/
critical. These bands apply to unavailable memory, never resident occupancy.
Memory connectors use the new sample's pressure and preserve endpoint colors;
passing a percentage tick cannot synthesize a warning or recolor history.

Use resolved configuration thresholds, not duplicated constants in renderers.
GPU bands mean utilization/load; red GPU utilization alone does not establish
a bottleneck and must not sound an alarm. GPU Journal events use load terms
such as saturated, eased and idle; they must not call utilization critical or
report recovery from missing readings. Recovery colors follow measured load. Thresholds stay in the original
units when an axis changes. A history sample keeps its captured color when a
new sample arrives, the axis rescales, or the viewport changes.

The SWAP capacity bar uses cyan and labels used/total plus percentage. Occupied
swap alone does not imply paging traffic or an alarm; keep paging severity
attached to the separate B/s trace. Distinguish unavailable, no allocated swap
and measured zero usage.

Series without a defined health threshold must not invent one from their
display range: a fast token rate, large prompt or large footprint is not by
itself a fault. Use labeled identity/measurement colors for those series:
generation/prefill and process footprint use cyan; a measured generation drop
can carry its recorded warning tone. Queue identifies active and waiting
series and marks overlap. Prompt bars distinguish reported cached tokens
(green) from live/retained uncached tokens (cyan/blue); size changes stay
numeric. Defining a new severity band requires recording its metric-specific
meaning and thresholds here, with boundary tests.

Keep numeric readings, units, state labels and legends readable without color.
Colors on chart titles or selected borders identify the chart/focus, not load.
System memory labels its physical occupancy as resident, with bytes/total and
Includes file cache. macOS uses total RAM minus free and speculative pages;
Linux uses MemTotal minus MemFree. Neither includes swapped-out bytes.
Missing counters yield a gap, never 0% or 100%. Resident occupancy does not
establish memory pressure. The PRESSURE label and newest numeric reading agree
on severity, including critical pressure at low occupancy and normal pressure
at high occupancy. Each earlier chart sample retains its own captured state.

## Navigation

Tab/Shift-Tab always switch views; 1/2/3 open a view directly. Arrows and mouse
clicks select charts in Overview. Enter enlarges a chart; Esc restores it.
Keep these roles distinct. A one-row header holds the brand, views and sampling
state; a bottom bar holds contextual controls and a persistent help/quit hint.
Only add complete hint fields that fit. The footer names the focused chart and
its zoom, while expanded views expose detailed window statistics.

## History, labels and review

- Process memory puts the current byte reading and OS/PID attribution above
  the trace. Show lifetime peak and add the complete signed growth field when
  it fits. Missing current memory must not retain a stale growth value.
- Keep one captured observation per time-series column at 1×; higher zoom
  widens observations. Never interpolate a gap or recolor earlier samples.
- Selected prompt history identifies its provider/model when it differs from
  SYSINFO, so retained requests cannot be mistaken for the current model.
- Prompt bars reserve enough width for every size label at every zoom level.
  Use fixed slots sized for the labels, right-aligning the newest request even
  when there is only one. Center each label beneath its bar; adding requests
  must not stretch sparse histories across the plot. Zoom widens the slots and
  reduces the visible request window. Horizontal spacing denotes request order,
  not time.
  `12.0k` means 12,000 tokens, not KB. The selected request retains its exact
  count, observation timestamp, age, cache information and selection marker.
  On narrow panels, prioritize the exact count, state and cache reading in the
  headline; output count and request speed use existing metadata rows. Keep the
  UTC clock and comparisons when they fit; expansion reveals full metadata.
  Never sacrifice a reading or bar label to fit optional fields.
- Only measured live rates enter throughput traces. Session averages and
  retained results use `AVG`/`LAST` labels and never become live samples.
- oMLX prefill progress initializes `speed` to zero before a chunk rate exists;
  treat that placeholder as missing, retaining the separately labeled average.
- Distinguish Cache's interval reading and last interval age from aggregate
  `TOTAL` reuse. Label chart statistics `window avg`; provider averages use
  `SERVER AVG` where space permits. Explain trailing gaps with the last sample
  and age, and mark isolated observations with a visible dot. Counter resets,
  stale intervals and inconsistent cache deltas leave gaps, never false zeros.
- Round traces to the nearest terminal row. Short Queue panels without at least
  three plot rows show exact counts and an expansion hint; they must not draw
  one request on a two-request tick.
- Preserve unknown counters as missing and keep provider/process boundaries
  disconnected. Scale and freshness must remain visible in expanded views.
- Review actual terminal output at 80×24, a medium size and a wide size,
  including live, idle, missing-data, zero, overflow and history-zoom cases.
- Test color boundaries, captured-color retention, real-unit axes, visible
  range scaling and the absence of duplicate panels when changing charts.

## Source semantics

- Apple [vm_stat source](https://github.com/apple-oss-distributions/system_cmds/blob/main/vm_stat/vm_stat.c)
  prints free pages excluding speculative pages, so both must be subtracted
  from total physical RAM to obtain the occupied/resident count.
- Apple's [memory_pressure source](https://github.com/apple-oss-distributions/system_cmds/blob/main/memory_pressure/memory_pressure.c)
  reads memorystatus level. XNU's [AVAILABLE_NON_COMPRESSED_MEMORY](https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/vm/vm_page.h)
  includes active and inactive pages. Its inverse is not physical RAM usage.
- An all-zero paging window keeps measured zero samples and gaps, says
  No paging traffic in this window, and suppresses the artificial nonzero
  ceiling and middle tick. Real historical traffic retains its automatic axis.
- Percentage ticks include `%`. LAST SAMPLE identifies a polled observation;
  SERVER AVG identifies provider counters which may predate this monitor.
  Cache TOTAL identifies aggregate reuse, distinct from interval samples.

- Prompt output speed belongs to the selected provider/model/request ID. Read
  oMLX generation speed or distributed per-request decode_tps, never prefill
  or aggregate throughput. LIVE marks active measurements; AVG requires a
  completed request with measured speed; LAST retains the last sampled rate
  and its age. Missing/invalid timing never becomes zero or another request's
  rate. A completion without timing must not promote a retained sample to AVG.
  Keep prompt panel allocation unchanged and preserve size labels on its bars.
