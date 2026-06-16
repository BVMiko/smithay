//! Foreign toplevel management.
//!
//! This protocol lets clients list and request actions on compositor-managed
//! toplevels, such as taskbars and docks.
//!
//! ```no_run
//! use smithay::wayland::foreign_toplevel_management::{
//!     ForeignToplevelManagementHandler, ForeignToplevelManagerState, ForeignToplevelState,
//! };
//!
//! pub struct State {
//!     foreign_toplevel_manager: ForeignToplevelManagerState,
//! }
//!
//! smithay::delegate_dispatch2!(State);
//!
//! impl ForeignToplevelManagementHandler for State {
//!     fn foreign_toplevel_manager_state(&mut self) -> &mut ForeignToplevelManagerState {
//!         &mut self.foreign_toplevel_manager
//!     }
//! }
//!
//! # let mut display = wayland_server::Display::<State>::new().unwrap();
//! # let display_handle = display.handle();
//! let mut state = State {
//!     foreign_toplevel_manager: ForeignToplevelManagerState::new::<State>(&display_handle),
//! };
//!
//! let handle = state.foreign_toplevel_manager.new_toplevel_with_details::<State>(
//!     "Window Title",
//!     "com.example",
//!     vec![ForeignToplevelState::Activated],
//!     Vec::new(),
//!     None,
//! );
//!
//! // Handle can be used to update title, app id, state, outputs, and parent.
//! handle.send_title("Window title has changed");
//! handle.send_state(vec![ForeignToplevelState::Maximized]);
//! handle.send_done();
//!
//! // Handle can also be used to close the window, after this call the handle will become inert,
//! // and the handle will no longer be announced to clients.
//! handle.send_closed();
//! ```

use std::sync::{Arc, Mutex, Weak as StdWeak};

use wayland_protocols_wlr::foreign_toplevel::v1::server::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak,
    backend::{ClientId, GlobalId},
    protocol::{wl_output::WlOutput, wl_seat::WlSeat, wl_surface::WlSurface},
};

use crate::{
    utils::user_data::UserDataMap,
    wayland::{Dispatch2, GlobalData, GlobalDispatch2},
};

/// Handler for the wlr foreign toplevel management protocol.
pub trait ForeignToplevelManagementHandler: 'static {
    /// [`ForeignToplevelManagerState`] getter.
    fn foreign_toplevel_manager_state(&mut self) -> &mut ForeignToplevelManagerState;

    /// A client requested the toplevel be maximized.
    fn maximize_request(&mut self, _toplevel: ForeignToplevelHandle) {}

    /// A client requested the toplevel be unmaximized.
    fn unmaximize_request(&mut self, _toplevel: ForeignToplevelHandle) {}

    /// A client requested the toplevel be minimized.
    fn minimize_request(&mut self, _toplevel: ForeignToplevelHandle) {}

    /// A client requested the toplevel be unminimized.
    fn unminimize_request(&mut self, _toplevel: ForeignToplevelHandle) {}

    /// A client requested the toplevel be activated for the given seat.
    fn activate_request(&mut self, _toplevel: ForeignToplevelHandle, _seat: WlSeat) {}

    /// A client requested the toplevel be closed.
    fn close_request(&mut self, _toplevel: ForeignToplevelHandle) {}

    /// A client requested the toplevel be fullscreened, optionally on the given output.
    fn fullscreen_request(&mut self, _toplevel: ForeignToplevelHandle, _output: Option<WlOutput>) {}

    /// A client requested the toplevel be unfullscreened.
    fn unfullscreen_request(&mut self, _toplevel: ForeignToplevelHandle) {}

    /// A client updated the rectangle representing this toplevel.
    fn rectangle_request(
        &mut self,
        _toplevel: ForeignToplevelHandle,
        _surface: WlSurface,
        _rectangle: ForeignToplevelRectangle,
    ) {
    }
}

