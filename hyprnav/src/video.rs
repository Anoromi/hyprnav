//! Record framing, bitstream parsing and the GOP cache for `frames.sock`.
//!
//! The wire format (FRAMES-VIDEO-PLAN.md §4) is a byte stream of records:
//!
//! ```text
//! u32 magic 'HNVF' | u32 len | u32 flags | u64 pts_us | u16 width | u16 height | payload[len]
//! flags: 1=KEYFRAME 2=CONFIG 4=KEEPALIVE
//! ```
//!
//! All integers are big-endian, which is what `DataView.getUint32(offset)`
//! reads by default in a browser.
//!
//! One record is one temporal unit (AV1) or one access unit (H.264). The
//! first record a client gets is always a CONFIG record whose payload starts
//! with a NUL-terminated codec string (`av01.0.08M.08`, `avc1.42E01E`)
//! followed by the codec's out-of-band configuration, if it has any: nothing
//! for AV1 beyond its sequence header, SPS+PPS in Annex-B for H.264. After it
//! comes the GOP cache burst, so a client that joins a live stream decodes its
//! first picture without waiting for the next keyframe.

use std::collections::VecDeque;

pub const MAGIC: u32 = u32::from_be_bytes(*b"HNVF");
pub const HEADER_BYTES: usize = 24;

pub const FLAG_KEYFRAME: u32 = 1;
pub const FLAG_CONFIG: u32 = 2;
pub const FLAG_KEEPALIVE: u32 = 4;

/// Longest silence before a KEEPALIVE tells the client "static, not dead".
pub const KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum Codec {
    Av1,
    H264,
    Mjpeg,
}

impl Codec {
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "av1" | "av01" => Some(Self::Av1),
            "h264" | "avc" | "avc1" => Some(Self::H264),
            "mjpeg" | "jpeg" => Some(Self::Mjpeg),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Av1 => "av1",
            Self::H264 => "h264",
            Self::Mjpeg => "mjpeg",
        }
    }

    /// The string a browser hands to `VideoDecoder.configure`.
    ///
    /// Both are the baseline-ish profiles the VAAPI encoders produce here:
    /// AV1 Main, 8-bit, level 4.0; H.264 Constrained Baseline 3.0. Getting
    /// the level wrong only matters to a decoder that refuses on capability
    /// grounds, and every decoder that matters accepts these.
    pub fn codec_string(self) -> &'static str {
        match self {
            Self::Av1 => "av01.0.08M.08",
            Self::H264 => "avc1.42E01E",
            Self::Mjpeg => "mjpeg",
        }
    }

    /// True when this codec streams as `HNVF` records rather than multipart.
    pub fn is_video(self) -> bool {
        !matches!(self, Self::Mjpeg)
    }
}

/// Build one record.
pub fn encode_record(flags: u32, pts_us: u64, width: u16, height: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_BYTES + payload.len());
    out.extend_from_slice(&MAGIC.to_be_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&pts_us.to_be_bytes());
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// A parsed record header, for tests and for the CLI's `--ivf` writer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordHeader {
    pub len: usize,
    pub flags: u32,
    pub pts_us: u64,
    pub width: u16,
    pub height: u16,
}

impl RecordHeader {
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < HEADER_BYTES {
            return None;
        }
        let read32 = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
        if read32(0) != MAGIC {
            return None;
        }
        Some(Self {
            len: read32(4) as usize,
            flags: read32(8),
            pts_us: u64::from_be_bytes(bytes[12..20].try_into().unwrap()),
            width: u16::from_be_bytes(bytes[20..22].try_into().unwrap()),
            height: u16::from_be_bytes(bytes[22..24].try_into().unwrap()),
        })
    }
}

