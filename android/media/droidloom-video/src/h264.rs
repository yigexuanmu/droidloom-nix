//! The H.264 syntax the engine feeds Vulkan Video with.
//!
//! Vulkan Video does not parse bitstreams: the application reads Annex B,
//! builds `StdVideoH264*` structures for the profile's SPS and PPS, and
//! drives the DPB through the slice headers' reference marking. This module
//! carries the bit reader and the parsers for exactly those structures; the
//! entropy-coded slice data stays untouched, the GPU consumes it raw.

use droidloom_gpu_protocol::generated::vk::{
    StdVideoDecodeH264PictureInfo, StdVideoDecodeH264PictureInfoFlags,
    StdVideoDecodeH264ReferenceInfo, StdVideoDecodeH264ReferenceInfoFlags, StdVideoH264PictureParameterSet,
    StdVideoH264SequenceParameterSet, StdVideoH264SpsFlags,
};

/// A slice of one Annex B stream, positioned for the reader.
pub struct Reader<'a> {
    data: &'a [u8],
    /// The bit the reader sits on, from the most significant end.
    bit: usize,
}

impl<'a> Reader<'a> {
    /// Reads one Annex B NAL's payload, start-code prefixes stripped.
    pub fn nal(data: &'a [u8]) -> Reader<'a> {
        Reader { data, bit: 0 }
    }

    fn bit(&mut self) -> Option<bool> {
        let byte = *self.data.get(self.bit / 8)?;
        let bit = (byte >> (7 - (self.bit % 8))) & 1 == 1;
        self.bit += 1;
        Some(bit)
    }

    /// `u(n)`: a big-endian field of `bits` bits.
    pub fn bits(&mut self, bits: usize) -> Option<u32> {
        let mut value = 0_u32;
        for _ in 0..bits {
            value = (value << 1) | u32::from(self.bit()?);
        }
        Some(value)
    }

    /// `ue(v)`: exp-Golomb.
    pub fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0_u32;
        while self.bit()? == false {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        if zeros == 0 {
            return Some(0);
        }
        let base = self.bits(zeros as usize)?;
        Some((1 << zeros) - 1 + base)
    }

    /// `se(v)`: signed exp-Golomb.
    pub fn se(&mut self) -> Option<i32> {
        let code = self.ue()?;
        let value = ((code + 1) / 2) as i32;
        Some(if code % 2 == 0 { -value } else { value })
    }
}

/// One access unit's NALs, with their types.
pub struct AccessUnit<'a> {
    /// SPS NALs seen since the last unit.
    pub sps: Vec<&'a [u8]>,
    pub pps: Vec<&'a [u8]>,
    /// Coded slices, in order, with their NAL kinds.
    pub slices: Vec<(u8, &'a [u8])>,
}

/// Splits an Annex B stream into the NALs the decoder acts on. Emulation
/// prevention stays in: only the parameter-set parsers remove it, and only
/// they read the bytes.
pub fn split_nals(data: &[u8]) -> AccessUnit<'_> {
    let mut unit = AccessUnit { sps: Vec::new(), pps: Vec::new(), slices: Vec::new() };
    let mut start = None;
    let mut index = 0;
    while index + 3 <= data.len() {
        if data[index] == 0 && data[index + 1] == 0 && data[index + 2] == 1 {
            if let Some(begin) = start.take() {
                let payload = &data[begin..index];
                push_nal(&mut unit, data[begin], payload);
            }
            index += 3;
            start = Some(index);
            continue;
        }
        index += 1;
    }
    if let Some(begin) = start {
        // A trailing start code with no payload (the RBSP trailing bits of
        // one NAL ending 00 00 01) is not a NAL; only a real byte after the
        // start code makes one, and the kind byte is that first byte.
        if begin < data.len() {
            push_nal(&mut unit, data[begin], &data[begin..]);
        }
    }
    unit
}

fn push_nal<'a>(unit: &mut AccessUnit<'a>, kind: u8, payload: &'a [u8]) {
    match kind & 0x1F {
        7 => unit.sps.push(payload),
        8 => unit.pps.push(payload),
        1 | 5 => unit.slices.push((kind, payload)),
        _ => {}
    }
}

/// Removes emulation prevention bytes from a NAL the parsers read.
fn unescape(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut zeros = 0;
    for byte in payload {
        if zeros >= 2 && *byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if *byte == 0 { zeros + 1 } else { 0 };
        out.push(*byte);
    }
    out
}

