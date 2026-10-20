"""使用生产参数做真实低码率对照。素材、产物和报告只写入指定目录。

先构建 src-tauri/examples/codec_plan.rs，再运行本脚本两次：
python scripts/benchmark-codecs.py --ffmpeg PATH --ffprobe PATH --output DIR --label baseline
修改参数并重新构建后，将 --label 改成 candidate。报告保留完整参数和工具版本。
默认素材为合成画面，不能代替真实影片、主观画质和 VRChat 播放测试。
"""

import argparse
import bisect
import hashlib
import json
from pathlib import Path
import re
import subprocess
import time


def run(command, cwd=None, timeout=300):
    result = subprocess.run(
        [str(arg) for arg in command], cwd=cwd, capture_output=True,
        encoding="utf-8", errors="replace", timeout=timeout,
    )
    if result.returncode:
        raise RuntimeError(f"{command[0]} exited {result.returncode}\n{result.stderr[-8000:]}")
    return result


def probe(binary, path, packets=False):
    flags = ["-show_packets", "-select_streams", "v:0"] if packets else ["-show_streams", "-show_format"]
    return json.loads(run([binary, "-v", "error", *flags, "-of", "json", path]).stdout)


def peak_mbps(packets, window):
    entries = sorted(
        (float(p.get("dts_time", p.get("pts_time", 0))), int(p["size"])) for p in packets
    )
    times = [entry[0] for entry in entries]
    prefix = [0]
    for _, size in entries:
        prefix.append(prefix[-1] + size)
    return max((
        (prefix[bisect.bisect_left(times, start + window)] - prefix[i]) * 8 / window / 1e6
        for i, (start, _) in enumerate(entries)
    ), default=0)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ffmpeg", required=True, type=Path)
    parser.add_argument("--ffprobe", required=True, type=Path)
    parser.add_argument("--plan", type=Path, default=Path("src-tauri/target/debug/examples/codec_plan.exe"))
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--label", required=True)
    parser.add_argument("--bitrates", nargs="+", type=float, default=[1.5, 3.0])
    parser.add_argument("--duration", type=float, default=6.0)
    parser.add_argument("--preserve-resolution", action="store_true")
    parser.add_argument("--av1-preset", type=int, choices=[6, 8, 10])
    parser.add_argument("--profiles", nargs="+", choices=["compatible", "av1"], default=["compatible", "av1"])
    args = parser.parse_args()
    if not re.fullmatch(r"[a-zA-Z0-9_-]+", args.label):
        parser.error("label 仅允许英文字母、数字、下划线和连字符")
    if args.duration < 2 or any(not 1 <= rate <= 50 for rate in args.bitrates):
        parser.error("duration 至少为 2；码率范围 1-50 Mbps")
    ffmpeg, ffprobe, plan = (path.resolve() for path in (args.ffmpeg, args.ffprobe, args.plan))
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=True)
    version = run([ffmpeg, "-version"]).stdout
    cases = [
        ("motion", "testsrc2=size=1920x1080:rate=30", "null"),
        ("dark", "testsrc2=size=1920x1080:rate=30", "eq=brightness=-0.25:contrast=0.7:saturation=0.5"),
    ]
    report = {"label": args.label, "ffmpeg": version, "synthetic": True, "results": []}
    report_path = root / f"{args.label}.json"
    if report_path.exists():
        raise FileExistsError(f"报告已存在，请换用新的 label：{report_path}")
    for name, source_filter, filtergraph in cases:
        source = root / f"{name}-{args.duration:g}s.mkv"
        if not source.exists():
            run([ffmpeg, "-hide_banner", "-nostdin", "-y", "-f", "lavfi", "-i", source_filter,
                 "-t", args.duration, "-vf", filtergraph, "-c:v", "ffv1", "-level", 3, source])
        source_probe = probe(ffprobe, source)
        source_probe_path = root / f"{source.stem}.probe.json"
        source_probe_path.write_text(json.dumps(source_probe), encoding="utf-8")
        source_hash = hashlib.sha256(source.read_bytes()).hexdigest()
        for profile in args.profiles:
            for bitrate in args.bitrates:
                case = root / f"{args.label}-{name}-{profile}-{bitrate:g}m"
                case.mkdir(exist_ok=False)
                settings = {key: bitrate for key in (
                    "streaming720pBitrateMbps", "streaming1080pBitrateMbps",
                    "compatible720pBitrateMbps", "compatible1080pBitrateMbps",
                )}
                if args.preserve_resolution:
                    settings["streamingAutoResolution"] = False
                if args.av1_preset is not None:
                    settings["streamingAv1Preset"] = args.av1_preset
                settings_path = case / "settings.json"
                settings_path.write_text(json.dumps(settings), encoding="utf-8")
                dest = case / "output.mp4"
                command_args = json.loads(run([plan, source, dest, profile, source_probe_path, settings_path]).stdout)
                start = time.perf_counter()
                encoded = run([ffmpeg, *command_args], timeout=600)
                elapsed = time.perf_counter() - start
                (case / "encode.txt").write_text(encoded.stderr, encoding="utf-8")
                result = probe(ffprobe, dest)
                video = next(s for s in result["streams"] if s["codec_type"] == "video")
                width = source_probe["streams"][0]["width"]
                height = source_probe["streams"][0]["height"]
                metrics_filter = (
                    f"[0:v]scale={width}:{height}:flags=bicubic,fps=30,setpts=PTS-STARTPTS,format=yuv420p,split[d1][d2];"
                    "[1:v]fps=30,setpts=PTS-STARTPTS,format=yuv420p,split[r1][r2];"
                    "[d1][r1]libvmaf=log_fmt=json:log_path=vmaf.json[v];[d2][r2]ssim[s]"
                )
                metrics = run([ffmpeg, "-hide_banner", "-nostdin", "-i", dest, "-i", source,
                               "-lavfi", metrics_filter, "-map", "[v]", "-map", "[s]", "-an", "-f", "null", "-"], cwd=case)
                vmaf = json.loads((case / "vmaf.json").read_text())["pooled_metrics"]["vmaf"]["mean"]
                ssim = float(re.search(r"All:([\d.]+)", metrics.stderr).group(1))
                packets = probe(ffprobe, dest, packets=True)["packets"]
                duration = float(result["format"]["duration"])
                row = {
                    "source": name, "source_sha256": source_hash, "profile": profile,
                    "budget_mbps": bitrate, "dimensions": [video["width"], video["height"]],
                    "pixel_format": video["pix_fmt"], "frame_rate": video["avg_frame_rate"],
                    "encode_seconds": round(elapsed, 3), "file_bytes": dest.stat().st_size,
                    "video_mbps": sum(int(p["size"]) for p in packets) * 8 / duration / 1e6,
                    "peak_1s_mbps": peak_mbps(packets, 1), "peak_2s_mbps": peak_mbps(packets, 2),
                    "vmaf": vmaf, "ssim": ssim, "args": command_args,
                }
                report["results"].append(row)
                report_path.write_text(json.dumps(report, indent=2, ensure_ascii=False), encoding="utf-8")
                print(json.dumps({k: v for k, v in row.items() if k not in ("args", "source_sha256")}), flush=True)


if __name__ == "__main__":
    main()
