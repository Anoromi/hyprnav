//! Building — and probing — the ffmpeg command line behind one pipeline.
//!
//! Tier T1 of FRAMES-VIDEO-PLAN.md §2: one ffmpeg child per pipeline, fed raw
//! pixels on stdin by the capture helper and writing a container on stdout.
//! The flags are not decoration: E2 measured `-async_depth 1` cutting encoder
//! latency from 136 ms to 2 ms, `-bf 0` alone doing nothing, and `-qp` being
//! ignored by the VAAPI encoders in favour of `-q:v`.

use crate::frames_config::{EncoderBackend, FramesConfig};
use crate::video::Codec;
use std::process::{Command, Stdio};

/// How a codec is going to be encoded on this machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncoderChoice {
    pub codec: Codec,
    pub vaapi: bool,
}

/// The container ffmpeg is told to write, and how we demux it again.
pub fn container(codec: Codec) -> &'static str {
    match codec {
        Codec::Av1 => "ivf",
        Codec::H264 => "h264", // Annex-B; WebCodecs needs it without a description
        Codec::Mjpeg => "mjpeg",
    }
}

/// The full argument list, ffmpeg's own name excluded.
pub fn ffmpeg_args(
    config: &FramesConfig,
    choice: EncoderChoice,
    pix_fmt: &str,
    width: u32,
    height: u32,
    fps: u32,
) -> Vec<String> {
    let settings = config.settings(choice.codec);
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-fflags".into(),
        "+nobuffer".into(),
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        pix_fmt.into(),
        "-s".into(),
        format!("{width}x{height}"),
        "-r".into(),
        fps.max(1).to_string(),
        "-i".into(),
        "pipe:0".into(),
    ];
    if choice.vaapi {
        args.extend([
            "-vaapi_device".into(),
            config.vaapi_device.clone(),
            "-vf".into(),
            "format=nv12,hwupload".into(),
            "-c:v".into(),
            match choice.codec {
                Codec::Av1 => "av1_vaapi".into(),
                Codec::H264 => "h264_vaapi".into(),
                Codec::Mjpeg => "mjpeg_vaapi".into(),
            },
            "-async_depth".into(),
            "1".into(),
        ]);
        if settings.bitrate > 0 {
            args.extend(["-b:v".into(), format!("{}k", settings.bitrate)]);
        } else {
            args.extend(["-q:v".into(), settings.q.to_string()]);
        }
    } else {
        // Software, chosen for latency rather than for size: every one of
        // these is configured to emit a picture as soon as it has one.
        match choice.codec {
            Codec::Av1 => args.extend([
                "-c:v".into(),
                "libsvtav1".into(),
                "-preset".into(),
                "12".into(),
                "-svtav1-params".into(),
                "lookahead=0:pred-struct=1".into(),
                "-crf".into(),
                settings.q.to_string(),
            ]),
            Codec::H264 => args.extend([
                "-c:v".into(),
                "libx264".into(),
                "-preset".into(),
                "ultrafast".into(),
                "-tune".into(),
                "zerolatency".into(),
                "-crf".into(),
                settings.q.to_string(),
            ]),
            Codec::Mjpeg => args.extend(["-c:v".into(), "mjpeg".into()]),
        }
    }
    args.extend([
        "-g".into(),
        settings.gop.to_string(),
        "-bf".into(),
        "0".into(),
        // Without this the muxer sits on up to 32 KB of output, which at the
        // 3 KB/s this path produces is ten seconds of latency.
        "-flush_packets".into(),
        "1".into(),
        "-f".into(),
        container(choice.codec).into(),
        "pipe:1".into(),
    ]);
    args
}

/// Which encoders this machine can actually run, honouring the allow-list.
///
/// `auto` does not trust `ffmpeg -encoders`: a name in that list only means
/// the build has the encoder, not that this GPU will initialise it. Each
/// candidate gets a two-frame null encode instead, which costs a few hundred
/// milliseconds once at startup and is the only answer that is not a guess.
pub fn probe(config: &FramesConfig) -> Vec<EncoderChoice> {
    let mut found = Vec::new();
    for codec in &config.codecs {
        if !codec.is_video() {
            continue;
        }
        let candidates = match config.encoder {
            EncoderBackend::Auto => vec![true, false],
            EncoderBackend::Vaapi => vec![true],
            EncoderBackend::Software => vec![false],
        };
        for vaapi in candidates {
            let choice = EncoderChoice { codec: *codec, vaapi };
            if try_encode(config, choice) {
                found.push(choice);
                break;
            }
        }
    }
    found
}