/// The CONFIG record's payload: codec string, NUL, then any out-of-band data.
///
/// For H.264 the codec string is read out of the SPS rather than assumed: the
/// VAAPI encoder here produces High profile, and a browser handed
/// `avc1.42E01E` for a High-profile stream is entitled to refuse it.
pub fn config_payload(codec: Codec, extra: &[u8]) -> Vec<u8> {
    let codec_string = match codec {
        Codec::H264 => h264_codec_string(extra)
            .unwrap_or_else(|| codec.codec_string().to_owned()),
        other => other.codec_string().to_owned(),
    };
    let mut payload = Vec::with_capacity(codec_string.len() + 1 + extra.len());
    payload.extend_from_slice(codec_string.as_bytes());
    payload.push(0);
    payload.extend_from_slice(extra);
    payload
}

/// `avc1.PPCCLL` from the three bytes that follow the SPS NAL header.
pub fn h264_codec_string(annex_b: &[u8]) -> Option<String> {
    for (_, payload_at) in nal_starts(annex_b) {
        let nal = &annex_b[payload_at..];
        if nal.first().map(|byte| byte & 0x1f) != Some(7) || nal.len() < 4 {
            continue;
        }
        return Some(format!("avc1.{:02X}{:02X}{:02X}", nal[1], nal[2], nal[3]));
    }
    None
}

// ---------------------------------------------------------------------------
// AV1: IVF demux and enough OBU parsing to spot a keyframe
// ---------------------------------------------------------------------------

/// Pull whole IVF frames out of a growing buffer, skipping the file header.
///
/// ffmpeg writes `-f ivf` as a 32-byte `DKIF` header and then, per temporal
/// unit, a 12-byte header (u32 LE size, u64 LE pts) and the payload. Every
/// field is little-endian, unlike our own records.
#[derive(Default)]
pub struct IvfDemuxer {
    header_seen: bool,
}

impl IvfDemuxer {
    /// Take as many complete frames as `buffer` holds, consuming them.
    pub fn drain(&mut self, buffer: &mut Vec<u8>) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        let mut at = 0usize;
        if !self.header_seen {
            if buffer.len() < 32 {
                return frames;
            }
            if &buffer[0..4] != b"DKIF" {
                // Not IVF after all; hand the bytes on as one blob rather than
                // silently eating the stream.
                self.header_seen = true;
                frames.push(std::mem::take(buffer));
                return frames;
            }
            at = 32;
            self.header_seen = true;
        }
        loop {
            if buffer.len() < at + 12 {
                break;
            }
            let size = u32::from_le_bytes(buffer[at..at + 4].try_into().unwrap()) as usize;
            if buffer.len() < at + 12 + size {
                break;
            }
            frames.push(buffer[at + 12..at + 12 + size].to_vec());
            at += 12 + size;
        }
        buffer.drain(..at);
        frames
    }
}

fn read_leb128(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for index in 0..8 {
        let byte = *bytes.get(*at)?;
        *at += 1;
        value |= ((byte & 0x7f) as u64) << (index * 7);
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    Some(value)
}

/// What an AV1 temporal unit contains, as far as a fan-out cache cares.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Av1Unit {
    pub keyframe: bool,
    /// The sequence header OBU, when this unit carries one.
    pub sequence_header: Option<Vec<u8>>,
}

/// Walk the OBUs of one temporal unit.
///
/// A unit is a keyframe when it carries a sequence header (the encoder emits
/// one before every keyframe) or a frame whose `frame_type` is KEY and which
/// is not a `show_existing_frame` repeat.
pub fn parse_av1_unit(unit: &[u8]) -> Av1Unit {
    const OBU_SEQUENCE_HEADER: u8 = 1;
    const OBU_FRAME_HEADER: u8 = 3;
    const OBU_FRAME: u8 = 6;

    let mut result = Av1Unit::default();
    let mut at = 0usize;
    while at < unit.len() {
        let start = at;
        let header = unit[at];
        at += 1;
        if header & 0x80 != 0 {
            break; // forbidden bit: not an OBU stream
        }
        let kind = (header >> 3) & 0x0f;
        let has_extension = header & 0x04 != 0;
        let has_size = header & 0x02 != 0;
        if has_extension {
            at += 1;
        }
        let size = if has_size {
            match read_leb128(unit, &mut at) {
                Some(size) => size as usize,
                None => break,
            }
        } else {
            unit.len().saturating_sub(at)
        };
        if at + size > unit.len() {
            break;
        }
        let payload = &unit[at..at + size];
        match kind {
            OBU_SEQUENCE_HEADER => {
                result.keyframe = true;
                if result.sequence_header.is_none() {
                    result.sequence_header = Some(unit[start..at + size].to_vec());
                }
            }
            OBU_FRAME | OBU_FRAME_HEADER => {
                // uncompressed_header starts with show_existing_frame (1 bit);
                // when it is 0, frame_type is the next 2 bits, 0 = KEY_FRAME.
                if let Some(first) = payload.first() {
                    let show_existing = first & 0x80 != 0;
                    let frame_type = (first >> 5) & 0x03;
                    if !show_existing && frame_type == 0 {
                        result.keyframe = true;
                    }
                }
            }
            _ => {}
        }
        at += size;
    }
    result
}

