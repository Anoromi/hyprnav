//! `[frames]` in `~/.config/hyprnav/config.toml`.
//!
//! ```toml
//! [frames]
//! encoder = "auto"              # auto | vaapi | software
//! vaapi_device = "/dev/dri/renderD128"
//! codecs = ["av1", "h264", "mjpeg"]   # allow-list, in preference order
//! default_width = 640
//! max_pipelines = 4
//! force_fallback = false        # ignore the plugin, pace captures on a timer
//!
//! [frames.codec.av1]
//! q = 30
//! bitrate = 0                   # 0 = constant quality
//! gop = 16
//! ```
//!
//! The parser below understands the subset of TOML that shape needs — tables,
//! strings, integers, booleans and string arrays — and nothing else. hyprnav
//! has no other configuration file and no TOML dependency, and a config that
//! is read once at startup does not justify acquiring one.

use crate::video::Codec;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncoderBackend {
    Auto,
    Vaapi,
    Software,
}

impl EncoderBackend {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "vaapi" => Some(Self::Vaapi),
            "software" | "cpu" => Some(Self::Software),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CodecSettings {
    /// `-q:v`. VAAPI ignores `-qp`, which is why this is not called qp.
    pub q: u32,
    /// kbit/s; 0 keeps the encoder in constant-quality mode.
    pub bitrate: u32,
    pub gop: u32,
}

impl CodecSettings {
    fn default_for(codec: Codec) -> Self {
        match codec {
            // AV1 at 640 wide: q 30 is ~3 KB/s on a terminal (E2).
            Codec::Av1 => Self { q: 30, bitrate: 0, gop: 16 },
            Codec::H264 => Self { q: 26, bitrate: 0, gop: 16 },
            Codec::Mjpeg => Self { q: 60, bitrate: 0, gop: 1 },
        }
    }
}

#[derive(Clone, Debug)]
pub struct FramesConfig {
    pub encoder: EncoderBackend,
    pub vaapi_device: String,
    /// Allow-list in preference order; the daemon picks the first entry a
    /// client also asked for and the machine can actually encode.
    pub codecs: Vec<Codec>,
    pub default_width: u32,
    pub max_pipelines: usize,
    /// Pretend the plugin is absent, for testing the paced fallback.
    pub force_fallback: bool,
    codec_settings: HashMap<Codec, CodecSettings>,
}

impl Default for FramesConfig {
    fn default() -> Self {
        Self {
            encoder: EncoderBackend::Auto,
            vaapi_device: "/dev/dri/renderD128".to_owned(),
            codecs: vec![Codec::Av1, Codec::H264, Codec::Mjpeg],
            default_width: crate::frames::DEFAULT_WIDTH,
            max_pipelines: 4,
            force_fallback: false,
            codec_settings: HashMap::new(),
        }
    }
}

impl FramesConfig {
    pub fn settings(&self, codec: Codec) -> CodecSettings {
        self.codec_settings
            .get(&codec)
            .copied()
            .unwrap_or_else(|| CodecSettings::default_for(codec))
    }

    /// `$XDG_CONFIG_HOME/hyprnav/config.toml`, else `~/.config/…`.
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(base.join("hyprnav/config.toml"))
    }

    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(_) => Self::default(),
        }
    }

    /// Read `[frames]` out of a config file. Anything unrecognised — other
    /// sections, unknown keys, malformed values — is ignored rather than
    /// fatal: a typo in an optional section must not stop the daemon.
    pub fn parse(text: &str) -> Self {
        let mut config = Self::default();
        let mut section = String::new();
        for raw in text.lines() {
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
                section = name.trim().to_ascii_lowercase();
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            if section == "frames" {
                config.apply(&key, value);
                continue;
            }
            if let Some(codec) = section
                .strip_prefix("frames.codec.")
                .and_then(Codec::parse)
            {
                let settings = config
                    .codec_settings
                    .entry(codec)
                    .or_insert_with(|| CodecSettings::default_for(codec));
                match key.as_str() {
                    "q" | "qp" | "quality" => {
                        if let Some(number) = parse_int(value) {
                            settings.q = number.clamp(1, 255) as u32;
                        }
                    }
                    "bitrate" => {
                        if let Some(number) = parse_int(value) {
                            settings.bitrate = number.max(0) as u32;
                        }
                    }
                    "gop" => {
                        if let Some(number) = parse_int(value) {
                            settings.gop = number.clamp(1, 600) as u32;
                        }
                    }
                    _ => {}
                }
            }
        }
        config
    }

    fn apply(&mut self, key: &str, value: &str) {
        match key {
            "encoder" => {
                if let Some(backend) = parse_string(value).as_deref().and_then(EncoderBackend::parse)
                {
                    self.encoder = backend;
                }
            }
            "vaapi_device" => {
                if let Some(device) = parse_string(value) {
                    self.vaapi_device = device;
                }
            }
            "codecs" => {
                let listed: Vec<Codec> =
                    parse_string_array(value).iter().filter_map(|n| Codec::parse(n)).collect();
                if !listed.is_empty() {
                    self.codecs = listed;
                }
            }
            "default_width" => {
                if let Some(number) = parse_int(value) {
                    self.default_width = (number as u32)
                        .clamp(crate::frames::MIN_WIDTH, crate::frames::MAX_WIDTH);
                }
            }
            "max_pipelines" => {
                if let Some(number) = parse_int(value) {
                    self.max_pipelines = number.clamp(1, 64) as usize;
                }
            }
            "force_fallback" => {
                if let Some(flag) = parse_bool(value) {
                    self.force_fallback = flag;
                }
            }
            _ => {}
        }
    }
}

