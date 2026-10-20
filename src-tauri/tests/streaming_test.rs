//! 可选的真实 FFmpeg 回归测试：只生成临时测试素材，不使用用户视频。
//! cargo test --test streaming_test -- --ignored

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use app_lib::transcode::{
    build_args, check_streaming_capabilities, streaming_options, StreamingProfile,
};
use serde_json::Value;

async fn run(binary: &str, args: &[String]) -> Vec<u8> {
    let mut cmd = tokio::process::Command::new(binary);
    cmd.args(args).stdin(Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x08000000);
    let output = tokio::time::timeout(Duration::from_secs(60), cmd.output())
        .await
        .expect("FFmpeg test timed out")
        .expect("FFmpeg / FFprobe must be available");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

async fn probe(ffprobe: &str, path: &Path) -> Value {
    serde_json::from_slice(
        &run(
            ffprobe,
            &[
                "-v".into(),
                "error".into(),
                "-show_streams".into(),
                "-show_format".into(),
                "-of".into(),
                "json".into(),
                path.to_string_lossy().into_owned(),
            ],
        )
        .await,
    )
    .unwrap()
}

fn atom_offset(data: &[u8], name: &[u8; 4]) -> usize {
    let mut offset = 0;
    while offset + 8 <= data.len() {
        let size = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        if &data[offset + 4..offset + 8] == name {
            return offset;
        }
        assert!(size >= 8 && offset + size <= data.len(), "invalid MP4 atom");
        offset += size;
    }
    panic!("missing MP4 atom");
}

#[tokio::test]
#[ignore = "requires FFmpeg and FFprobe; generates and encodes synthetic video"]
async fn real_streaming_profiles() {
    let ffmpeg = std::env::var("FFMPEG_TEST").unwrap_or_else(|_| "ffmpeg".into());
    let ffprobe = std::env::var("FFPROBE_TEST").unwrap_or_else(|_| "ffprobe".into());
    let dir = tempfile::tempdir().unwrap();
    let settings = app_lib::settings::Settings::default();
    for (name, size, fps, audio, transfer) in [
        ("odd", "321x241", "24000/1001", true, None),
        ("wide", "2200x360", "60", true, None),
        ("silent", "320x240", "25", false, None),
        ("hdr10", "320x240", "24", true, Some("smpte2084")),
        ("hlg", "320x240", "24", true, Some("arib-std-b67")),
        ("lowrate", "1920x1080", "60", true, None),
    ] {
        let mut settings = settings.clone();
        if name == "lowrate" {
            settings.streaming_auto_resolution = true;
            settings.compatible_1080p_bitrate_mbps = 1.5;
            settings.streaming_1080p_bitrate_mbps = 1.5;
        }
        let source = dir.path().join(format!("{}.mkv", name));
        let dest = dir.path().join(format!("{}-compatible.mp4", name));
        let mut args = vec![
            "-y".into(),
            "-v".into(),
            "error".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            format!("testsrc=size={}:rate={}:duration=1", size, fps),
        ];
        if audio {
            args.extend([
                "-f".into(),
                "lavfi".into(),
                "-i".into(),
                "sine=sample_rate=44100:duration=1".into(),
            ]);
        }
        args.extend([
            "-c:v".into(),
            if transfer.is_some() {
                "libx265"
            } else {
                "ffv1"
            }
            .into(),
            "-pix_fmt".into(),
            if transfer.is_some() {
                "yuv420p10le"
            } else {
                "yuv444p"
            }
            .into(),
        ]);
        if let Some(transfer) = transfer {
            args.extend([
                "-vf".into(),
                format!("format=yuv420p10le,setparams=color_primaries=bt2020:color_trc={}:colorspace=bt2020nc", transfer),
                "-preset".into(), "ultrafast".into(), "-x265-params".into(), "log-level=error".into(),
                "-color_primaries".into(),
                "bt2020".into(),
                "-color_trc".into(),
                transfer.into(),
                "-colorspace".into(),
                "bt2020nc".into(),
            ]);
        }
        if audio {
            args.extend(["-c:a".into(), "ac3".into(), "-ac".into(), "6".into()]);
        }
        args.push(source.to_string_lossy().into_owned());
        run(&ffmpeg, &args).await;
        let json = probe(&ffprobe, &source).await;
        if let Some(transfer) = transfer {
            assert_eq!(
                json["streams"][0]["color_transfer"], transfer,
                "HDR fixture must preserve color metadata"
            );
        }
        let options = streaming_options(StreamingProfile::Compatible, &json, &settings).unwrap();
        check_streaming_capabilities(Path::new(&ffmpeg), &options)
            .await
            .unwrap();
        run(
            &ffmpeg,
            &build_args(source.to_str().unwrap(), &dest, &options, None, None, None),
        )
        .await;
        let result = probe(&ffprobe, &dest).await;
        let streams = result["streams"].as_array().unwrap();
        assert_eq!(streams.len(), 2);
        let video = streams.iter().find(|s| s["codec_type"] == "video").unwrap();
        let sound = streams.iter().find(|s| s["codec_type"] == "audio").unwrap();
        assert_eq!(video["codec_name"], "h264");
        assert_eq!(video["profile"], "Main");
        assert_eq!(video["level"], 41);
        assert_eq!(video["pix_fmt"], "yuv420p");
        let width = video["width"].as_u64().unwrap();
        let height = video["height"].as_u64().unwrap();
        assert!(width <= 1920 && height <= 1080 && width % 2 == 0 && height % 2 == 0);
        assert!(width <= json["streams"][0]["width"].as_u64().unwrap());
        assert!(height <= json["streams"][0]["height"].as_u64().unwrap());
        if name == "lowrate" {
            assert_eq!((width, height), (854, 480));
        }
        let (n, d) = video["avg_frame_rate"]
            .as_str()
            .unwrap()
            .split_once('/')
            .unwrap();
        assert!(n.parse::<f64>().unwrap() / d.parse::<f64>().unwrap() <= 30.0);
        assert_eq!(sound["codec_name"], "aac");
        assert_eq!(sound["profile"], "LC");
        assert_eq!(sound["sample_rate"], "48000");
        assert_eq!(sound["channels"], 2);
        if transfer.is_some() {
            assert_eq!(video["color_transfer"], "bt709");
            assert_eq!(video["color_primaries"], "bt709");
        }
        let duration: f64 = result["format"]["duration"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            (0.8..1.3).contains(&duration),
            "{} duration={}",
            name,
            duration
        );
        let data = std::fs::read(&dest).unwrap();
        assert!(atom_offset(&data, b"moov") < atom_offset(&data, b"mdat"));
        run(&ffmpeg, &["-v".into(), "error".into(), "-i".into(), dest.to_string_lossy().into_owned(), "-f".into(), "null".into(), "-".into()]).await;
        let compatibility = app_lib::compat::evaluate(&app_lib::compat::parse_probe(&result));
        assert!(compatibility.supported, "{:?}", compatibility.reason);

        {
            let av1 = streaming_options(StreamingProfile::Av1, &json, &settings).unwrap();
            match check_streaming_capabilities(Path::new(&ffmpeg), &av1).await {
                Ok(()) => {
                    let av1_dest = dir.path().join(format!("{}-av1.mp4", name));
                    run(
                        &ffmpeg,
                        &build_args(source.to_str().unwrap(), &av1_dest, &av1, None, None, None),
                    )
                    .await;
                    let result = probe(&ffprobe, &av1_dest).await;
                    let video = result["streams"].as_array().unwrap().iter().find(|s| s["codec_type"] == "video").unwrap();
                    assert_eq!(video["codec_name"], "av1");
                    assert_eq!(video["pix_fmt"], "yuv420p");
                    if transfer.is_some() {
                        assert_eq!(video["color_transfer"], "bt709");
                        assert_eq!(video["color_primaries"], "bt709");
                    }
                    if name == "lowrate" {
                        assert_eq!(video["width"], 1280);
                        assert_eq!(video["height"], 720);
                        assert_eq!(video["avg_frame_rate"], "30/1");
                    }
                    run(&ffmpeg, &["-v".into(), "error".into(), "-i".into(), av1_dest.to_string_lossy().into_owned(), "-f".into(), "null".into(), "-".into()]).await;
                }
                Err(error) => assert!(error.starts_with("FFmpeg 缺少 libsvtav1"), "{}", error),
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires FFmpeg; checks real encoder parameter rejection"]
async fn rejects_unsupported_encoder_parameters() {
    let ffmpeg = std::env::var("FFMPEG_TEST").unwrap_or_else(|_| "ffmpeg".into());
    let invalid = app_lib::transcode::TranscodeOptions {
        video_mode: Some("h264".into()),
        preset: Some("not-a-real-x264-preset".into()),
        streaming_profile: Some(StreamingProfile::Compatible),
        ..Default::default()
    };
    let error = check_streaming_capabilities(Path::new(&ffmpeg), &invalid).await.unwrap_err();
    assert!(error.starts_with("FFmpeg 无法使用当前压制参数"), "{}", error);
}
