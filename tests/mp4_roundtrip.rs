//! Round-trips a small A+V recording through the in-house muxer and validates
//! the result with the independent `mp4-atom` parser. Also checks the HEVC
//! (`hvc1`/`hvcC`) and AAC (`esds`) sample entries with synthetic streams.

use std::cell::RefCell;
use std::io::{Cursor, Seek, SeekFrom, Write};
use std::rc::Rc;
use std::time::Duration;

use mp4_atom::{Any, Codec, FourCC, ReadFrom, StszSamples};
use openclip::audio::{AudioCodecConfig, AudioEncoder, Mp3Encoder};
use openclip::mux::{AudioTrackConfig, Mp4Writer, VideoCodecConfig, VideoTrackConfig, VIDEO_TIMESCALE};
use openclip::video::{Converter, EncoderRequest, H264Encoder, InputLayout, PixelFormat, RawFrame, VideoEncoder};

const W: u32 = 320;
const H: u32 = 180;
const FPS: u32 = 30;
const FRAMES: u32 = 75;
const RATE: u32 = 48_000;

/// In-memory writer we can still read after the muxer consumed it.
#[derive(Clone)]
struct Shared(Rc<RefCell<Cursor<Vec<u8>>>>);

impl Shared {
    fn new() -> Self {
        Shared(Rc::new(RefCell::new(Cursor::new(Vec::new()))))
    }

    fn bytes(&self) -> Vec<u8> {
        self.0.borrow().get_ref().clone()
    }
}

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().flush()
    }
}

impl Seek for Shared {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.0.borrow_mut().seek(pos)
    }
}

struct Built {
    bytes: Vec<u8>,
    video_samples: usize,
    keyframes: Vec<bool>,
    audio_samples: usize,
}

fn build(with_audio: bool) -> Built {
    let shared = Shared::new();
    let mut converter = Converter::new(W, H, InputLayout::I420).unwrap();
    let mut video = H264Encoder::new(&EncoderRequest::simple(W, H, FPS, 800_000)).unwrap();
    let mut audio = Mp3Encoder::new(RATE, 2, 128).unwrap();
    let audio_cfg = with_audio.then(|| AudioTrackConfig {
        sample_rate: RATE,
        channels: 2,
        bitrate_bps: audio.bitrate_bps(),
        samples_per_frame: audio.samples_per_frame(),
        codec: audio.codec_config(),
    });
    let mut mux = Mp4Writer::new(
        shared.clone(),
        Some(VideoTrackConfig { width: W, height: H, fps: FPS as f64, codec: VideoCodecConfig::H264 }),
        audio_cfg,
    )
    .unwrap();

    let mut frame = RawFrame {
        data: vec![0u8; (W * H * 4) as usize],
        width: W,
        height: H,
        stride: W * 4,
        format: PixelFormat::Bgra,
        pts: Duration::ZERO,
        mouse: None,
    };
    let mut keyframes = Vec::new();
    let mut video_samples = 0;
    let mut audio_samples = 0;
    let mut pcm = Vec::new();
    for i in 0..FRAMES {
        for (n, px) in frame.data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let x = (n as u32 % W + i * 3) % W;
            px[0] = (x * 255 / W) as u8;
            px[1] = ((n as u32 / W) * 255 / H) as u8;
            px[2] = if (x / 32).is_multiple_of(2) { 200 } else { 30 };
            px[3] = 255;
        }
        frame.pts = Duration::from_secs_f64(i as f64 / FPS as f64);
        converter.convert(&frame).unwrap();
        let enc = video.encode(converter.frame(), frame.pts).unwrap();
        if let Some(p) = video.codec_params() {
            mux.set_codec_params(p);
        }
        for f in enc {
            assert!(f.data.len() > 4);
            assert!(f.data.starts_with(&[0, 0, 0, 1]), "encoder output must be Annex-B");
            mux.push_video(&f.data, f.pts, f.keyframe).unwrap();
            keyframes.push(f.keyframe);
            video_samples += 1;
        }
        if with_audio {
            pcm.clear();
            for s in 0..(RATE / FPS) {
                let t = (i * (RATE / FPS) + s) as f32 / RATE as f32;
                let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.3;
                pcm.push(v);
                pcm.push(v);
            }
            for f in AudioEncoder::encode(&mut audio, &pcm).unwrap() {
                mux.push_audio(&f.data).unwrap();
                audio_samples += 1;
            }
        }
    }
    if with_audio {
        for f in AudioEncoder::flush(&mut audio).unwrap() {
            mux.push_audio(&f.data).unwrap();
            audio_samples += 1;
        }
    }
    mux.finalize().unwrap();
    Built { bytes: shared.bytes(), video_samples, keyframes, audio_samples }
}

