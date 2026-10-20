//! 输出生产代码生成的 FFmpeg 参数，供低码率对照测试使用。
//! cargo run --example codec_plan -- input output av1|compatible probe.json [settings.json]

use app_lib::transcode::{build_args, streaming_options, StreamingProfile};
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !(4..=5).contains(&args.len()) {
        return Err("需要 input output av1|compatible probe.json [settings.json]".into());
    }
    let profile = match args[2].as_str() {
        "av1" => StreamingProfile::Av1,
        "compatible" => StreamingProfile::Compatible,
        _ => return Err("profile 必须为 av1 或 compatible".into()),
    };
    let probe = serde_json::from_slice(&std::fs::read(&args[3])?)?;
    let settings = if let Some(path) = args.get(4) {
        serde_json::from_slice(&std::fs::read(path)?)?
    } else {
        Default::default()
    };
    let options = streaming_options(profile, &probe, &settings)?;
    let ffmpeg_args = build_args(&args[0], Path::new(&args[1]), &options, None, None, None);
    println!("{}", serde_json::to_string_pretty(&ffmpeg_args)?);
    Ok(())
}