/// The parsed sequence parameter set, in the fields `StdVideoH264...` wants.
#[derive(Clone, Debug)]
pub struct Sps {
    pub profile_idc: u32,
    pub constraint_flags: u8,
    pub level_idc: u32,
    pub seq_parameter_set_id: u32,
    pub chroma_format_idc: u32,
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
    pub log2_max_frame_num_minus4: u32,
    pub pic_order_cnt_type: u32,
    pub log2_max_pic_order_cnt_lsb_minus4: u32,
    pub delta_pic_order_always_zero_flag: bool,
    pub offset_for_nonref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub num_ref_frames_in_pic_order_cnt_cycle: u32,
    pub offsets_for_ref_frame: [i32; 255],
    pub max_num_ref_frames: u32,
    pub pic_width_in_mbs_minus1: u32,
    pub pic_height_in_map_units_minus1: u32,
    pub frame_mbs_only_flag: bool,
    pub mb_adaptive_frame_field_flag: bool,
    pub direct_8x8_inference_flag: bool,
}

impl Default for Sps {
    fn default() -> Self {
        Self {
            profile_idc: 66,
            constraint_flags: 0,
            level_idc: 0,
            seq_parameter_set_id: 0,
            chroma_format_idc: 1,
            bit_depth_luma_minus8: 0,
            bit_depth_chroma_minus8: 0,
            log2_max_frame_num_minus4: 0,
            pic_order_cnt_type: 0,
            log2_max_pic_order_cnt_lsb_minus4: 0,
            delta_pic_order_always_zero_flag: false,
            offset_for_nonref_pic: 0,
            offset_for_top_to_bottom_field: 0,
            num_ref_frames_in_pic_order_cnt_cycle: 0,
            offsets_for_ref_frame: [0; 255],
            max_num_ref_frames: 1,
            pic_width_in_mbs_minus1: 0,
            pic_height_in_map_units_minus1: 0,
            frame_mbs_only_flag: true,
            mb_adaptive_frame_field_flag: false,
            direct_8x8_inference_flag: true,
        }
    }
}

/// Parses one SPS NAL. VUI carries no fields the engine forwards: the parsed
/// structure omits it, which the driver reads as "no VUI".
pub fn parse_sps(payload: &[u8]) -> Option<Sps> {
    let body = unescape(payload);
    let mut r = Reader::nal(&body);
    let profile_idc = r.bits(8)?;
    let constraints = r.bits(8)? as u8;
    let level_idc = r.bits(8)?;
    let seq_parameter_set_id = r.ue()?;
    let mut sps = Sps { profile_idc, constraint_flags: constraints, level_idc, seq_parameter_set_id, ..Sps::default() };
    if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        sps.chroma_format_idc = r.ue()?;
        if sps.chroma_format_idc == 3 {
            r.bits(1)?; // separate_colour_plane_flag
        }
        sps.bit_depth_luma_minus8 = r.ue()?;
        sps.bit_depth_chroma_minus8 = r.ue()?;
        r.bits(1)?; // qpprime_y_zero_transform_bypass_flag
        r.bits(1)?; // seq_scaling_matrix_present_flag; scaling lists stay out
    }
    sps.log2_max_frame_num_minus4 = r.ue()?;
    sps.pic_order_cnt_type = r.ue()?;
    match sps.pic_order_cnt_type {
        0 => {
            sps.log2_max_pic_order_cnt_lsb_minus4 = r.ue()?;
        }
        1 => {
            sps.delta_pic_order_always_zero_flag = r.bits(1)? == 1;
            sps.offset_for_nonref_pic = r.se()?;
            sps.offset_for_top_to_bottom_field = r.se()?;
            sps.num_ref_frames_in_pic_order_cnt_cycle = r.ue()?;
            for slot in sps
                .offsets_for_ref_frame
                .iter_mut()
                .take(sps.num_ref_frames_in_pic_order_cnt_cycle as usize)
            {
                *slot = r.se()?;
            }
        }
        _ => {}
    }
    sps.max_num_ref_frames = r.ue()?;
    let _ = r.bits(1)?; // gaps_in_frame_num_value_allowed_flag
    sps.pic_width_in_mbs_minus1 = r.ue()?;
    sps.pic_height_in_map_units_minus1 = r.ue()?;
    sps.frame_mbs_only_flag = r.bits(1)? == 1;
    if !sps.frame_mbs_only_flag {
        sps.mb_adaptive_frame_field_flag = r.bits(1)? == 1;
    }
    sps.direct_8x8_inference_flag = r.bits(1)? == 1;
    let _ = r.bits(1)?; // frame_cropping_flag; crops stay out of the session
    Some(sps)
}