fn parse(bytes: &[u8]) -> Vec<Any> {
    let mut cursor = Cursor::new(bytes);
    let mut atoms = Vec::new();
    while let Some(atom) = <Option<Any> as ReadFrom>::read_from(&mut cursor).unwrap() {
        atoms.push(atom);
    }
    atoms
}

fn sample_sizes(s: &StszSamples) -> Vec<u32> {
    match s {
        StszSamples::Different { sizes } => sizes.clone(),
        StszSamples::Identical { count, size } => vec![*size; *count as usize],
    }
}

/// Number of samples in each chunk according to stsc.
fn chunk_sample_counts(stbl: &mp4_atom::Stbl, chunk_count: usize) -> Vec<u32> {
    (0..chunk_count)
        .map(|ci| {
            let chunk_no = ci as u32 + 1;
            stbl.stsc
                .entries
                .iter()
                .rev()
                .find(|e| e.first_chunk <= chunk_no)
                .map(|e| e.samples_per_chunk)
                .expect("stsc covers chunk")
        })
        .collect()
}

#[test]
fn video_and_audio_roundtrip() {
    let b = build(true);
    let bytes = &b.bytes;
    assert!(b.video_samples >= FRAMES as usize - 2, "video samples {}", b.video_samples);
    assert!(b.audio_samples > 50, "audio samples {}", b.audio_samples);
    assert!(b.keyframes[0], "first frame must be a keyframe");

    let atoms = parse(bytes);
    assert_eq!(atoms.len(), 3, "top-level boxes: {atoms:?}");
    assert!(matches!(atoms[0], Any::Ftyp(_)));
    let mdat_len = match &atoms[1] {
        Any::Mdat(m) => m.data.len(),
        other => panic!("expected mdat, got {other:?}"),
    };
    assert!(mdat_len > 10_000);
    let moov = match &atoms[2] {
        Any::Moov(m) => m,
        other => panic!("expected moov, got {other:?}"),
    };
    assert_eq!(moov.trak.len(), 2);
    assert_eq!(moov.mvhd.timescale, 1000);
    let expected_ms = (FRAMES as u64 * 1000) / FPS as u64;
    assert!(
        (moov.mvhd.duration as i64 - expected_ms as i64).abs() <= 40,
        "movie duration {}",
        moov.mvhd.duration
    );

    // --- video track ---
    let vtrak = &moov.trak[0];
    assert_eq!(vtrak.tkhd.track_id, 1);
    assert_eq!(vtrak.mdia.hdlr.handler, FourCC::new(b"vide"));
    assert_eq!(vtrak.mdia.mdhd.timescale, VIDEO_TIMESCALE);
    let stbl = &vtrak.mdia.minf.stbl;
    let avc1 = match &stbl.stsd.codecs[0] {
        Codec::Avc1(a) => a,
        other => panic!("expected avc1, got {other:?}"),
    };
    assert_eq!((avc1.visual.width as u32, avc1.visual.height as u32), (W, H));
    assert_eq!(avc1.avcc.length_size, 4);
    assert_eq!(avc1.avcc.sequence_parameter_sets.len(), 1);
    assert_eq!(avc1.avcc.picture_parameter_sets.len(), 1);
    assert_eq!(avc1.avcc.sequence_parameter_sets[0][0] & 0x1F, 7);
    assert_eq!(avc1.avcc.picture_parameter_sets[0][0] & 0x1F, 8);

    let sizes = sample_sizes(&stbl.stsz.samples);
    assert_eq!(sizes.len(), b.video_samples);
    let total_stts: u32 = stbl.stts.entries.iter().map(|e| e.sample_count).sum();
    assert_eq!(total_stts as usize, b.video_samples);
    for e in &stbl.stts.entries {
        assert!((e.sample_delta as i64 - 3000).abs() <= 1, "delta {}", e.sample_delta);
    }
    let stss = stbl.stss.as_ref().expect("stss present");
    let expected_keys: Vec<u32> =
        b.keyframes.iter().enumerate().filter(|(_, k)| **k).map(|(i, _)| i as u32 + 1).collect();
    assert_eq!(stss.entries, expected_keys);

    // Walk chunks and verify every sample is a sequence of length-prefixed NALs
    // with the parameter sets stripped out.
    let stco = stbl.stco.as_ref().expect("stco present");
    let counts = chunk_sample_counts(stbl, stco.entries.len());
    let mut sample_idx = 0usize;
    for (ci, off) in stco.entries.iter().enumerate() {
        let mut pos = *off as usize;
        for _ in 0..counts[ci] {
            let size = sizes[sample_idx] as usize;
            let mut p = pos;
            while p < pos + size {
                let len = u32::from_be_bytes(bytes[p..p + 4].try_into().unwrap()) as usize;
                assert!(
                    len > 0 && p + 4 + len <= pos + size,
                    "bad NAL length {len} in sample {sample_idx}"
                );
                let nal_type = bytes[p + 4] & 0x1F;
                assert!(matches!(nal_type, 1 | 5 | 6), "unexpected NAL type {nal_type}");
                p += 4 + len;
            }
            pos += size;
            sample_idx += 1;
        }
    }
    assert_eq!(sample_idx, b.video_samples);

    // --- audio track ---
    let atrak = &moov.trak[1];
    assert_eq!(atrak.tkhd.track_id, 2);
    assert_eq!(atrak.mdia.hdlr.handler, FourCC::new(b"soun"));
    assert_eq!(atrak.mdia.mdhd.timescale, RATE);
    let astbl = &atrak.mdia.minf.stbl;
    let mp4a = match &astbl.stsd.codecs[0] {
        Codec::Mp4a(a) => a,
        other => panic!("expected mp4a, got {other:?}"),
    };
    assert_eq!(mp4a.audio.channel_count, 2);
    assert_eq!(mp4a.esds.es_desc.dec_config.object_type_indication, 0x6B);
    assert_eq!(mp4a.esds.es_desc.dec_config.stream_type, 0x05);
    assert_eq!(astbl.stts.entries.len(), 1);
    assert_eq!(astbl.stts.entries[0].sample_delta, 1152);
    assert_eq!(astbl.stts.entries[0].sample_count as usize, b.audio_samples);

    // Every audio sample must start with an MP3 sync word.
    let asizes = sample_sizes(&astbl.stsz.samples);
    let astco = astbl.stco.as_ref().unwrap();
    let acounts = chunk_sample_counts(astbl, astco.entries.len());
    let mut idx = 0;
    for (ci, off) in astco.entries.iter().enumerate() {
        let mut pos = *off as usize;
        for _ in 0..acounts[ci] {
            assert_eq!(bytes[pos], 0xFF, "audio sample {idx} lacks sync");
            assert_eq!(bytes[pos + 1] & 0xE0, 0xE0);
            pos += asizes[idx] as usize;
            idx += 1;
        }
    }
    assert_eq!(idx, b.audio_samples);
}

