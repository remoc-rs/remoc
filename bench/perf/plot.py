#!/usr/bin/env python3
"""Plots the results of the Remoc throughput benchmark.

Usage:
    cargo run --release -- --out results.json
    ./plot.py results.json
"""

import argparse
import json
import sys
from collections import OrderedDict
from pathlib import Path

try:
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
except ImportError:
    sys.exit("matplotlib is required: pip install matplotlib")


# Display names of the emulated links, as they are written outside a config file.
LINK_NAMES = {"lan": "LAN", "wifi": "Wi-Fi", "lte": "LTE", "wan": "WAN"}

# Display names of the codecs.
CODEC_NAMES = {"postbag": "postbag", "postbag_slim": "postbag slim", "bincode": "bincode"}

# Legend labels, spelled out for a reader who does not know Remoc's module names.
# Every plain TCP layer is a reference: the same work without Remoc in between.
LAYER_LABELS = {
    "raw_tcp": "Plain TCP connection sending raw bytes (reference)",
    "chmux": "Remoc multiplexed port sending raw bytes",
    "base": "Remoc base channel sending raw bytes",
    "mpsc": "Remoc MPSC channel sending raw bytes",
    "tcp_struct": "Plain TCP connection, records serialized with {codec} codec (reference)",
    "mpsc_struct": "Remoc MPSC channel using {codec} codec",
    "mpsc_struct_buf32": "Remoc MPSC channel using {codec} codec, receive buffer of 32 items",
}

# The plain TCP layers are references rather than measurements of Remoc, and are drawn
# accordingly: grey and dashed, so that a glance separates them from what Remoc achieves.
REFERENCE_STYLE = dict(color="tab:grey", linestyle="--", marker="s")
REMOC_STYLES = [
    dict(color="tab:blue", linestyle="-", marker="o"),
    dict(color="tab:red", linestyle="-", marker="^"),
    dict(color="tab:green", linestyle="-", marker="v"),
]


def load(path):
    with open(path) as f:
        report = json.load(f)

    runs = report["runs"]

    # Reports written before the baseline became selectable name the field after the
    # layer that was always the baseline then.
    for run in runs:
        if "fraction_of_baseline" not in run and "fraction_of_raw_tcp" in run:
            run["fraction_of_baseline"] = run["fraction_of_raw_tcp"]

    links = list(OrderedDict.fromkeys(r["link"] for r in runs))
    layers = list(OrderedDict.fromkeys(r["layer"] for r in runs))
    sizes = sorted({r["msg_size"] for r in runs})
    return report, runs, links, layers, sizes


def baseline_layer(report, layers):
    """The layer every other one is reported against, which a subset run may choose."""
    return report.get("baseline_layer", layers[0])


def series(runs, link, layer, sizes, field):
    by_size = {r["msg_size"]: r[field] for r in runs if r["link"] == link and r["layer"] == layer}
    return [by_size.get(s) for s in sizes]


def size_label(size):
    """Message size as it would be written, rather than as a power of two."""
    if size >= 1024 and size % 1024 == 0:
        return f"{size // 1024} KiB"
    return f"{size} B"


def size_axis(ax, sizes):
    """Labels the message size axis with the sizes themselves."""
    ax.set_xscale("log", base=2)
    ax.set_xticks(sizes)
    ax.set_xticklabels([size_label(s) for s in sizes], rotation=30)
    ax.set_xticks([], minor=True)
    ax.set_xlabel("message size")


def link_name(link):
    return LINK_NAMES.get(link, link.upper())


def codec_name(codec):
    if not codec:
        return ""
    return CODEC_NAMES.get(codec, codec.replace("_", " "))


def label(runs, layer):
    """Legend label of a layer, preferring the spelled-out form over the module names."""
    codec = next((r["codec"] for r in runs if r["layer"] == layer), None)
    key = layer[: -len(f"_{codec}")] if codec else layer

    if key in LAYER_LABELS:
        return LAYER_LABELS[key].format(codec=codec_name(codec))

    return next(r["layer_description"] for r in runs if r["layer"] == layer)


# Ratios a log axis may span, so that the span is a round factor rather than whatever
# the data happens to need.
NICE_RATIOS = [1.5, 2, 3, 5, 10, 20, 50, 100, 1000]


def equalize_log_axes(axes):
    """Gives every panel the same span on its log y axis.

    Panels covering different link speeds are auto-scaled to different ratios, which makes
    a small relative gap on a slow link look like a large one. Spanning the same factor
    everywhere, anchored at the top of each panel's data, makes the gap between two curves
    mean the same thing in every panel while keeping the absolute values readable.
    """
    limits = []
    for ax in axes:
        values = [v for line in ax.lines for v in line.get_ydata() if v and v > 0]
        limits.append((min(values), max(values)) if values else None)

    needed = max((hi * 1.10) / lo for lo, hi in limits if lo)
    ratio = next((r for r in NICE_RATIOS if r >= needed), needed)

    for ax, limit in zip(axes, limits):
        if limit:
            top = limit[1] * 1.10
            ax.set_ylim(top / ratio, top)


