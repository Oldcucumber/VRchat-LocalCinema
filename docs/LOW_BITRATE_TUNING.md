# 低码率调优与验证

目标是在相近实际码率下改善观感，同时维持当前播放格式约束。下面的自动规格只是可选策略；合成片段的结果不能代替真实影片、主观对照和 VRChat 播放验证。

## 实现参数

| 项目 | AV1 省流 | H.264 兼容 |
| --- | --- | --- |
| 编码器 | libsvtav1 | libx264 |
| 速度 | 缺省 8，可选 10 / 8 / 6；已保存的值保留 | 缺省 slow，可选 medium |
| 画质与码率 | CRF 28，tune=0，maxrate，mbr-overshoot-pct=10 | CRF 22，VBV maxrate 与两秒 bufsize |
| 感知参数 | 使用编码器视觉调优，暂不开启 variance boost | aq-mode=3、aq-strength=1.0，其他心理视觉参数沿用 preset |
| 像素格式 | yuv420p 8-bit | yuv420p 8-bit，Main Level 4.1 |
| 参考帧 | 编码器默认 | 4，避免 slow 默认值超出 1080P Level 4.1 的 DPB |
| GOP | 约两秒，使用编码器默认闭合关键帧策略 | 最大约两秒、闭合 GOP；场景切换可提前插帧 |
| 帧率 | 默认保留源帧率 | 保留低于 30fps 的源帧率，最高 30fps |
| 音频 | AAC-LC 双声道；视频预算 ≤3 Mbps 为 128k，其余 160k | 同左，采样率固定 48kHz |

AV1 的 capped CRF 超调目标不等于任意一秒窗口的硬限制。SVT 的 CRF 码率控制不能按 x264 VBV 来解释，故一键 AV1 不再传入 bufsize。音频、封装及 HTTP 传输开销独立于视频码率。

提交任务时先检查编码器和滤镜，再用实际编码参数压制一帧黑画面到 null，提前发现组件版本不接受的参数；不会读取片源或写媒体文件。

## 分辨率与预算

先把源宽高按比例缩入 1920×1080，若像素量不超过 1280×720，则选用设置中的 720P 档预算；否则选 1080P 档。不会因为后续自动降分辨率而再切换预算。所有一键压制保持宽高比、不放大、输出偶数尺寸。

「低码率自动调整画面」默认关闭。开启后：

| 视频预算 | AV1 最大画面 | H.264 最大画面 |
| --- | --- | --- |
| ≤1.5 Mbps | 1280×720 | 854×480 |
| >1.5 且 ≤2 Mbps | 1280×720 | 1280×720 |
| >2 且 ≤3 Mbps | 1920×1080 | 1280×720 |
| >3 Mbps | 1920×1080 | 1920×1080 |

开启时，≤3 Mbps 的 AV1 也限制至最高 30fps；关闭则保留 AV1 源帧率。文字、字幕、桌面及细线内容需要特别对照，降低分辨率可能比压缩伪影损失更多细节。当前合成运动/暗场样本已出现这种情况，因此没有默认启用。

10-bit AV1、variance boost、四秒 GOP 和两遍编码继续留在实验阶段，需要分别验证播放器、暗部细节、跳转恢复和队列流程后再推广。

## 复现对照

需要可用的 Rust Windows 构建环境，以及包含 libsvtav1、libx264、libvmaf、FFV1、zscale、tonemap 的 FFmpeg/FFprobe。脚本不会启动 HTTP 服务或对外分享媒体。

```powershell
cargo build --manifest-path src-tauri/Cargo.toml --example codec_plan
python scripts/benchmark-codecs.py --ffmpeg C:/tools/ffmpeg.exe --ffprobe C:/tools/ffprobe.exe --output components/codec-validation/bench --label baseline
# 修改参数、重新构建 codec_plan 后使用不同 label 对照
python scripts/benchmark-codecs.py --ffmpeg C:/tools/ffmpeg.exe --ffprobe C:/tools/ffprobe.exe --output components/codec-validation/bench --label tuned
```

工具通过 `codec_plan` 调用生产代码中的 `streaming_options` 和 `build_args`，避免测试脚本另写一套参数。使用两段 6 秒 1080P / 30fps FFV1 合成参考，分别为运动测试画面和降低亮度的版本；不含真实影片。输出包含工具版本、源 SHA256、完整参数、实际平均视频码率、滑动 1 / 2 秒峰值、编码耗时、文件大小及 VMAF / SSIM。各输出先按相同方式缩放回源尺寸并对齐帧率与时间戳，再进行质量比较。

可通过 `--bitrates 1.5 3`、`--duration 12`、`--preserve-resolution`、`--av1-preset 6` 调整测试。报告和测试媒体留在指定目录，脚本拒绝覆盖已有 label 报告。要比较旧参数，必须在修改前构建并运行一次 baseline。