#[test]
fn video_only_roundtrip() {
    let b = build(false);
    let atoms = parse(&b.bytes);
    let moov = atoms.iter().find_map(|a| if let Any::Moov(m) = a { Some(m) } else { None }).unwrap();
    assert_eq!(moov.trak.len(), 1);
    let stbl = &moov.trak[0].mdia.minf.stbl;
    let n: u32 = stbl.stts.entries.iter().map(|e| e.sample_count).sum();
    assert_eq!(n as usize, b.video_samples);
}

/// Finds a box by fourcc anywhere in `bytes` and returns its payload.
fn find_box<'a>(bytes: &'a [u8], fourcc: &[u8; 4]) -> Option<&'a [u8]> {
    let pos = bytes.windows(4).position(|w| w == fourcc)?;
    let start = pos - 4;
    let size = u32::from_be_bytes(bytes[start..start + 4].try_into().unwrap()) as usize;
    if size == 1 {
        // 64-bit largesize (used by mdat).
        let large = u64::from_be_bytes(bytes[start + 8..start + 16].try_into().unwrap()) as usize;
        return Some(&bytes[start + 16..start + large]);
    }
    Some(&bytes[start + 8..start + size])
}

/// Synthetic HEVC stream (VPS/SPS/PPS from a real x265 encode, fake slices):
/// the muxer must write `hvc1` + `hvcC`, strip parameter sets from samples
/// and length-prefix the rest.
#[test]
fn hevc_sample_entry() {
    const VPS: [u8; 24] = [
        0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x03, 0x00, 0x5d, 0x95, 0x98, 0x09,
    ];
    const SPS: [u8; 41] = [
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x5d,
        0xa0, 0x02, 0x80, 0x80, 0x2d, 0x16, 0x59, 0x59, 0xa4, 0x93, 0x2b, 0x9a, 0x02, 0x00, 0x00, 0x03, 0x00, 0x02,
        0x00, 0x00, 0x03, 0x00, 0x3c,
    ];
    const PPS: [u8; 7] = [0x44, 0x01, 0xc1, 0x72, 0xb4, 0x62, 0x40];
    let annexb = |nals: &[&[u8]]| -> Vec<u8> {
        let mut v = Vec::new();
        for n in nals {
            v.extend_from_slice(&[0, 0, 0, 1]);
            v.extend_from_slice(n);
        }
        v
    };
    let idr: Vec<u8> = [0x26u8, 0x01, 0xaf, 0x12, 0x34].to_vec(); // NAL type 19 (IDR_W_RADL)
    let slice: Vec<u8> = [0x02u8, 0x01, 0xd0, 0x56].to_vec(); // NAL type 1 (TRAIL_R)

    let shared = Shared::new();
    let mut mux = Mp4Writer::new(
        shared.clone(),
        Some(VideoTrackConfig { width: 1280, height: 720, fps: 30.0, codec: VideoCodecConfig::Hevc }),
        None,
    )
    .unwrap();
    // No set_codec_params: the writer must harvest VPS/SPS/PPS from the keyframe.
    mux.push_video(&annexb(&[&VPS, &SPS, &PPS, &idr]), Duration::ZERO, true).unwrap();
    for i in 1..5u64 {
        mux.push_video(&annexb(&[&slice]), Duration::from_millis(i * 33), false).unwrap();
    }
    mux.finalize().unwrap();
    let bytes = shared.bytes();

    let ftyp = find_box(&bytes, b"ftyp").unwrap();
    assert!(ftyp.windows(4).any(|w| w == b"hvc1"), "ftyp should carry the hvc1 brand");
    // Search inside moov so the ftyp brand bytes cannot be mistaken for boxes.
    let moov = find_box(&bytes, b"moov").expect("moov");
    let hvc1 = find_box(moov, b"hvc1").expect("hvc1 sample entry");
    assert_eq!(u16::from_be_bytes([hvc1[24], hvc1[25]]), 1280);
    assert_eq!(u16::from_be_bytes([hvc1[26], hvc1[27]]), 720);
    let hvcc = find_box(moov, b"hvcC").expect("hvcC");
    assert_eq!(hvcc[0], 1, "configurationVersion");
    assert_eq!(hvcc[1] & 0x1F, 1, "general_profile_idc Main");
    assert_eq!(u32::from_be_bytes(hvcc[2..6].try_into().unwrap()), 0x6000_0000);
    assert_eq!(hvcc[12], 93, "level_idc");
    assert_eq!(hvcc[16] & 3, 1, "chroma 4:2:0");
    assert_eq!(hvcc[21] & 3, 3, "lengthSizeMinusOne");
    assert_eq!(hvcc[22], 3, "three parameter-set arrays");
    let mut p = 23;
    for (t, nal) in [(32u8, &VPS[..]), (33, &SPS[..]), (34, &PPS[..])] {
        assert_eq!(hvcc[p] & 0x3F, t);
        assert_eq!(u16::from_be_bytes([hvcc[p + 1], hvcc[p + 2]]), 1);
        let len = u16::from_be_bytes([hvcc[p + 3], hvcc[p + 4]]) as usize;
        assert_eq!(&hvcc[p + 5..p + 5 + len], nal);
        p += 5 + len;
    }

    // Samples: parameter sets stripped, each NAL length-prefixed.
    let stsz = find_box(moov, b"stsz").unwrap();
    let count = u32::from_be_bytes(stsz[8..12].try_into().unwrap());
    assert_eq!(count, 5);
    let first_size = u32::from_be_bytes(stsz[12..16].try_into().unwrap()) as usize;
    assert_eq!(first_size, 4 + idr.len(), "keyframe sample holds only the IDR NAL");
    let payload = find_box(&bytes, b"mdat").unwrap();
    assert_eq!(&payload[..4], &(idr.len() as u32).to_be_bytes());
    assert_eq!(&payload[4..4 + idr.len()], &idr[..]);
    assert_eq!(&payload[4 + idr.len()..8 + idr.len()], &(slice.len() as u32).to_be_bytes());
}

