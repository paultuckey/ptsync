# Test coverage

Coverage is measured with [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov),
which uses LLVM's source-based instrumentation. It is not part of `cargo test`
and nothing in CI depends on it — run it by hand when you want to see which
lines the suite never reaches.

## One-time setup

```shell
rustup component add llvm-tools-preview
```

```shell
cargo install cargo-llvm-cov
```

## Running it

A per-file summary in the terminal:

```shell
cargo llvm-cov --summary-only
```

The line-by-line report, which is the one worth reading — uncovered lines are
highlighted in red:

```shell
cargo llvm-cov --html --open
```

The report is written to `target/llvm-cov/html/`. Coverage builds use their own
`target/llvm-cov-target/` directory, so a coverage run never invalidates your
normal `cargo build` or `cargo test` artifacts (and the first run after a code
change recompiles from scratch).

To measure a subset, everything after `--` goes to the test harness:

```shell
cargo llvm-cov --html -- exif_util
```

If a run is interrupted, stale profile data can skew the next one. Clear it:

```shell
cargo llvm-cov clean
```

## Reading the numbers

**The percentages read high.** `ptsync` keeps its unit tests inline, in a
`#[cfg(test)] mod tests` at the bottom of each module. Source-based coverage
instruments everything that compiles, so those test bodies count as covered
lines and lift every file's number. Treat the percentage as a rough signal and
use the HTML report to look at which *non-test* lines are red — that is the part
that means anything.

**The end-to-end sync is counted.** `cargo-llvm-cov` points `LLVM_PROFILE_FILE`
at a per-process pattern, and child processes inherit it. The real `ptsync`
binary that `tests/sync_snapshot.rs` spawns therefore writes its own profile
data, so that run shows up in the totals rather than being invisible.

**Coverage is not the goal.** `Development.md` asks for high-value black box
tests and warns against low-value tests for trivial logic. A red line is a
prompt to ask whether it matters, not an instruction to write a test for it.
