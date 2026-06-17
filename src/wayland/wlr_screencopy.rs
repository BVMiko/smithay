//! Wlr screencopy protocol.
//!
//! This module implements the `wlr-screencopy-unstable-v1` protocol, which allows
//! clients to capture the content of an output into client-provided buffers.
//!
//! ```no_run
//! use std::time::Duration;
//!
//! use smithay::{
//!     output::Output,
//!     reexports::wayland_server::protocol::wl_shm,
//!     utils::{Buffer as BufferCoords, Logical, Rectangle, Size},
//!     wayland::wlr_screencopy::{
//!         WlrScreencopyBufferConstraints, WlrScreencopyFrame, WlrScreencopyFrameFlags,
//!         WlrScreencopyHandler, WlrScreencopyState,
//!     },
//! };
//!
//! pub struct State {
//!     screencopy: WlrScreencopyState,
//! }
//!
//! impl WlrScreencopyHandler for State {
//!     fn screencopy_state(&mut self) -> &mut WlrScreencopyState {
//!         &mut self.screencopy
//!     }
//!
//!     fn capture_output_constraints(
//!         &mut self,
//!         output: &Output,
//!         region: Option<Rectangle<i32, Logical>>,
//!         overlay_cursor: bool,
//!     ) -> Option<WlrScreencopyBufferConstraints> {
//!         # let _ = (output, region, overlay_cursor);
//!         Some(WlrScreencopyBufferConstraints::shm(
//!             Size::<i32, BufferCoords>::from((1920, 1080)),
//!             wl_shm::Format::Xrgb8888,
//!             1920 * 4,
//!         ))
//!     }
//!
//!     fn frame(&mut self, frame: WlrScreencopyFrame) {
//!         // Render the requested output or region into frame.buffer(), then
//!         // report the result.
//!         frame.success(WlrScreencopyFrameFlags::empty(), None, Duration::ZERO);
//!     }
//! }
//!
//! # let mut display = wayland_server::Display::<State>::new().unwrap();
//! # let display_handle = display.handle();
//! let state = State {
//!     screencopy: WlrScreencopyState::new::<State>(&display_handle),
//! };
//! # let _ = state;
//!
//! smithay::delegate_dispatch2!(State);
//! ```

use std::{
    ops,
    sync::{Arc, Mutex},
    time::Duration,
};

use wayland_protocols_wlr::screencopy::v1::server::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::{ClientId, GlobalId},
    protocol::{wl_buffer::WlBuffer, wl_output::WlOutput, wl_shm},
};

use crate::{
    backend::{
        allocator::{Buffer as AllocBuffer, Fourcc},
        renderer::{BufferType, buffer_type},
    },
    output::Output,
    utils::{Buffer as BufferCoords, IsAlive, Logical, Rectangle, Size, user_data::UserDataMap},
    wayland::{Dispatch2, GlobalData, GlobalDispatch2, dmabuf::get_dmabuf, shm::with_buffer_contents},
};

/// Flags describing the copied frame.
pub use zwlr_screencopy_frame_v1::Flags as WlrScreencopyFrameFlags;

/// SHM buffer layout supported by a screencopy frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WlrScreencopyShmFormat {
    /// SHM pixel format.
    pub format: wl_shm::Format,
    /// Required stride in bytes.
    pub stride: i32,
}

/// Buffer constraints for a screencopy request.
#[derive(Debug, Clone)]
pub struct WlrScreencopyBufferConstraints {
    /// Required buffer size in pixels.
    pub size: Size<i32, BufferCoords>,
    /// Supported SHM buffer layouts.
    pub shm: Vec<WlrScreencopyShmFormat>,
    /// Supported DMA-BUF formats.
    pub dma: Vec<Fourcc>,
}

impl WlrScreencopyBufferConstraints {
    /// Create constraints for one SHM format.
    pub fn shm(size: Size<i32, BufferCoords>, format: wl_shm::Format, stride: i32) -> Self {
        Self {
            size,
            shm: vec![WlrScreencopyShmFormat { format, stride }],
            dma: Vec::new(),
        }
    }

    fn has_valid_size(&self) -> bool {
        self.size.w > 0 && self.size.h > 0
    }

    fn has_compatible_buffer_type(&self, version: u32) -> bool {
        let has_shm = self.shm.iter().any(|format| format.stride > 0);
        let has_dma = version >= 3 && !self.dma.is_empty();
        self.has_valid_size() && (has_shm || has_dma)
    }
}

#[derive(Debug)]
struct FrameInner {
    constraints: Option<WlrScreencopyBufferConstraints>,
    output: Option<Output>,
    region: Option<Rectangle<i32, Logical>>,
    overlay_cursor: bool,
    buffer: Option<WlBuffer>,
    copy_requested: bool,
    copy_with_damage: bool,
    failed: bool,
    ready: bool,
}