/// AAC audio gets an `esds` with objectTypeIndication 0x40 and the
/// AudioSpecificConfig as DecoderSpecificInfo.
#[test]
fn aac_sample_entry() {
    let asc = vec![0x11, 0x90]; // AAC-LC, 48 kHz, stereo
    let shared = Shared::new();
    let mut mux = Mp4Writer::new(
        shared.clone(),
        None,
        Some(AudioTrackConfig {
            sample_rate: 48_000,
            channels: 2,
            bitrate_bps: 192_000,
            samples_per_frame: 1024,
            codec: AudioCodecConfig::Aac { asc: asc.clone() },
        }),
    )
    .unwrap();
    for i in 0..10u8 {
        mux.push_audio(&[0x21, i, 0x00, 0x03]).unwrap();
    }
    mux.finalize().unwrap();
    let bytes = shared.bytes();

    let atoms = parse(&bytes);
    let moov = atoms.iter().find_map(|a| if let Any::Moov(m) = a { Some(m) } else { None }).unwrap();
    let stbl = &moov.trak[0].mdia.minf.stbl;
    let mp4a = match &stbl.stsd.codecs[0] {
        Codec::Mp4a(a) => a,
        other => panic!("expected mp4a, got {other:?}"),
    };
    assert_eq!(mp4a.audio.sample_rate.integer(), 48_000);
    assert_eq!(mp4a.esds.es_desc.dec_config.object_type_indication, 0x40);
    let dsi = mp4a.esds.es_desc.dec_config.dec_specific.as_ref().expect("DecoderSpecificInfo");
    assert_eq!(dsi.raw, asc);
    assert_eq!((dsi.profile, dsi.freq_index, dsi.chan_conf), (2, 3, 2));
    assert_eq!(stbl.stts.entries[0].sample_delta, 1024);
    assert_eq!(stbl.stts.entries[0].sample_count, 10);

    // PCM is refused in MP4.
    let err = Mp4Writer::new(
        Shared::new(),
        None,
        Some(AudioTrackConfig {
            sample_rate: 48_000,
            channels: 2,
            bitrate_bps: 1_536_000,
            samples_per_frame: 960,
            codec: AudioCodecConfig::Pcm { bits: 16 },
        }),
    );
    assert!(err.is_err());
}