// ---------------------------------------------------------------------------
// H.264: Annex-B access units
// ---------------------------------------------------------------------------

/// What an access unit contains.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct H264Unit {
    pub keyframe: bool,
    /// SPS and PPS in Annex-B form, when this unit carries them.
    pub parameter_sets: Option<Vec<u8>>,
}

fn nal_starts(bytes: &[u8]) -> Vec<(usize, usize)> {
    // (start of the start code, start of the NAL payload)
    let mut starts = Vec::new();
    let mut at = 0usize;
    while at + 3 <= bytes.len() {
        if bytes[at] == 0 && bytes[at + 1] == 0 {
            if bytes[at + 2] == 1 {
                starts.push((at, at + 3));
                at += 3;
                continue;
            }
            if at + 4 <= bytes.len() && bytes[at + 2] == 0 && bytes[at + 3] == 1 {
                starts.push((at, at + 4));
                at += 4;
                continue;
            }
        }
        at += 1;
    }
    starts
}

pub fn parse_h264_unit(unit: &[u8]) -> H264Unit {
    let mut result = H264Unit::default();
    let starts = nal_starts(unit);
    let mut sets: Vec<u8> = Vec::new();
    for (index, (code_at, payload_at)) in starts.iter().enumerate() {
        let end = starts.get(index + 1).map(|next| next.0).unwrap_or(unit.len());
        let kind = unit.get(*payload_at).copied().unwrap_or(0) & 0x1f;
        match kind {
            5 => result.keyframe = true,
            7 | 8 => {
                result.keyframe = true;
                sets.extend_from_slice(&unit[*code_at..end]);
            }
            _ => {}
        }
    }
    if !sets.is_empty() {
        result.parameter_sets = Some(sets);
    }
    result
}

/// Split an Annex-B byte stream into access units.
///
/// The encoder runs with `-bf 0`, so each picture is one slice preceded by
/// whatever parameter sets and SEI it needs. An access unit therefore ends
/// right before the next non-VCL NAL that follows a VCL NAL — which is what
/// this looks for, holding back the tail until the next unit starts so that a
/// partially received unit is never emitted.
#[derive(Default)]
pub struct AnnexBSplitter {
    buffer: Vec<u8>,
}

impl AnnexBSplitter {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    pub fn drain(&mut self) -> Vec<Vec<u8>> {
        let starts = nal_starts(&self.buffer);
        let mut units = Vec::new();
        let mut unit_start = None;
        let mut seen_vcl = false;
        let mut cut_at = 0usize;
        for (code_at, payload_at) in &starts {
            let kind = self.buffer.get(*payload_at).copied().unwrap_or(0) & 0x1f;
            let is_vcl = (1..=5).contains(&kind);
            // With `-bf 0` every picture is a single slice, so anything at all
            // after a VCL NAL belongs to the next access unit.
            if unit_start.is_some() && seen_vcl {
                units.push(self.buffer[unit_start.unwrap()..*code_at].to_vec());
                cut_at = *code_at;
                unit_start = None;
                seen_vcl = false;
            }
            if unit_start.is_none() {
                unit_start = Some(*code_at);
            }
            if is_vcl {
                seen_vcl = true;
            }
        }
        self.buffer.drain(..cut_at);
        units
    }

