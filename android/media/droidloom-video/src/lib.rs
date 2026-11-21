//! The H.264 decode engine of the Codec2 bridge: it parses Annex B, feeds
//! `StdVideoH264*` structures to Vulkan Video over the forwarded GPU, and
//! manages the DPB. The C2 component drives it through the C ABI in `ffi`;
//! the Vulkan objects live in `session`.

pub mod ffi;
pub mod h264;
pub mod session;

use std::collections::VecDeque;

/// One decoded picture the component receives: the output buffer this frame
/// rendered into, the DPB slot it landed in, and the work ordinal.
pub struct Picture {
    pub dmabuf: i32,
    /// The DPB slot the decoded picture occupies; the copy-out reads it.
    pub slot: u32,
    pub ordinal: u64,
}

/// The decode session of one Codec2 component: parse, parameter sets, DPB,
/// and the Vulkan submission the `session` module owns.
pub struct Session {
    inner: h264::H264Session,
    session: session::VideoSession,
    /// Bytes not yet a complete access unit.
    pending: VecDeque<u8>,
}

impl Session {
    /// Opens a session on the cell's forwarded GPU for `coded_extent`. The
    /// DPB grant comes from the capabilities query inside the session.
    pub fn new(coded_extent: (u32, u32)) -> Result<Self, String> {
        let session = session::VideoSession::open(coded_extent)?;
        let slots = session.caps.max_dpb_slots;
        Ok(Self { inner: h264::H264Session::new(slots), session, pending: VecDeque::new() })
    }

    /// Copies one DPB picture's planes out, for verification.
    pub fn copy_out(&mut self, slot: u32, out: &mut Vec<u8>) -> Result<(), String> {
        self.session.copy_out(slot, out)
    }

    /// The driver's DPB grant, for the component's delay tuning.
    pub fn dpb_slots(&self) -> u32 {
        self.inner.max_dpb_slots
    }

    /// Feeds a CSD (codec config) record: SPS and PPS NALs, Annex B, and
    /// pushes them into the session's parameters.
    pub fn configure(&mut self, csd: &[u8]) -> Result<(), String> {
        let unit = h264::split_nals(csd);
        for payload in &unit.sps {
            let Some(sps) = h264::parse_sps(payload) else { continue };
            self.inner.store_sps(sps);
        }
        for payload in &unit.pps {
            let Some(pps) = h264::parse_pps(payload) else { continue };
            self.inner.store_pps(pps);
        }
        let Some(sps) = self.inner.sps.first() else {
            return Err("the codec config carried no sequence parameter set".into());
        };
        let Some(pps) = self.inner.pps.first() else {
            return Err("the codec config carried no picture parameter set".into());
        };
        self.session.update_parameters(&self.inner.std_sps(0).expect("stored SPS"), &self.inner.std_pps(0).expect("stored PPS"))
    }

    /// Feeds one frame's bytes — one access unit, its slices included — and
    /// decodes it. Returns the DPB slot the picture occupies, for the
    /// copy-out the caller drives.
    pub fn decode(&mut self, data: &[u8], _dmabuf: i32, _ordinal: u64) -> Result<Option<u32>, String> {
        self.pending.extend(data.iter().copied());
        let bytes: Vec<u8> = self.pending.drain(..).collect();
        let unit = h264::split_nals(&bytes);
        let Some((kind, payload)) = unit.slices.first().copied() else {
            // A parameter-set-only unit: nothing to decode.
            self.pending.extend(bytes);
            return Ok(None);
        };
        let sps = self.inner.sps.first().ok_or("decode before the sequence parameter set")?;
        let pps = self.inner.pps.first().ok_or("decode before the picture parameter set")?;
        let Some(header) = h264::parse_slice(payload, pps, sps, kind) else {
            return Err("slice header did not parse".into());
        };
        // The slice NALs of this frame, start codes left in: the bitstream
        // buffer carries complete NAL units.
        let mut slices = Vec::new();
        for (kind, payload) in &unit.slices {
            let mut nal = Vec::with_capacity(payload.len());
            nal.push(kind | (kind & 0xE0));
            nal.extend_from_slice(payload);
            slices.push((*kind, nal));
        }
        let slot = self.session.decode_frame(&self.inner, &header, self.session.coded_extent(), &slices)?;
        Ok(Some(slot))
    }
}