/// The parsed picture parameter set, minus its optional scaling lists.
#[derive(Clone, Debug, Default)]
pub struct Pps {
    pub pic_parameter_set_id: u32,
    pub seq_parameter_set_id: u32,
    pub entropy_coding_mode_flag: bool,
    pub pic_order_present_flag: bool,
    pub num_slice_groups_minus1: u32,
    pub num_ref_idx_l0_default_active_minus1: u32,
    pub num_ref_idx_l1_default_active_minus1: u32,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_idc: u32,
    pub pic_init_qp_minus26: i32,
    pub pic_init_qs_minus26: i32,
    pub chroma_qp_index_offset: i32,
    pub deblocking_filter_control_present_flag: bool,
    pub constrained_intra_pred_flag: bool,
    pub redundant_pic_cnt_present_flag: bool,
    pub transform_8x8_mode_flag: bool,
}

/// Parses one PPS NAL.
pub fn parse_pps(payload: &[u8]) -> Option<Pps> {
    let body = unescape(payload);
    let mut r = Reader::nal(&body);
    let mut pps = Pps {
        pic_parameter_set_id: r.ue()?,
        seq_parameter_set_id: r.ue()?,
        ..Pps::default()
    };
    pps.entropy_coding_mode_flag = r.bits(1)? == 1;
    pps.pic_order_present_flag = r.bits(1)? == 1;
    pps.num_slice_groups_minus1 = r.ue()?;
    if pps.num_slice_groups_minus1 > 0 {
        // FMO slice groups are not a decode path Android streams take; the
        // parser reads the run-length form far enough to stay aligned.
        let style = r.ue()?;
        if style == 0 {
            r.ue()?;
        }
    }
    pps.num_ref_idx_l0_default_active_minus1 = r.ue()?;
    pps.num_ref_idx_l1_default_active_minus1 = r.ue()?;
    pps.weighted_pred_flag = r.bits(1)? == 1;
    pps.weighted_bipred_idc = r.bits(2)?;
    pps.pic_init_qp_minus26 = r.se()?;
    pps.pic_init_qs_minus26 = r.se()?;
    pps.chroma_qp_index_offset = r.se()?;
    pps.deblocking_filter_control_present_flag = r.bits(1)? == 1;
    pps.constrained_intra_pred_flag = r.bits(1)? == 1;
    pps.redundant_pic_cnt_present_flag = r.bits(1)? == 1;
    if r.bit()? {
        pps.transform_8x8_mode_flag = r.bits(1)? == 1;
        let _ = r.bits(1)?;
        let _ = r.bits(1)?;
    }
    Some(pps)
}

/// What one slice header names: the picture the DPB slot books.
#[derive(Clone, Debug, Default)]
pub struct SliceHeader {
    pub first_mb_in_slice: u32,
    pub slice_type: u32,
    pub pic_parameter_set_id: u32,
    pub frame_num: u32,
    pub field_pic_flag: bool,
    pub idr_pic_id: u32,
    pub pic_order_cnt_lsb: u32,
    pub delta_pic_order_cnt_bottom: i32,
    pub is_idr: bool,
    pub is_ref: bool,
    /// `dec_ref_pic_marking` operations, as (opcode, value) pairs.
    pub marking: Vec<(u32, u32)>,
}

/// Parses the prefix of one slice NAL: everything before the entropy-coded
/// data, which the GPU consumes raw.
pub fn parse_slice(payload: &[u8], pps: &Pps, sps: &Sps, nal_kind: u8) -> Option<SliceHeader> {
    let body = unescape(payload);
    let mut r = Reader::nal(&body);
    let mut header = SliceHeader {
        first_mb_in_slice: r.ue()?,
        slice_type: r.ue()?,
        pic_parameter_set_id: r.ue()?,
        is_idr: nal_kind == 5,
        is_ref: nal_kind == 5,
        ..SliceHeader::default()
    };
    let frame_num_bits = (sps.log2_max_frame_num_minus4 + 4) as usize;
    header.frame_num = r.bits(frame_num_bits)?;
    if !sps.frame_mbs_only_flag {
        header.field_pic_flag = r.bits(1)? == 1;
        if header.field_pic_flag {
            r.bits(1)?; // bottom_field_flag
        }
    }
    if header.is_idr {
        header.idr_pic_id = r.ue()?;
    }
    if sps.pic_order_cnt_type == 0 {
        header.pic_order_cnt_lsb = r.bits((sps.log2_max_pic_order_cnt_lsb_minus4 + 4) as usize)?;
        if pps.pic_order_present_flag && !header.field_pic_flag {
            header.delta_pic_order_cnt_bottom = r.se()?;
        }
    }
    if sps.pic_order_cnt_type == 1 && !sps.delta_pic_order_always_zero_flag {
        let _ = r.se()?;
        if pps.pic_order_present_flag && header.field_pic_flag {
            let _ = r.se()?;
        }
    }
    if header.is_ref {
        let mut done = false;
        while !done {
            let opcode = r.ue()?;
            match opcode {
                1 | 3 => {
                    let value = r.bits((sps.log2_max_frame_num_minus4 + 4) as usize)?;
                    header.marking.push((opcode, value));
                    done = opcode == 3;
                }
                2 => {
                    let long_term = r.ue()?;
                    let value = r.bits((sps.log2_max_pic_order_cnt_lsb_minus4 + 4) as usize)?;
                    header.marking.push((opcode, (long_term << 16) | value));
                }
                4 | 5 | 6 => {
                    header.marking.push((opcode, 0));
                    done = opcode != 4;
                }
                _ => {
                    header.marking.push((opcode, 0));
                }
            }
        }
    } else if r.bit()? {
        // A non-reference picture's marking names only short-term release.
    }
    Some(header)
}

