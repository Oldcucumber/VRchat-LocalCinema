//! 转码选项与 ffmpeg 参数构建。
//!
//! 前端「转码设置」对话框产出一个 [`TranscodeOptions`]，本模块负责把它翻译成
//! ffmpeg 参数：视频可直拷或重编码、音频可直拷或转 AAC、字幕可烧录 / 内嵌 / 丢弃。
//! 同时负责解析 ffprobe 输出的完整流清单（供对话框展示音轨 / 字幕轨）。

use serde::{Deserialize, Serialize};
use std::path::Path;

/// 可直拷进 MP4 的视频编码（其余一律重编码）
pub const COPYABLE_VIDEO: &[&str] = &["h264", "hevc", "vp9", "av1", "mpeg4"];
/// 可直拷进 MP4 的音频编码（DTS / TrueHD 等 MP4 不支持，必须转 AAC）
pub const COPYABLE_AUDIO: &[&str] = &["aac", "mp3", "ac3", "eac3"];
/// 文本字幕编码（可烧录 / 转文本 / 内嵌）
pub const TEXT_SUBTITLE_CODECS: &[&str] = &[
    "subrip", "srt", "ass", "ssa", "mov_text", "webvtt", "text", "dvb_teletext", "eia_608",
];
/// 图形字幕编码（只能烧录，不能内嵌或外挂）
pub const GRAPHIC_SUBTITLE_CODECS: &[&str] = &[
    "hdmv_pgs_subtitle", "dvd_subtitle", "dvdsub", "pgssub", "dvb_subtitle",
];
/// 外挂（同目录同名）字幕文件扩展名 → ffprobe 风格编码名
pub const SIDECAR_SUBTITLE_EXTS: &[(&str, &str)] = &[
    ("srt", "subrip"),
    ("ass", "ass"),
    ("ssa", "ssa"),
    ("vtt", "webvtt"),
];
/// 外挂字幕在流清单里使用的合成索引基数。
/// 真实流索引由 ffprobe 报告，绝不可能达到这个量级；前端与后端凭
/// `index >= SIDECAR_INDEX_BASE` 即可区分「旁边的字幕文件」与内嵌字幕轨。
pub const SIDECAR_INDEX_BASE: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamingProfile {
    Av1,
    Compatible,
}

/// 转码配置（字段缺省时按「自动」处理）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TranscodeOptions {
    /// 视频流绝对索引；None = 第一路视频
    pub video_stream: Option<usize>,
    /// 音频流绝对索引；None = 第一路音频
    pub audio_stream: Option<usize>,
    /// 是否保留音频（默认保留）
    pub include_audio: Option<bool>,
    /// 字幕流绝对索引；`subtitle_mode` 为烧录 / 内嵌时生效，None = 自动选第一条文本字幕
    pub subtitle_stream: Option<usize>,
    /// 字幕处理：burn（烧录）/ embed（内嵌）/ drop（丢弃）；None = 丢弃
    pub subtitle_mode: Option<String>,
    /// 选中的字幕流是否为图形（位图）字幕：图形字幕只能用 overlay 方式烧录
    pub subtitle_graphic: Option<bool>,
    /// 视频处理：copy / h264 / h265 / av1
    pub video_mode: Option<String>,
    /// 目标分辨率：original / 高度数字；一键压制使用不放大的 fit1080 / fit720 / fit480。
    pub resolution: Option<String>,
    /// 质量（CRF，0-51，越小越清晰）
    pub crf: Option<u32>,
    /// 编码预设：ultrafast … veryslow
    pub preset: Option<String>,
    /// 视频像素格式：auto / yuv420p / yuv420p10le
    pub pix_fmt: Option<String>,
    /// 音频处理：copy / aac
    pub audio_mode: Option<String>,
    /// AAC 码率
    pub audio_bitrate: Option<String>,
    /// 音频声道数（降混）：None 或 0 = 保持原样
    pub audio_channels: Option<u32>,
    /// 音频采样率
    pub audio_sample_rate: Option<u32>,
    /// MP4 faststart（默认开启）
    pub faststart: Option<bool>,
    /// 视频峰值码率（如 6000k），用于平滑网络突发带宽
    pub max_video_bitrate: Option<String>,
    /// 视频码率缓冲区（如 12000k）
    pub video_buffer_size: Option<String>,
    /// 闭合 GOP 的目标最大帧数
    pub keyframe_interval: Option<u32>,
    /// 一键压制预设；普通转码留空，沿用原有参数与命名
    pub streaming_profile: Option<StreamingProfile>,
    /// 固定输出帧率（兼容模式或可选低码率自动调整启用时最高 30fps）
    pub frame_rate: Option<String>,
    /// HDR10 / HLG 转 SDR；仅在探测到明确 HDR 元数据时启用
    pub tone_map: bool,
    /// 无音轨输入补静音，使产物满足单视频 + 单 AAC 音轨的播放约定
    pub silent_audio: bool,
}