#[test]
fn multi_audio_tracks_roundtrip() {
    let shared = Shared::new();
    let mut converter = Converter::new(W, H, InputLayout::I420).unwrap();
    let mut video = H264Encoder::new(&EncoderRequest::simple(W, H, FPS, 800_000)).unwrap();
    let mut enc0 = Mp3Encoder::new(RATE, 2, 128).unwrap();
    let mut enc1 = Mp3Encoder::new(RATE, 2, 128).unwrap();
    let mut enc2 = Mp3Encoder::new(RATE, 2, 128).unwrap();
    let cfg = |e: &Mp3Encoder| AudioTrackConfig {
        sample_rate: RATE,
        channels: 2,
        bitrate_bps: e.bitrate_bps(),
        samples_per_frame: e.samples_per_frame(),
        codec: e.codec_config(),
    };
    let mut mux = Mp4Writer::with_audio_tracks(
        shared.clone(),
        Some(VideoTrackConfig { width: W, height: H, fps: FPS as f64, codec: VideoCodecConfig::H264 }),
        vec![cfg(&enc0), cfg(&enc1), cfg(&enc2)],
    )
    .unwrap();
    assert_eq!(mux.audio_track_count(), 3);

    let mut frame = RawFrame {
        data: vec![128u8; (W * H * 4) as usize],
        width: W,
        height: H,
        stride: W * 4,
        format: PixelFormat::Bgra,
        pts: Duration::ZERO,
        mouse: None,
    };
    let mut track_samples = [0usize; 3];
    let mut pcm0 = Vec::new();
    let mut pcm1 = Vec::new();
    let mut pcm2 = Vec::new();
    for i in 0..30u32 {
        frame.pts = Duration::from_secs_f64(i as f64 / FPS as f64);
        converter.convert(&frame).unwrap();
        for f in video.encode(converter.frame(), frame.pts).unwrap() {
            if let Some(p) = video.codec_params() {
                mux.set_codec_params(p);
            }
            mux.push_video(&f.data, f.pts, f.keyframe).unwrap();
        }
        pcm0.clear();
        pcm1.clear();
        pcm2.clear();
        for s in 0..(RATE / FPS) {
            let t = (i * (RATE / FPS) + s) as f32 / RATE as f32;
            let a = (t * 220.0 * std::f32::consts::TAU).sin() * 0.25;
            let b = (t * 660.0 * std::f32::consts::TAU).sin() * 0.25;
            pcm0.extend_from_slice(&[a + b, a + b]);
            pcm1.extend_from_slice(&[a, a]);
            pcm2.extend_from_slice(&[b, b]);
        }
        for (idx, (enc, pcm)) in
            [(&mut enc0, &pcm0), (&mut enc1, &pcm1), (&mut enc2, &pcm2)].into_iter().enumerate()
        {
            for f in AudioEncoder::encode(enc, pcm).unwrap() {
                mux.push_audio_track(idx, &f.data).unwrap();
                track_samples[idx] += 1;
            }
        }
    }
    for (idx, enc) in [&mut enc0, &mut enc1, &mut enc2].into_iter().enumerate() {
        for f in AudioEncoder::flush(enc).unwrap() {
            mux.push_audio_track(idx, &f.data).unwrap();
            track_samples[idx] += 1;
        }
        assert_eq!(mux.audio_track_samples(idx), track_samples[idx]);
    }
    mux.finalize().unwrap();

    let bytes = shared.bytes();
    let atoms = parse(&bytes);
    let moov = atoms.iter().find_map(|a| if let Any::Moov(m) = a { Some(m) } else { None }).unwrap();
    assert_eq!(moov.trak.len(), 4, "1 video + 3 audio tracks");
    assert_eq!(moov.trak[0].tkhd.track_id, 1);
    for (idx, &expected_samples) in track_samples.iter().enumerate() {
        let trak = &moov.trak[idx + 1];
        assert_eq!(trak.tkhd.track_id, idx as u32 + 2);
        assert_eq!(trak.mdia.hdlr.handler, FourCC::new(b"soun"));
        let stbl = &trak.mdia.minf.stbl;
        assert!(matches!(&stbl.stsd.codecs[0], Codec::Mp4a(_)));
        let sizes = sample_sizes(&stbl.stsz.samples);
        assert_eq!(sizes.len(), expected_samples);
        let stco = stbl.stco.as_ref().unwrap();
        let counts = chunk_sample_counts(stbl, stco.entries.len());
        let mut sidx = 0usize;
        for (ci, off) in stco.entries.iter().enumerate() {
            let mut pos = *off as usize;
            for _ in 0..counts[ci] {
                assert_eq!(bytes[pos], 0xFF, "track {idx} sample {sidx} lacks MP3 sync");
                assert_eq!(bytes[pos + 1] & 0xE0, 0xE0);
                pos += sizes[sidx] as usize;
                sidx += 1;
            }
        }
        assert_eq!(sidx, expected_samples);
    }
}