/// The rectangle a foreign toplevel client uses to represent a toplevel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForeignToplevelRectangle {
    /// X coordinate in surface-local coordinates.
    pub x: i32,
    /// Y coordinate in surface-local coordinates.
    pub y: i32,
    /// Width in surface-local coordinates.
    pub width: i32,
    /// Height in surface-local coordinates.
    pub height: i32,
}

/// State of a foreign toplevel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ForeignToplevelState {
    /// The toplevel is maximized.
    Maximized,
    /// The toplevel is minimized.
    Minimized,
    /// The toplevel is active.
    Activated,
    /// The toplevel is fullscreen.
    Fullscreen,
}

impl ForeignToplevelState {
    fn as_protocol_state(self) -> zwlr_foreign_toplevel_handle_v1::State {
        match self {
            ForeignToplevelState::Maximized => zwlr_foreign_toplevel_handle_v1::State::Maximized,
            ForeignToplevelState::Minimized => zwlr_foreign_toplevel_handle_v1::State::Minimized,
            ForeignToplevelState::Activated => zwlr_foreign_toplevel_handle_v1::State::Activated,
            ForeignToplevelState::Fullscreen => zwlr_foreign_toplevel_handle_v1::State::Fullscreen,
        }
    }

    fn since(self) -> u32 {
        match self {
            ForeignToplevelState::Fullscreen => 2,
            _ => 1,
        }
    }
}

#[derive(Debug)]
struct ForeignToplevelHandleInner {
    title: String,
    app_id: String,
    outputs: Vec<WlOutput>,
    states: Vec<ForeignToplevelState>,
    parent: Option<ForeignToplevelWeakHandle>,
    instances: Vec<Weak<ZwlrForeignToplevelHandleV1>>,
    closed: bool,
}

impl ForeignToplevelHandleInner {
    fn send_closed(&mut self) {
        if self.closed {
            return;
        }

        self.closed = true;
        for toplevel in self.instances.drain(..) {
            if let Ok(toplevel) = toplevel.upgrade() {
                toplevel.closed();
            }
        }
    }
}

impl Drop for ForeignToplevelHandleInner {
    fn drop(&mut self) {
        self.send_closed()
    }
}

/// Weak version of [`ForeignToplevelHandle`].
#[derive(Debug, Clone)]
pub struct ForeignToplevelWeakHandle {
    inner: StdWeak<(Mutex<ForeignToplevelHandleInner>, UserDataMap)>,
}

impl ForeignToplevelWeakHandle {
    /// Upgrade weak [`ForeignToplevelWeakHandle`] to strong [`ForeignToplevelHandle`].
    pub fn upgrade(&self) -> Option<ForeignToplevelHandle> {
        Some(ForeignToplevelHandle {
            inner: self.inner.upgrade()?,
        })
    }
}

/// A handle to a foreign toplevel.
#[derive(Debug, Clone)]
pub struct ForeignToplevelHandle {
    inner: Arc<(Mutex<ForeignToplevelHandleInner>, UserDataMap)>,
}

impl ForeignToplevelHandle {
    fn new(
        title: String,
        app_id: String,
        outputs: Vec<WlOutput>,
        states: Vec<ForeignToplevelState>,
        parent: Option<ForeignToplevelHandle>,
        instances: Vec<Weak<ZwlrForeignToplevelHandleV1>>,
    ) -> Self {
        Self {
            inner: Arc::new((
                Mutex::new(ForeignToplevelHandleInner {
                    title,
                    app_id,
                    outputs,
                    states,
                    parent: parent.map(|parent| parent.downgrade()),
                    instances,
                    closed: false,
                }),
                UserDataMap::new(),
            )),
        }
    }