/// 统一生成一键压制参数，所有前端入口共用；不根据分享者本机推断朋友的解码能力。
pub fn streaming_options(
    profile: StreamingProfile,
    json: &serde_json::Value,
    settings: &crate::settings::Settings,
) -> Result<TranscodeOptions, String> {
    let streams = parse_streams(json);
    let video_index = default_video_stream(&streams).ok_or("未检测到可压制的视频轨")?;
    let video = json["streams"].as_array()
        .and_then(|streams| streams.iter().find(|s| s["index"].as_u64() == Some(video_index as u64)))
        .ok_or("缺少视频轨信息")?;
    if video["side_data_list"].as_array().is_some_and(|data| data.iter().any(|s| {
        s["side_data_type"].as_str().unwrap_or("").to_ascii_lowercase().contains("dovi")
    })) {
        return Err("一键压制暂不支持 Dolby Vision，请使用经验证的 SDR 片源或高级转码流程".into());
    }
    let tone_map = matches!(video["color_transfer"].as_str(), Some("smpte2084" | "arib-std-b67"));
    if video["color_primaries"].as_str() == Some("bt2020") && !tone_map {
        return Err("片源使用 BT.2020，但无法确认 HDR 类型；请先转换为 SDR，避免压制后颜色异常".into());
    }
    let rate = ["avg_frame_rate", "r_frame_rate"]
        .into_iter()
        .filter_map(|key| video[key].as_str())
        .find(|rate| parse_frame_rate(rate).is_some())
        .unwrap_or("0/0");
    let fps = parse_frame_rate(rate);
    let compatible = profile == StreamingProfile::Compatible;
    // 按源画面缩入 1080P 后的像素量选择预算，兼顾竖屏与超宽片源。
    let width = video["width"].as_f64().filter(|v| *v > 0.0).unwrap_or(1920.0);
    let height = video["height"].as_f64().filter(|v| *v > 0.0).unwrap_or(1080.0);
    let scale = (1920.0 / width).min(1080.0 / height).min(1.0);
    let small = width * height * scale * scale <= 1280.0 * 720.0;
    let (mbps, fallback) = match (profile, small) {
        (StreamingProfile::Av1, true) => (settings.streaming_720p_bitrate_mbps, 3.0),
        (StreamingProfile::Av1, false) => (settings.streaming_1080p_bitrate_mbps, 6.0),
        (StreamingProfile::Compatible, true) => (settings.compatible_720p_bitrate_mbps, 4.0),
        (StreamingProfile::Compatible, false) => (settings.compatible_1080p_bitrate_mbps, 8.0),
    };
    let mbps = if mbps.is_finite() {
        mbps.clamp(1.0, if compatible { 50.0 } else { 100.0 })
    } else {
        fallback
    };
    let kbps = (mbps * 1000.0).round() as u32;
    let low_rate = settings.streaming_auto_resolution && kbps <= 3000;
    let frame_rate = (compatible || low_rate).then(|| {
        if fps.is_some_and(|fps| fps <= 30.0) {
            rate.to_string()
        } else {
            "30".into()
        }
    });
    let output_fps = if compatible || low_rate {
        Some(fps.unwrap_or(30.0).min(30.0))
    } else {
        fps
    };
    let preset = match settings.streaming_av1_preset {
        6 | 8 | 10 => settings.streaming_av1_preset,
        _ => 8,
    };
    let resolution = if settings.streaming_auto_resolution && compatible && kbps <= 1500 {
        "fit480"
    } else if settings.streaming_auto_resolution && (kbps <= 2000 || (compatible && kbps <= 3000)) {
        "fit720"
    } else {
        "fit1080"
    };
    let audio_stream = default_audio_stream(&streams);
    Ok(TranscodeOptions {
        streaming_profile: Some(profile),
        video_stream: Some(video_index),
        audio_stream,
        video_mode: Some(if compatible { "h264" } else { "av1" }.into()),
        audio_mode: Some("aac".into()),
        include_audio: Some(true),
        subtitle_mode: Some("drop".into()),
        resolution: Some(resolution.into()),
        crf: Some(if compatible { 22 } else { 28 }),
        preset: Some(if compatible {
            match settings.compatible_preset.as_str() {
                "medium" => "medium".into(),
                _ => "slow".into(),
            }
        } else { preset.to_string() }),
        pix_fmt: Some("yuv420p".into()),
        audio_bitrate: Some(if kbps <= 3000 { "128k" } else { "160k" }.into()),
        audio_channels: Some(2),
        audio_sample_rate: compatible.then_some(48000),
        faststart: Some(true),
        frame_rate,
        tone_map,
        silent_audio: audio_stream.is_none(),
        max_video_bitrate: Some(format!("{}k", kbps)),
        // SVT 的 capped CRF 不使用 x264 VBV 缓冲语义，不传无效的 bufsize。
        video_buffer_size: compatible.then(|| format!("{}k", (kbps * 2).min(62500))),
        keyframe_interval: output_fps.map(|fps| (fps * 2.0).ceil().clamp(1.0, 600.0) as u32),
        ..Default::default()
    })
}

fn parse_frame_rate(rate: &str) -> Option<f64> {
    let fps = if let Some((n, d)) = rate.split_once('/') {
        n.parse::<f64>().ok()? / d.parse::<f64>().ok()?
    } else {
        rate.parse().ok()?
    };
    (fps.is_finite() && fps > 0.0).then_some(fps)
}

pub fn output_label(options: &TranscodeOptions) -> &'static str {
    match options.streaming_profile {
        Some(StreamingProfile::Av1) => "省流 AV1",
        Some(StreamingProfile::Compatible) => "兼容 H264",
        None => "转码",
    }
}

/// 仅在用户提交一键压制时探测，不为每张媒体卡片反复启动 FFmpeg。
pub async fn check_streaming_capabilities(
    ffmpeg: &Path,
    options: &TranscodeOptions,
) -> Result<(), String> {
    let encoder = if options.video_mode() == "av1" { "libsvtav1" } else { "libx264" };
    let mut filters = vec!["scale"];
    if options.tone_map {
        filters.extend(["zscale", "tonemap", "sidedata"]);
    }
    if options.silent_audio {
        filters.push("anullsrc");
    }
    for (flag, names) in [
        ("-encoders", vec![encoder, "aac"]),
        ("-filters", filters),
    ] {
        let mut cmd = tokio::process::Command::new(ffmpeg);
        cmd.args(["-hide_banner", flag]).kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x08000000);
        let output = tokio::time::timeout(std::time::Duration::from_secs(10), cmd.output())
            .await.map_err(|_| "检查 FFmpeg 能力超时".to_string())?
            .map_err(|e| format!("检查 FFmpeg 能力失败: {}", e))?;
        if !output.status.success() {
            return Err("FFmpeg 无法列出编码器或滤镜，请检查组件".into());
        }
        let text = String::from_utf8_lossy(&output.stdout);
        for name in names {
            if !text.lines().any(|line| line.split_whitespace().nth(1) == Some(name)) {
                return Err(format!("FFmpeg 缺少 {}，请更新组件或选择其他压制方式", name));
            }
        }
    }
    // 列出编码器不代表当前版本接受所用参数；用相同参数编码一帧到 null，提前报错。
    // 只生成内存中的黑帧，不读取片源、不写文件。
    let mut probe_options = options.clone();
    probe_options.video_stream = Some(0);
    probe_options.include_audio = Some(false);
    probe_options.silent_audio = false;
    probe_options.tone_map = false;
    probe_options.resolution = None;
    probe_options.frame_rate = None;
    probe_options.faststart = Some(false);
    let mut args = build_args("color=c=black:s=128x128:r=24:d=0.05", Path::new("-"), &probe_options, None, None, None);
    let input = args.iter().position(|arg| arg == "-i").unwrap();
    args.splice(input..input, ["-f".into(), "lavfi".into()]);
    args.pop();
    args.extend(["-frames:v".into(), "1".into(), "-f".into(), "null".into(), "-".into()]);
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-loglevel", "error"])
        .args(args).stdin(std::process::Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x08000000);
    let output = tokio::time::timeout(std::time::Duration::from_secs(20), cmd.output())
        .await.map_err(|_| "检查压制参数超时，请检查 FFmpeg 组件".to_string())?
        .map_err(|e| format!("检查压制参数失败: {}", e))?;
    if !output.status.success() {
        return Err(format!("FFmpeg 无法使用当前压制参数，请更新组件或更换压制模式：{}",
            String::from_utf8_lossy(&output.stderr).chars().take(2000).collect::<String>()));
    }
    Ok(())
}

