#!/usr/bin/env python3
"""Decode-layer MPEG-TS monitor: record N seconds, ffprobe frames + ffmpeg warnings.

Requires ffmpeg / ffprobe on PATH.

Usage:
  ./script/monitor_decode_ffprobe.py http://127.0.0.1:8080/live/demo/mpegts
  ./script/monitor_decode_ffprobe.py http://127.0.0.1:8080/live/demo/mpegts 180
  MPEGTS_URL=http://127.0.0.1:8080/live/demo/mpegts ./script/monitor_decode_ffprobe.py

Do not hard-code real production stream URLs in this script.
"""
from __future__ import annotations

import argparse
import os
import re
import statistics
import subprocess
import sys
import tempfile
from collections import Counter, defaultdict


def run(cmd: list[str], timeout: float | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)


def require_tools() -> None:
    missing = []
    for tool in ("ffmpeg", "ffprobe"):
        r = run(["which", tool])
        if r.returncode != 0:
            missing.append(tool)
    if missing:
        print(f"error: missing required tools: {', '.join(missing)}", file=sys.stderr)
        sys.exit(1)


def record_stream(url: str, path: str, duration: int) -> int:
    print(f"Recording {duration}s → {path}", flush=True)
    r = run(
        [
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-i",
            url,
            "-t",
            str(duration),
            "-c",
            "copy",
            "-f",
            "mpegts",
            path,
        ],
        timeout=duration + 60,
    )
    if r.returncode not in (0, 255):  # 255 sometimes on live EOF
        print(f"record stderr: {r.stderr[:500]}", flush=True)
    size = os.path.getsize(path) if os.path.exists(path) else 0
    print(f"  recorded {size / 1024 / 1024:.1f} MB", flush=True)
    return size


def ffprobe_format(path: str) -> str:
    r = run(
        [
            "ffprobe",
            "-v",
            "error",
            "-show_format",
            "-show_streams",
            "-of",
            "default=noprint_wrappers=1",
            path,
        ]
    )
    return r.stdout + r.stderr


def ffprobe_frames(path: str) -> tuple[str, str]:
    print("ffprobe frame timing ...", flush=True)
    r = run(
        [
            "ffprobe",
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "frame=key_frame,pkt_pts_time,pict_type",
            "-of",
            "csv=p=0",
            path,
        ],
        timeout=300,
    )
    return r.stdout, r.stderr


def ffmpeg_decode(path: str) -> str:
    print("ffmpeg full decode pass ...", flush=True)
    r = run(
        [
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "warning",
            "-i",
            path,
            "-map",
            "0:v:0",
            "-map",
            "0:a:0?",
            "-f",
            "null",
            "-",
        ],
        timeout=300,
    )
    return r.stderr


def _f(s: str) -> float | None:
    try:
        return float(s) if s and s != "N/A" else None
    except ValueError:
        return None


def parse_frames(csv_text: str) -> list[dict]:
    frames = []
    for line in csv_text.strip().splitlines():
        if line.startswith("[") or "@" in line[:20]:
            continue
        parts = [p.strip() for p in line.split(",")]
        if len(parts) < 3:
            continue
        # ffprobe csv: key_frame,pkt_pts_time,pict_type
        key = parts[0] == "1"
        pts = _f(parts[1])
        pict = parts[2] if parts[2] in ("I", "P", "B") else "?"
        if pts is None:
            continue
        frames.append({"pts": pts, "type": pict, "key": key})
    return frames


def analyze_frame_gaps(frames: list[dict], thresholds=(0.5, 1.0, 2.0)):
    gaps = []
    for i in range(1, len(frames)):
        dt = frames[i]["pts"] - frames[i - 1]["pts"]
        if dt > 0:
            gaps.append((frames[i]["pts"], dt, frames[i]["type"], frames[i]["key"]))
    sg = sorted(g for _, g, _, _ in gaps)
    stalls = {t: [] for t in thresholds}
    for pts, dt, pict, key in gaps:
        for t in thresholds:
            if dt >= t:
                stalls[t].append((pts, dt, pict, key))
    return gaps, sg, stalls


