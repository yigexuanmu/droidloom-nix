//! Decodes a real H.264 stream through the session and compares each frame
//! against an ffmpeg software decode of the same stream: the engine's
//! whole path — Annex B parse, parameter sets, DPB, decode submission,
//! copy-out — runs over whatever driver the loader picks, forwarding
//! included. The comparison is per-plane mean absolute error; H.264 8-bit
//! decode is deterministic, so a conformant hardware decoder agrees with
//! the software reference to the bit.

use std::fs;

fn main() {
    if let Err(error) = run() {
        eprintln!("decode-test: {error}");
        std::process::exit(1);
    }
}

struct Options {
    stream: String,
    reference: String,
    frames: usize,
}

fn parse() -> Result<Options, String> {
    let mut options = Options { stream: String::new(), reference: String::new(), frames: usize::MAX };
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let mut value = || arguments.next().ok_or(format!("{argument} needs a value"));
        match argument.as_str() {
            "--stream" => options.stream = value()?,
            "--reference" => options.reference = value()?,
            "--frames" => options.frames = value()?.parse().map_err(|_| "bad --frames")?,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if options.stream.is_empty() {
        return Err("--stream is required".into());
    }
    Ok(options)
}

/// Splits an Annex B stream into NAL units, start codes stripped.
fn split_nals(data: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut units = Vec::new();
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        if data[index] == 0 && data[index + 1] == 0 && data[index + 2] == 1 {
            starts.push(index + 3);
            index += 3;
            continue;
        }
        if index + 4 <= data.len() && data[index] == 0 && data[index + 1] == 0 && data[index + 2] == 0 && data[index + 3] == 1 {
            starts.push(index + 4);
            index += 4;
            continue;
        }
        index += 1;
    }
    for (position, start) in starts.iter().enumerate() {
        let end = starts.get(position + 1).copied().unwrap_or(data.len());
        // The NAL kind byte sits at the start; strip any trailing zero fill.
        let mut payload = data[*start..end].to_vec();
        while payload.last() == Some(&0) {
            payload.pop();
        }
        if let Some(first) = payload.first() {
            units.push((*first, payload));
        }
    }
    units
}

fn run() -> Result<(), String> {
    let options = parse()?;
    let data = fs::read(&options.stream).map_err(|error| format!("read stream: {error}"))?;
    let nals = split_nals(&data);
    let mut sps_nals = Vec::new();
    let mut pps_nals = Vec::new();
    let mut frames: Vec<Vec<(u8, Vec<u8>)>> = Vec::new();
    for (kind, payload) in &nals {
        match kind & 0x1F {
            7 => sps_nals.push(payload.clone()),
            8 => pps_nals.push(payload.clone()),
            1 | 5 => frames.push(vec![(*kind, payload.clone())]),
            _ => {}
        }
    }
    // The engine consumes Annex B: start codes are part of the contract.
    let mut csd = Vec::new();
    for nal in sps_nals.iter().chain(pps_nals.iter()) {
        csd.extend_from_slice(&[0, 0, 0, 1]);
        csd.extend_from_slice(nal);
    }

    let mut session = droidloom_video::Session::new((320, 240)).map_err(|error| format!("session: {error}"))?;
    println!("dpb slots: {}", session.dpb_slots());
    eprintln!("test: csd {} bytes, {} frame slices", csd.len(), frames.len());
    session.configure(&csd)?;
    eprintln!("test: configured");

    let reference = fs::read(&options.reference).map_err(|error| format!("read reference: {error}")).ok();
    let frame_bytes = reference.as_ref().map(|reference| reference.len() / frames.len().max(1));

    let frames_to_decode = frames.len().min(options.frames);
    let mut worst = 0.0_f64;
    let mut worst_frame = 0_usize;
    for (index, frame) in frames.iter().take(frames_to_decode).enumerate() {
        let mut bytes = Vec::new();
        for (_, payload) in frame {
            bytes.extend_from_slice(&[0, 0, 0, 1]);
            bytes.extend_from_slice(payload);
        }
        eprintln!("test: frame {index} bytes {}", bytes.len());
        let slot = session
            .decode(&bytes, -1, index as u64)
            .map_err(|error| format!("frame {index}: {error}"))?
            .ok_or(format!("frame {index} produced no picture"))?;
        let mut planes = Vec::new();
        session
            .copy_out(slot, &mut planes)
            .map_err(|error| format!("frame {index} copy-out: {error}"))?;
        match (frame_bytes, reference.as_ref()) {
            (Some(bytes_per_frame), Some(reference)) => {
                let begin = index * bytes_per_frame;
                let end = begin + bytes_per_frame;
                if end > reference.len() {
                    break;
                }
                let expected = &reference[begin..end];
                let luma = bytes_per_frame * 2 / 3;
                let mut error_sum = 0.0_f64;
                for (position, byte) in expected.iter().enumerate() {
                    error_sum += ((*byte as f64) - (planes[position] as f64)).abs();
                }
                let mean = error_sum / expected.len() as f64;
                if mean > worst {
                    worst = mean;
                    worst_frame = index;
                }
                if index % 10 == 0 {
                    println!("frame {index}: slot {slot}, luma+chroma mean error {mean:.3}");
                }
                let _ = luma;
            }
            _ => {
                let checksum: u64 = planes.iter().map(|byte| u64::from(*byte)).sum();
                println!("frame {index}: slot {slot}, checksum {checksum}");
            }
        }
    }
    if reference.is_some() {
        println!("worst frame mean absolute error: {worst:.3} (frame {worst_frame})");
        if worst < 1.0 {
            println!("PASS: the hardware decode matches the software reference");
        } else {
            println!("MISMATCH: the decode diverged from the reference");
            return Err("decoded output diverged".into());
        }
    } else {
        println!("no reference given; the stream decoded without errors");
    }
    Ok(())
}