impl FrameInner {
    fn fail(&mut self, frame: &ZwlrScreencopyFrameV1) {
        if self.ready || self.failed {
            return;
        }

        self.failed = true;
        frame.failed();
    }
}

/// A cloneable reference to a wlr screencopy frame.
#[derive(Debug, Clone)]
pub struct WlrScreencopyFrameRef {
    obj: ZwlrScreencopyFrameV1,
    inner: Arc<(Mutex<FrameInner>, UserDataMap)>,
}

impl PartialEq for WlrScreencopyFrameRef {
    fn eq(&self, other: &Self) -> bool {
        self.obj == other.obj
    }
}

impl IsAlive for WlrScreencopyFrameRef {
    fn alive(&self) -> bool {
        self.obj.is_alive() && {
            let inner = self.inner.0.lock().unwrap();
            !inner.ready && !inner.failed
        }
    }
}

impl WlrScreencopyFrameRef {
    /// Access the [`UserDataMap`] associated with this frame.
    pub fn user_data(&self) -> &UserDataMap {
        &self.inner.1
    }

    /// The output requested for capture.
    pub fn output(&self) -> Option<Output> {
        self.inner.0.lock().unwrap().output.clone()
    }

    /// The requested output-local logical region, or `None` for the whole output.
    pub fn region(&self) -> Option<Rectangle<i32, Logical>> {
        self.inner.0.lock().unwrap().region
    }

    /// Whether the client requested composited cursor content.
    pub fn overlay_cursor(&self) -> bool {
        self.inner.0.lock().unwrap().overlay_cursor
    }

    /// Whether the client requested damage-aware copying.
    pub fn copy_with_damage(&self) -> bool {
        self.inner.0.lock().unwrap().copy_with_damage
    }

    /// Get the buffer attached to this frame.
    ///
    /// # Panics
    ///
    /// Panics if the client has not requested copying yet. Compositors should
    /// only call this from [`WlrScreencopyHandler::frame`].
    pub fn buffer(&self) -> WlBuffer {
        self.inner
            .0
            .lock()
            .unwrap()
            .buffer
            .clone()
            .expect("no buffer attached")
    }

    /// Buffer constraints announced for this frame.
    pub fn constraints(&self) -> Option<WlrScreencopyBufferConstraints> {
        self.inner.0.lock().unwrap().constraints.clone()
    }
}

/// An owned wlr screencopy frame.
///
/// The compositor receives this via [`WlrScreencopyHandler::frame`] and must
/// call either [`Self::success`] or [`Self::fail`] after handling the copy.
#[derive(Debug)]
pub struct WlrScreencopyFrame {
    frame: WlrScreencopyFrameRef,
    completed: bool,
}

impl ops::Deref for WlrScreencopyFrame {
    type Target = WlrScreencopyFrameRef;

    fn deref(&self) -> &Self::Target {
        &self.frame
    }
}

impl WlrScreencopyFrame {
    /// Signal successful frame capture.
    ///
    /// If this frame was requested with `copy_with_damage`, `damage` is sent to
    /// the client before `ready`. Passing `None` reports the entire buffer as
    /// damaged.
    pub fn success(
        mut self,
        flags: WlrScreencopyFrameFlags,
        damage: impl Into<Option<Vec<Rectangle<i32, BufferCoords>>>>,
        presented: impl Into<Duration>,
    ) {
        let (copy_with_damage, size) = {
            let mut inner = self.frame.inner.0.lock().unwrap();
            if !inner.copy_requested || inner.failed || inner.ready {
                return;
            }

            inner.ready = true;
            let size = inner
                .constraints
                .as_ref()
                .map(|constraints| constraints.size)
                .unwrap_or_default();
            (inner.copy_with_damage, size)
        };

        self.frame.obj.flags(flags);

        if copy_with_damage && self.frame.obj.version() >= 2 {
            let damage = damage.into().unwrap_or_else(|| vec![Rectangle::from_size(size)]);
            for damage in damage {
                if damage.loc.x < 0 || damage.loc.y < 0 || damage.size.w <= 0 || damage.size.h <= 0 {
                    continue;
                }

                self.frame.obj.damage(
                    damage.loc.x as u32,
                    damage.loc.y as u32,
                    damage.size.w as u32,
                    damage.size.h as u32,
                );
            }
        }

        let time = presented.into();
        let tv_sec_hi = (time.as_secs() >> 32) as u32;
        let tv_sec_lo = (time.as_secs() & 0xFFFFFFFF) as u32;
        let tv_nsec = time.subsec_nanos();

        self.frame.obj.ready(tv_sec_hi, tv_sec_lo, tv_nsec);
        self.completed = true;
    }