def parse_ffmpeg_warnings(text: str):
    patterns = {
        "sps_pps": re.compile(
            r"non-existing SPS|no frame|SPS \d+ out of range|missing picture|no poc",
            re.I,
        ),
        "ref_missing": re.compile(
            r"reference picture missing|mmco: unref|illegal|corrupt|error while decoding",
            re.I,
        ),
        "dts": re.compile(r"non monotonically increasing dts|Invalid data|Packet corrupt", re.I),
        "discontinuity": re.compile(r"discontinuity|timestamp.*jump", re.I),
        "aac": re.compile(r"AAC|audio.*error|Sample rate", re.I),
    }
    hits: Counter[str] = Counter()
    samples: dict[str, list[str]] = defaultdict(list)
    for line in text.splitlines():
        for name, pat in patterns.items():
            if pat.search(line):
                hits[name] += 1
                if len(samples[name]) < 5:
                    samples[name].append(line.strip()[:200])
    return hits, samples


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Record MPEG-TS and check decode-layer frame gaps / warnings"
    )
    parser.add_argument(
        "url",
        nargs="?",
        default=os.environ.get("MPEGTS_URL"),
        help="MPEG-TS URL (or set MPEGTS_URL)",
    )
    parser.add_argument(
        "duration",
        nargs="?",
        type=int,
        default=int(os.environ.get("MONITOR_SECS", "180")),
        help="record duration in seconds (default: 180)",
    )
    args = parser.parse_args()
    if not args.url:
        print(
            "error: provide MPEG-TS URL as arg or MPEGTS_URL env\n"
            "  example: ./script/monitor_decode_ffprobe.py "
            "http://127.0.0.1:8080/live/demo/mpegts 180",
            file=sys.stderr,
        )
        sys.exit(1)
    if args.duration < 5:
        print("error: duration must be >= 5", file=sys.stderr)
        sys.exit(1)

    require_tools()

    print("=== Decode-layer monitor ===", flush=True)
    print(f"URL: {args.url}", flush=True)
    print(f"Duration: {args.duration}s\n", flush=True)

    with tempfile.NamedTemporaryFile(suffix=".ts", delete=False) as f:
        path = f.name

    try:
        size = record_stream(args.url, path, args.duration)
        if size < 100_000:
            print("ERROR: recording too small", flush=True)
            sys.exit(1)

        fmt = ffprobe_format(path)
        for line in fmt.splitlines():
            if any(
                k in line
                for k in ("codec_name", "width", "height", "r_frame_rate", "duration", "bit_rate")
            ):
                print(f"  {line}", flush=True)

        csv, ferr = ffprobe_frames(path)
        if ferr.strip():
            print(f"ffprobe frame stderr: {ferr[:300]}", flush=True)

        frames = parse_frames(csv)
        print(f"\nvideo frames parsed: {len(frames)}", flush=True)
        if len(frames) < 2:
            print("ERROR: not enough frames", flush=True)
            sys.exit(1)

        duration_pts = frames[-1]["pts"] - frames[0]["pts"]
        fps = len(frames) / duration_pts if duration_pts > 0 else 0
        keys = sum(1 for f in frames if f["key"])
        gaps, sg, stalls = analyze_frame_gaps(frames)

        print(
            f"media span:       {duration_pts:.1f}s "
            f"(first_pts={frames[0]['pts']:.3f} last={frames[-1]['pts']:.3f})"
        )
        print(f"implied fps:      {fps:.2f}")
        print(f"keyframes:        {keys}")
        if sg:
            print(
                f"frame gap (s):    min={sg[0]:.4f} med={statistics.median(sg):.4f} "
                f"p90={sg[int(len(sg) * 0.9)]:.4f} p99={sg[int(len(sg) * 0.99)]:.4f} "
                f"max={sg[-1]:.4f}"
            )

        for thr in (0.5, 1.0, 2.0):
            ev = stalls[thr]
            print(f"decode gaps >={thr}s: {len(ev)}")
            for pts, dt, pict, key in ev[:15]:
                print(f"  pts={pts:8.3f}s gap={dt:.3f}s type={pict} key={key}")
            if len(ev) > 15:
                print(f"  ... and {len(ev) - 15} more")

        bins: Counter[str] = Counter()
        for _, g, _, _ in gaps:
            if g < 0.05:
                bins["<50ms"] += 1
            elif g < 0.1:
                bins["50-100ms"] += 1
            elif g < 0.2:
                bins["100-200ms"] += 1
            elif g < 0.5:
                bins["200-500ms"] += 1
            elif g < 1.0:
                bins["0.5-1.0s"] += 1
            else:
                bins[">=1.0s"] += 1
        print("frame gap histogram:")
        for k in ["<50ms", "50-100ms", "100-200ms", "200-500ms", "0.5-1.0s", ">=1.0s"]:
            print(f"  {k}: {bins[k]}")

        dec_err = ffmpeg_decode(path)
        hits, samples = parse_ffmpeg_warnings(dec_err)
        print("\nffmpeg decode warnings:")
        if not hits:
            print("  (none)")
        for k, n in hits.most_common():
            print(f"  {k}: {n}")
            for s in samples[k]:
                print(f"    {s}")

        print("\n=== VERDICT ===", flush=True)
        big = len(stalls[1.0])
        med = len(stalls[0.5])
        warn = sum(hits.values())
        if big > 0:
            print(f"DECODE STUTTER: {big} frame gaps >=1.0s — likely visible freeze")
            sys.exit(2)
        if med > 5:
            print(f"MILD DECODE STUTTER: {med} frame gaps >=0.5s")
            sys.exit(2)
        if warn > 50:
            print(f"DECODE WARNINGS: {warn} (SPS/ref/corrupt) — may cause micro-stutter")
            sys.exit(2)
        if warn > 0:
            print(f"MINOR WARNINGS: {warn} decode warnings, {med} gaps >=0.5s")
            return
        print("DECODE OK: no significant frame gaps or decode warnings")
    finally:
        try:
            os.unlink(path)
        except OSError:
            pass


if __name__ == "__main__":
    main()
