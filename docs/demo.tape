# Records docs/demo.gif, the animation at the top of the README.
#
#   cargo test --test sync_snapshot   # builds target/demo/ with the fixture zip
#   vhs docs/demo.tape                # re-records the GIF from it
#
# Run from the repo root. The test must run first: it builds the fixture takeout
# zip this records against, and the debug binary this types `ptsync` for.
#
# This script is fixed, so the paths below are hard-coded. tests/snapshots/sync.txt
# is a snapshot of the same sync's output, and a plain `cargo test` fails when it
# drifts — that is the signal this GIF wants re-recording.
#
# https://github.com/charmbracelet/vhs

Output docs/demo.gif

Set Shell "bash"
Set FontSize 16
Set Width 1200
Set Height 800
# A neutral dark theme, so ptsync's own colours (green INFO) read as themselves.
# `vhs themes` lists the rest — note they are VHS's names, not agg's.
Set Theme "Builtin Dark"
Set TypingSpeed 35ms

# Put the freshly built binary on PATH so the demo can type a bare `ptsync`
# without the viewer seeing a path into target/. Then start from a clean archive:
# ptsync is idempotent, so a second sync into an existing photo-archive would
# output nothing and the demo would show no work.
Hide
Type 'export PATH="$PWD/target/debug:$PATH" && PS1="\[\e[32m\]$\[\e[0m\] "'
Enter
Type 'cd target/demo && rm -rf photo-archive && clear'
Enter
Show

Sleep 600ms

# 1. The headline: a takeout zip goes in, a tidy archive comes out.
Type "ptsync sync --input takeout-20240715.zip --output photo-archive"
Enter
Sleep 2s

# 2. What that produced. Files only — the directory entries add ten lines of
#    noise and the date hierarchy is already visible in each path. `tree` would
#    be prettier but isn't installed by default on macOS, so it would become one
#    more thing to install before re-recording.
Type "find photo-archive -type f | sort"
Enter
Sleep 2500ms

# 3. The payoff: a note ptsync wrote, with metadata you own in plain text. This
#    is the photo that arrived twice, so it carries two original-paths and an
#    album wikilink — the point ptsync is making.
Type "cat photo-archive/2024/05/22/0017-51000.md"
Enter
Sleep 4s
