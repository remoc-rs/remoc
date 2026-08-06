# Throughput benchmark

Measures what Remoc costs against a plain TCP connection doing the same work, over
emulated network links. Both endpoints run in this process, so every result includes
the peer's work and, on shaped links, the emulator's own CPU cost; treat the numbers
as a lower bound on what Remoc achieves between separate machines.

```
cargo run --release -- --out results.json
./plot.py results.json
```

The full sweep is every layer against every link at every message size and takes well
over an hour. `--list` prints what those are, and `--layer`, `--link` and `--size`
narrow the run to a subset:

```
cargo run --release -- --list
cargo run --release -- --layer raw_tcp,mpsc_struct_postbag --link lan --size 1024
```

The first selected layer is the baseline the others are reported against, so keep
`raw_tcp` first unless a different baseline is what you want.

`--quick` shortens every transfer from five seconds to one. That is noisy, and the
report is marked as such, but it is the way to iterate on the harness itself.

## Layers

Each layer adds a piece of Remoc on top of the previous one, so the results show where
the cost appears.

| Layer | What it measures |
| --- | --- |
| `raw_tcp` | A plain socket moving bytes, the baseline |
| `chmux` | A raw multiplexer port |
| `base` | An `rch::base` channel |
| `mpsc` | An `rch::mpsc` channel, still raw bytes |
| `tcp_struct_*` | A plain socket that serializes records itself, the reference for the struct layers |
| `mpsc_struct_*` | An `rch::mpsc` channel carrying batches of records |

The struct layers exist once per codec, and the `par0` to `par4` variants differ in how
many extra transfer channels the channel is given, which is how many messages it may
serialize at once.

## Links

Bandwidth and round-trip time are emulated in the process, so a slow or distant
connection can be measured without one:

| Link | Bandwidth | Round-trip time |
| --- | --- | --- |
| `lan` | 125 MB/s | 0 ms |
| `wifi` | 12 MB/s | 10 ms |
| `lte` | 6 MB/s | 50 ms |
| `wan` | 25 MB/s | 100 ms |

Round-trip times must be either zero or well above the emulator's floor of roughly two
milliseconds, which is why a gigabit LAN is modelled as pure bandwidth limiting.

`--validate` checks that the emulator reproduces those figures and exits without
measuring anything:

```
cargo run --release -- --validate
```

The emulator is deterministic, so this is worth rerunning when the machine or the
emulator changes rather than before every measurement.

## Plots

`plot.py` draws every layer and codec, which is what analysis needs:

```
./plot.py results.json --outdir plots
./plot.py results.json --baseline earlier.json    # report what moved
```

The two figures on <https://remoc.rs/benchmarks.html> are drawn by `plot_web.py` in the
website repository instead, which reduces the same report to one codec and one channel
configuration; see `_tools/README.md` there.