    /// Everything still held back, for the end of the stream.
    pub fn flush(&mut self) -> Option<Vec<u8>> {
        if self.buffer.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut self.buffer))
    }
}

// ---------------------------------------------------------------------------
// GOP cache and per-client queues
// ---------------------------------------------------------------------------

/// The smallest byte sequence that makes a joining client decodable: the
/// CONFIG record, the last keyframe, and every record since.
#[derive(Default)]
pub struct GopCache {
    config: Option<Vec<u8>>,
    since_keyframe: Vec<Vec<u8>>,
    /// A GOP is 16 frames; this bounds a pathological encoder.
    limit: usize,
}

impl GopCache {
    pub fn new(limit: usize) -> Self {
        Self { config: None, since_keyframe: Vec::new(), limit: limit.max(1) }
    }

    pub fn set_config(&mut self, record: Vec<u8>) {
        self.config = Some(record);
    }

    pub fn has_config(&self) -> bool {
        self.config.is_some()
    }

    pub fn push(&mut self, record: Vec<u8>, keyframe: bool) {
        if keyframe {
            self.since_keyframe.clear();
        } else if self.since_keyframe.is_empty() {
            // Nothing to predict from yet: caching this would hand a joining
            // client a record its decoder cannot use.
            return;
        }
        if self.since_keyframe.len() >= self.limit {
            self.since_keyframe.remove(0);
        }
        self.since_keyframe.push(record);
    }

    /// What a client gets the moment it joins.
    pub fn burst(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::with_capacity(self.since_keyframe.len() + 1);
        if let Some(config) = &self.config {
            out.push(config.clone());
        }
        out.extend(self.since_keyframe.iter().cloned());
        out
    }
}

/// One subscriber's queue. Latest-wins is wrong for video, so this queues
/// whole records and, when it overflows, throws the backlog away and starts
/// again from the cached keyframe rather than handing the decoder a hole.
pub struct FrameQueue {
    queue: VecDeque<std::sync::Arc<Vec<u8>>>,
    limit: usize,
    pub closed: bool,
    pub resyncs: u64,
}