#[test]
fn finish_video_extends_final_frame_duration_for_vfr() {
    let shared = Shared::new();
    let mut mux = Mp4Writer::new(
        shared.clone(),
        Some(VideoTrackConfig { width: W, height: H, fps: 120.0, codec: VideoCodecConfig::H264 }),
        None,
    )
    .unwrap();
    let sps = vec![0x67, 0x64, 0x00, 0x1F, 0xAC];
    let pps = vec![0x68, 0xEE, 0x3C, 0x80];
    mux.set_codec_params(&openclip::video::CodecParams::H264 { sps, pps });
    // Two frames at 0 ms and 100 ms, then screen stays static until stop at 3100 ms.
    let idr = [0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x21];
    let slice = [0x00, 0x00, 0x00, 0x01, 0x41, 0x9A, 0x22, 0x11];
    mux.push_video(&idr, Duration::from_millis(0), true).unwrap();
    mux.push_video(&slice, Duration::from_millis(100), false).unwrap();
    mux.finish_video(Duration::from_millis(3100));
    mux.finalize().unwrap();

    let bytes = shared.bytes();
    let atoms = parse(&bytes);
    let moov = atoms.iter().find_map(|a| if let Any::Moov(m) = a { Some(m) } else { None }).unwrap();
    let mdhd = &moov.trak[0].mdia.mdhd;
    assert_eq!(mdhd.timescale, VIDEO_TIMESCALE);
    // 3.1 s * 90_000 = 279_000 ticks, so the static tail is preserved without duplicate frames.
    assert_eq!(mdhd.duration, 3100 * VIDEO_TIMESCALE as u64 / 1000);
    let stts = &moov.trak[0].mdia.minf.stbl.stts;
    assert_eq!(stts.entries.len(), 2);
    assert_eq!(stts.entries[0].sample_delta, 9_000);
    assert_eq!(stts.entries[1].sample_delta, 270_000);
}