    /// Signal failed frame capture.
    pub fn fail(mut self) {
        self.frame.inner.0.lock().unwrap().fail(&self.frame.obj);
        self.completed = true;
    }
}

impl Drop for WlrScreencopyFrame {
    fn drop(&mut self) {
        if !self.completed {
            self.frame.inner.0.lock().unwrap().fail(&self.frame.obj);
        }
    }
}

/// Handler for the wlr screencopy protocol.
pub trait WlrScreencopyHandler:
    GlobalDispatch<ZwlrScreencopyManagerV1, WlrScreencopyGlobalData>
    + Dispatch<ZwlrScreencopyManagerV1, GlobalData>
    + Dispatch<ZwlrScreencopyFrameV1, WlrScreencopyFrameData>
    + 'static
{
    /// [`WlrScreencopyState`] getter.
    fn screencopy_state(&mut self) -> &mut WlrScreencopyState;

    /// Return buffer constraints for capturing the requested output.
    ///
    /// `region` is in output-local logical coordinates. Return `None` to reject
    /// the capture request.
    fn capture_output_constraints(
        &mut self,
        output: &Output,
        region: Option<Rectangle<i32, Logical>>,
        overlay_cursor: bool,
    ) -> Option<WlrScreencopyBufferConstraints>;

    /// Called when a client has attached a valid buffer and requested capture.
    fn frame(&mut self, frame: WlrScreencopyFrame);

    /// Called when a frame is destroyed before being completed.
    ///
    /// Note: Destruction might happen explicitly by the client, or implicitly
    /// when the client quits. In case of implicit destruction the order is
    /// undefined.
    fn frame_aborted(&mut self, frame: WlrScreencopyFrameRef) {
        let _ = frame;
    }
}

/// State of the wlr screencopy protocol.
#[derive(Debug)]
pub struct WlrScreencopyState {
    global: GlobalId,
}

impl WlrScreencopyState {
    /// Register a new [`ZwlrScreencopyManagerV1`] global.
    pub fn new<D>(display: &DisplayHandle) -> Self
    where
        D: WlrScreencopyHandler,
    {
        Self::new_with_filter::<D, _>(display, |_| true)
    }

    /// Register a new [`ZwlrScreencopyManagerV1`] global with a client filter.
    pub fn new_with_filter<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: WlrScreencopyHandler,
        F: Fn(&Client) -> bool + Send + Sync + 'static,
    {
        let global = display.create_global::<D, ZwlrScreencopyManagerV1, _>(
            3,
            WlrScreencopyGlobalData {
                filter: Box::new(filter),
            },
        );

        Self { global }
    }

    /// [`ZwlrScreencopyManagerV1`] [`GlobalId`] getter.
    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }
}

/// Global data for [`ZwlrScreencopyManagerV1`].
pub struct WlrScreencopyGlobalData {
    filter: Box<dyn Fn(&Client) -> bool + Send + Sync>,
}

impl std::fmt::Debug for WlrScreencopyGlobalData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WlrScreencopyGlobalData").finish_non_exhaustive()
    }
}

/// User data for [`ZwlrScreencopyFrameV1`] protocol resources.
#[doc(hidden)]
#[derive(Debug)]
pub struct WlrScreencopyFrameData {
    inner: Arc<(Mutex<FrameInner>, UserDataMap)>,
}

impl<D> GlobalDispatch2<ZwlrScreencopyManagerV1, D> for WlrScreencopyGlobalData
where
    D: WlrScreencopyHandler,
{
    fn bind(
        &self,
        _state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, GlobalData);
    }

    fn can_view(&self, client: &Client) -> bool {
        (self.filter)(client)
    }
}

impl<D> Dispatch2<ZwlrScreencopyManagerV1, D> for GlobalData
where
    D: WlrScreencopyHandler,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        _resource: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_screencopy_manager_v1::Request::CaptureOutput {
                frame,
                overlay_cursor,
                output,
            } => {
                init_frame(state, data_init, frame, overlay_cursor != 0, output, None, false);
            }
            zwlr_screencopy_manager_v1::Request::CaptureOutputRegion {
                frame,
                overlay_cursor,
                output,
                x,
                y,
                width,
                height,
            } => {
                let invalid_region = width <= 0 || height <= 0;
                let region = (!invalid_region).then(|| Rectangle::new((x, y).into(), (width, height).into()));
                init_frame(
                    state,
                    data_init,
                    frame,
                    overlay_cursor != 0,
                    output,
                    region,
                    invalid_region,
                );
            }
            zwlr_screencopy_manager_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