impl TranscodeOptions {
    pub fn video_mode(&self) -> &str {
        self.video_mode.as_deref().unwrap_or("h264")
    }
    pub fn audio_mode(&self) -> &str {
        self.audio_mode.as_deref().unwrap_or("aac")
    }
    pub fn include_audio(&self) -> bool {
        self.include_audio.unwrap_or(true)
    }
    pub fn faststart(&self) -> bool {
        self.faststart.unwrap_or(true)
    }
    pub fn video_copy(&self) -> bool {
        self.video_mode() == "copy"
    }
    pub fn audio_copy(&self) -> bool {
        self.audio_mode() == "copy"
    }
    /// 是否需要把字幕烧进画面（需要视频重编码）
    pub fn burn_subtitles(&self) -> bool {
        self.subtitle_mode.as_deref() == Some("burn")
    }
    /// 是否需要把字幕作为独立轨道内嵌
    pub fn embed_subtitles(&self) -> bool {
        self.subtitle_mode.as_deref() == Some("embed")
    }
    /// 是否把字幕抽取为独立文件放在产物旁边（外挂字幕）
    pub fn external_subtitles(&self) -> bool {
        self.subtitle_mode.as_deref() == Some("external")
    }
    pub fn wants_subtitle(&self) -> bool {
        self.burn_subtitles() || self.embed_subtitles() || self.external_subtitles()
    }
    /// 选中的字幕轨是否为图形字幕（默认非图形）
    pub fn subtitle_graphic(&self) -> bool {
        self.subtitle_graphic.unwrap_or(false)
    }
    /// 目标分辨率是否要求重新编码（非 original 即需要缩放，不能直拷）
    pub fn resolution_requires_reencode(&self) -> bool {
        matches!(self.resolution.as_deref(), Some(r) if r != "original" && !r.is_empty())
    }
    /// 烧录字幕 / 改变分辨率都会改动画面，故视频必须重编码，不能直拷
    pub fn effective_video_mode(&self) -> &str {
        if self.video_copy() && (self.burn_subtitles() || self.resolution_requires_reencode()) {
            "h264"
        } else {
            self.video_mode()
        }
    }
}

/// 一条媒体流的关键信息（供「转码设置」对话框展示与选择）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamInfo {
    /// ffprobe 报告流的绝对索引
    pub index: usize,
    /// video / audio / subtitle
    pub kind: String,
    pub codec: String,
    pub profile: Option<String>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub is_default: bool,
    // 视频
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub pix_fmt: Option<String>,
    // 音频
    pub channels: Option<u32>,
    pub channel_layout: Option<String>,
    pub sample_rate: Option<u32>,
    /// 是否可直接直拷进 MP4（视频 / 音频轨用；字幕轨恒为 false）
    pub copyable: bool,
    /// 是否为文本字幕（可烧录 / 内嵌 / 外挂）
    pub text_subtitle: bool,
    /// 是否为图形字幕（只能烧录，不能内嵌或外挂）
    pub graphic_subtitle: bool,
    /// 外挂（同目录同名）字幕文件的绝对路径；仅外挂字幕条目有值，内嵌字幕轨为 None
    pub sidecar_path: Option<String>,
}

impl StreamInfo {
    /// 供对话框展示的单行描述，如 `H.265 Main10 1920×816 10bit`、`DTS-HD MA 5.1ch 48kHz`
    pub fn describe(&self) -> String {
        match self.kind.as_str() {
            "video" => {
                let mut parts = vec![self.codec.to_uppercase()];
                if let Some(p) = &self.profile {
                    parts.push(p.clone());
                }
                if let (Some(w), Some(h)) = (self.width, self.height) {
                    parts.push(format!("{}×{}", w, h));
                }
                if let Some(pf) = &self.pix_fmt {
                    parts.push(pf.clone());
                }
                parts.join(" ")
            }
            "audio" => {
                let mut parts = vec![self.codec.to_uppercase()];
                if let Some(p) = &self.profile {
                    parts.push(p.clone());
                }
                if let Some(c) = self.channels {
                    parts.push(format!("{}ch", c));
                }
                if let Some(sr) = self.sample_rate {
                    parts.push(format!("{}kHz", sr / 1000));
                }
                parts.join(" ")
            }
            _ => {
                let mut parts = vec![self.codec.to_uppercase()];
                if let Some(t) = &self.title {
                    parts.push(t.clone());
                }
                parts.join(" ")
            }
        }
    }
}

fn json_u32(v: Option<&serde_json::Value>) -> Option<u32> {
    v.and_then(|x| x.as_u64()).map(|n| n as u32)
}