struct TempMp4(std::path::PathBuf);

impl TempMp4 {
    fn new(name: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!("openclip_test_{}_{name}.mp4", std::process::id()));
        let _ = std::fs::remove_file(&p);
        Self(p)
    }
}

impl Drop for TempMp4 {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn verify_mp4_decodes_cleanly(path: &std::path::Path, expected_frames: usize) {
    assert!(expected_frames > 0, "expected at least one video frame");

    #[cfg(windows)]
    {
        use openclip::capture::FramePool;
        use openclip::player::decode::{Packet, Reader};
        use openclip::video::mf::ComGuard;

        let _com = ComGuard::new();
        let pool = FramePool::new(4);
        let mut reader = Reader::open(path, 640).expect("Media Foundation Reader::open");
        let mut decoded = 0usize;
        loop {
            match reader.read(None, &pool, false).expect("MP4 frame decode must not error") {
                Packet::Video { .. } => decoded += 1,
                Packet::VideoSkipped { .. } | Packet::Audio { .. } | Packet::Idle => {}
                Packet::End => break,
            }
        }
        assert_eq!(
            decoded, expected_frames,
            "Media Foundation decoded {decoded} frames vs {expected_frames} encoded in {}",
            path.display()
        );
    }

    match std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-xerror", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
    {
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                out.status.success() && stderr.trim().is_empty(),
                "ffmpeg reported decode errors (status {:?}): {stderr}",
                out.status.code()
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Optional in stripped CI containers; Media Foundation decode above still ran on Windows.
        }
        Err(e) => panic!("spawning ffmpeg decode verifier failed: {e}"),
    }
}

#[test]
fn adaptive_grid_cpu_mp4_decodes_cleanly() {
    use openclip::pipeline::FrameClock;

    let tmp = TempMp4::new("adaptive_cpu");
    let mut converter = Converter::new(W, H, InputLayout::I420).unwrap();
    let mut enc = H264Encoder::new(&EncoderRequest::simple(W, H, 120, 1_500_000)).unwrap();
    let file = std::fs::File::create(&tmp.0).unwrap();
    let mut mux = Mp4Writer::new(
        std::io::BufWriter::new(file),
        Some(VideoTrackConfig { width: W, height: H, fps: 120.0, codec: VideoCodecConfig::H264 }),
        None,
    )
    .unwrap();

    let mut frame = RawFrame {
        data: vec![0u8; (W * H * 4) as usize],
        width: W,
        height: H,
        stride: W * 4,
        format: PixelFormat::Bgra,
        pts: Duration::ZERO,
        mouse: None,
    };

    let mut clock = FrameClock::new(120, Duration::ZERO);
    let mut pushed = 0usize;
    let mut params_sent = false;
    let mut idx = 0u32;

    for target_fps in [120u32, 90, 72, 60, 90] {
        clock.set_fps(target_fps);
        for _ in 0..30 {
            let pts = clock.slot_pts(clock.next);
            for (n, px) in frame.data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let x = (n as u32 % W + idx * 5) % W;
                let y = n as u32 / W;
                px[0] = (x * 255 / W) as u8;
                px[1] = (y * 255 / H) as u8;
                px[2] = if (x / 24).is_multiple_of(2) { 210 } else { 40 };
                px[3] = 255;
            }
            converter.convert(&frame).unwrap();
            for pkt in enc.encode(converter.frame(), pts).unwrap() {
                if !params_sent && let Some(p) = enc.codec_params() {
                    mux.set_codec_params(p);
                    params_sent = true;
                }
                mux.push_video(&pkt.data, pkt.pts, pkt.keyframe).unwrap();
                pushed += 1;
            }
            clock.mark_emitted(pts);
            clock.next += 1;
            idx += 1;
        }
    }
    for pkt in enc.flush().unwrap() {
        if !params_sent && let Some(p) = enc.codec_params() {
            mux.set_codec_params(p);
            params_sent = true;
        }
        mux.push_video(&pkt.data, pkt.pts, pkt.keyframe).unwrap();
        pushed += 1;
    }
    mux.finalize().unwrap();
    verify_mp4_decodes_cleanly(&tmp.0, pushed);
}

