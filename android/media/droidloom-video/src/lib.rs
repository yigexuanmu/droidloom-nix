//! The H.264 decode engine of the Codec2 bridge: it parses Annex B, feeds
//! `StdVideoH264*` structures to Vulkan Video over the forwarded GPU, and
//! manages the DPB. The C2 component drives it through the C ABI in `ffi`;
//! the Vulkan objects live in `session`.

pub mod ffi;
pub mod h264;
pub mod session;

use std::collections::VecDeque;

/// One decoded picture the component receives: the output buffer this frame
/// rendered into, the GPU fence the copy carries, and the work ordinal.
pub struct Picture {
    pub dmabuf: i32,
    pub fence: i32,
    pub ordinal: u64,
}

/// The decode session of one Codec2 component: parse, parameter sets, DPB,
/// and the Vulkan submission the `session` module owns.
pub struct Session {
    inner: h264::H264Session,
    session: session::VideoSession,
    /// Bytes not yet a complete access unit.
    pending: VecDeque<u8>,
    /// Ordinals waiting for their output picture, in decode order.
    in_flight: VecDeque<u64>,
}

impl Session {
    /// Opens a session on the cell's forwarded GPU, sized to the DPB grant
    /// the capabilities query returns. `max_dpb_slots` comes from there.
    pub fn new(max_dpb_slots: u32) -> Result<Self, String> {
        let session = session::VideoSession::open(max_dpb_slots)?;
        Ok(Self {
            inner: h264::H264Session::new(max_dpb_slots),
            session,
            pending: VecDeque::new(),
            in_flight: VecDeque::new(),
        })
    }

    /// Feeds a CSD (codec config) record: SPS and PPS NALs, Annex B.
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
        self.session
            .update_parameters(&self.inner.std_sps(0).expect("stored SPS"), self.inner.std_pps(0))
    }

    /// Feeds one frame's bytes and decodes it, returning the picture when the
    /// GPU finished with it. `dmabuf` names the output buffer this frame
    /// renders into; the caller owns its lifecycle.
    pub fn decode(&mut self, data: &[u8], dmabuf: i32, ordinal: u64) -> Result<Option<Picture>, String> {
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
        self.in_flight.push_back(ordinal);
        let picture = self.session.decode(&self.inner, &header, payload, dmabuf)?;
        // The engine reports one picture per decode, in ordinal order: the
        // component's output path follows Android's ordering rules.
        Ok(picture.map(|fence| {
            self.in_flight.pop_front();
            Picture { dmabuf, fence, ordinal }
        }))
    }

    /// The driver's DPB grant, for the component's delay tuning.
    pub fn dpb_slots(&self) -> u32 {
        self.inner.max_dpb_slots
    }
}