验证命令：

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --locked
$env:FFMPEG_TEST = 'C:/tools/ffmpeg.exe'
$env:FFPROBE_TEST = 'C:/tools/ffprobe.exe'
cargo test --manifest-path src-tauri/Cargo.toml --test streaming_test -- --ignored
node --test scripts/check-upstream-compat.mjs
npm run build
npm exec --yes --package=vue-tsc --package=typescript@6.0.2 -- vue-tsc --noEmit
```

真实 FFmpeg 回归覆盖奇数尺寸、超宽高帧率、无音轨、HDR10、HLG 和低预算自动降规格；检查输出流、像素格式、色彩、FastStart，并完整解码两种编码的产物。它不能证明 VRChat 或远端设备的实播效果。

## 本轮实测（2026-10-20）

测试工具为 FFmpeg `N-127790-g56dbd4cd37-20261020`、SVT-AV1 `4.2.0-151-g5ed9ffbcb`。`baseline` 为本轮修改前的本地参数，`tuned` 为本轮最终默认参数，两者均保留 1080P。全部原始参数与结果保存在 [JSON 报告](benchmarks/2026-10-20-codecs.json)，中间开启降分辨率的实验另记为 `candidate`，可选 preset 6 记为 `av1-quality`。

| AV1 样本 / 预算 | 实际视频码率：修改前 → 后 | VMAF：修改前 → 后 | 文件大小变化 |
| --- | --- | --- | --- |
| 运动 / 1.5 Mbps | 1.397 → 1.173 Mbps | 84.36 → 84.02 | -15.9% |
| 运动 / 3 Mbps | 2.780 → 2.432 Mbps | 87.02 → 89.20 | -12.5% |
| 暗场 / 1.5 Mbps | 1.445 → 1.167 Mbps | 85.73 → 87.85 | -19.1% |
| 暗场 / 3 Mbps | 2.988 → 2.503 Mbps | 90.48 → 92.05 | -16.2% |

H.264 四组 VMAF 提高约 0.11–0.29，文件大小基本相同（-0.6% 至 +0.3%）；小幅差异不构成普遍的主观画质承诺。编码耗时增加：本机短片段 H.264 约 14%–32%，AV1 均衡档约 18%–44%。preset 6 的耗时约为均衡档的两倍，画质与体积表现更好，因此保留为可选档。

自动降分辨率实验使 H.264 运动样本在 1.5 / 3 Mbps 预算下的 VMAF 从 84.36 / 92.41 降为 78.13 / 87.62，故最终默认关闭。结果也显示：降低 capped CRF 的超调目标仍不能保证一秒峰值不超预算。例如最终 AV1 3 Mbps 运动样本的一秒峰值为 4.71 Mbps。

这些对照使用相同预算，实际平均码率并不完全相同，不能视为严格等码率率失真曲线。合成素材没有真实音轨，静音 AAC 不代表真实音频的压缩表现；短片段的编码时间也不能直接外推到长片。尚未验证真实影片主观画质、远端网络和 VRChat 首播/同步跳转。

本机测试采用 `components/codec-validation/` 下的独立 FFmpeg 与 Windows SDK，通过该目录的 `cargo-local.ps1` 设置当前构建进程的 PATH / INCLUDE / LIB，没有修改系统环境或安装注册信息；工具及合成产物不随 Git 提交。

## Cloudflare Tunnel 优化方向

项目当前使用免账号的 Quick Tunnel。链路是观看方 → Cloudflare 公网入口 → Tunnel 边缘连接 → 分享者本机。只优化其中一段，不代表整条链路吞吐都会提高。

1. 优先实测 `auto / quic / http2` 与 IPv4 / IPv6 的组合。`auto` 会优先 QUIC，在无法建立 UDP 连接时回退 HTTP2；已经连通但 UDP 质量差时，手动 HTTP2 仍值得对照。两者使用 Tunnel 专用的 7844 端口，不是普通 CDN 的 443。
2. 优选应以观看方持续读取实际测试文件的吞吐、停顿、首次响应和重连次数为依据。仅测分享者到边缘的 ping 或 TCP 握手，不足以判断观看体验。建议下一步增加独立测试素材的端到端诊断，而不是后台自动扫描/切换节点。
3. 普通 CDN 优选 IP 不能直接当作 Tunnel 上行地址。cloudflared 的隐藏 `--edge` 在官方源代码中标为内部测试用途，不应成为应用正式选项；`--region` 公开可选的是 global / us，不能据此选择香港或日本机房。
4. 观看方入口优选属于另一个方向。稳定域名、正确的 TLS SNI/Host 和接收端测量是进一步研究的前提；不能把播放 URL 的域名直接替换为 IP。Quick Tunnel 域名每次创建会变化，应用也无法替所有观看者修改 DNS。
5. 固定命名 Tunnel 可以提供稳定域名，便于长期复测与诊断，但没有自动提升带宽的保证。Quick Tunnel 无可用性保证，最多 200 个同时在途请求，该限制不是 200 名观众。

本轮只实现编码调优和复测工具，未改变 Tunnel 连接参数，也未启动对外隧道。后续优先考虑：显示实际协议/边缘位置、提供连接方式选项、使用临时测试素材做接收端测速，再决定是否需要更复杂的入口优化。

参考官方资料（检查日期：2026-10-20）：

- [SVT-AV1 参数](https://github.com/AOMediaCodec/SVT-AV1/blob/main/Docs/Parameters.md)
- [FFmpeg SVT-AV1 封装实现](https://github.com/FFmpeg/FFmpeg/blob/master/libavcodec/libsvtav1.c)
- [Cloudflare Tunnel 启动参数](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/configure-tunnels/run-parameters/)
- [Tunnel 防火墙与端口](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/configure-tunnels/tunnel-with-firewall/)
- [Quick Tunnel 限制](https://developers.cloudflare.com/tunnel/get-started/quick-tunnels/)
- [cloudflared 参数定义](https://github.com/cloudflare/cloudflared/blob/master/cmd/cloudflared/tunnel/cmd.go)