/// Drop a trailing `#` comment, respecting quoted strings.
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    for (at, byte) in line.char_indices() {
        match byte {
            '"' => in_string = !in_string,
            '#' if !in_string => return &line[..at],
            _ => {}
        }
    }
    line
}

fn parse_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let inner = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .or_else(|| trimmed.strip_prefix('\'').and_then(|rest| rest.strip_suffix('\'')))?;
    Some(inner.to_owned())
}

fn parse_int(value: &str) -> Option<i64> {
    value.trim().parse::<i64>().ok()
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn parse_string_array(value: &str) -> Vec<String> {
    let trimmed = value.trim();
    let Some(inner) = trimmed.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) else {
        return Vec::new();
    };
    inner
        .split(',')
        .filter_map(|entry| parse_string(entry.trim()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_or_empty_config_is_the_documented_default() {
        let config = FramesConfig::parse("");
        assert_eq!(config.encoder, EncoderBackend::Auto);
        assert_eq!(config.codecs, vec![Codec::Av1, Codec::H264, Codec::Mjpeg]);
        assert_eq!(config.default_width, 640);
        assert_eq!(config.max_pipelines, 4);
        assert!(!config.force_fallback);
        assert_eq!(config.settings(Codec::Av1), CodecSettings { q: 30, bitrate: 0, gop: 16 });
    }

    #[test]
    fn every_frames_key_is_read_and_other_sections_are_ignored() {
        let config = FramesConfig::parse(
            r#"
            [server]
            codecs = ["nonsense"]   # not ours

            [frames]
            encoder = "vaapi"       # comment after a value
            vaapi_device = "/dev/dri/renderD129"
            codecs = ["h264", "av1"]
            default_width = 960
            max_pipelines = 2
            force_fallback = true

            [frames.codec.h264]
            q = 22
            gop = 30
            bitrate = 800
            "#,
        );
        assert_eq!(config.encoder, EncoderBackend::Vaapi);
        assert_eq!(config.vaapi_device, "/dev/dri/renderD129");
        assert_eq!(config.codecs, vec![Codec::H264, Codec::Av1]);
        assert_eq!(config.default_width, 960);
        assert_eq!(config.max_pipelines, 2);
        assert!(config.force_fallback);
        assert_eq!(config.settings(Codec::H264), CodecSettings { q: 22, bitrate: 800, gop: 30 });
        // Untouched codecs keep their defaults.
        assert_eq!(config.settings(Codec::Av1).q, 30);
    }

    #[test]
    fn nonsense_values_leave_the_defaults_alone() {
        let config = FramesConfig::parse(
            "[frames]\nencoder = \"quantum\"\ncodecs = [\"bogus\"]\ndefault_width = wide\n\
             max_pipelines =\nforce_fallback = yes\nunknown_key = 1\n",
        );
        assert_eq!(config.encoder, EncoderBackend::Auto);
        assert_eq!(config.codecs, vec![Codec::Av1, Codec::H264, Codec::Mjpeg]);
        assert_eq!(config.default_width, 640);
        assert!(!config.force_fallback);
    }

    #[test]
    fn widths_are_clamped_to_what_the_capture_path_supports() {
        assert_eq!(FramesConfig::parse("[frames]\ndefault_width = 4\n").default_width, 64);
        assert_eq!(FramesConfig::parse("[frames]\ndefault_width = 99999\n").default_width, 3840);
    }

    #[test]
    fn a_hash_inside_a_string_is_not_a_comment() {
        let config = FramesConfig::parse("[frames]\nvaapi_device = \"/dev/dri/by-path/pci#0\"\n");
        assert_eq!(config.vaapi_device, "/dev/dri/by-path/pci#0");
    }
}
