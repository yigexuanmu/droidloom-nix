//! The C ABI the Codec2 component drives the engine through: one session per
//! component, its lifetime bound to the component's. Every call is safe to
//! make from a single work thread, which is the component's model.

/// An opaque session handle.
pub struct NvVideoSession {
    inner: crate::Session,
}

/// Opens a session sized to the DPB grant. Returns null on failure; the
/// caller logs why through the component's own path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn droidloom_video_open(max_dpb_slots: u32) -> *mut NvVideoSession {
    match crate::Session::new(max_dpb_slots) {
        Ok(session) => Box::into_raw(Box::new(NvVideoSession { inner: session })),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Feeds the codec config: SPS and PPS NALs, Annex B.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn droidloom_video_configure(
    session: *mut NvVideoSession,
    csd: *const u8,
    size: usize,
) -> i32 {
    let Some(session) = (unsafe { session.as_mut() }) else { return -1 };
    match session.inner.configure(unsafe { std::slice::from_raw_parts(csd, size) }) {
        Ok(()) => 0,
        Err(_) => -2,
    }
}

/// Decodes one frame into `dmabuf`, returning the completion fence or -1
/// when the frame needs more input before a picture comes out.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn droidloom_video_decode(
    session: *mut NvVideoSession,
    data: *const u8,
    size: usize,
    dmabuf: i32,
    ordinal: u64,
    out_fence: *mut i32,
) -> i32 {
    let Some(session) = (unsafe { session.as_mut() }) else { return -1 };
    let bytes = unsafe { std::slice::from_raw_parts(data, size) };
    match session.inner.decode(bytes, dmabuf, ordinal) {
        Ok(Some(picture)) => {
            if !out_fence.is_null() {
                unsafe {
                    *out_fence = picture.fence;
                }
            }
            1
        }
        Ok(None) => 0,
        Err(_) => -2,
    }
}

/// The driver's DPB grant, for the component's delay tuning.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn droidloom_video_dpb_slots(session: *mut NvVideoSession) -> u32 {
    unsafe { session.as_ref() }.map(|session| session.inner.dpb_slots()).unwrap_or(0)
}

/// Closes the session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn droidloom_video_close(session: *mut NvVideoSession) {
    if !session.is_null() {
        unsafe {
            drop(Box::from_raw(session));
        }
    }
}