    /// Downgrade strong [`ForeignToplevelHandle`] to weak [`ForeignToplevelWeakHandle`].
    pub fn downgrade(&self) -> ForeignToplevelWeakHandle {
        ForeignToplevelWeakHandle {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Attempt to retrieve [`ForeignToplevelHandle`] from an existing resource.
    pub fn from_resource(resource: &ZwlrForeignToplevelHandleV1) -> Option<Self> {
        resource.data::<Self>().cloned()
    }

    /// Retrieve [`ZwlrForeignToplevelHandleV1`] instances for this handle.
    pub fn resources(&self) -> Vec<ZwlrForeignToplevelHandleV1> {
        let inner = self.inner.0.lock().unwrap();
        inner
            .instances
            .iter()
            .filter_map(|weak| weak.upgrade().ok())
            .collect()
    }

    /// Retrieve [`ZwlrForeignToplevelHandleV1`] instances for this handle of a given [`Client`].
    pub fn resources_for_client(&self, client: &Client) -> Vec<ZwlrForeignToplevelHandleV1> {
        self.resources()
            .into_iter()
            .filter(|handle| handle.client().as_ref().is_some_and(|c| c == client))
            .collect()
    }

    /// Access the [`UserDataMap`] associated with this [`ForeignToplevelHandle`].
    pub fn user_data(&self) -> &UserDataMap {
        &self.inner.1
    }

    /// The title of the toplevel has changed.
    ///
    /// [`Self::send_done`] has to be called to finalize the update.
    pub fn send_title(&self, title: &str) {
        let mut inner = self.inner.0.lock().unwrap();
        if inner.closed || inner.title == title {
            return;
        }

        inner.title = title.to_string();

        for toplevel in inner.instances.iter() {
            if let Ok(toplevel) = toplevel.upgrade() {
                toplevel.title(title.to_string());
            }
        }
    }

    /// The app id of the toplevel has changed.
    ///
    /// [`Self::send_done`] has to be called to finalize the update.
    pub fn send_app_id(&self, app_id: &str) {
        let mut inner = self.inner.0.lock().unwrap();
        if inner.closed || inner.app_id == app_id {
            return;
        }

        inner.app_id = app_id.to_string();

        for toplevel in inner.instances.iter() {
            if let Ok(toplevel) = toplevel.upgrade() {
                toplevel.app_id(app_id.to_string());
            }
        }
    }

    /// The toplevel became visible on an output.
    ///
    /// [`Self::send_done`] has to be called to finalize the update.
    pub fn send_output_enter(&self, output: &WlOutput) {
        let mut inner = self.inner.0.lock().unwrap();
        if inner.closed || inner.outputs.iter().any(|o| o == output) {
            return;
        }

        inner.outputs.push(output.clone());

        for toplevel in inner.instances.iter() {
            if let Ok(toplevel) = toplevel.upgrade() {
                toplevel.output_enter(output);
            }
        }
    }

    /// The toplevel stopped being visible on an output.
    ///
    /// [`Self::send_done`] has to be called to finalize the update.
    pub fn send_output_leave(&self, output: &WlOutput) {
        let mut inner = self.inner.0.lock().unwrap();
        if inner.closed {
            return;
        }

        let Some(pos) = inner.outputs.iter().position(|o| o == output) else {
            return;
        };
        inner.outputs.remove(pos);

        for toplevel in inner.instances.iter() {
            if let Ok(toplevel) = toplevel.upgrade() {
                toplevel.output_leave(output);
            }
        }
    }

    /// The toplevel state has changed.
    ///
    /// [`Self::send_done`] has to be called to finalize the update.
    pub fn send_state(&self, states: Vec<ForeignToplevelState>) {
        let mut inner = self.inner.0.lock().unwrap();
        if inner.closed || inner.states == states {
            return;
        }

        inner.states = states;

        for toplevel in inner.instances.iter() {
            if let Ok(toplevel) = toplevel.upgrade() {
                toplevel.state(serialize_states(&inner.states, toplevel.version()));
            }
        }
    }

    /// The toplevel parent has changed.
    ///
    /// [`Self::send_done`] has to be called to finalize the update.
    pub fn send_parent(&self, parent: Option<ForeignToplevelHandle>) {
        {
            let mut inner = self.inner.0.lock().unwrap();
            if inner.closed {
                return;
            }

            inner.parent = parent.as_ref().map(|parent| parent.downgrade());
        }

        for toplevel in self.resources() {
            if toplevel.version() < 3 {
                continue;
            }

            let parent = parent
                .as_ref()
                .and_then(|parent| parent.resource_for_same_client(&toplevel));
            toplevel.parent(parent.as_ref());
        }
    }

    /// This event should be sent after all changes in the toplevel state have been sent.
    pub fn send_done(&self) {
        let inner = self.inner.0.lock().unwrap();
        if inner.closed {
            return;
        }

        for toplevel in inner.instances.iter() {
            if let Ok(toplevel) = toplevel.upgrade() {
                toplevel.done();
            }
        }
    }

    /// The toplevel has been closed.
    pub fn send_closed(&self) {
        self.inner.0.lock().unwrap().send_closed();
    }

    /// The title of the toplevel.
    pub fn title(&self) -> String {
        self.inner.0.lock().unwrap().title.clone()
    }

    /// The app id of the toplevel.
    pub fn app_id(&self) -> String {
        self.inner.0.lock().unwrap().app_id.clone()
    }

    /// Outputs the toplevel is visible on.
    pub fn outputs(&self) -> Vec<WlOutput> {
        self.inner.0.lock().unwrap().outputs.clone()
    }

    /// States of the toplevel.
    pub fn states(&self) -> Vec<ForeignToplevelState> {
        self.inner.0.lock().unwrap().states.clone()
    }

    /// Parent of the toplevel.
    pub fn parent(&self) -> Option<ForeignToplevelHandle> {
        self.inner
            .0
            .lock()
            .unwrap()
            .parent
            .as_ref()
            .and_then(ForeignToplevelWeakHandle::upgrade)
    }

    /// The toplevel has been closed.
    pub fn is_closed(&self) -> bool {
        self.inner.0.lock().unwrap().closed
    }

    /// Check if two toplevel handles belong to the same toplevel.
    pub fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    fn init_new_instance(&self, toplevel: ZwlrForeignToplevelHandleV1) {
        debug_assert!(
            !self.is_closed(),
            "No handles should ever be created for closed toplevel"
        );

        toplevel.title(self.title());
        toplevel.app_id(self.app_id());

        for output in self.outputs() {
            toplevel.output_enter(&output);
        }

        toplevel.state(serialize_states(&self.states(), toplevel.version()));

        if toplevel.version() >= 3 {
            let parent = self
                .parent()
                .and_then(|parent| parent.resource_for_same_client(&toplevel));
            toplevel.parent(parent.as_ref());
        }

        toplevel.done();

        self.inner.0.lock().unwrap().instances.push(toplevel.downgrade());
    }

    fn remove_instance(&self, instance: &ZwlrForeignToplevelHandleV1) {
        let mut inner = self.inner.0.lock().unwrap();
        if let Some(pos) = inner.instances.iter().position(|i| i == instance) {
            inner.instances.remove(pos);
        }
    }

    fn resource_for_same_client(
        &self,
        resource: &ZwlrForeignToplevelHandleV1,
    ) -> Option<ZwlrForeignToplevelHandleV1> {
        let client = resource.client()?;
        self.resources_for_client(&client).into_iter().next()
    }
}

/// State of the [`ZwlrForeignToplevelManagerV1`] global.
#[derive(Debug)]
pub struct ForeignToplevelManagerState {
    global: GlobalId,
    toplevels: Vec<ForeignToplevelWeakHandle>,
    manager_instances: Vec<ZwlrForeignToplevelManagerV1>,
    dh: DisplayHandle,
}

impl ForeignToplevelManagerState {
    /// Register a new [`ZwlrForeignToplevelManagerV1`] global.
    pub fn new<D>(dh: &DisplayHandle) -> Self
    where
        D: ForeignToplevelManagementHandler
            + GlobalDispatch<ZwlrForeignToplevelManagerV1, ForeignToplevelManagerGlobalData>,
    {
        Self::new_with_filter::<D>(dh, |_| true)
    }

    /// Register a new [`ZwlrForeignToplevelManagerV1`] global with a client filter.
    pub fn new_with_filter<D>(
        dh: &DisplayHandle,
        can_view: impl Fn(&Client) -> bool + Send + Sync + 'static,
    ) -> Self
    where
        D: ForeignToplevelManagementHandler
            + GlobalDispatch<ZwlrForeignToplevelManagerV1, ForeignToplevelManagerGlobalData>,
    {
        let global = dh.create_global::<D, ZwlrForeignToplevelManagerV1, _>(
            3,
            ForeignToplevelManagerGlobalData {
                filter: Box::new(can_view),
            },
        );

        Self {
            global,
            toplevels: Vec::new(),
            manager_instances: Vec::new(),
            dh: dh.clone(),
        }
    }

    /// [`ZwlrForeignToplevelManagerV1`] [`GlobalId`] getter.
    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }

    /// This event is emitted whenever a new toplevel window is created.
    pub fn new_toplevel<D>(
        &mut self,
        title: impl Into<String>,
        app_id: impl Into<String>,
    ) -> ForeignToplevelHandle
    where
        D: ForeignToplevelManagementHandler + Dispatch<ZwlrForeignToplevelHandleV1, ForeignToplevelHandle>,
    {
        self.new_toplevel_with_details::<D>(title, app_id, Vec::new(), Vec::new(), None)
    }

    /// This event is emitted whenever a new toplevel window is created with initial details.
    pub fn new_toplevel_with_details<D>(
        &mut self,
        title: impl Into<String>,
        app_id: impl Into<String>,
        states: Vec<ForeignToplevelState>,
        outputs: Vec<WlOutput>,
        parent: Option<ForeignToplevelHandle>,
    ) -> ForeignToplevelHandle
    where
        D: ForeignToplevelManagementHandler + Dispatch<ZwlrForeignToplevelHandleV1, ForeignToplevelHandle>,
    {
        let handle = ForeignToplevelHandle::new(
            title.into(),
            app_id.into(),
            outputs,
            states,
            parent,
            Vec::with_capacity(self.manager_instances.len()),
        );

        for instance in &self.manager_instances {
            let Ok(client) = self.dh.get_client(instance.id()) else {
                continue;
            };

            let Ok(toplevel) = client.create_resource::<ZwlrForeignToplevelHandleV1, _, D>(
                &self.dh,
                instance.version(),
                handle.clone(),
            ) else {
                continue;
            };

            instance.toplevel(&toplevel);
            handle.init_new_instance(toplevel);
        }

        self.toplevels.push(handle.downgrade());

        handle
    }

    /// Remove the toplevel, and send the closed event if needed.
    ///
    /// The compositor owns strong [`ForeignToplevelHandle`]s: call this method
    /// or drop the last handle to close a toplevel; [`Self::cleanup_closed_handles`]
    /// only removes stale weak entries.
    pub fn remove_toplevel(&mut self, handle: &ForeignToplevelHandle) {
        handle.send_closed();

        if let Some(pos) = self
            .toplevels
            .iter()
            .filter_map(|h| h.upgrade())
            .position(|h| Arc::ptr_eq(&h.inner, &handle.inner))
        {
            self.toplevels.remove(pos);
        }
    }

    /// Auto-cleanup closed handles.
    pub fn cleanup_closed_handles(&mut self) {
        self.toplevels.retain(|handle| {
            let Some(handle) = handle.upgrade() else {
                return false;
            };
            !handle.is_closed()
        });
    }
}

/// Global data for [`ZwlrForeignToplevelManagerV1`].
pub struct ForeignToplevelManagerGlobalData {
    filter: Box<dyn Fn(&Client) -> bool + Send + Sync>,
}

impl std::fmt::Debug for ForeignToplevelManagerGlobalData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForeignToplevelManagerGlobalData")
            .finish_non_exhaustive()
    }
}