/// The engine's session: parameter sets, DPB slots, and the structures the
/// driver consumes. The Vulkan objects live in the crate root's session.
pub struct H264Session {
    /// Sequence parameter sets by id.
    pub sps: Vec<Sps>,
    pub pps: Vec<Pps>,
    /// The count the capabilities query granted.
    pub max_dpb_slots: u32,
    /// Frames decoded so far, for the session parameters' sequence count.
    pub frames: u64,
}

impl H264Session {
    /// A session sized to the driver's DPB grant.
    pub fn new(max_dpb_slots: u32) -> Self {
        Self { sps: Vec::new(), pps: Vec::new(), max_dpb_slots, frames: 0 }
    }

    /// Keeps the SPS the slice stream names, returning whether it changed.
    pub fn store_sps(&mut self, sps: Sps) -> bool {
        let id = sps.seq_parameter_set_id as usize;
        let changed = self.sps.get(id).map(|old| old.level_idc != sps.level_idc).unwrap_or(true);
        if self.sps.len() <= id {
            self.sps.resize(id + 1, Sps::default());
        }
        self.sps[id] = sps;
        changed
    }

    /// Keeps the PPS the slice stream names.
    pub fn store_pps(&mut self, pps: Pps) {
        let id = pps.pic_parameter_set_id as usize;
        if self.pps.len() <= id {
            self.pps.resize(id + 1, Pps::default());
        }
        self.pps[id] = pps;
    }

    /// The `StdVideoH264SequenceParameterSet` of the stored SPS at `id`.
    pub fn std_sps(&self, id: usize) -> Option<StdVideoH264SequenceParameterSet> {
        let sps = self.sps.get(id)?;
        Some(StdVideoH264SequenceParameterSet {
            flags: {
                let mut flags = StdVideoH264SpsFlags { constraint_set0_flag_bits: 0 };
                let bits = flags.constraint_set0_flag_bits;
                let mut packed = bits & 0x3F; // constraint set 0..5
                if sps.direct_8x8_inference_flag {
                    packed |= StdVideoH264SpsFlags::DIRECT_8X8_INFERENCE_FLAG;
                }
                if sps.mb_adaptive_frame_field_flag {
                    packed |= StdVideoH264SpsFlags::MB_ADAPTIVE_FRAME_FIELD_FLAG;
                }
                if sps.frame_mbs_only_flag {
                    packed |= StdVideoH264SpsFlags::FRAME_MBS_ONLY_FLAG;
                }
                if sps.delta_pic_order_always_zero_flag {
                    packed |= StdVideoH264SpsFlags::DELTA_PIC_ORDER_ALWAYS_ZERO_FLAG;
                }
                flags.constraint_set0_flag_bits = packed;
                flags
            },
            profile_idc: sps.profile_idc as i32,
            level_idc: Self::std_level_idc(sps.level_idc),
            chroma_format_idc: sps.chroma_format_idc as i32,
            seq_parameter_set_id: sps.seq_parameter_set_id as u8,
            bit_depth_luma_minus8: sps.bit_depth_luma_minus8 as u8,
            bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8 as u8,
            log2_max_frame_num_minus4: sps.log2_max_frame_num_minus4 as u8,
            pic_order_cnt_type: sps.pic_order_cnt_type as i32,
            offset_for_non_ref_pic: sps.offset_for_nonref_pic,
            offset_for_top_to_bottom_field: sps.offset_for_top_to_bottom_field,
            log2_max_pic_order_cnt_lsb_minus4: sps.log2_max_pic_order_cnt_lsb_minus4 as u8,
            num_ref_frames_in_pic_order_cnt_cycle: sps.num_ref_frames_in_pic_order_cnt_cycle as u8,
            max_num_ref_frames: sps.max_num_ref_frames as u8,
            reserved1: 0,
            pic_width_in_mbs_minus1: sps.pic_width_in_mbs_minus1,
            pic_height_in_map_units_minus1: sps.pic_height_in_map_units_minus1,
            frame_crop_left_offset: 0,
            frame_crop_right_offset: 0,
            frame_crop_top_offset: 0,
            frame_crop_bottom_offset: 0,
            reserved2: 0,
            pOffsetForRefFrame: sps.offsets_for_ref_frame.as_ptr(),
            pScalingLists: std::ptr::null(),
            pSequenceParameterSetVui: std::ptr::null(),
        })
    }