impl<D> Dispatch2<ZwlrScreencopyFrameV1, D> for WlrScreencopyFrameData
where
    D: WlrScreencopyHandler,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_screencopy_frame_v1::Request::Copy { buffer } => {
                copy_frame(state, resource, &self.inner, buffer, false);
            }
            zwlr_screencopy_frame_v1::Request::CopyWithDamage { buffer } => {
                copy_frame(state, resource, &self.inner, buffer, true);
            }
            zwlr_screencopy_frame_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(&self, state: &mut D, _client: ClientId, resource: &ZwlrScreencopyFrameV1) {
        let frame = WlrScreencopyFrameRef {
            obj: resource.clone(),
            inner: self.inner.clone(),
        };

        if frame.alive() {
            state.frame_aborted(frame);
        }
    }
}

fn init_frame<D>(
    state: &mut D,
    data_init: &mut DataInit<'_, D>,
    frame: New<ZwlrScreencopyFrameV1>,
    overlay_cursor: bool,
    output: WlOutput,
    region: Option<Rectangle<i32, Logical>>,
    invalid_request: bool,
) where
    D: WlrScreencopyHandler,
{
    let output = Output::from_resource(&output);
    let constraints = (!invalid_request)
        .then(|| {
            output
                .as_ref()
                .and_then(|output| state.capture_output_constraints(output, region, overlay_cursor))
        })
        .flatten();

    let inner = Arc::new((
        Mutex::new(FrameInner {
            constraints: constraints.clone(),
            output,
            region,
            overlay_cursor,
            buffer: None,
            copy_requested: false,
            copy_with_damage: false,
            failed: false,
            ready: false,
        }),
        UserDataMap::new(),
    ));

    let obj = data_init.init(frame, WlrScreencopyFrameData { inner: inner.clone() });

    let Some(constraints) = constraints else {
        inner.0.lock().unwrap().fail(&obj);
        return;
    };

    if !constraints.has_compatible_buffer_type(obj.version()) {
        inner.0.lock().unwrap().fail(&obj);
        return;
    }

    send_buffer_constraints(&obj, &constraints);
}

fn send_buffer_constraints(frame: &ZwlrScreencopyFrameV1, constraints: &WlrScreencopyBufferConstraints) {
    let width = constraints.size.w as u32;
    let height = constraints.size.h as u32;

    for shm in constraints.shm.iter().filter(|shm| shm.stride > 0) {
        frame.buffer(shm.format, width, height, shm.stride as u32);
    }

    if frame.version() >= 3 {
        for format in &constraints.dma {
            frame.linux_dmabuf(*format as u32, width, height);
        }

        frame.buffer_done();
    }
}

fn copy_frame<D>(
    state: &mut D,
    resource: &ZwlrScreencopyFrameV1,
    inner: &Arc<(Mutex<FrameInner>, UserDataMap)>,
    buffer: WlBuffer,
    copy_with_damage: bool,
) where
    D: WlrScreencopyHandler,
{
    {
        let mut inner = inner.0.lock().unwrap();
        if inner.copy_requested {
            resource.post_error(
                zwlr_screencopy_frame_v1::Error::AlreadyUsed,
                "frame has already been used",
            );
            return;
        }

        if inner.failed || inner.ready {
            return;
        }

        let Some(constraints) = inner.constraints.as_ref() else {
            inner.fail(resource);
            return;
        };

        if !validate_buffer(&buffer, constraints) {
            resource.post_error(
                zwlr_screencopy_frame_v1::Error::InvalidBuffer,
                "buffer does not match screencopy constraints",
            );
            return;
        }

        inner.buffer = Some(buffer);
        inner.copy_requested = true;
        inner.copy_with_damage = copy_with_damage;
    }

    let frame = WlrScreencopyFrame {
        frame: WlrScreencopyFrameRef {
            obj: resource.clone(),
            inner: inner.clone(),
        },
        completed: false,
    };
    state.frame(frame);
}

fn validate_buffer(buffer: &WlBuffer, constraints: &WlrScreencopyBufferConstraints) -> bool {
    match buffer_type(buffer) {
        Some(BufferType::Shm) => validate_shm_buffer(buffer, constraints),
        Some(BufferType::Dma) => validate_dmabuf(buffer, constraints),
        _ => false,
    }
}

fn validate_shm_buffer(buffer: &WlBuffer, constraints: &WlrScreencopyBufferConstraints) -> bool {
    with_buffer_contents(buffer, |_, _, data| {
        data.width == constraints.size.w
            && data.height == constraints.size.h
            && constraints
                .shm
                .iter()
                .any(|shm| shm.format == data.format && shm.stride == data.stride)
    })
    .unwrap_or(false)
}

fn validate_dmabuf(buffer: &WlBuffer, constraints: &WlrScreencopyBufferConstraints) -> bool {
    let Ok(dmabuf) = get_dmabuf(buffer) else {
        return false;
    };

    dmabuf.size() == constraints.size && constraints.dma.contains(&dmabuf.format().code)
}