def link_title(runs, link):
    """Title naming the link together with the limits it imposes."""
    limit = next(r["mbytes_per_s_limit"] for r in runs if r["link"] == link)
    rtt = next(r["rtt_ms"] for r in runs if r["link"] == link)
    return f"{link_name(link)} (rtt {rtt:.0f} ms, limit {limit:.0f} MB/s)"


def codecs(runs):
    """Codecs the struct layers were run with."""
    return list(OrderedDict.fromkeys(r["codec"] for r in runs if r.get("codec")))


def grid(links):
    """One subplot per link, laid out as squarely as possible."""
    cols = 2 if len(links) <= 4 else 3
    rows = (len(links) + cols - 1) // cols
    fig, axes = plt.subplots(rows, cols, figsize=(6.5 * cols, 4.2 * rows), squeeze=False)
    return fig, [axes[i // cols][i % cols] for i in range(len(links))], axes, cols


def finish(fig, axes_grid, links, cols, title, out):
    for i in range(len(links), len(axes_grid) * cols):
        row, col = divmod(i, cols)
        if row < len(axes_grid):
            axes_grid[row][col].axis("off")

    handles, labels = fig.axes[0].get_legend_handles_labels()

    # Two legend rows, matching the two rows of subplots, unless the labels are too long
    # to fit that many columns across the figure; then the legend wraps into more rows.
    width = fig.get_size_inches()[0]
    fits = max(1, int(width / (0.075 * (max(len(l) for l in labels) + 6))))
    ncol = min(-(-len(labels) // 2), fits)
    height = 0.030 * -(-len(labels) // ncol) + 0.015

    fig.legend(handles, labels, loc="lower center", ncol=ncol, frameon=False)
    fig.suptitle(title)
    fig.tight_layout(rect=(0, height + 0.02, 1, 0.97))
    fig.savefig(out, dpi=140)
    print(f"Wrote {out}")


def plot_throughput(runs, links, layers, sizes, out):
    fig, axes, axes_grid, cols = grid(links)

    for ax, link in zip(axes, links):
        limit = next(r["mbytes_per_s_limit"] for r in runs if r["link"] == link)

        for layer in layers:
            ax.plot(sizes, series(runs, link, layer, sizes, "mbytes_per_s"),
                    marker="o", label=label(runs, layer))

        ax.axhline(limit, color="grey", linestyle=":", linewidth=1)

        size_axis(ax, sizes)
        ax.set_yscale("log")
        ax.set_title(link_title(runs, link))
        ax.set_ylabel("throughput [MB/s]")
        ax.grid(True, which="both", alpha=0.3)

    equalize_log_axes(axes)
    finish(fig, axes_grid, links, cols, "Remoc throughput", out)


def plot_fraction(runs, links, layers, baseline, sizes, out):
    fig, axes, axes_grid, cols = grid(links)
    others = [l for l in layers if l != baseline]
    width = 0.8 / len(others)

    for ax, link in zip(axes, links):
        positions = range(len(sizes))
        for i, layer in enumerate(others):
            values = [(v or 0) * 100 for v in series(runs, link, layer, sizes, "fraction_of_baseline")]
            offsets = [p + (i - len(others) / 2 + 0.5) * width for p in positions]
            ax.bar(offsets, values, width=width, label=label(runs, layer))

        ax.axhline(100, color="grey", linestyle=":", linewidth=1)
        ax.set_xticks(list(positions))
        ax.set_xlabel("message size")
        ax.set_xticklabels([size_label(s) for s in sizes], rotation=30)
        ax.set_title(link_title(runs, link))
        ax.set_ylabel(f"share of {label(runs, baseline)} [%]")
        ax.grid(True, axis="y", alpha=0.3)

    finish(fig, axes_grid, links, cols, f"Throughput relative to {label(runs, baseline)}", out)


def plot_cpu(runs, links, layers, sizes, out):
    fig, axes, axes_grid, cols = grid(links)

    for ax, link in zip(axes, links):
        for layer in layers:
            ax.plot(sizes, series(runs, link, layer, sizes, "cpu_secs_per_gbyte"),
                    marker="o", label=label(runs, layer))

        size_axis(ax, sizes)
        ax.set_yscale("log")
        ax.set_title(link_title(runs, link))
        ax.set_ylabel("CPU [s/GB, both endpoints]")
        ax.grid(True, which="both", alpha=0.3)

    equalize_log_axes(axes)
    finish(fig, axes_grid, links, cols, "CPU cost per transferred gigabyte", out)


def plot_records(runs, links, layers, codec, sizes, sample_bytes, out):
    """Records per second for one codec, against its plain TCP reference.

    Codecs encode a record to different sizes, so throughput in encoded bytes understates
    a compact codec. Records per second is what compares codecs fairly.
    """
    fig, axes, axes_grid, cols = grid(links)

    reference = f"tcp_struct_{codec}"
    remoc = [l for l in layers if l.startswith("mpsc_struct") and l.endswith(f"_{codec}")]
    drawn = [(reference, REFERENCE_STYLE)] if reference in layers else []
    drawn += [(l, REMOC_STYLES[i % len(REMOC_STYLES)]) for i, l in enumerate(remoc)]

    for ax, link in zip(axes, links):
        for layer, style in drawn:
            ax.plot(sizes, series(runs, link, layer, sizes, "records_per_s"),
                    label=label(runs, layer), **style)

        size_axis(ax, sizes)
        ax.set_yscale("log")
        ax.set_title(link_title(runs, link))
        ax.set_ylabel("records/s")
        ax.grid(True, which="both", alpha=0.3)

    equalize_log_axes(axes)

    title = f"Records per second, {codec_name(codec)} codec"
    if sample_bytes:
        title += f" ({sample_bytes} B per record)"
    finish(fig, axes_grid, links, cols, title, out)


def plot_cpu_per_record(runs, links, layers, sizes, out):
    """CPU cost per million records for the struct layers.

    The byte-normalized cost cannot compare codecs: a compact codec spends its CPU on
    fewer bytes for the same information.
    """
    struct_layers = [l for l in layers if any(r["layer"] == l and r.get("records_per_s") for r in runs)]
    fig, axes, axes_grid, cols = grid(links)

    for ax, link in zip(axes, links):
        for layer in struct_layers:
            ax.plot(sizes, series(runs, link, layer, sizes, "cpu_secs_per_mrecords"),
                    marker="o", label=label(runs, layer))

        size_axis(ax, sizes)
        ax.set_yscale("log")
        ax.set_title(link_title(runs, link))
        ax.set_ylabel("CPU [s/million records]")
        ax.grid(True, which="both", alpha=0.3)

    equalize_log_axes(axes)
    finish(fig, axes_grid, links, cols, "CPU cost per million records", out)


def print_summary(runs, links, layers, sizes):
    for link in links:
        print(f"\n{link_name(link)}")
        header = f"{'layer':<32}" + "".join(f"{s:>12,}" for s in sizes)
        print(header)
        for layer in layers:
            row = f"{label(runs, layer):<32}"
            for value in series(runs, link, layer, sizes, "fraction_of_baseline"):
                row += f"{value * 100:>11.0f}%" if value is not None else f"{'-':>12}"
            print(row)


def compare(runs, baseline_path, threshold):
    _, base_runs, _, _, _ = load(baseline_path)
    key = lambda r: (r["link"], r["layer"], r["msg_size"])
    base = {key(r): r["mbytes_per_s"] for r in base_runs}

    print(f"\nChanges beyond {threshold:.0%} against {baseline_path}")
    changed = False
    for run in runs:
        before = base.get(key(run))
        if not before:
            continue
        change = run["mbytes_per_s"] / before - 1.0
        if abs(change) >= threshold:
            changed = True
            link, layer, size = key(run)
            print(f"  {link:<10} {layer:<14} {size:>9,}  {change:+.1%}")

    if not changed:
        print("  none")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("results", type=Path, help="JSON written by the benchmark")
    parser.add_argument("--baseline", type=Path, help="earlier results to compare against")
    parser.add_argument("--threshold", type=float, default=0.1, help="reporting threshold for --baseline")
    parser.add_argument("--outdir", type=Path, default=Path("plots"))
    parser.add_argument("--format", default="png", choices=["png", "svg"],
                        help="image format; svg stays sharp when published")
    args = parser.parse_args()

    report, runs, links, layers, sizes = load(args.results)
    if report.get("quick"):
        print("Note: these are --quick results and are correspondingly noisy.\n")

    args.outdir.mkdir(parents=True, exist_ok=True)
    out = lambda name: args.outdir / f"{name}.{args.format}"

    baseline = baseline_layer(report, layers)

    plot_throughput(runs, links, layers, sizes, out("throughput"))
    plot_fraction(runs, links, layers, baseline, sizes, out("fraction_of_baseline"))
    plot_cpu(runs, links, layers, sizes, out("cpu"))
    for codec in codecs(runs):
        sample_bytes = report.get("sample_bytes", {}).get(codec)
        plot_records(runs, links, layers, codec, sizes, sample_bytes, out(f"records_{codec}"))
    plot_cpu_per_record(runs, links, layers, sizes, out("cpu_per_record"))

    print_summary(runs, links, layers, sizes)

    if args.baseline:
        compare(runs, args.baseline, args.threshold)


if __name__ == "__main__":
    main()
