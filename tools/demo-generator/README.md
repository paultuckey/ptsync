## Demo GIF

`tools/demo-generator/generated/demo.gif` (shown at the top of the README) is generated, never recorded by
hand. Regenerate it after changing anything the demo shows — console output, the
output directory layout, or the markdown frontmatter:

```shell
cd ../../
cargo run --manifest-path tools/demo-generator/Cargo.toml
```

It needs [agg](https://github.com/asciinema/agg), asciinema's gif generator, once:

```shell
cargo install --git https://github.com/asciinema/agg
```

The `demo-generator/` crate builds the real `ptsync` binary, runs a short script of real
commands against a takeout zip built from the committed `test/takeout_basic`
fixture, and records what they actually print. So:

- **The content can't drift.** It's captured from the binary built out of the
  current working tree, not transcribed into a script that goes stale.
- **The timing is authored.** A real sync of the fixture takes ~15ms, which would
  render as one unreadable frame, so `demo-generator/src/main.rs` paces the output line by
  line. The `*_SECS` constants there are the only invented numbers.
- **It can't leak private photos.** The input is always the committed fixture,
  never a real library.

It writes two files, both committed: `docs/demo.cast` (the asciicast, reviewable
as text in a diff — worth a look when the GIF changes) and `docs/demo.gif`.

To change what the demo *shows*, edit the script at the top of `demo-generator/src/main.rs`.
It's a plain sequence of steps: each one displays a command, runs it, and plays
back the real output.
