# Contributing

Thank you for helping improve mlxtop. Bug reports, documentation, provider
fixtures and focused implementation changes are all useful contributions.

## Before opening a change

Run the local checks:

macOS builds require Xcode Command Line Tools (including the macOS SDK and
libclang) for the safe `libproc` wrapper's generated bindings. The application
continues to forbid unsafe Rust in its own source.

~~~sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
python3 -m unittest discover -s scripts -p 'test_*.py'
shellcheck scripts/cicd.sh scripts/coverage.sh scripts/install.sh scripts/package-dmg.sh
cargo build --release --locked
~~~

### Coverage gate

Production line coverage must stay at or above 90 percent on each supported
platform. Install the tools once, then run the gate:

~~~sh
cargo install cargo-llvm-cov --locked
rustup component add llvm-tools-preview
scripts/coverage.sh
~~~

Cargo, rustc and the LLVM coverage tools must use matching toolchains. If
multiple Rust installations are present, select the intended toolchain before
running the gate. `LLVM_COV` and `LLVM_PROFDATA` can point to matching tools.

The script writes `coverage.json`, `summary.txt` and `uncovered.txt` to
`target/coverage/` (override with `COVERAGE_DIR`) and fails below
`COVERAGE_MIN_LINES` (default `90`). Tests live only in `src/tests/`; each
module declares `#[cfg(test)] #[path = "tests/<module>.rs"] mod tests;`. The
report excludes exactly `(^|/)src/tests/[^/]+\.rs$`, so the denominator is all
production Rust compiled for the host platform. The script refuses to run if a
production file contains `cfg(coverage)`, `coverage(off)`, `cfg(not(test))`,
or any other `#[cfg(test)]` item. Code that reads the host goes through the
`host::Host` seam, so collectors are tested with recorded command and file
fixtures instead of being excluded.

After running the gate, generate a browsable report from the same profiles:

~~~sh
cargo llvm-cov report --html --output-dir target/coverage \
  --ignore-filename-regex '(^|/)src/tests/[^/]+\.rs$'
~~~

Open `target/coverage/html/index.html`. CI uploads the JSON, summary and
uncovered-line reports as `coverage-macos-latest` and `coverage-ubuntu-latest`
artifacts. Coverage describes code exercised by tests; review assertions and
missing cases alongside the percentage.

For UI changes, test both the interactive dashboard and the static report:

~~~sh
./target/release/mlxtop
./target/release/mlxtop --once
~~~

## Release candidates

Use SemVer release candidates before a final release: `1.1.2-rc.1`,
`1.1.2-rc.2`, then `1.1.2`. Increment the positive RC number for each new
candidate of the same target version. When starting a new target version,
restart at `rc.1`.

Update the package version in `Cargo.toml` and the `mlxtop` entry in
`Cargo.lock` together, and add the candidate's changes to `CHANGELOG.md`.
The CLI and dashboard obtain their version from Cargo. Rebuild with
`cargo build --locked` and check `./target/debug/mlxtop --version`.

Use matching Git tags such as `v1.1.2-rc.1` when publishing and mark GitHub
RC releases as prereleases. `scripts/package-dmg.sh` accepts both final and
RC versions and uses the full version in the package and artifact names.
The README links and `scripts/install.sh` follow GitHub's latest release,
which never includes prereleases, so they need no per-release edits. Attach
each platform's download and one `SHA256SUMS` covering all of them.

## Design boundaries

Follow the [UX design rules](docs/UX_DESIGN.md) for naming, typography, color,
layout, metric semantics and interaction. Chart titles use lowercase words
with acronyms preserved: `prompt load`, `generation`, `GPU`.
The [chart specification](docs/CHART_SPEC.md) is mandatory for every chart:
use consistent green/yellow/red bands where defined, preserve captured colors,
and automatically scale numeric axes in their actual units. Only percentages
use 0–100. Consolidate related readings instead of duplicating summary cards.

- Keep Overview focused on current health and LLM impact.
- Keep MLX Top focused on live process/resource inspection.
- Keep Journal focused on meaningful historical transitions.
- Never display historical provider values as live telemetry.
- Show telemetry provenance and age whenever a provider API is unavailable.
- Use fixed-width stepped time-series traces for indicator history. One
  displayed column maps to one captured sample; new samples enter on the right
  and old samples leave on the left once the viewport is full. Gaps must remain
  disconnected, and a sample's recorded severity tone must not be recolored by
  a later refresh. If smoothing is needed for readability, make it causal and
  derive it from the current drawable resolution using only the samples up to
  that point. Keep raw readings and captured tones unchanged; never recalculate
  history from a future or centered display-time window.
- Treat color as a secondary cue. Every visible severity or operating condition
  must also have a semantic label that does not depend on color perception.
- Keep unavailable counters as unavailable; do not substitute a healthy zero.
- Preserve visual hierarchy: serving throughput is the outcome; GPU, memory,
  paging and compression are supporting evidence.
- Use responsive disclosure instead of squeezing cards or columns. A medium
  terminal must retain status, throughput, prompt sizes and resource readings; process-table
  columns may collapse in documented priority order.
- Keep keyboard hints contextual to the active view and reserve color for
  identity, severity and selected state rather than decoration.
- Keep provider network/authentication and metric parsing out of the native
  collector. Add or extend a provider adapter instead; process signatures may
  be used only for lightweight detection and labeling.

## Pull requests

Describe the user-visible behavior, the platform assumptions, and the checks
you ran. Include a terminal screenshot for substantial layout changes when
possible.

Do not include API keys, prompts, private logs or other workload data in an
issue, fixture or screenshot. Report suspected vulnerabilities according to
[SECURITY.md](SECURITY.md).

## Contribution license

By submitting a contribution, you confirm that you have the right to provide
it and agree that it is licensed under the project's MIT License.