impl<D> GlobalDispatch2<ZwlrForeignToplevelManagerV1, D> for ForeignToplevelManagerGlobalData
where
    D: ForeignToplevelManagementHandler
        + Dispatch<ZwlrForeignToplevelManagerV1, GlobalData>
        + Dispatch<ZwlrForeignToplevelHandleV1, ForeignToplevelHandle>,
{
    fn bind(
        &self,
        state: &mut D,
        dh: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, GlobalData);
        let state = state.foreign_toplevel_manager_state();

        state.toplevels.retain(|handle| {
            let Some(handle) = handle.upgrade() else {
                return false;
            };

            if handle.is_closed() {
                return false;
            }

            if let Ok(toplevel) = client.create_resource::<ZwlrForeignToplevelHandleV1, _, D>(
                dh,
                manager.version(),
                handle.clone(),
            ) {
                manager.toplevel(&toplevel);
                handle.init_new_instance(toplevel);
            }

            true
        });

        state.manager_instances.push(manager);
    }

    fn can_view(&self, client: &Client) -> bool {
        (self.filter)(client)
    }
}

impl<D> Dispatch2<ZwlrForeignToplevelManagerV1, D> for GlobalData
where
    D: ForeignToplevelManagementHandler,
{
    fn request(
        &self,
        state: &mut D,
        client: &Client,
        manager: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_foreign_toplevel_manager_v1::Request::Stop => {
                self.destroyed(state, client.id(), manager);
                manager.finished();
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(&self, state: &mut D, _client: ClientId, resource: &ZwlrForeignToplevelManagerV1) {
        state
            .foreign_toplevel_manager_state()
            .manager_instances
            .retain(|manager| manager != resource);
    }
}

impl<D> Dispatch2<ZwlrForeignToplevelHandleV1, D> for ForeignToplevelHandle
where
    D: ForeignToplevelManagementHandler,
{
    fn request(
        &self,
        state: &mut D,
        _client: &Client,
        resource: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        if self.is_closed() {
            return;
        }

        match request {
            zwlr_foreign_toplevel_handle_v1::Request::Destroy => {}
            zwlr_foreign_toplevel_handle_v1::Request::SetMaximized => {
                state.maximize_request(self.clone());
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMaximized => {
                state.unmaximize_request(self.clone());
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetMinimized => {
                state.minimize_request(self.clone());
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetMinimized => {
                state.unminimize_request(self.clone());
            }
            zwlr_foreign_toplevel_handle_v1::Request::Activate { seat } => {
                state.activate_request(self.clone(), seat);
            }
            zwlr_foreign_toplevel_handle_v1::Request::Close => {
                state.close_request(self.clone());
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetRectangle {
                surface,
                x,
                y,
                width,
                height,
            } => {
                if width < 0 || height < 0 || (width == 0) != (height == 0) {
                    resource.post_error(
                        zwlr_foreign_toplevel_handle_v1::Error::InvalidRectangle,
                        "invalid rectangle dimensions",
                    );
                    return;
                }

                state.rectangle_request(
                    self.clone(),
                    surface,
                    ForeignToplevelRectangle { x, y, width, height },
                );
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetFullscreen { output } => {
                state.fullscreen_request(self.clone(), output);
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetFullscreen => {
                state.unfullscreen_request(self.clone());
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(&self, _state: &mut D, _client: ClientId, resource: &ZwlrForeignToplevelHandleV1) {
        self.remove_instance(resource);
    }
}

fn serialize_states(states: &[ForeignToplevelState], version: u32) -> Vec<u8> {
    states
        .iter()
        .filter(|state| state.since() <= version)
        .flat_map(|state| u32::from(state.as_protocol_state()).to_ne_bytes())
        .collect()
}