    /// The `StdVideoH264PictureParameterSet` of the stored PPS at `id`.
    pub fn std_pps(&self, id: usize) -> Option<StdVideoH264PictureParameterSet> {
        let pps = self.pps.get(id)?;
        Some(StdVideoH264PictureParameterSet {
            flags: droidloom_gpu_protocol::generated::vk::StdVideoH264PpsFlags {
                transform_8x8_mode_flag_bits: u32::from(pps.transform_8x8_mode_flag)
                    | (u32::from(pps.redundant_pic_cnt_present_flag) << 1)
                    | (u32::from(pps.constrained_intra_pred_flag) << 2)
                    | (u32::from(pps.deblocking_filter_control_present_flag) << 3),
            },
            seq_parameter_set_id: pps.seq_parameter_set_id as u8,
            pic_parameter_set_id: pps.pic_parameter_set_id as u8,
            num_ref_idx_l0_default_active_minus1: pps.num_ref_idx_l0_default_active_minus1 as u8,
            num_ref_idx_l1_default_active_minus1: pps.num_ref_idx_l1_default_active_minus1 as u8,
            weighted_bipred_idc: pps.weighted_bipred_idc as i32,
            pic_init_qp_minus26: pps.pic_init_qp_minus26 as i8,
            pic_init_qs_minus26: pps.pic_init_qs_minus26 as i8,
            chroma_qp_index_offset: pps.chroma_qp_index_offset as i8,
            second_chroma_qp_index_offset: pps.chroma_qp_index_offset as i8,
            pScalingLists: std::ptr::null(),
        })
    }

    /// The decode picture info of one slice, from the parsed header.
    pub fn picture_info(&self, header: &SliceHeader) -> StdVideoDecodeH264PictureInfo {
        let mut flags = StdVideoDecodeH264PictureInfoFlags { field_pic_flag_bits: 0 };
        let is_intra = header.is_idr
            || header.slice_type == 2 || header.slice_type == 4 || header.slice_type == 7
            || header.slice_type == 9;
        flags.field_pic_flag_bits |= u32::from(header.field_pic_flag) << 0;
        flags.field_pic_flag_bits |= u32::from(is_intra) << 1;
        flags.field_pic_flag_bits |= u32::from(header.is_idr) << 2;
        flags.field_pic_flag_bits |= u32::from(header.is_ref) << 4;
        StdVideoDecodeH264PictureInfo {
            flags,
            seq_parameter_set_id: 0,
            pic_parameter_set_id: header.pic_parameter_set_id as u8,
            reserved1: 0,
            reserved2: 0,
            frame_num: header.frame_num as u16,
            idr_pic_id: header.idr_pic_id as u16,
            PicOrderCnt: [0, 0],
        }
    }

    /// Maps the stream's raw H.264 `level_idc` (10..52) to the
    /// `StdVideoH264LevelIdc` sequential enum (0..15).
    pub fn std_level_idc(raw: u32) -> i32 {
        match raw {
            10 => 0, 11 => 1, 12 => 2, 13 => 3,
            20 => 4, 21 => 5, 22 => 6,
            30 => 7, 31 => 8, 32 => 9,
            40 => 10, 41 => 11, 42 => 12,
            50 => 13, 51 => 14,
            _ => 15,
        }
    }

    /// The reference info the DPB slot books.
    pub fn reference_info(&self, frame_num: u32) -> StdVideoDecodeH264ReferenceInfo {
        StdVideoDecodeH264ReferenceInfo {
            flags: StdVideoDecodeH264ReferenceInfoFlags { top_field_flag_bits: 0 },
            FrameNum: frame_num as u16,
            reserved: 0,
            PicOrderCnt: [0; 2],
        }
    }
}