#[cfg(windows)]
#[test]
fn adaptive_grid_gpu_d3d11_mp4_decodes_cleanly() {
    use openclip::pipeline::FrameClock;
    use openclip::video::create_video_encoder_with_d3d;
    use openclip::video::d3d11_vp::{enable_multithread, GpuCaptureProcessor};
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
        D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
        D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

    const GW: u32 = 1280;
    const GH: u32 = 720;

    let mut dev_opt: Option<ID3D11Device> = None;
    let mut ctx_opt: Option<ID3D11DeviceContext> = None;
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let hr = unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut dev_opt),
            None,
            Some(&mut ctx_opt),
        )
    };
    let Ok(()) = hr else { return };
    let (Some(device), Some(context)) = (dev_opt, ctx_opt) else { return };
    enable_multithread(&device);

    let mut req = EncoderRequest::simple(GW, GH, 120, 12_000_000);
    req.codec = openclip::settings::VideoCodec::Auto;
    let Ok((mut enc, _note)) = create_video_encoder_with_d3d(&req, Some(&device)) else { return };
    if !enc.supports_d3d11() {
        // No hardware D3D11 encoder on this runner; skip silently while CPU test always runs.
        return;
    }
    let Ok(mut vp) = GpuCaptureProcessor::new(device.clone(), context.clone(), (GW, GH), (GW, GH), (GW, GH)) else {
        return;
    };

    let tex_desc = D3D11_TEXTURE2D_DESC {
        Width: GW,
        Height: GH,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut src_opt: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&tex_desc, None, Some(&mut src_opt)) }.unwrap();
    let src_tex = src_opt.unwrap();

    let tmp = TempMp4::new("adaptive_gpu");
    let file = std::fs::File::create(&tmp.0).unwrap();
    let mut mux = Mp4Writer::new(
        std::io::BufWriter::new(file),
        Some(VideoTrackConfig {
            width: GW,
            height: GH,
            fps: 120.0,
            codec: if enc.is_hevc() { VideoCodecConfig::Hevc } else { VideoCodecConfig::H264 },
        }),
        None,
    )
    .unwrap();

    let mut bgra = vec![0u8; (GW * GH * 4) as usize];
    let src_rect = RECT { left: 0, top: 0, right: GW as i32, bottom: GH as i32 };
    let mut clock = FrameClock::new(120, Duration::ZERO);
    let mut pushed = 0usize;
    let mut params_sent = false;
    let mut idx = 0u32;

    for target_fps in [120u32, 90, 72, 60, 90] {
        clock.set_fps(target_fps);
        for _ in 0..24 {
            let pts = clock.slot_pts(clock.next);
            let bar_x = (idx * 17) % GW;
            for y in 0..GH {
                let row = &mut bgra[((y * GW) as usize * 4)..(((y + 1) * GW) as usize * 4)];
                for x in 0..GW {
                    let off = (x as usize) * 4;
                    let in_bar = x >= bar_x && x < (bar_x + 96).min(GW);
                    row[off] = if in_bar { 240 } else { (x * 255 / GW) as u8 };
                    row[off + 1] = if in_bar { 180 } else { (y * 255 / GH) as u8 };
                    row[off + 2] = if in_bar { 40 } else { ((idx * 9) % 255) as u8 };
                    row[off + 3] = 255;
                }
            }
            unsafe {
                context.UpdateSubresource(&src_tex, 0, None, bgra.as_ptr().cast(), GW * 4, 0);
            }
            let nv12_slot = vp.convert(&src_tex, (GW, GH), src_rect).unwrap();
            let _ = nv12_slot.wait_complete(&context);
            for pkt in enc.encode_d3d11(&nv12_slot.texture, pts).unwrap() {
                if !params_sent && let Some(p) = enc.codec_params() {
                    mux.set_codec_params(p);
                    params_sent = true;
                }
                mux.push_video(&pkt.data, pkt.pts, pkt.keyframe).unwrap();
                pushed += 1;
            }
            clock.mark_emitted(pts);
            clock.next += 1;
            idx += 1;
        }
    }
    for pkt in enc.flush().unwrap() {
        if !params_sent && let Some(p) = enc.codec_params() {
            mux.set_codec_params(p);
            params_sent = true;
        }
        mux.push_video(&pkt.data, pkt.pts, pkt.keyframe).unwrap();
        pushed += 1;
    }
    mux.finalize().unwrap();
    verify_mp4_decodes_cleanly(&tmp.0, pushed);
}