fn json_str(v: Option<&serde_json::Value>) -> Option<String> {
    v.and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

/// 解析 `ffprobe -show_streams -print_format json` 输出中的流清单
pub fn parse_streams(json: &serde_json::Value) -> Vec<StreamInfo> {
    let Some(streams) = json.get("streams").and_then(|s| s.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for s in streams {
        let Some(kind) = json_str(s.get("codec_type")) else {
            continue;
        };
        if kind != "video" && kind != "audio" && kind != "subtitle" {
            continue; // 跳过 attachment / data 等
        }
        // mjpeg 内嵌封面不算视频轨
        if kind == "video" && s.get("disposition").and_then(|d| d.get("attached_pic")).and_then(|v| v.as_u64()) == Some(1) {
            continue;
        }
        let codec = json_str(s.get("codec_name")).unwrap_or_else(|| "unknown".into());
        let tags = s.get("tags");
        let language = tags.and_then(|t| json_str(t.get("language")));
        let title = tags.and_then(|t| json_str(t.get("title")));
        let is_default = s
            .get("disposition")
            .and_then(|d| d.get("default"))
            .and_then(|v| v.as_u64())
            == Some(1);
        let copyable = match kind.as_str() {
            "video" => COPYABLE_VIDEO.contains(&codec.as_str()),
            "audio" => COPYABLE_AUDIO.contains(&codec.as_str()),
            _ => false,
        };
        let text_subtitle = kind == "subtitle" && TEXT_SUBTITLE_CODECS.contains(&codec.as_str());
        let graphic_subtitle = kind == "subtitle" && GRAPHIC_SUBTITLE_CODECS.contains(&codec.as_str());
        out.push(StreamInfo {
            index: json_u32(s.get("index")).unwrap_or(0) as usize,
            kind,
            codec,
            profile: json_str(s.get("profile")),
            language,
            title,
            is_default,
            width: json_u32(s.get("width")),
            height: json_u32(s.get("height")),
            pix_fmt: json_str(s.get("pix_fmt")),
            channels: json_u32(s.get("channels")),
            channel_layout: json_str(s.get("channel_layout")),
            sample_rate: json_str(s.get("sample_rate")).and_then(|v| v.parse::<u32>().ok()),
            copyable,
            text_subtitle,
            graphic_subtitle,
            sidecar_path: None,
        });
    }
    out
}

/// 扫描视频同目录下的外挂字幕文件。
///
/// 匹配规则：扩展名为 `.srt` / `.ass` / `.ssa` / `.vtt`，且文件名（去扩展名后）与视频同名，
/// 或形如「影片名.语言标签」（如 `影片.zh-CN.srt`、`影片.eng.ass`，大小写不敏感）。
/// 返回的条目使用 [`SIDECAR_INDEX_BASE`] 起的合成索引，`sidecar_path` 记录字幕文件绝对路径，
/// 从而与内嵌字幕轨一样出现在「转码设置」对话框并可选来烧录。
pub fn parse_sidecar_subtitles(video: &Path) -> Vec<StreamInfo> {
    let Some(dir) = video.parent() else {
        return Vec::new();
    };
    let Some(stem) = video
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .filter(|s| !s.is_empty())
    else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    let mut out = Vec::new();
    for p in files {
        let Some(ext) = p.extension().map(|e| e.to_string_lossy().to_lowercase()) else {
            continue;
        };
        let Some((_, codec)) = SIDECAR_SUBTITLE_EXTS.iter().find(|(e, _)| *e == ext) else {
            continue;
        };
        let Some(sub_stem) = p.file_stem().map(|s| s.to_string_lossy().to_lowercase()) else {
            continue;
        };
        let suffix = sub_stem.strip_prefix(&format!("{}.", stem));
        // 同名，或「影片名.语言」形式
        if sub_stem != stem && suffix.is_none() {
            continue;
        }
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        out.push(StreamInfo {
            index: SIDECAR_INDEX_BASE + out.len(),
            kind: "subtitle".into(),
            codec: (*codec).to_string(),
            profile: None,
            language: suffix.filter(|s| !s.is_empty()).map(|s| s.to_string()),
            title: Some(name),
            is_default: false,
            width: None,
            height: None,
            pix_fmt: None,
            channels: None,
            channel_layout: None,
            sample_rate: None,
            copyable: false,
            text_subtitle: true,
            graphic_subtitle: false,
            sidecar_path: Some(p.to_string_lossy().to_string()),
        });
    }
    out
}

/// 选取默认字幕流：优先第一条文本字幕
pub fn first_text_subtitle(streams: &[StreamInfo]) -> Option<usize> {
    streams
        .iter()
        .find(|s| s.kind == "subtitle" && s.text_subtitle)
        .map(|s| s.index)
}

/// 选取默认音频流：优先标记 default 的，否则第一条
pub fn default_audio_stream(streams: &[StreamInfo]) -> Option<usize> {
    streams
        .iter()
        .find(|s| s.kind == "audio" && s.is_default)
        .or_else(|| streams.iter().find(|s| s.kind == "audio"))
        .map(|s| s.index)
}

/// 选取默认视频流：第一条
pub fn default_video_stream(streams: &[StreamInfo]) -> Option<usize> {
    streams.iter().find(|s| s.kind == "video").map(|s| s.index)
}

/// 烧录字幕时传给 `subtitles` 滤镜的取值：**只取文件名**（不含目录）。
///
/// ffmpeg 的滤镜参数先把整段值剥离外层引号，再按 `:` 切分成一个个选项，
/// 因此直接传 `C:/.../sub.srt` 时会被解析成 filename=`C` + 选项 `original_size=/...`
/// 而报错（实测退出码 -22）；文件路径中的空格、逗号、括号、单引号也有各自的转义坑，
/// 极难穷举。改为「裸文件名 + 把 ffmpeg 工作目录设为字幕所在目录」（见
/// [`subtitle_filter_cwd`]），可一次性绕开全部转义问题：
/// 文件名由本程序生成（形如 `sub-<pid>-<idx>.srt`），只含字母数字与 `.`/`-`。
pub fn subtitle_filter_value(sub: &Path) -> String {
    sub.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| sub.to_string_lossy().replace('\\', "/"))
}

/// 烧录字幕时 ffmpeg 进程应设置的工作目录（字幕文件所在目录），配合 [`subtitle_filter_value`] 使用
pub fn subtitle_filter_cwd(sub: &Path) -> Option<std::path::PathBuf> {
    sub.parent().map(|p| p.to_path_buf())
}

/// 把 preset 参数转成 libsvtav1 的整数档位（-2..13，越大越快）。
/// 既接受整数，也接受 x264/x265 风格的名称（近似映射到最接近的档位）。
fn svtav1_preset(preset: &str) -> Option<i32> {
    if let Ok(n) = preset.parse::<i32>() {
        return Some(n.clamp(-2, 13));
    }
    Some(match preset {
        "ultrafast" => 13,
        "superfast" => 12,
        "veryfast" => 11,
        "faster" => 10,
        "fast" => 9,
        "medium" => 8,
        "slow" => 6,
        "slower" => 4,
        "veryslow" => 2,
        _ => return None,
    })
}

/// 构建转码参数。
///
/// `subtitle_file` 为烧录模式时预先抽出的字幕文件路径；`None` 表示不烧录。
/// **注意**：烧录模式下本函数只把字幕「文件名」写进 `subtitles` 滤镜，调用方必须同时
/// 以 [`subtitle_filter_cwd`] 的返回值作为 ffmpeg 进程的工作目录，否则滤镜会找不到字幕文件。
pub fn build_args(
    input: &str,
    dest: &Path,
    o: &TranscodeOptions,
    subtitle_file: Option<&Path>,
    start_sec: Option<f64>,
    dur_sec: Option<f64>,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["-y".into()];
    if let Some(s) = start_sec {
        args.extend(["-ss".into(), format!("{:.3}", s)]);
    }
    args.extend(["-i".into(), input.to_string()]);
    if o.silent_audio {
        args.extend(["-f".into(), "lavfi".into(), "-i".into(), "anullsrc=r=48000:cl=stereo".into()]);
    }

    // ===== 视频轨 =====
    // 图形（位图）字幕烧录需要用 filter_complex 把字幕画面 overlay 到视频上，
    // 此时视频走滤镜输出 [v]，不能再单独 -map 原视频流。
    let scale_filter: Option<String> = match o.resolution.as_deref() {
        Some("fit1080") => Some("scale=w='min(iw,1920)':h='min(ih,1080)':force_original_aspect_ratio=decrease:force_divisible_by=2".into()),
        Some("fit720") => Some("scale=w='min(iw,1280)':h='min(ih,720)':force_original_aspect_ratio=decrease:force_divisible_by=2".into()),
        Some("fit480") => Some("scale=w='min(iw,854)':h='min(ih,480)':force_original_aspect_ratio=decrease:force_divisible_by=2".into()),
        Some(res) if res != "original" && !res.is_empty() => {
            // 按高度缩放，宽度取偶数保持宽高比
            Some(format!("scale=-2:{}", res))
        }
        _ => None,
    };
    let graphic_burn =
        o.burn_subtitles() && o.subtitle_graphic() && o.subtitle_stream.is_some();
    if graphic_burn {
        let idx = o.subtitle_stream.unwrap();
        // 先 overlay（两者尺寸一致）再缩放，避免字幕位图与缩放后画面错位
        let fc = match &scale_filter {
            Some(sc) => format!("[0:v][0:{}]overlay[ov];[ov]{}[v]", idx, sc),
            None => format!("[0:v][0:{}]overlay[v]", idx),
        };
        args.extend(["-filter_complex".into(), fc]);
        args.extend(["-map".into(), "[v]".into()]);
    } else {
        match o.video_stream {
            Some(i) => args.extend(["-map".into(), format!("0:{}", i)]),
            None => args.extend(["-map".into(), "0:v:0?".into()]),
        }
    }

    // ===== 音频轨 =====
    if o.include_audio() {
        if o.silent_audio {
            args.extend(["-map".into(), "1:a:0".into()]);
        } else {
            match o.audio_stream {
                Some(i) => args.extend(["-map".into(), format!("0:{}", i)]),
                None => args.extend(["-map".into(), "0:a:0?".into()]),
            }
        }
    }

    // ===== 字幕轨（内嵌模式；烧录 / 外挂模式不映射字幕流）=====
    if o.embed_subtitles() {
        if let Some(i) = o.subtitle_stream {
            args.extend(["-map".into(), format!("0:{}", i)]);
        }
    }

    // ===== 视频编码 =====
    let vmode = o.effective_video_mode();
    let reencode = vmode != "copy";
    match vmode {
        "h265" | "hevc" => {
            args.extend(["-c:v".into(), "libx265".into()]);
        }
        "av1" => {
            // libaom-av1 速度极慢，改用 SVT-AV1（同画质下快 1~2 个数量级）
            args.extend(["-c:v".into(), "libsvtav1".into()]);
        }
        "copy" => {
            args.extend(["-c:v".into(), "copy".into()]);
        }
        _ => {
            args.extend(["-c:v".into(), "libx264".into()]);
        }
    }
    if reencode {
        // -keyint_min / -sc_threshold 是 x264 / x265 私有选项，AV1 编码器不支持
        let x26x = matches!(vmode, "h264" | "h265" | "hevc");
        if x26x {
            let preset = o.preset.as_deref().unwrap_or("medium");
            args.extend(["-preset".into(), preset.to_string()]);
        } else if vmode == "av1" {
            // libsvtav1 的 -preset 是整数档位（-2..13，越大越快），默认取偏快的档位
            let preset = o.preset.as_deref().and_then(svtav1_preset).unwrap_or(8);
            args.extend(["-preset".into(), preset.to_string()]);
            if o.streaming_profile == Some(StreamingProfile::Av1) {
                args.extend(["-svtav1-params".into(), "tune=0:mbr-overshoot-pct=10".into()]);
            }
        }
        let crf = o.crf.unwrap_or(20).min(51);
        args.extend(["-crf".into(), crf.to_string()]);
        let pix = o.pix_fmt.as_deref().unwrap_or(if vmode == "h265" || vmode == "hevc" {
            "yuv420p10le"
        } else {
            "yuv420p"
        });
        if pix != "auto" {
            args.extend(["-pix_fmt".into(), pix.to_string()]);
        }
        if let Some(maxrate) = &o.max_video_bitrate {
            args.extend(["-maxrate".into(), maxrate.clone()]);
        }
        if let Some(bufsize) = &o.video_buffer_size {
            args.extend(["-bufsize".into(), bufsize.clone()]);
        }
        if let Some(gop) = o.keyframe_interval.filter(|g| *g > 0) {
            args.extend(["-g".into(), gop.to_string()]);
            if x26x {
                if o.streaming_profile == Some(StreamingProfile::Compatible) {
                    // 两秒为最大间隔，场景切换可提前插入关键帧。
                    args.extend(["-keyint_min".into(), "1".into(), "-sc_threshold".into(), "40".into()]);
                } else {
                    args.extend(["-keyint_min".into(), gop.to_string(), "-sc_threshold".into(), "0".into()]);
                }
            }
        }
        if o.streaming_profile == Some(StreamingProfile::Compatible) {
            args.extend(["-profile:v".into(), "main".into(), "-level:v".into(), "4.1".into(), "-flags".into(), "+cgop".into()]);
            // slow 默认的 5 个参考帧超出 1080P Level 4.1 DPB；限制为 4。
            args.extend(["-refs".into(), "4".into(), "-aq-mode".into(), "3".into(), "-aq-strength".into(), "1.0".into()]);
        }
        if let Some(fps) = &o.frame_rate {
            args.extend(["-r".into(), fps.clone(), "-fps_mode".into(), "cfr".into()]);
        }
    }

    // ===== 视频滤镜：缩放 + 文本字幕烧录（图形字幕已在上面的 filter_complex 中处理）=====
    if !graphic_burn {
        let mut vf: Vec<String> = Vec::new();
        if o.tone_map {
            let pix = if o.pix_fmt.as_deref() == Some("yuv420p10le") { "yuv420p10le" } else { "yuv420p" };
            vf.push(format!("zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format={},sidedata=mode=delete", pix));
        }
        if let Some(sc) = &scale_filter {
            vf.push(sc.clone());
        }
        if o.burn_subtitles() {
            if let Some(p) = subtitle_file {
                vf.push(format!("subtitles={}", subtitle_filter_value(p)));
            }
        }
        if !vf.is_empty() {
            args.extend(["-vf".into(), vf.join(",")]);
        }
    }
    if o.tone_map {
        args.extend(["-color_primaries".into(), "bt709".into(), "-color_trc".into(), "bt709".into(), "-colorspace".into(), "bt709".into(), "-color_range".into(), "tv".into()]);
    }

    // ===== 音频编码 =====
    if o.include_audio() {
        if o.audio_copy() {
            args.extend(["-c:a".into(), "copy".into()]);
        } else {
            args.extend(["-c:a".into(), "aac".into()]);
            let br = o.audio_bitrate.as_deref().unwrap_or("192k");
            args.extend(["-b:a".into(), br.to_string()]);
            if o.streaming_profile.is_some() {
                args.extend(["-profile:a".into(), "aac_low".into()]);
            }
        }
        // 直拷时不能再改声道 / 采样率
        if !o.audio_copy() {
            if let Some(ch) = o.audio_channels.filter(|c| *c > 0) {
                args.extend(["-ac".into(), ch.to_string()]);
            }
            if let Some(sr) = o.audio_sample_rate.filter(|s| *s > 0) {
                args.extend(["-ar".into(), sr.to_string()]);
            }
        }
    } else {
        args.push("-an".into());
    }

    // ===== 字幕编码（内嵌模式）=====
    if o.embed_subtitles() {
        args.extend(["-c:s".into(), "mov_text".into()]);
    } else if !o.burn_subtitles() {
        args.push("-sn".into());
    }

    if o.faststart() {
        args.extend(["-movflags".into(), "+faststart".into()]);
    }
    if o.silent_audio {
        args.push("-shortest".into());
    }
    args.extend(["-progress".into(), "pipe:1".into(), "-nostats".into()]);
    if let Some(d) = dur_sec {
        args.extend(["-t".into(), format!("{:.3}", d)]);
    }
    args.push(dest.to_string_lossy().to_string());
    args
}

/// 抽出字幕流到临时文件时，按字幕编码选择容器扩展名（文本字幕统一用 srt / ass）
pub fn subtitle_ext(codec: &str) -> &'static str {
    match codec {
        "ass" | "ssa" => "ass",
        _ => "srt",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn base() -> TranscodeOptions {
        TranscodeOptions {
            video_mode: Some("h264".into()),
            audio_mode: Some("aac".into()),
            ..Default::default()
        }
    }

    fn streaming_source() -> serde_json::Value {
        serde_json::json!({"streams": [
            {"index":0,"codec_type":"video","codec_name":"hevc","width":3840,"height":2160,"avg_frame_rate":"60000/1001","pix_fmt":"yuv420p10le"},
            {"index":1,"codec_type":"audio","codec_name":"ac3","channels":6,"sample_rate":"44100"}
        ]})
    }

    #[test]
    fn compatible_streaming_preset_is_bounded_and_uses_common_codecs() {
        let o = streaming_options(StreamingProfile::Compatible, &streaming_source(), &Default::default()).unwrap();
        let args = build_args("in.mkv", Path::new("out.mp4"), &o, None, None, None).join(" ");
        for expected in ["-c:v libx264", "-profile:v main", "-level:v 4.1", "-crf 22", "-preset slow", "-pix_fmt yuv420p", "-r 30", "-c:a aac", "-profile:a aac_low", "-ac 2", "-ar 48000", "-maxrate 8000k", "-g 60", "-keyint_min 1", "-sc_threshold 40", "-refs 4", "-aq-mode 3", "+faststart", "force_divisible_by=2"] {
            assert!(args.contains(expected), "missing {}: {}", expected, args);
        }
        assert_eq!(output_label(&o), "兼容 H264");
        assert!(!o.silent_audio && !o.tone_map);
    }

    #[test]
    fn streaming_preserves_low_frame_rate_and_has_separate_av1_settings() {
        let mut source = streaming_source();
        source["streams"][0]["avg_frame_rate"] = "0/0".into();
        source["streams"][0]["r_frame_rate"] = "24000/1001".into();
        source["streams"][0]["height"] = 720.into();
        let compatible = streaming_options(StreamingProfile::Compatible, &source, &Default::default()).unwrap();
        assert_eq!(compatible.frame_rate.as_deref(), Some("24000/1001"));
        assert_eq!(compatible.keyframe_interval, Some(48));
        assert_eq!(compatible.max_video_bitrate.as_deref(), Some("4000k"));
        for preset in [6, 8, 10, 99] {
            let mut settings = crate::settings::Settings::default();
            settings.streaming_av1_preset = preset;
            let av1 = streaming_options(StreamingProfile::Av1, &source, &settings).unwrap();
            assert_eq!(av1.preset, Some(if preset == 99 { "8".into() } else { preset.to_string() }));
            assert_eq!(av1.max_video_bitrate.as_deref(), Some("3000k"));
            assert!(av1.frame_rate.is_none());
            let args = build_args("in.mkv", Path::new("out.mp4"), &av1, None, None, None).join(" ");
            assert!(args.contains("-c:v libsvtav1"));
            assert!(args.contains("-svtav1-params tune=0:mbr-overshoot-pct=10"));
            assert!(!args.contains("-bufsize"));
            assert!(!args.contains("-keyint_min") && !args.contains("-sc_threshold"));
        }
    }

    #[test]
    fn low_bitrate_resolution_policy_keeps_budget_and_can_be_disabled() {
        let source = streaming_source();
        for (profile, budget, resolution) in [
            (StreamingProfile::Compatible, 1.0, "fit480"),
            (StreamingProfile::Compatible, 1.5, "fit480"),
            (StreamingProfile::Compatible, 2.0, "fit720"),
            (StreamingProfile::Compatible, 3.0, "fit720"),
            (StreamingProfile::Compatible, 3.01, "fit1080"),
            (StreamingProfile::Av1, 1.5, "fit720"),
            (StreamingProfile::Av1, 2.0, "fit720"),
            (StreamingProfile::Av1, 3.0, "fit1080"),
        ] {
            let mut settings = crate::settings::Settings::default();
            settings.streaming_auto_resolution = true;
            settings.compatible_1080p_bitrate_mbps = budget;
            settings.streaming_1080p_bitrate_mbps = budget;
            // 降分辨率之后不可又改用 720P 档的另一个码率预算。
            settings.compatible_720p_bitrate_mbps = 40.0;
            settings.streaming_720p_bitrate_mbps = 40.0;
            let options = streaming_options(profile, &source, &settings).unwrap();
            assert_eq!(options.resolution.as_deref(), Some(resolution));
            assert_eq!(options.max_video_bitrate, Some(format!("{}k", (budget * 1000.0).round() as u32)));
            assert_eq!(options.audio_bitrate.as_deref(), Some(if budget <= 3.0 { "128k" } else { "160k" }));
            let args = build_args("in.mkv", Path::new("out.mp4"), &options, None, None, None).join(" ");
            assert!(args.contains("force_divisible_by=2"));
            settings.streaming_auto_resolution = false;
            let preserved = streaming_options(profile, &source, &settings).unwrap();
            assert_eq!(preserved.resolution.as_deref(), Some("fit1080"));
            if profile == StreamingProfile::Av1 {
                assert!(preserved.frame_rate.is_none());
            } else {
                assert_eq!(preserved.frame_rate.as_deref(), Some("30"));
            }
        }
    }

    #[test]
    fn streaming_budgets_follow_fitted_pixels_and_invalid_settings_are_safe() {
        let mut source = streaming_source();
        source["streams"][0]["width"] = 720.into();
        source["streams"][0]["height"] = 1280.into();
        let mut settings = crate::settings::Settings::default();
        let portrait = streaming_options(StreamingProfile::Compatible, &source, &settings).unwrap();
        assert_eq!(portrait.max_video_bitrate.as_deref(), Some("4000k"));
        settings.compatible_720p_bitrate_mbps = f64::NAN;
        settings.compatible_preset = "veryslow".into();
        let invalid = streaming_options(StreamingProfile::Compatible, &source, &settings).unwrap();
        assert_eq!(invalid.max_video_bitrate.as_deref(), Some("4000k"));
        assert_eq!(invalid.preset.as_deref(), Some("slow"));
        settings.compatible_preset = "medium".into();
        assert_eq!(streaming_options(StreamingProfile::Compatible, &source, &settings).unwrap().preset.as_deref(), Some("medium"));
    }

    #[test]
    fn streaming_hdr_is_tonemapped_and_unknown_hdr_is_rejected() {
        let mut source = streaming_source();
        source["streams"][0]["color_primaries"] = "bt2020".into();
        for transfer in ["smpte2084", "arib-std-b67"] {
            source["streams"][0]["color_transfer"] = transfer.into();
            let o = streaming_options(StreamingProfile::Compatible, &source, &Default::default()).unwrap();
            assert!(o.tone_map);
            let args = build_args("in.mkv", Path::new("out.mp4"), &o, None, None, None).join(" ");
            assert!(args.contains("tonemap=") && args.contains("-color_trc bt709"));
        }
        source["streams"][0]["color_transfer"] = "unknown".into();
        assert!(streaming_options(StreamingProfile::Compatible, &source, &Default::default()).is_err());
        source["streams"][0]["color_transfer"] = "smpte2084".into();
        source["streams"][0]["side_data_list"] = serde_json::json!([{"side_data_type":"DOVI configuration record"}]);
        assert!(streaming_options(StreamingProfile::Compatible, &source, &Default::default()).is_err());
    }

    #[test]
    fn streaming_handles_silent_sources_and_level_limits() {
        let mut source = streaming_source();
        source["streams"].as_array_mut().unwrap().pop();
        let mut settings = crate::settings::Settings::default();
        settings.compatible_1080p_bitrate_mbps = 100.0;
        let o = streaming_options(StreamingProfile::Compatible, &source, &settings).unwrap();
        assert!(o.silent_audio);
        assert_eq!(o.max_video_bitrate.as_deref(), Some("50000k"));
        assert_eq!(o.video_buffer_size.as_deref(), Some("62500k"));
        let args = build_args("in.mkv", Path::new("out.mp4"), &o, None, None, None).join(" ");
        assert!(args.contains("anullsrc=") && args.contains("-map 1:a:0") && args.contains("-shortest"));
        let old: TranscodeOptions = serde_json::from_str("{}").unwrap();
        assert!(old.streaming_profile.is_none() && !old.silent_audio && !old.tone_map);
        assert_eq!(output_label(&old), "转码");
        assert!(streaming_options(StreamingProfile::Compatible, &serde_json::json!({"streams":[]}), &settings).is_err());
    }

    #[test]
    fn burn_forces_video_reencode() {
        let mut o = base();
        o.video_mode = Some("copy".into());
        o.subtitle_mode = Some("burn".into());
        assert_eq!(o.effective_video_mode(), "h264");
        assert!(o.burn_subtitles());
    }

    #[test]
    fn streaming_options_set_vbv_and_closed_gop() {
        let mut o = base();
        o.max_video_bitrate = Some("6000k".into());
        o.video_buffer_size = Some("12000k".into());
        o.keyframe_interval = Some(120);
        let args = build_args("in.mkv", &PathBuf::from("out.mp4"), &o, None, None, None);
        let joined = args.join(" ");
        assert!(joined.contains("-maxrate 6000k"));
        assert!(joined.contains("-bufsize 12000k"));
        assert!(joined.contains("-g 120 -keyint_min 120 -sc_threshold 0"));
    }

    #[test]
    fn args_for_audio_only_copy() {
        let mut o = TranscodeOptions::default();
        o.video_mode = Some("copy".into());
        o.audio_mode = Some("aac".into());
        o.audio_bitrate = Some("256k".into());
        let dest = PathBuf::from("out.mp4");
        let args = build_args("in.mkv", &dest, &o, None, None, None);
        let joined = args.join(" ");
        assert!(joined.contains("-c:v copy"));
        assert!(joined.contains("-c:a aac"));
        assert!(joined.contains("-b:a 256k"));
        assert!(joined.contains("-movflags +faststart"));
        assert!(args.last().unwrap() == "out.mp4");
    }

    #[test]
    fn args_burn_subtitle_uses_vf_and_reencodes() {
        let mut o = base();
        o.subtitle_mode = Some("burn".into());
        o.video_mode = Some("copy".into());
        let dest = PathBuf::from("out.mp4");
        let sub = PathBuf::from(r"C:\Temp\sub.srt");
        let args = build_args("in.mkv", &dest, &o, Some(&sub), None, None);
        let joined = args.join(" ");
        assert!(joined.contains("-c:v libx264"), "burn 必须重编码: {}", joined);
        assert!(joined.contains("subtitles=sub.srt"), "只应传字幕文件名: {}", joined);
        assert!(!joined.contains("-c:s"), "烧录模式不内嵌字幕轨: {}", joined);
    }

    #[test]
    fn args_embed_subtitle_and_drop_when_none() {
        let mut o = base();
        o.subtitle_mode = Some("embed".into());
        o.subtitle_stream = Some(3);
        let dest = PathBuf::from("out.mp4");
        let args = build_args("in.mkv", &dest, &o, None, None, None);
        let joined = args.join(" ");
        assert!(joined.contains("-map 0:3"));
        assert!(joined.contains("-c:s mov_text"));

        let o2 = base();
        let args2 = build_args("in.mkv", &dest, &o2, None, None, None);
        assert!(args2.join(" ").contains("-sn"));
    }

    #[test]
    fn subtitle_filter_uses_basename_and_parent_cwd() {
        let p = PathBuf::from(r"C:\Temp\sub-1.srt");
        assert_eq!(subtitle_filter_value(&p), "sub-1.srt");
        assert_eq!(subtitle_filter_cwd(&p), Some(PathBuf::from(r"C:\Temp")));
    }

    #[test]
    fn parse_streams_detects_types_and_copyability() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"streams":[
              {"index":0,"codec_type":"video","codec_name":"hevc","profile":"Main 10","width":1920,"height":816,"pix_fmt":"yuv420p10le","disposition":{"default":1}},
              {"index":1,"codec_type":"audio","codec_name":"dts","profile":"DTS-HD MA","channels":6,"sample_rate":"48000","tags":{"language":"eng"},"disposition":{"default":1}},
              {"index":2,"codec_type":"subtitle","codec_name":"subrip","tags":{"language":"chi"},"disposition":{"default":0}},
              {"index":3,"codec_type":"subtitle","codec_name":"hdmv_pgs_subtitle","disposition":{"default":0}},
              {"index":4,"codec_type":"video","codec_name":"mjpeg","disposition":{"attached_pic":1}}
            ]}"#,
        )
        .unwrap();
        let s = parse_streams(&json);
        assert_eq!(s.len(), 4, "封面 mjpeg 应被剔除");
        assert_eq!(s[0].kind, "video");
        assert!(s[0].copyable, "hevc 可直拷进 MP4");
        assert!(!s[1].copyable, "DTS 不能直拷进 MP4");
        assert!(s[2].text_subtitle, "subrip 为文本字幕");
        assert!(!s[3].text_subtitle, "PGS 为图形字幕");
        assert_eq!(s[1].describe(), "DTS DTS-HD MA 6ch 48kHz");
        assert_eq!(first_text_subtitle(&s), Some(2));
        assert_eq!(default_audio_stream(&s), Some(1));
    }

    #[test]
    fn sidecar_subtitles_matched_by_name_and_language_suffix() {
        let dir = std::env::temp_dir().join(format!("vlc-sidecar-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("Movie (2020).mkv");
        std::fs::write(&video, b"x").unwrap();
        std::fs::write(dir.join("Movie (2020).zh-CN.srt"), b"1").unwrap();
        std::fs::write(dir.join("Movie (2020).ass"), b"2").unwrap();
        std::fs::write(dir.join("Movie (2020).txt"), b"3").unwrap(); // 非字幕扩展名
        std::fs::write(dir.join("Other.srt"), b"4").unwrap(); // 不同名
        std::fs::write(dir.join("Movie (2020).sub"), b"5").unwrap(); // VobSub 不支持

        let subs = parse_sidecar_subtitles(&video);
        let names: Vec<String> = subs
            .iter()
            .filter_map(|s| s.sidecar_path.as_deref())
            .map(|p| std::path::Path::new(p).file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(subs.len(), 2, "只应识别同名的 srt/ass：{:?}", names);
        assert!(names.contains(&"Movie (2020).ass".to_string()));
        assert!(names.contains(&"Movie (2020).zh-CN.srt".to_string()));
        assert!(subs.iter().all(|s| s.kind == "subtitle" && s.text_subtitle));
        assert!(subs.iter().all(|s| s.index >= SIDECAR_INDEX_BASE));
        assert!(subs
            .iter()
            .any(|s| s.language.as_deref() == Some("zh-cn")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