fn try_encode(config: &FramesConfig, choice: EncoderChoice) -> bool {
    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        "color=c=black:s=320x240:r=8:d=0.25".into(),
    ];
    let settings = config.settings(choice.codec);
    if choice.vaapi {
        args.extend([
            "-vaapi_device".into(),
            config.vaapi_device.clone(),
            "-vf".into(),
            "format=nv12,hwupload".into(),
            "-c:v".into(),
            match choice.codec {
                Codec::Av1 => "av1_vaapi".into(),
                Codec::H264 => "h264_vaapi".into(),
                Codec::Mjpeg => "mjpeg_vaapi".into(),
            },
            "-q:v".into(),
            settings.q.to_string(),
        ]);
    } else {
        match choice.codec {
            Codec::Av1 => args.extend(["-c:v".into(), "libsvtav1".into(), "-preset".into(), "12".into()]),
            Codec::H264 => args.extend(["-c:v".into(), "libx264".into(), "-preset".into(), "ultrafast".into()]),
            Codec::Mjpeg => args.extend(["-c:v".into(), "mjpeg".into()]),
        }
    }
    args.extend(["-f".into(), "null".into(), "-".into()]);
    Command::new("ffmpeg")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(args: &[String]) -> String {
        args.join(" ")
    }

    #[test]
    fn the_vaapi_av1_line_carries_the_flags_e2_measured() {
        let config = FramesConfig::default();
        let args = ffmpeg_args(
            &config,
            EncoderChoice { codec: Codec::Av1, vaapi: true },
            "bgr24",
            640,
            368,
            8,
        );
        let line = joined(&args);
        assert!(line.contains("-f rawvideo -pix_fmt bgr24 -s 640x368 -r 8 -i pipe:0"));
        assert!(line.contains("-vf format=nv12,hwupload"));
        assert!(line.contains("-c:v av1_vaapi"));
        assert!(line.contains("-async_depth 1"), "E2: 136 ms -> 2 ms");
        assert!(line.contains("-g 16"));
        assert!(line.contains("-bf 0"));
        assert!(line.contains("-flush_packets 1"), "or the muxer hoards 32 KB");
        assert!(line.contains("-q:v 30"), "VAAPI ignores -qp");
        assert!(line.ends_with("-f ivf pipe:1"));
    }

    #[test]
    fn h264_is_annex_b_because_webcodecs_needs_it_without_a_description() {
        let args = ffmpeg_args(
            &FramesConfig::default(),
            EncoderChoice { codec: Codec::H264, vaapi: true },
            "rgb24",
            320,
            192,
            4,
        );
        let line = joined(&args);
        assert!(line.contains("-c:v h264_vaapi"));
        assert!(line.ends_with("-f h264 pipe:1"));
        assert_eq!(container(Codec::H264), "h264");
    }

    #[test]
    fn a_configured_bitrate_replaces_constant_quality() {
        let config = FramesConfig::parse("[frames]\n[frames.codec.av1]\nbitrate = 600\n");
        let line = joined(&ffmpeg_args(
            &config,
            EncoderChoice { codec: Codec::Av1, vaapi: true },
            "bgr24",
            640,
            368,
            8,
        ));
        assert!(line.contains("-b:v 600k"));
        assert!(!line.contains("-q:v"));
    }

    #[test]
    fn the_software_fallbacks_are_configured_for_latency_not_size() {
        let config = FramesConfig::default();
        let av1 = joined(&ffmpeg_args(
            &config,
            EncoderChoice { codec: Codec::Av1, vaapi: false },
            "bgr24",
            640,
            368,
            8,
        ));
        assert!(av1.contains("-c:v libsvtav1"));
        assert!(av1.contains("lookahead=0"));
        let h264 = joined(&ffmpeg_args(
            &config,
            EncoderChoice { codec: Codec::H264, vaapi: false },
            "bgr24",
            640,
            368,
            8,
        ));
        assert!(h264.contains("-tune zerolatency"));
    }
}