impl FrameQueue {
    pub fn new(limit: usize) -> Self {
        Self { queue: VecDeque::new(), limit: limit.max(2), closed: false, resyncs: 0 }
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn push(&mut self, record: std::sync::Arc<Vec<u8>>) {
        self.queue.push_back(record);
    }

    /// Queue `record`, dropping back to `resync` when the client is too slow.
    /// Returns true when a resync happened.
    pub fn push_or_resync(
        &mut self,
        record: std::sync::Arc<Vec<u8>>,
        resync: &[std::sync::Arc<Vec<u8>>],
    ) -> bool {
        if self.queue.len() < self.limit {
            self.queue.push_back(record);
            return false;
        }
        self.queue.clear();
        for entry in resync {
            self.queue.push_back(entry.clone());
        }
        self.resyncs += 1;
        true
    }

    pub fn pop(&mut self) -> Option<std::sync::Arc<Vec<u8>>> {
        self.queue.pop_front()
    }
}

// ---------------------------------------------------------------------------
// IVF writing, for `hyprnav frames --ivf`
// ---------------------------------------------------------------------------

/// A 32-byte IVF file header. `frames` is patched in afterwards when the
/// output is seekable; ffprobe copes with 0 either way.
pub fn ivf_header(fourcc: &[u8; 4], width: u16, height: u16, fps: u32, frames: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.extend_from_slice(b"DKIF");
    out.extend_from_slice(&0u16.to_le_bytes()); // version
    out.extend_from_slice(&32u16.to_le_bytes()); // header length
    out.extend_from_slice(fourcc);
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&fps.max(1).to_le_bytes()); // time base denominator
    out.extend_from_slice(&1u32.to_le_bytes()); // time base numerator
    out.extend_from_slice(&frames.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // unused
    out
}

pub fn ivf_frame(index: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&index.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_record_round_trips_through_its_header() {
        let record = encode_record(FLAG_KEYFRAME | FLAG_CONFIG, 1_234_567, 640, 368, &[1, 2, 3]);
        assert_eq!(record.len(), HEADER_BYTES + 3);
        assert_eq!(&record[0..4], b"HNVF");
        let header = RecordHeader::parse(&record).unwrap();
        assert_eq!(header.len, 3);
        assert_eq!(header.flags, FLAG_KEYFRAME | FLAG_CONFIG);
        assert_eq!(header.pts_us, 1_234_567);
        assert_eq!(header.width, 640);
        assert_eq!(header.height, 368);
        assert_eq!(&record[HEADER_BYTES..], &[1, 2, 3]);
        assert!(RecordHeader::parse(&record[..HEADER_BYTES - 1]).is_none());
        assert!(RecordHeader::parse(b"NOPE....................").is_none());
    }

    #[test]
    fn the_config_payload_leads_with_a_nul_terminated_codec_string() {
        let payload = config_payload(Codec::Av1, &[9, 9]);
        let nul = payload.iter().position(|byte| *byte == 0).unwrap();
        assert_eq!(&payload[..nul], b"av01.0.08M.08");
        assert_eq!(&payload[nul + 1..], &[9, 9]);
        // Nothing to read a profile out of: fall back to the documented one.
        assert_eq!(config_payload(Codec::H264, &[]), b"avc1.42E01E\0".to_vec());
    }

    /// h264_vaapi on this machine emits High profile, and a client told
    /// "constrained baseline" may refuse to decode it.
    #[test]
    fn the_h264_codec_string_comes_from_the_sps() {
        let sets = nal(7, &[0x64, 0x0c, 0x16, 0xac]);
        assert_eq!(h264_codec_string(&sets).as_deref(), Some("avc1.640C16"));
        let payload = config_payload(Codec::H264, &sets);
        assert_eq!(&payload[..payload.iter().position(|b| *b == 0).unwrap()], b"avc1.640C16");
        assert!(h264_codec_string(&nal(1, &[1, 2, 3])).is_none());
    }

    #[test]
    fn the_ivf_demuxer_skips_the_file_header_and_waits_for_whole_frames() {
        let mut demuxer = IvfDemuxer::default();
        let mut buffer = ivf_header(b"AV01", 640, 368, 8, 0);
        assert!(demuxer.drain(&mut buffer).is_empty(), "no frames yet");
        buffer.extend_from_slice(&ivf_frame(0, &[1, 2, 3, 4]));
        buffer.extend_from_slice(&ivf_frame(1, &[5, 6]));
        // A torn third frame must not be handed out.
        buffer.extend_from_slice(&ivf_frame(2, &[7, 7, 7])[..6]);
        let frames = demuxer.drain(&mut buffer);
        assert_eq!(frames, vec![vec![1, 2, 3, 4], vec![5, 6]]);
        buffer.extend_from_slice(&ivf_frame(2, &[7, 7, 7])[6..]);
        assert_eq!(demuxer.drain(&mut buffer), vec![vec![7, 7, 7]]);
        assert!(buffer.is_empty());
    }

    /// An OBU header byte: type in bits 6..3, has_size_field set.
    fn obu(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![(kind << 3) | 0x02, payload.len() as u8];
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn av1_keyframes_are_found_by_sequence_header_or_frame_type() {
        // A sequence header alone marks the unit as a keyframe.
        let unit = obu(1, &[0x00, 0x11]);
        let parsed = parse_av1_unit(&unit);
        assert!(parsed.keyframe);
        assert_eq!(parsed.sequence_header.as_deref(), Some(unit.as_slice()));

        // OBU_FRAME with show_existing_frame=0, frame_type=KEY (bits 0b0_00…).
        let key = parse_av1_unit(&obu(6, &[0b0_00_00000, 0x42]));
        assert!(key.keyframe);
        assert!(key.sequence_header.is_none());

        // frame_type = INTER (0b01) is not a keyframe.
        let inter = parse_av1_unit(&obu(6, &[0b0_01_00000, 0x42]));
        assert!(!inter.keyframe);

        // show_existing_frame repeats are never keyframes either.
        assert!(!parse_av1_unit(&obu(6, &[0b1_00_00000])).keyframe);

        // A temporal delimiter followed by an inter frame stays inter.
        let mut unit = obu(2, &[]);
        unit.extend_from_slice(&obu(6, &[0b0_01_00000]));
        assert!(!parse_av1_unit(&unit).keyframe);

        // Garbage must not panic or loop.
        assert!(!parse_av1_unit(&[0xff, 0xff, 0xff]).keyframe);
        assert!(!parse_av1_unit(&[]).keyframe);
    }

    fn nal(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0, 0, 0, 1, kind & 0x1f];
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn h264_keyframes_are_idr_or_parameter_sets() {
        let mut unit = nal(7, &[0x42, 0xe0, 0x1e]);
        unit.extend_from_slice(&nal(8, &[0xce]));
        unit.extend_from_slice(&nal(5, &[0x88]));
        let parsed = parse_h264_unit(&unit);
        assert!(parsed.keyframe);
        let sets = parsed.parameter_sets.expect("SPS and PPS are kept");
        assert_eq!(sets.len(), nal(7, &[0x42, 0xe0, 0x1e]).len() + nal(8, &[0xce]).len());

        let inter = parse_h264_unit(&nal(1, &[0x9a]));
        assert!(!inter.keyframe);
        assert!(inter.parameter_sets.is_none());
    }

    #[test]
    fn annex_b_units_are_cut_between_pictures_and_never_half_emitted() {
        let mut splitter = AnnexBSplitter::default();
        let mut stream = nal(7, &[1]);
        stream.extend_from_slice(&nal(8, &[2]));
        stream.extend_from_slice(&nal(5, &[3]));
        splitter.push(&stream);
        // Only the trailing, possibly incomplete, unit is held back.
        assert!(splitter.drain().is_empty(), "no boundary seen yet");
        splitter.push(&nal(1, &[4]));
        let units = splitter.drain();
        assert_eq!(units.len(), 1);
        assert!(parse_h264_unit(&units[0]).keyframe);
        assert_eq!(units[0], stream);
        let tail = splitter.flush().unwrap();
        assert_eq!(tail, nal(1, &[4]));
    }

    #[test]
    fn the_gop_cache_joins_at_a_keyframe_and_bounds_itself() {
        let mut cache = GopCache::new(4);
        // Records before the first keyframe are useless to a joiner.
        cache.push(vec![b'p'], false);
        assert!(cache.burst().is_empty());

        cache.set_config(vec![b'c']);
        cache.push(vec![b'K'], true);
        cache.push(vec![b'1'], false);
        assert_eq!(cache.burst(), vec![vec![b'c'], vec![b'K'], vec![b'1']]);

        // A second keyframe throws the old GOP away.
        cache.push(vec![b'L'], true);
        assert_eq!(cache.burst(), vec![vec![b'c'], vec![b'L']]);

        for n in 0..10u8 {
            cache.push(vec![n], false);
        }
        assert_eq!(cache.burst().len(), 1 + 4, "config plus the bound");
        assert!(cache.has_config());
    }

    #[test]
    fn an_overflowing_queue_restarts_from_the_cached_keyframe() {
        let mut queue = FrameQueue::new(3);
        let resync: Vec<Arc<Vec<u8>>> =
            vec![Arc::new(vec![b'c']), Arc::new(vec![b'K'])];
        for n in 0..3u8 {
            assert!(!queue.push_or_resync(Arc::new(vec![n]), &resync));
        }
        assert_eq!(queue.len(), 3);
        assert!(queue.push_or_resync(Arc::new(vec![9]), &resync), "overflow resyncs");
        assert_eq!(queue.resyncs, 1);
        assert_eq!(queue.pop().unwrap().as_slice(), b"c");
        assert_eq!(queue.pop().unwrap().as_slice(), b"K");
        assert!(queue.pop().is_none(), "the backlog is gone, not replayed");
    }
}
