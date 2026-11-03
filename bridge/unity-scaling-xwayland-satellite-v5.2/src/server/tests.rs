use super::{InnerServerState, NoConnection, ServerState, WindowDims, selection::Clipboard};
use crate::server::selection::{Primary, SelectionType};
use crate::xstate::{SetState, WinSize, WmName};
use crate::{XConnection, timespec_from_millis};
use rustix::event::{PollFd, PollFlags, poll};
use std::collections::HashMap;
use std::io::Write;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use testwl::{SendDataForMimeFn, SurfaceRole};
use wayland_client::{
    Connection, Proxy, WEnum,
    backend::{Backend, ObjectData, ObjectId, WaylandError, protocol::Message},
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_display::WlDisplay,
        wl_keyboard::WlKeyboard,
        wl_output::{self, WlOutput},
        wl_pointer::WlPointer,
        wl_registry::WlRegistry,
        wl_seat::{self, WlSeat},
        wl_shm::{Format, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
        wl_touch::{self, WlTouch},
    },
};
use wayland_protocols::wp::linux_drm_syncobj::v1::client::wp_linux_drm_syncobj_manager_v1::WpLinuxDrmSyncobjManagerV1;
use wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1;
use wayland_protocols::{
    wp::{
        linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        pointer_constraints::zv1::client::{
            zwp_locked_pointer_v1::ZwpLockedPointerV1,
            zwp_pointer_constraints_v1::{self, ZwpPointerConstraintsV1},
        },
        relative_pointer::zv1::client::{
            zwp_relative_pointer_manager_v1::{self, ZwpRelativePointerManagerV1},
            zwp_relative_pointer_v1::{self, ZwpRelativePointerV1},
        },
        tablet::zv2::client::{
            zwp_tablet_manager_v2::{self, ZwpTabletManagerV2},
            zwp_tablet_pad_group_v2::{
                self, EVT_RING_OPCODE, EVT_STRIP_OPCODE, ZwpTabletPadGroupV2,
            },
            zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2,
            zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2,
            zwp_tablet_pad_v2::{self, EVT_GROUP_OPCODE, ZwpTabletPadV2},
            zwp_tablet_seat_v2::{
                self, EVT_PAD_ADDED_OPCODE, EVT_TABLET_ADDED_OPCODE, EVT_TOOL_ADDED_OPCODE,
                ZwpTabletSeatV2,
            },
            zwp_tablet_tool_v2::{self, ZwpTabletToolV2},
            zwp_tablet_v2::{self, ZwpTabletV2},
        },
    },
    xdg::{
        shell::server::{xdg_positioner, xdg_toplevel},
        xdg_output::zv1::client::{
            zxdg_output_manager_v1::{self, ZxdgOutputManagerV1},
            zxdg_output_v1::{self, ZxdgOutputV1},
        },
    },
    xwayland::shell::v1::client::{
        xwayland_shell_v1::XwaylandShellV1, xwayland_surface_v1::XwaylandSurfaceV1,
    },
};
use wayland_server::{Display, Resource, protocol as s_proto};
use wl_drm::client::wl_drm::WlDrm;
use xcb::x::{self, Window};

use xcb::XidNew;

macro_rules! with_optional {
    (
        $( #[$attr:meta] )?
        struct $name:ident$(<$($lifetimes:lifetime),+>)? {
            $(
                $field:ident: $type:ty
            ),+$(,)?
        } => $name_optional:ident
    ) => {
        $( #[$attr] )?
        struct $name$(<$($lifetimes),+>)? {
            $(
                $field: $type
            ),+
        }

        #[derive(Default)]
        struct $name_optional {
            $(
                $field: Option<$type>
            ),+
        }

        impl From<$name_optional> for $name {
            fn from(opt: $name_optional) -> Self {
                Self {
                    $(
                        $field: opt.$field.expect(concat!("uninitialized field ", stringify!($field)))
                    ),+
                }
            }
        }
    }
}

with_optional! {

struct Compositor {
    compositor: TestObject<WlCompositor>,
    shm: TestObject<WlShm>,
    shell: TestObject<XwaylandShellV1>,
    seat: TestObject<WlSeat>,
    tablet_man: TestObject<ZwpTabletManagerV2>,
    pointer_constraints: TestObject<ZwpPointerConstraintsV1>,
    relative_pointer_man: TestObject<ZwpRelativePointerManagerV1>,
} => CompositorOptional

}

impl Compositor {
    fn create_surface(&self) -> (TestObject<WlBuffer>, TestObject<WlSurface>) {
        let fd = unsafe { BorrowedFd::borrow_raw(0) };
        let pool = TestObject::<WlShmPool>::from_request(
            &self.shm.obj,
            Req::<WlShm>::CreatePool { fd, size: 1024 },
        );
        let buffer = TestObject::<WlBuffer>::from_request(
            &pool.obj,
            Req::<WlShmPool>::CreateBuffer {
                offset: 0,
                width: 10,
                height: 10,
                stride: 1,
                format: WEnum::Value(Format::Xrgb8888A8),
            },
        );
        let surface = TestObject::<WlSurface>::from_request(
            &self.compositor.obj,
            Req::<WlCompositor>::CreateSurface {},
        );
        surface
            .send_request(Req::<WlSurface>::Attach {
                buffer: Some(buffer.obj.clone()),
                x: 0,
                y: 0,
            })
            .unwrap();

        (buffer, surface)
    }
}

#[derive(Debug, Default)]
struct WindowData {
    mapped: bool,
    fullscreen: bool,
    dims: WindowDims,
}
#[derive(Default)]
struct FakeXConnection {
    focused_window: Option<Window>,
    send_take_focus_window: Option<Window>,
    windows: HashMap<Window, WindowData>,
    set_window_dims_counter: usize,
}

impl FakeXConnection {
    #[track_caller]
    fn window_mut(&mut self, window: Window) -> &mut WindowData {
        self.windows
            .get_mut(&window)
            .unwrap_or_else(|| panic!("Unknown window: {window:?}"))
    }

    #[track_caller]
    fn window(&self, window: Window) -> &WindowData {
        self.windows
            .get(&window)
            .unwrap_or_else(|| panic!("Unknown window: {window:?}"))
    }
}

type FakeX11Selection = Vec<testwl::PasteData>;
impl crate::X11Selection for Vec<testwl::PasteData> {
    fn mime_types(&self) -> Vec<&str> {
        self.iter().map(|data| data.mime_type.as_str()).collect()
    }

    fn write_to(
        &self,
        mime: &str,
        mut pipe: smithay_client_toolkit::data_device_manager::WritePipe,
    ) {
        let data = self
            .iter()
            .find(|data| data.mime_type == mime)
            .unwrap_or_else(|| panic!("Couldn't find mime type {mime}"));
        pipe.write_all(&data.data)
            .expect("Couldn't write paste data");
    }
}

impl super::XConnection for FakeXConnection {
    type X11Selection = FakeX11Selection;
    #[track_caller]
    fn close_window(&mut self, window: Window) {
        log::debug!("closing window {window:?}");
        self.window_mut(window).mapped = false;
    }

    #[track_caller]
    fn set_fullscreen(&mut self, window: xcb::x::Window, fullscreen: bool) {
        self.window_mut(window).fullscreen = fullscreen;
    }

    #[track_caller]
    fn set_window_dims(&mut self, window: Window, state: super::PendingSurfaceState) -> bool {
        self.window_mut(window).dims = WindowDims {
            x: state.x as _,
            y: state.y as _,
            width: state.width as _,
            height: state.height as _,
        };
        self.set_window_dims_counter += 1;
        true
    }

    #[track_caller]
    fn focus_window(&mut self, window: Window, _output_name: Option<String>) {
        assert!(
            self.windows.contains_key(&window),
            "Unknown window: {window:?}"
        );
        self.focused_window = window.into();
    }

    #[track_caller]
    fn send_take_focus(&mut self, window: x::Window) {
        assert!(
            self.windows.contains_key(&window),
            "Unknown window: {window:?}"
        );
        self.send_take_focus_window = window.into();
    }

    fn raise_to_top(&mut self, window: Window) {
        assert!(
            self.windows.contains_key(&window),
            "Unknown window: {window:?}"
        );
    }

    fn unmap_window(&mut self, _: x::Window) {
        todo!()
    }
}

type EarlyTestFixture = TestFixture<NoConnection<FakeX11Selection>>;

struct TestFixture<C: XConnection> {
    testwl: testwl::Server,
    satellite: ServerState<C>,
    /// Our connection to satellite - i.e., where Xwayland sends requests to
    xwls_connection: Arc<Connection>,
    /// Satellite's display - must dispatch this for our server state to advance
    xwls_display: Display<InnerServerState<C::X11Selection>>,
    surface_serial: u64,
    registry: TestObject<WlRegistry>,
}

static INIT: std::sync::Once = std::sync::Once::new();

struct PopupBuilder {
    window: Window,
    parent_window: Window,
    parent_surface: testwl::SurfaceId,
    dims: WindowDims,
    scale: f64,
    check_size_and_pos: bool,
}

impl PopupBuilder {
    fn new(window: Window, parent_window: Window, parent_surface: testwl::SurfaceId) -> Self {
        Self {
            window,
            parent_window,
            parent_surface,
            dims: WindowDims {
                x: 0,
                y: 0,
                width: 50,
                height: 50,
            },
            scale: 1.0,
            check_size_and_pos: true,
        }
    }

    fn x(mut self, x: i16) -> Self {
        self.dims.x = x;
        self
    }

    fn y(mut self, y: i16) -> Self {
        self.dims.y = y;
        self
    }

    fn width(mut self, width: u16) -> Self {
        self.dims.width = width;
        self
    }

    fn height(mut self, height: u16) -> Self {
        self.dims.height = height;
        self
    }

    fn check_size_and_pos(mut self, check: bool) -> Self {
        self.check_size_and_pos = check;
        self
    }

    fn scale(mut self, scale: impl Into<f64>) -> Self {
        self.scale = scale.into();
        self
    }
}

trait PreConnectFn: Sized {
    fn call(self, _: &mut testwl::Server) {}
}
impl<F: FnOnce(&mut testwl::Server)> PreConnectFn for F {
    fn call(self, server: &mut testwl::Server) {
        self(server);
    }
}
impl PreConnectFn for () {}

struct SetupOptions<F> {
    pre_connect: Option<F>,
}

impl<F: PreConnectFn> SetupOptions<F> {
    fn pre_connect(pre_connect: F) -> Self {
        Self {
            pre_connect: Some(pre_connect),
        }
    }
}

impl Default for SetupOptions<()> {
    fn default() -> Self {
        Self { pre_connect: None }
    }
}

impl EarlyTestFixture {
    fn new_early_with_options<F: PreConnectFn>(options: SetupOptions<F>) -> Self {
        INIT.call_once(|| {
            pretty_env_logger::env_logger::builder()
                .is_test(true)
                .filter_level(log::LevelFilter::Trace)
                .init()
        });

        let (client_s, server_s) = UnixStream::pair().unwrap();
        let mut testwl = testwl::Server::new(true);
        if let Some(pre_connect) = options.pre_connect {
            pre_connect.call(&mut testwl);
        }
        let display = Display::new().unwrap();
        testwl.connect(server_s);
        // Handle initial globals roundtrip setup requirement
        let thread = std::thread::spawn(move || {
            let mut pollfd = [PollFd::from_borrowed_fd(testwl.poll_fd(), PollFlags::IN)];
            let timeout = timespec_from_millis(1000);
            if poll(&mut pollfd, Some(&timeout)).unwrap() == 0 {
                panic!("Did not get events for testwl!");
            }
            testwl.dispatch();
            testwl
        });

        let (fake_client, xwls_server) = UnixStream::pair().unwrap();
        let satellite = ServerState::new(display.handle(), Some(client_s), xwls_server);
        let testwl = thread.join().unwrap();

        let xwls_connection = Connection::from_socket(fake_client).unwrap();
        let registry = TestObject::<WlRegistry>::from_request(
            &xwls_connection.display(),
            Req::<WlDisplay>::GetRegistry {},
        );

        let mut f = EarlyTestFixture {
            testwl,
            satellite,
            xwls_connection: xwls_connection.into(),
            xwls_display: display,
            surface_serial: 1,
            registry,
        };
        f.run();
        f
    }

    fn upgrade_connection(self, connection: FakeXConnection) -> TestFixture<FakeXConnection> {
        TestFixture {
            testwl: self.testwl,
            satellite: self.satellite.upgrade_connection(connection),
            xwls_connection: self.xwls_connection,
            xwls_display: self.xwls_display,
            surface_serial: self.surface_serial,
            registry: self.registry,
        }
    }
}

impl<C: XConnection> TestFixture<C> {
    fn compositor(&mut self) -> Compositor {
        let mut ret = CompositorOptional::default();
        let events = std::mem::take(&mut *self.registry.data.events.lock().unwrap());
        assert!(!events.is_empty());

        fn bind<T: Proxy + Sync + Send + 'static>(
            registry: &TestObject<WlRegistry>,
            name: u32,
            version: u32,
        ) -> TestObject<T>
        where
            T::Event: Sync + Send + std::fmt::Debug,
        {
            TestObject::from_request(
                &registry.obj,
                Req::<WlRegistry>::Bind {
                    name,
                    id: (T::interface(), version),
                },
            )
        }

        for event in events {
            if let Ev::<WlRegistry>::Global {
                name,
                interface,
                version,
            } = event
            {
                macro_rules! bind {
                    ($field:ident) => {
                        ret.$field = Some(bind(&self.registry, name, version))
                    };
                }

                match interface {
                    x if x == WlCompositor::interface().name => bind!(compositor),
                    x if x == WlShm::interface().name => bind!(shm),
                    x if x == XwaylandShellV1::interface().name => bind!(shell),
                    x if x == WlSeat::interface().name => bind!(seat),
                    x if x == ZwpTabletManagerV2::interface().name => bind!(tablet_man),
                    x if x == ZwpRelativePointerManagerV1::interface().name => {
                        bind!(relative_pointer_man)
                    }
                    x if x == ZwpPointerConstraintsV1::interface().name => {
                        bind!(pointer_constraints)
                    }
                    _ => {}
                }
            }
        }

        // Activate keyboard for focus.
        TestObject::<WlKeyboard>::from_request(
            &ret.seat.as_ref().expect("Seat global missing").obj,
            wl_seat::Request::GetKeyboard {},
        );

        ret.into()
    }

    /// Cascade our requests/events through satellite and testwl
    fn run(&mut self) {
        // Flush our requests to satellite
        self.xwls_connection.flush().unwrap();

        // Have satellite dispatch our requests
        self.xwls_display
            .dispatch_clients(&mut *self.satellite)
            .unwrap();
        self.xwls_display.flush_clients().unwrap();

        // Dispatch any clientside requests
        self.satellite.run();

        // Have testwl dispatch the clientside requests
        self.testwl.dispatch();

        // Handle clientside events
        self.satellite.handle_clientside_events();

        self.testwl.dispatch();

        // Get our events
        let res = self.xwls_connection.prepare_read().unwrap().read();
        if res.is_err()
            && matches!(res, Err(WaylandError::Io(ref e)) if e.kind() != std::io::ErrorKind::WouldBlock)
        {
            panic!("Read failed: {res:?}")
        }
    }

    fn new_output(
        &mut self,
        x: i32,
        y: i32,
    ) -> (
        TestObject<WlOutput>,
        wayland_server::protocol::wl_output::WlOutput,
    ) {
        self.testwl.new_output();
        self.run();
        self.run();
        let mut events = std::mem::take(&mut *self.registry.data.events.lock().unwrap());
        assert_eq!(events.len(), 1);
        let event = events.pop().unwrap();
        let Ev::<WlRegistry>::Global {
            name,
            interface,
            version,
        } = event
        else {
            panic!("Unexpected event: {event:?}");
        };

        assert_eq!(interface, WlOutput::interface().name);
        let output = TestObject::<WlOutput>::from_request(
            &self.registry.obj,
            Req::<WlRegistry>::Bind {
                name,
                id: (WlOutput::interface(), version),
            },
        );
        self.run();
        self.run();
        (output, self.testwl.finalize_output(x, y))
    }

    fn remove_output(&mut self, output_s: wayland_server::protocol::wl_output::WlOutput) {
        self.testwl.remove_output(output_s);
        self.run();
        self.run();
        let mut events = std::mem::take(&mut *self.registry.data.events.lock().unwrap());
        assert_eq!(events.len(), 1);
        let event = events.pop().unwrap();
        let Ev::<WlRegistry>::GlobalRemove { .. } = event else {
            panic!("Unexpected event: {event:?}");
        };
    }
}

impl TestFixture<FakeXConnection> {
    fn new_with_options<F: PreConnectFn>(options: SetupOptions<F>) -> Self {
        EarlyTestFixture::new_early_with_options(options)
            .upgrade_connection(FakeXConnection::default())
    }

    fn new() -> Self {
        Self::new_with_options(SetupOptions::default())
    }

    fn new_pre_connect(pre_connect: impl FnOnce(&mut testwl::Server)) -> Self {
        Self::new_with_options(SetupOptions::pre_connect(pre_connect))
    }

    fn new_with_compositor() -> (Self, Compositor) {
        let mut f = Self::new();
        let compositor = f.compositor();
        (f, compositor)
    }

    fn connection(&self) -> &FakeXConnection {
        &self.satellite.connection
    }

    fn object_data<P>(&self, obj: &P) -> Arc<TestObjectData<P>>
    where
        P: Proxy + Send + Sync + 'static,
        P::Event: Send + Sync + std::fmt::Debug,
    {
        self.xwls_connection
            .get_object_data(obj.id())
            .unwrap()
            .downcast_arc::<TestObjectData<P>>()
            .unwrap()
    }

    fn enable_xdg_output(&mut self) -> TestObject<ZxdgOutputManagerV1> {
        self.testwl.enable_xdg_output_manager();
        self.run();
        self.run();

        let mut events = std::mem::take(&mut *self.registry.data.events.lock().unwrap());
        assert_eq!(
            events.len(),
            1,
            "Unexpected number of global events after enabling xdg output"
        );
        let event = events.pop().unwrap();
        let Ev::<WlRegistry>::Global {
            name,
            interface,
            version,
        } = event
        else {
            panic!("Unexpected event: {event:?}");
        };

        assert_eq!(interface, ZxdgOutputManagerV1::interface().name);
        let man = TestObject::<ZxdgOutputManagerV1>::from_request(
            &self.registry.obj,
            Req::<WlRegistry>::Bind {
                name,
                id: (ZxdgOutputManagerV1::interface(), version),
            },
        );
        self.run();
        man
    }

    fn create_xdg_output(
        &mut self,
        man: &TestObject<ZxdgOutputManagerV1>,
        output: WlOutput,
    ) -> TestObject<ZxdgOutputV1> {
        let xdg = TestObject::<ZxdgOutputV1>::from_request(
            &man.obj,
            zxdg_output_manager_v1::Request::GetXdgOutput { output },
        );
        self.run();
        self.run();
        xdg
    }

    fn register_window(&mut self, window: Window, data: WindowData) {
        self.satellite.connection.windows.insert(window, data);
    }

    fn new_window(&mut self, window: Window, override_redirect: bool, data: WindowData) {
        let dims = data.dims;
        self.register_window(window, data);
        self.satellite
            .new_window(window, override_redirect, dims, None);
    }

    fn map_window(
        &mut self,
        comp: &Compositor,
        window: Window,
        surface: &WlSurface,
        buffer: &TestObject<WlBuffer>,
    ) {
        self.satellite.map_window(window);
        self.associate_window(comp, window, surface);
        self.run();
        surface
            .send_request(Req::<WlSurface>::Attach {
                buffer: Some(buffer.obj.clone()),
                x: 0,
                y: 0,
            })
            .unwrap();
    }

    fn associate_window(&mut self, comp: &Compositor, window: Window, surface: &WlSurface) {
        let xwl = TestObject::<XwaylandSurfaceV1>::from_request(
            &comp.shell.obj,
            Req::<XwaylandShellV1>::GetXwaylandSurface {
                surface: surface.clone(),
            },
        );

        let serial = self.surface_serial;
        let serial_lo = (serial & 0xFF) as u32;
        let serial_hi = ((serial >> 8) & 0xFF) as u32;
        self.surface_serial += 1;

        xwl.send_request(Req::<XwaylandSurfaceV1>::SetSerial {
            serial_lo,
            serial_hi,
        })
        .unwrap();
        self.satellite
            .set_window_serial(window, [serial_lo, serial_hi]);
    }

    #[track_caller]
    fn check_new_surface(&mut self) -> testwl::SurfaceId {
        let id = self
            .testwl
            .last_created_surface_id()
            .expect("Surface not created");
        assert!(self.testwl.get_surface_data(id).is_some());
        id
    }

    fn create_toplevel(
        &mut self,
        comp: &Compositor,
        window: Window,
    ) -> (TestObject<WlSurface>, testwl::SurfaceId) {
        let (buffer, surface) = comp.create_surface();

        let data = WindowData {
            mapped: true,
            dims: WindowDims {
                x: 0,
                y: 0,
                width: 50,
                height: 50,
            },
            fullscreen: false,
        };

        self.new_window(window, false, data);
        self.map_window(comp, window, &surface.obj, &buffer);
        self.run();
        let id = self.check_new_surface();

        {
            let surface_data = self.testwl.get_surface_data(id).unwrap();
            assert!(
                surface_data.surface
                    == self
                        .testwl
                        .get_object::<s_proto::wl_surface::WlSurface>(id)
                        .unwrap()
            );
            assert!(surface_data.buffer.is_none());
            assert!(
                matches!(surface_data.role, Some(testwl::SurfaceRole::Toplevel(_))),
                "surface role: {:?}",
                surface_data.role
            );
        }

        self.testwl
            .configure_toplevel(id, 100, 100, vec![xdg_toplevel::State::Activated]);
        self.testwl.focus_toplevel(id);
        self.run();

        {
            let surface_data = self.testwl.get_surface_data(id).unwrap();
            assert!(surface_data.buffer.is_some());
        }

        let scale = self.satellite.current_scale;
        let expected_width = (100.0 * scale) as u16;
        let expected_height = (100.0 * scale) as u16;

        let win_data = self.connection().windows.get(&window).map(|d| &d.dims);
        assert!(
            matches!(
                win_data,
                Some(&super::WindowDims {
                    x: 0,
                    y: 0,
                    width,
                    height
                }) if width == expected_width && height == expected_height
            ),
            "Incorrect window geometry: {win_data:?}"
        );

        (surface, id)
    }

    #[track_caller]
    fn create_popup(
        &mut self,
        comp: &Compositor,
        builder: PopupBuilder,
    ) -> (TestObject<WlSurface>, testwl::SurfaceId) {
        let (buffer, surface) = comp.create_surface();
        let PopupBuilder {
            window,
            parent_window,
            parent_surface,
            dims,
            scale,
            check_size_and_pos,
        } = builder;

        let data = WindowData {
            mapped: true,
            dims,
            fullscreen: false,
        };
        self.new_window(window, true, data);
        self.map_window(comp, window, &surface.obj, &buffer);
        self.run();

        let popup_id = self.check_new_surface();
        assert_ne!(popup_id, parent_surface);

        {
            let surface_data = self.testwl.get_surface_data(popup_id).unwrap();
            assert!(
                surface_data.surface
                    == self
                        .testwl
                        .get_object::<s_proto::wl_surface::WlSurface>(popup_id)
                        .unwrap()
            );
            assert!(surface_data.buffer.is_none());
            assert!(
                matches!(surface_data.role, Some(testwl::SurfaceRole::Popup(_))),
                "surface was not a popup (role: {:?})",
                surface_data.role
            );

            let parent_xdg = &self
                .testwl
                .get_surface_data(parent_surface)
                .unwrap()
                .xdg()
                .surface;
            assert_eq!(surface_data.popup().parent.id(), parent_xdg.id());

            let pos = &surface_data.popup().positioner_state;
            if check_size_and_pos {
                assert_eq!(
                    pos.size.as_ref().unwrap(),
                    &testwl::Vec2 {
                        x: (dims.width as f64 / scale) as i32,
                        y: (dims.height as f64 / scale) as i32
                    }
                );

                let parent_win = &self.connection().windows[&parent_window];
                assert_eq!(
                    pos.anchor_rect.as_ref().unwrap(),
                    &testwl::Rect {
                        size: testwl::Vec2 {
                            x: (parent_win.dims.width as f64 / scale) as i32,
                            y: (parent_win.dims.height as f64 / scale) as i32
                        },
                        offset: testwl::Vec2::default()
                    }
                );
                assert_eq!(
                    pos.offset,
                    testwl::Vec2 {
                        x: ((dims.x - parent_win.dims.x) as f64 / scale) as i32,
                        y: ((dims.y - parent_win.dims.y) as f64 / scale) as i32
                    }
                );
            }
            assert_eq!(pos.anchor, xdg_positioner::Anchor::TopLeft);
            assert_eq!(pos.gravity, xdg_positioner::Gravity::BottomRight);
        }

        self.testwl.configure_popup(popup_id);
        self.run();

        {
            let surface_data = self.testwl.get_surface_data(popup_id).unwrap();
            assert!(surface_data.buffer.is_some());
        }

        (surface, popup_id)
    }

    #[track_caller]
    fn setup_popup_focus_case(
        &mut self,
        comp: &Compositor,
        override_redirect: bool,
        accepts_input: bool,
        take_focus: bool,
    ) -> (Window, Window) {
        let win_toplevel = Window::new(1);
        self.create_toplevel(comp, win_toplevel);

        let win_popup = Window::new(2);
        let (buffer, surface) = comp.create_surface();
        let data = WindowData {
            mapped: true,
            dims: WindowDims {
                x: 10,
                y: 20,
                width: 100,
                height: 50,
            },
            fullscreen: false,
        };
        self.new_window(win_popup, override_redirect, data);
        if !override_redirect {
            self.satellite
                .set_window_role(win_popup, crate::xstate::WindowRole::Popup);
        }

        self.satellite.set_win_hints(
            win_popup,
            super::WmHints {
                window_group: None,
                accepts_input,
            },
        );
        if take_focus {
            self.satellite.set_take_focus(win_popup, true);
        }
        self.map_window(comp, win_popup, &surface.obj, &buffer);
        self.run();

        let popup_id = self.check_new_surface();
        self.testwl.configure_popup(popup_id);
        self.run();

        (win_toplevel, win_popup)
    }

    #[track_caller]
    fn assert_window_dimensions(
        &self,
        window: x::Window,
        surface_id: testwl::SurfaceId,
        dims: WindowDims,
    ) {
        let data = self.testwl.get_surface_data(surface_id).unwrap();
        match data.role {
            Some(SurfaceRole::Popup(_)) => {
                assert_eq!(
                    data.popup().positioner_state.size,
                    Some(testwl::Vec2 {
                        x: dims.width as _,
                        y: dims.height as _
                    })
                );
                let win_data = &self.connection().windows[&window];
                assert_eq!(win_data.dims, dims);
            }
            Some(SurfaceRole::Toplevel(_)) => {
                let win_data = self
                    .satellite
                    .windows
                    .get(&window)
                    .copied()
                    .and_then(|id| {
                        let d = self.satellite.world.entity(id).unwrap();
                        d.get::<&crate::server::WindowData>()
                    })
                    .unwrap();
                assert_eq!(win_data.attrs.dims, dims);

                let viewport = data.viewport.as_ref().expect("Missing viewport");
                assert_eq!(viewport.width, dims.width as _);
                assert_eq!(viewport.height, dims.height as _);
            }
            ref e => {
                panic!("tried to assert dimensions of something not a toplevel or popup: {e:?}",);
            }
        }
    }

    fn reconfigure_window(&mut self, window: Window, dims: WindowDims, override_redirect: bool) {
        self.satellite
            .reconfigure_window(x::ConfigureNotifyEvent::new(
                window,
                window,
                x::WINDOW_NONE,
                dims.x,
                dims.y,
                dims.width,
                dims.height,
                0,
                override_redirect,
            ));
    }
}

struct TestObjectData<T: Proxy> {
    events: Mutex<Vec<T::Event>>,
    _phantom: std::marker::PhantomData<T>,
}

impl<T: Proxy> Default for TestObjectData<T> {
    fn default() -> Self {
        Self {
            events: Default::default(),
            _phantom: Default::default(),
        }
    }
}

impl<T: Proxy + Send + Sync + 'static> ObjectData for TestObjectData<T>
where
    T::Event: Send + Sync + std::fmt::Debug,
{
    fn event(
        self: Arc<Self>,
        backend: &Backend,
        msg: Message<ObjectId, std::os::fd::OwnedFd>,
    ) -> Option<Arc<dyn ObjectData>> {
        fn obj_data<T>() -> Arc<dyn ObjectData>
        where
            T: Proxy + Send + Sync + 'static,
            T::Event: Send + Sync + std::fmt::Debug,
        {
            Arc::new(TestObjectData::<T>::default())
        }

        let new_data = match (msg.sender_id.interface().name, msg.opcode) {
            (x, opcode) if x == ZwpTabletSeatV2::interface().name => match opcode {
                EVT_TABLET_ADDED_OPCODE => Some(obj_data::<ZwpTabletV2>()),
                EVT_TOOL_ADDED_OPCODE => Some(obj_data::<ZwpTabletToolV2>()),
                EVT_PAD_ADDED_OPCODE => Some(obj_data::<ZwpTabletPadV2>()),
                _ => None,
            },
            (x, EVT_GROUP_OPCODE) if x == ZwpTabletPadV2::interface().name => {
                Some(obj_data::<ZwpTabletPadGroupV2>())
            }
            (x, opcode) if x == ZwpTabletPadGroupV2::interface().name => match opcode {
                EVT_RING_OPCODE => Some(obj_data::<ZwpTabletPadRingV2>()),
                EVT_STRIP_OPCODE => Some(obj_data::<ZwpTabletPadStripV2>()),
                _ => None,
            },
            _ => None,
        };
        let connection = Connection::from_backend(backend.clone());
        let event = T::parse_event(&connection, msg).unwrap().1;
        self.events.lock().unwrap().push(event);
        new_data
    }

    fn destroyed(&self, _: ObjectId) {}
}

struct TestObject<T: Proxy> {
    obj: T,
    data: Arc<TestObjectData<T>>,
}

impl<T: Proxy> std::ops::Deref for TestObject<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.obj
    }
}

impl<T: Proxy + Sync + Send + 'static> TestObject<T>
where
    T::Event: Sync + Send + std::fmt::Debug,
{
    fn from_request<P: Proxy>(object: &P, request: P::Request<'_>) -> Self {
        let data = Arc::<TestObjectData<T>>::default();
        let obj: T = P::send_constructor(object, request, data.clone()).unwrap();
        Self { obj, data }
    }
}

type Req<'a, T> = <T as Proxy>::Request<'a>;
type Ev<T> = <T as Proxy>::Event;

// Matches Xwayland flow.
#[test]
fn toplevel_flow() {
    let (mut f, compositor) = TestFixture::new_with_compositor();

    let window = Window::new(1);
    let (surface, testwl_id) = f.create_toplevel(&compositor, window);
    {
        let surface_data = f.testwl.get_surface_data(testwl_id).unwrap();
        assert!(surface_data.buffer.is_some());
    }

    f.testwl.close_toplevel(testwl_id);
    f.run();

    assert!(!f.satellite.connection.windows[&window].mapped);

    assert!(
        f.testwl.get_surface_data(testwl_id).is_some(),
        "Surface should still exist for closed toplevel"
    );
    assert!(surface.obj.is_alive());

    // For some reason, we can get two UnmapNotify events
    // https://tronche.com/gui/x/icccm/sec-4.html#s-4.1.4
    f.satellite.unmap_window(window);
    f.satellite.unmap_window(window);
    f.satellite.destroy_window(window);
    surface.obj.destroy();
    f.run();

    assert!(f.testwl.get_surface_data(testwl_id).is_none());
}

#[test]
fn popup_flow_simple() {
    let (mut f, compositor) = TestFixture::new_with_compositor();

    let win_toplevel = Window::new(1);
    let (_, toplevel_id) = f.create_toplevel(&compositor, win_toplevel);

    let win_popup = Window::new(2);
    let (popup_surface, popup_id) = f.create_popup(
        &compositor,
        PopupBuilder::new(win_popup, win_toplevel, toplevel_id),
    );

    f.satellite.unmap_window(win_popup);
    f.satellite.destroy_window(win_popup);
    popup_surface.obj.destroy();
    f.run();

    assert!(f.testwl.get_surface_data(popup_id).is_none());
}

#[test]
fn pass_through_globals() {
    use wayland_client::protocol::wl_output::WlOutput;

    let mut f = TestFixture::new();
    f.testwl.new_output();
    f.testwl.enable_xdg_output_manager();
    f.run();
    f.run();

    const fn check<T: Proxy>() {}

    macro_rules! globals_struct {
        ($($field:ident),+) => {
            $( check::<$field>(); )+
            #[derive(Default)]
            #[allow(non_snake_case)]
            struct SupportedGlobals {
                $( $field: bool ),+
            }

            impl SupportedGlobals {
                fn check_globals(&self) {
                    $( assert!(self.$field, "Missing global {}", stringify!($field)); )+
                }

                fn global_found(&mut self, interface: String) {
                    match interface {
                        $(
                            x if x == $field::interface().name => {
                                self.$field = true;
                            }
                        )+
                        _ => panic!("Found an unhandled global: {interface}")
                    }
                }
            }
        }
    }

    // New globals need to be added here and in testwl.
    globals_struct! {
        WlCompositor,
        WlShm,
        WlOutput,
        WlSeat,
        ZwpLinuxDmabufV1,
        ZwpRelativePointerManagerV1,
        ZxdgOutputManagerV1,
        WlDrm,
        ZwpPointerConstraintsV1,
        XwaylandShellV1,
        ZwpTabletManagerV2,
        WpLinuxDrmSyncobjManagerV1
    }

    let mut globals = SupportedGlobals::default();
    f.run();
    let events = std::mem::take(&mut *f.registry.data.events.lock().unwrap());
    assert!(!events.is_empty());
    for event in events {
        let Ev::<WlRegistry>::Global { interface, .. } = event else {
            unreachable!();
        };

        globals.global_found(interface);
    }

    globals.check_globals();
}

#[test]
fn take_focus_toplevel_is_still_the_focus_restore_target() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let win = Window::new(1);
    f.create_toplevel(&comp, win);
    f.satellite.to_focus = Some(super::FocusData {
        window: win,
        output_name: None,
        is_popup: false,
        has_take_focus: true,
    });
    f.run();
    assert_eq!(f.connection().send_take_focus_window, Some(win));
    assert_eq!(
        f.satellite.last_focused_toplevel,
        Some(win),
        "WM_TAKE_FOCUS toplevel must remain the focus restore target"
    );
}

#[test]
fn last_activated_toplevel_is_focused() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let win1 = Window::new(1);

    let (_surface1, id1) = f.create_toplevel(&comp, win1);
    assert_eq!(
        f.connection().focused_window,
        Some(win1),
        "new toplevel's window is not focused"
    );

    let win2 = Window::new(2);
    let _data2 = f.create_toplevel(&comp, win2);
    assert_eq!(
        f.connection().focused_window,
        Some(win2),
        "toplevel focus did not switch"
    );

    f.testwl.configure_toplevel(id1, 100, 100, vec![]);
    f.run();
    assert_eq!(
        f.connection().focused_window,
        Some(win2),
        "toplevel focus did not stay the same"
    );
}

#[test]
fn surface_serial_before_window_serial_still_associates() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let t_win = Window::new(1);
    let (_, toplevel_id) = f.create_toplevel(&comp, t_win);

    let win = Window::new(2);
    let (surface, _) = f.create_popup(&comp, PopupBuilder::new(win, t_win, toplevel_id));
    f.satellite.unmap_window(win);
    surface.obj.destroy();
    f.run();

    // Remap: Xwayland announces the surface serial before the window serial.
    let (_, new_surface) = comp.create_surface();
    let xwl = TestObject::<XwaylandSurfaceV1>::from_request(
        &comp.shell.obj,
        Req::<XwaylandShellV1>::GetXwaylandSurface {
            surface: new_surface.obj.clone(),
        },
    );
    let serial = f.surface_serial;
    f.surface_serial += 1;
    let serial_lo = (serial & 0xFF) as u32;
    let serial_hi = ((serial >> 8) & 0xFF) as u32;
    xwl.send_request(Req::<XwaylandSurfaceV1>::SetSerial {
        serial_lo,
        serial_hi,
    })
    .unwrap();
    f.run();

    f.satellite.map_window(win);
    f.satellite.set_window_serial(win, [serial_lo, serial_hi]);
    f.run();

    let id = f.testwl.last_created_surface_id().expect("no surface");
    let data = f.testwl.get_surface_data(id).unwrap();
    assert!(
        matches!(data.role, Some(testwl::SurfaceRole::Popup(_))),
        "popup role missing after a reversed association: {:?}",
        data.role
    );
}

#[test]
fn popup_window_changes_surface() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let t_win = Window::new(1);
    let (_, toplevel_id) = f.create_toplevel(&comp, t_win);

    let win = Window::new(2);
    let (surface, old_id) = f.create_popup(&comp, PopupBuilder::new(win, t_win, toplevel_id));

    f.satellite.unmap_window(win);
    surface.obj.destroy();
    f.run();

    assert!(f.testwl.get_surface_data(old_id).is_none());

    let (_, surface) = comp.create_surface();
    f.run();
    let id = f
        .testwl
        .last_created_surface_id()
        .expect("No surface created");

    assert_ne!(old_id, id);
    assert!(f.testwl.get_surface_data(id).is_some());

    f.satellite.map_window(win);
    f.associate_window(&comp, win, &surface.obj);
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert!(data.popup().popup.is_alive());

    f.testwl.configure_popup(id);
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert!(data.popup().popup.is_alive());
}

#[test]
fn override_redirect_window_after_toplevel_close() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let win1 = Window::new(1);
    let (obj, first) = f.create_toplevel(&comp, win1);
    f.testwl.close_toplevel(first);
    f.run();

    f.satellite.unmap_window(win1);
    f.satellite.destroy_window(win1);
    obj.obj.destroy();
    f.run();

    assert!(f.testwl.get_surface_data(first).is_none());

    let win2 = Window::new(2);
    let (buffer, surface) = comp.create_surface();
    f.new_window(win2, true, WindowData::default());
    f.map_window(&comp, win2, &surface.obj, &buffer);
    let second = f.check_new_surface();
    let data = f.testwl.get_surface_data(second).unwrap();
    assert!(
        matches!(data.role, Some(testwl::SurfaceRole::Toplevel(_))),
        "wrong role: {:?}",
        data.role
    )
}

#[test]
fn fullscreen() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let win = Window::new(1);
    let (_, id) = f.create_toplevel(&comp, win);

    f.satellite.set_fullscreen(win, SetState::Add);
    f.run();
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert!(
        data.toplevel()
            .states
            .contains(&xdg_toplevel::State::Fullscreen)
    );

    f.satellite.set_fullscreen(win, SetState::Remove);
    f.run();
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert!(
        !data
            .toplevel()
            .states
            .contains(&xdg_toplevel::State::Fullscreen)
    );

    f.satellite.set_fullscreen(win, SetState::Toggle);
    f.run();
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert!(
        data.toplevel()
            .states
            .contains(&xdg_toplevel::State::Fullscreen)
    );

    f.satellite.set_fullscreen(win, SetState::Toggle);
    f.run();
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert!(
        !data
            .toplevel()
            .states
            .contains(&xdg_toplevel::State::Fullscreen)
    );
}

#[test]
fn window_title_and_class() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let win = Window::new(1);
    let (_, id) = f.create_toplevel(&comp, win);

    f.satellite
        .set_win_title(win, WmName::WmName("window".into()));
    f.satellite.set_win_class(win, "class".into());
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert_eq!(data.toplevel().title, Some("window".into()));
    assert_eq!(data.toplevel().app_id, Some("class".into()));

    f.satellite
        .set_win_title(win, WmName::NetWmName("superwindow".into()));
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert_eq!(data.toplevel().title, Some("superwindow".into()));

    f.satellite
        .set_win_title(win, WmName::WmName("shwindow".into()));
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    assert_eq!(data.toplevel().title, Some("superwindow".into()));
}

#[test]
fn window_group_properties() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let prop_win = Window::new(1);
    f.satellite.new_window(
        prop_win,
        false,
        super::WindowDims {
            width: 1,
            height: 1,
            ..Default::default()
        },
        None,
    );
    f.satellite
        .set_win_title(prop_win, WmName::WmName("window".into()));
    f.satellite.set_win_class(prop_win, "class".into());

    let win = Window::new(2);
    let data = WindowData {
        mapped: true,
        dims: WindowDims {
            width: 50,
            height: 50,
            ..Default::default()
        },
        fullscreen: false,
    };

    let (_, surface) = comp.create_surface();
    let dims = data.dims;
    f.register_window(win, data);
    f.satellite.new_window(win, false, dims, None);
    f.satellite.set_win_hints(
        win,
        super::WmHints {
            window_group: Some(prop_win),
            accepts_input: false,
        },
    );
    f.satellite.map_window(win);
    f.associate_window(&comp, win, &surface.obj);
    f.run();

    let id = f.testwl.last_created_surface_id().unwrap();
    let data = f.testwl.get_surface_data(id).unwrap();

    assert_eq!(data.toplevel().title, Some("window".into()));
    assert_eq!(data.toplevel().app_id, Some("class".into()));
}

#[test]
fn splash_window_fixed_size() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let splash = Window::new(1);
    let (buffer, surface) = comp.create_surface();
    let dims = WindowDims {
        x: 0,
        y: 0,
        width: 400,
        height: 300,
    };
    let data = WindowData {
        mapped: false,
        dims,
        fullscreen: false,
    };
    f.new_window(splash, false, data);
    f.satellite
        .set_window_role(splash, crate::xstate::WindowRole::Splash);
    f.map_window(&comp, splash, &surface.obj, &buffer);
    f.run();

    let id = f.testwl.last_created_surface_id().unwrap();
    let data = f.testwl.get_surface_data(id).unwrap();
    let toplevel = data.toplevel();
    assert_eq!(toplevel.min_size, Some(testwl::Vec2 { x: 400, y: 300 }));
    assert_eq!(toplevel.max_size, Some(testwl::Vec2 { x: 400, y: 300 }));
}

trait SelectionTest {
    type SelectionType: SelectionType;
    fn mimes(testwl: &mut testwl::Server) -> Vec<String>;
    fn paste_data(
        testwl: &mut testwl::Server,
        send_data: impl SendDataForMimeFn,
    ) -> Vec<testwl::PasteData>;
    fn create_offer(testwl: &mut testwl::Server, data: Vec<testwl::PasteData>);
}

macro_rules! selection_tests {
    ($name:ident, $selection_type:ty, $get_mime_fn:ident, $get_paste_data_fn:ident, $create_offer_fn:ident) => {
        impl SelectionTest for $selection_type {
            type SelectionType = $selection_type;
            fn mimes(testwl: &mut testwl::Server) -> Vec<String> {
                testwl.$get_mime_fn()
            }
            fn paste_data(
                testwl: &mut testwl::Server,
                send_data: impl SendDataForMimeFn,
            ) -> Vec<testwl::PasteData> {
                testwl.$get_paste_data_fn(send_data)
            }
            fn create_offer(testwl: &mut testwl::Server, data: Vec<testwl::PasteData>) {
                testwl.$create_offer_fn(data);
            }
        }

        mod $name {
            use super::*;
            #[test]
            fn copy_from_x11() {
                super::copy_from_x11::<$selection_type>();
            }

            #[test]
            fn copy_from_wayland() {
                super::copy_from_wayland::<$selection_type>();
            }

            #[test]
            fn x11_then_wayland() {
                super::selection_x11_then_wayland::<$selection_type>();
            }
        }
    };
}

selection_tests!(
    clipboard,
    Clipboard,
    data_source_mimes,
    clipboard_paste_data,
    create_data_offer
);
selection_tests!(
    primary,
    Primary,
    primary_source_mimes,
    primary_paste_data,
    create_primary_offer
);

fn copy_from_x11<T: SelectionTest>() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let win = Window::new(1);
    let (_surface, _id) = f.create_toplevel(&comp, win);

    let mimes = std::rc::Rc::new(vec![
        testwl::PasteData {
            mime_type: "text".to_string(),
            data: b"abc".to_vec(),
        },
        testwl::PasteData {
            mime_type: "data".to_string(),
            data: vec![1, 2, 3, 4, 6, 10],
        },
    ]);

    f.satellite.set_selection_source::<T::SelectionType>(&mimes);
    f.run();

    let server_mimes = T::mimes(&mut f.testwl);
    for mime in mimes.iter() {
        assert!(server_mimes.contains(&mime.mime_type));
    }

    let data = T::paste_data(&mut f.testwl, |_, _| {
        f.satellite.run();
        true
    });
    assert_eq!(*mimes, data);
}

fn copy_from_wayland<T: SelectionTest>() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    TestObject::<WlKeyboard>::from_request(&comp.seat.obj, wl_seat::Request::GetKeyboard {});
    let win = Window::new(1);
    let (_surface, _id) = f.create_toplevel(&comp, win);

    let mimes = vec![
        testwl::PasteData {
            mime_type: "text".to_string(),
            data: b"abc".to_vec(),
        },
        testwl::PasteData {
            mime_type: "data".to_string(),
            data: vec![1, 2, 3, 4, 6, 10],
        },
    ];
    T::create_offer(&mut f.testwl, mimes.clone());
    f.run();

    let selection = f
        .satellite
        .new_selection::<T::SelectionType>()
        .expect("No new selection");
    for mime in &mimes {
        let data = std::thread::scope(|s| {
            // receive requires a queue flush - dispatch testwl from another thread
            s.spawn(|| {
                let pollfd = unsafe { BorrowedFd::borrow_raw(f.testwl.poll_fd().as_raw_fd()) };
                let mut pollfd = [PollFd::from_borrowed_fd(pollfd, PollFlags::IN)];
                let timeout = timespec_from_millis(100);
                if poll(&mut pollfd, Some(&timeout)).unwrap() == 0 {
                    panic!("Did not get events for testwl!");
                }
                f.testwl.dispatch();
                while poll(&mut pollfd, Some(&timeout)).unwrap() > 0 {
                    f.testwl.dispatch();
                }
            });
            selection.receive(mime.mime_type.clone(), &f.satellite)
        });
        f.run();
        assert_eq!(data, mime.data);
    }
}

fn selection_x11_then_wayland<T: SelectionTest>() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    TestObject::<WlKeyboard>::from_request(&comp.seat.obj, wl_seat::Request::GetKeyboard {});
    let win = Window::new(1);
    let (_surface, _id) = f.create_toplevel(&comp, win);

    let x11data = std::rc::Rc::new(vec![
        testwl::PasteData {
            mime_type: "text".to_string(),
            data: b"abc".to_vec(),
        },
        testwl::PasteData {
            mime_type: "data".to_string(),
            data: vec![1, 2, 3, 4, 6, 10],
        },
    ]);

    f.satellite
        .set_selection_source::<T::SelectionType>(&x11data);
    f.run();

    let waylanddata = vec![
        testwl::PasteData {
            mime_type: "asdf".to_string(),
            data: b"fdaa".to_vec(),
        },
        testwl::PasteData {
            mime_type: "boing".to_string(),
            data: vec![10, 20, 40, 50],
        },
    ];
    T::create_offer(&mut f.testwl, waylanddata.clone());
    f.run();
    f.run();

    let selection = f
        .satellite
        .new_selection::<T::SelectionType>()
        .expect("No new selection");
    for mime in &waylanddata {
        let data = std::thread::scope(|s| {
            // receive requires a queue flush - dispatch testwl from another thread
            s.spawn(|| {
                let pollfd = unsafe { BorrowedFd::borrow_raw(f.testwl.poll_fd().as_raw_fd()) };
                let mut pollfd = [PollFd::from_borrowed_fd(pollfd, PollFlags::IN)];
                let timeout = timespec_from_millis(100);
                if poll(&mut pollfd, Some(&timeout)).unwrap() == 0 {
                    panic!("Did not get events for testwl!");
                }
                f.testwl.dispatch();
                while poll(&mut pollfd, Some(&timeout)).unwrap() > 0 {
                    f.testwl.dispatch();
                }
            });
            selection.receive(mime.mime_type.clone(), &f.satellite)
        });
        f.run();
        assert_eq!(data, mime.data);
    }
}

#[test]
fn raise_window_on_pointer_event() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    TestObject::<WlPointer>::from_request(&comp.seat.obj, wl_seat::Request::GetPointer {});
    let win1 = Window::new(1);
    let (_, id1) = f.create_toplevel(&comp, win1);
    f.testwl.configure_toplevel(id1, 100, 100, vec![]);

    let win2 = Window::new(2);
    let (_, id2) = f.create_toplevel(&comp, win2);
    assert_eq!(f.connection().focused_window, Some(win2));

    f.testwl.move_pointer_to(id2, 0.0, 0.0);
    f.run();
    assert_eq!(f.connection().focused_window, Some(win2));
    assert_eq!(f.satellite.last_hovered, Some(win2));

    f.testwl.move_pointer_to(id1, 0.0, 0.0);
    f.run();
    assert_eq!(f.connection().focused_window, Some(win2));
    assert_eq!(f.satellite.last_hovered, Some(win1));
}

#[test]
fn override_redirect_choose_hover_window() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    TestObject::<WlPointer>::from_request(&comp.seat.obj, wl_seat::Request::GetPointer {});
    let win1 = Window::new(1);
    let (_, id1) = f.create_toplevel(&comp, win1);
    f.testwl.configure_toplevel(id1, 100, 100, vec![]);

    let win2 = Window::new(2);
    let _ = f.create_toplevel(&comp, win2);
    assert_eq!(f.connection().focused_window, Some(win2));

    f.testwl.move_pointer_to(id1, 0.0, 0.0);
    f.run();
    assert_eq!(f.satellite.last_hovered, Some(win1));

    let win3 = Window::new(3);
    let (buffer, surface) = comp.create_surface();
    f.new_window(
        win3,
        true,
        WindowData {
            dims: WindowDims {
                width: 1,
                height: 1,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    f.map_window(&comp, win3, &surface.obj, &buffer);
    f.run();
    let id3 = f.check_new_surface();
    let popup_data = f.testwl.get_surface_data(id3).unwrap();
    let win1_xdg = &f.testwl.get_surface_data(id1).unwrap().xdg().surface;
    assert_eq!(&popup_data.popup().parent, win1_xdg);
}

#[test]
fn popup_override_redirect_never_focused_nor_offered() {
    for accepts_input in [true, false] {
        for take_focus in [false, true] {
            let (mut f, comp) = TestFixture::new_with_compositor();

            let (win_toplevel, _) =
                f.setup_popup_focus_case(&comp, true, accepts_input, take_focus);
            assert_eq!(
                f.connection().focused_window,
                Some(win_toplevel),
                "accepts_input={accepts_input:?} take_focus={take_focus}"
            );
            assert_eq!(
                f.connection().send_take_focus_window,
                None,
                "accepts_input={accepts_input:?} take_focus={take_focus}"
            );
        }
    }
}

#[test]
fn popup_send_take_focus_when_advertised() {
    for accepts_input in [true, false] {
        let (mut f, comp) = TestFixture::new_with_compositor();

        let (win_toplevel, win_popup) = f.setup_popup_focus_case(&comp, false, accepts_input, true);
        assert_eq!(f.connection().focused_window, Some(win_toplevel));
        assert_eq!(f.connection().send_take_focus_window, Some(win_popup));
    }
}

#[test]
fn popup_focus_on_map_with_input_hint() {
    let (mut f, comp) = TestFixture::new_with_compositor();

    let (_, win_popup) = f.setup_popup_focus_case(&comp, false, true, false);
    assert_eq!(f.connection().focused_window, Some(win_popup));
}

#[test]
fn popup_no_focus_without_input_hint() {
    let (mut f, comp) = TestFixture::new_with_compositor();

    let (win_toplevel, _) = f.setup_popup_focus_case(&comp, false, false, false);
    assert_eq!(f.connection().focused_window, Some(win_toplevel));
}

#[track_caller]
fn check_output_position_event(output: &TestObject<WlOutput>, pos: (i32, i32)) {
    let mut geo = None;
    let mut done = false;
    let events = std::mem::take(&mut *output.data.events.lock().unwrap());
    log::debug!("events: {events:?}");
    for event in events {
        match event {
            wl_output::Event::Geometry { x, y, .. } => {
                geo = Some((x, y));
            }
            wl_output::Event::Done => {
                done = true;
            }
            _ => {}
        }
    }
    assert!(done, "Did not receive a done event");
    let Some(geo) = geo else {
        panic!("Did not receive any geometry events");
    };
    assert_eq!(geo, pos);
}

#[track_caller]
fn check_output_position_event_xdg(
    xdg_out: &TestObject<ZxdgOutputV1>,
    out: &TestObject<WlOutput>,
    pos: (i32, i32),
    goo_updated: bool,
) {
    let mut done = false;
    let events = std::mem::take(&mut *xdg_out.data.events.lock().unwrap())
        .into_iter()
        .rev();
    for event in events {
        if let zxdg_output_v1::Event::LogicalPosition { x, y } = event {
            assert_eq!(pos, (x, y));
            done = true;
            break;
        }
    }
    assert!(done, "Did not get zxdg_output_v1 logical_position");
    let events = std::mem::take(&mut *out.data.events.lock().unwrap());
    assert_eq!(
        events
            .into_iter()
            .filter(|e| matches!(*e, wl_output::Event::Done))
            .count(),
        goo_updated as usize,
        "Did not get expected wl_output done event"
    );
}

#[test]
fn output_offset_one_output() {
    // If there is only one output, that output is always positioned at 0x0
    let (mut f, _) = TestFixture::new_with_compositor();
    let (output_obj, output) = f.new_output(0, 0);
    f.run();
    f.run();
    check_output_position_event(&output_obj, (0, 0));

    f.testwl.move_output(&output, 500, 100);
    f.run();
    f.run();
    check_output_position_event(&output_obj, (0, 0));

    f.testwl.move_output(&output, -500, -100);
    f.run();
    f.run();
    check_output_position_event(&output_obj, (0, 0));
}

#[test]
fn output_offset_multi_output() {
    // With multiple outputs, the top-most output is on the X-axis, the left-most output is on the
    // Y-axis, and they always maintain relative positioning.
    let (mut f, _) = TestFixture::new_with_compositor();

    let (output_obj_1, output_1) = f.new_output(1000, 0);
    f.run();
    check_output_position_event(&output_obj_1, (0, 0));

    let (output_obj_2, _) = f.new_output(0, 1000);
    f.run();
    f.run();
    check_output_position_event(&output_obj_1, (1000, 0));
    check_output_position_event(&output_obj_2, (0, 1000));

    f.testwl.move_output(&output_1, 1000, 2000);
    f.run();
    f.run();
    check_output_position_event(&output_obj_1, (1000, 1000));
    check_output_position_event(&output_obj_2, (0, 0));

    // Global output offset does not change
    f.testwl.move_output(&output_1, 1000, 1000);
    f.run();
    f.run();
    check_output_position_event(&output_obj_1, (1000, 0));
    assert!(&output_obj_2.data.events.lock().unwrap().is_empty());
}

#[test]
fn output_offset_multi_output_xdg() {
    let (mut f, _) = TestFixture::new_with_compositor();
    let man = f.enable_xdg_output();

    let (output_obj_1, output_1) = f.new_output(0, 0);
    let output_xdg_1 = f.create_xdg_output(&man, output_obj_1.obj.clone());
    f.run();
    std::mem::take(&mut *output_obj_1.data.events.lock().unwrap());
    f.testwl.move_xdg_output(&output_1, 1000, 0);
    f.run();
    f.run();
    check_output_position_event_xdg(&output_xdg_1, &output_obj_1, (0, 0), true);

    let (output_obj_2, output_2) = f.new_output(1000, 1000);
    let output_xdg_2 = f.create_xdg_output(&man, output_obj_2.obj.clone());
    f.run();
    std::mem::take(&mut *output_obj_2.data.events.lock().unwrap());
    f.testwl.move_xdg_output(&output_2, 0, 1000);
    f.run();
    f.run();
    check_output_position_event_xdg(&output_xdg_1, &output_obj_1, (1000, 0), true);
    check_output_position_event_xdg(&output_xdg_2, &output_obj_2, (0, 1000), true);

    f.testwl.move_xdg_output(&output_1, 1000, 2000);
    f.run();
    f.run();
    check_output_position_event_xdg(&output_xdg_1, &output_obj_1, (1000, 1000), true);
    check_output_position_event_xdg(&output_xdg_2, &output_obj_2, (0, 0), true);

    f.testwl.move_xdg_output(&output_1, 1000, 1000);
    f.run();
    f.run();
    check_output_position_event_xdg(&output_xdg_1, &output_obj_1, (1000, 0), false);
    assert!(output_xdg_2.data.events.lock().unwrap().is_empty());
    assert!(output_obj_2.data.events.lock().unwrap().is_empty());
}

#[test]
fn output_offset_remove_output() {
    let (mut f, _) = TestFixture::new_with_compositor();

    let (output_ext_c, output_ext) = f.new_output(0, 0);
    let (output_main_c, _) = f.new_output(1000, 500);
    f.run();
    f.run();

    check_output_position_event(&output_ext_c, (0, 0));
    check_output_position_event(&output_main_c, (1000, 500));

    f.remove_output(output_ext);
    f.run();
    f.run();
    check_output_position_event(&output_main_c, (0, 0));
}

#[test]
fn remove_all_outputs() {
    let (mut f, _) = TestFixture::new_with_compositor();

    let (_, output) = f.new_output(0, 0);
    f.run();

    f.remove_output(output);
    f.run();
}

#[test]
fn late_output_scale_after_remove_output() {
    let (mut f, _) = TestFixture::new_with_compositor();

    let (_, output) = f.new_output(0, 0);
    f.run();

    f.remove_output(output.clone());
    output.scale(2);
    output.done();
    f.run();
}

#[test]
fn output_offset_surface_positioning() {
    let (mut f, comp) = TestFixture::new_with_compositor();

    f.new_output(0, 0);
    let (_, output) = f.new_output(500, 100);
    f.run();

    let window = Window::new(1);
    let (_, toplevel_id) = f.create_toplevel(&comp, window);
    f.testwl.move_surface_to_output(toplevel_id, &output);
    f.run();

    let mut toplevel_pos = WindowDims {
        x: 500,
        y: 100,
        width: 100,
        height: 100,
    };
    f.assert_window_dimensions(window, toplevel_id, toplevel_pos);

    let popup = Window::new(2);
    let (_, p_id) = f.create_popup(
        &comp,
        PopupBuilder::new(popup, window, toplevel_id).x(510).y(110),
    );
    let mut popup_dims = WindowDims {
        x: 510,
        y: 110,
        width: 50,
        height: 50,
    };
    f.testwl.move_surface_to_output(p_id, &output);
    f.run();
    let data = f.testwl.get_surface_data(p_id).unwrap();
    assert_eq!(
        data.popup().positioner_state.offset,
        testwl::Vec2 { x: 10, y: 10 }
    );
    f.assert_window_dimensions(popup, p_id, popup_dims);

    f.testwl.move_output(&output, 600, 200);
    f.run();
    f.run();

    toplevel_pos.x = 600;
    toplevel_pos.y = 200;
    f.assert_window_dimensions(window, toplevel_id, toplevel_pos);
    let data = f.testwl.get_surface_data(p_id).unwrap();
    assert_eq!(
        data.popup().positioner_state.offset,
        testwl::Vec2 { x: 10, y: 10 }
    );
    popup_dims.x = 610;
    popup_dims.y = 210;
    f.assert_window_dimensions(popup, p_id, popup_dims);

    f.testwl.move_output(&output, -100, -200);
    f.run();
    f.run();

    toplevel_pos.x = 0;
    toplevel_pos.y = 0;
    f.assert_window_dimensions(window, toplevel_id, toplevel_pos);
    let data = f.testwl.get_surface_data(p_id).unwrap();
    assert_eq!(
        data.popup().positioner_state.offset,
        testwl::Vec2 { x: 10, y: 10 }
    );
    popup_dims.x = 10;
    popup_dims.y = 10;
    f.assert_window_dimensions(popup, p_id, popup_dims);
}

#[test]
fn output_offset_xdg_override() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    f.new_output(0, 0);
    f.run();

    let (output_obj, output) = f.new_output(500, 100);
    let window = Window::new(1);
    let (_, id) = f.create_toplevel(&comp, window);
    f.testwl.move_surface_to_output(id, &output);
    f.run();

    let test_position = |f: &TestFixture<_>, x, y| {
        let data = &f.connection().windows[&window];
        assert_eq!(data.dims.x, x);
        assert_eq!(data.dims.y, y);
    };
    test_position(&f, 500, 100);

    let man = f.enable_xdg_output();
    f.create_xdg_output(&man, output_obj.obj.clone());
    f.testwl.move_xdg_output(&output, 0, 0);
    f.run();
    test_position(&f, 0, 0);

    f.testwl.move_xdg_output(&output, 1000, 22);
    f.run();
    test_position(&f, 1000, 22);

    f.testwl.move_output(&output, 600, 200);
    f.run();
    f.run();
    test_position(&f, 1000, 22);
}

#[test]
fn output_offset_negative_position() {
    let mut f = TestFixture::new();
    std::mem::take(&mut *f.registry.data.events.lock().unwrap());
    let (output, _) = f.new_output(-500, -500);
    f.run();
    f.run();
    check_output_position_event(&output, (0, 0));

    let (output2, _) = f.new_output(0, 0);
    f.run();
    f.run();
    check_output_position_event(&output2, (500, 500));
    assert!(output.data.events.lock().unwrap().is_empty());

    let (output3, _) = f.new_output(500, 500);
    f.run();
    f.run();
    check_output_position_event(&output3, (1000, 1000));
    assert!(output.data.events.lock().unwrap().is_empty());
    assert!(output2.data.events.lock().unwrap().is_empty());
}

#[test]
fn output_offset_negative_position_update() {
    let mut f = TestFixture::new();
    std::mem::take(&mut *f.registry.data.events.lock().unwrap());

    let (output, _) = f.new_output(-500, -500);
    f.run();
    f.run();
    check_output_position_event(&output, (0, 0));

    let (output2, _) = f.new_output(0, -1000);
    f.run();
    f.run();
    check_output_position_event(&output, (0, 500));
    check_output_position_event(&output2, (500, 0));

    let (output3, o3) = f.new_output(-1000, 0);
    f.run();
    f.run();
    check_output_position_event(&output, (500, 500));
    check_output_position_event(&output2, (1000, 0));
    check_output_position_event(&output3, (0, 1000));

    f.testwl.move_output(&o3, 0, 0);
    f.run();
    f.run();
    check_output_position_event(&output, (0, 500));
    check_output_position_event(&output2, (500, 0));
    check_output_position_event(&output3, (500, 1000));
}

#[test]
fn output_offset_negative_position_update_xdg() {
    let mut f = TestFixture::new();
    std::mem::take(&mut *f.registry.data.events.lock().unwrap());
    let xdg = f.enable_xdg_output();

    let (output, _) = f.new_output(-500, -500);
    f.run();
    f.run();
    check_output_position_event(&output, (0, 0));

    let (output2, output_s) = f.new_output(0, 0);
    let xdg_output = f.create_xdg_output(&xdg, output2.obj.clone());
    f.run();
    std::mem::take(&mut *output2.data.events.lock().unwrap());
    f.testwl.move_xdg_output(&output_s, 0, -1000);
    f.run();
    f.run();
    check_output_position_event(&output, (0, 500));
    check_output_position_event_xdg(&xdg_output, &output2, (500, 0), true);
}

#[test]
fn popup_uses_parent_output_origin_before_its_own_enter() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    f.new_output(0, 0);
    let (_, output) = f.new_output(500, 100);
    f.run();

    let toplevel = Window::new(1);
    let (_, toplevel_id) = f.create_toplevel(&comp, toplevel);
    f.testwl.move_surface_to_output(toplevel_id, &output);
    f.run();
    f.assert_window_dimensions(
        toplevel,
        toplevel_id,
        WindowDims {
            x: 500,
            y: 100,
            width: 100,
            height: 100,
        },
    );

    let popup = Window::new(2);
    let (_, popup_id) = f.create_popup(
        &comp,
        PopupBuilder::new(popup, toplevel, toplevel_id)
            .x(510)
            .y(110),
    );
    // The popup has not received its own wl_surface enter event yet. Its first
    // configure must still use the parent's output origin instead of jumping to
    // the X root origin.
    f.assert_window_dimensions(
        popup,
        popup_id,
        WindowDims {
            x: 510,
            y: 110,
            width: 50,
            height: 50,
        },
    );
}

#[test]
fn popup_offset_is_relative_to_its_parent_window() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let toplevel = Window::new(1);
    let (_, toplevel_id) = f.create_toplevel(&comp, toplevel);

    // Move the parent away from the output origin, as a floating window would be.
    {
        let entity = f.satellite.windows[&toplevel];
        let mut data = f
            .satellite
            .world
            .get::<&mut super::WindowData>(entity)
            .unwrap();
        data.attrs.dims.x = 300;
        data.attrs.dims.y = 200;
    }

    let popup = Window::new(2);
    let (_, popup_id) = f.create_popup(
        &comp,
        PopupBuilder::new(popup, toplevel, toplevel_id)
            .x(340)
            .y(260)
            .check_size_and_pos(false),
    );
    let data = f.testwl.get_surface_data(popup_id).unwrap();
    assert_eq!(
        data.popup().positioner_state.offset,
        testwl::Vec2 { x: 40, y: 60 },
        "popup creation used the output origin instead of the parent window"
    );
    drop(data);

    // A client reposition must stay relative to the parent too, not to the output.
    f.reconfigure_window(
        popup,
        WindowDims {
            x: 360,
            y: 280,
            width: 50,
            height: 50,
        },
        true,
    );
    f.run();
    let data = f.testwl.get_surface_data(popup_id).unwrap();
    assert_eq!(
        data.popup().positioner_state.offset,
        testwl::Vec2 { x: 60, y: 80 },
        "popup reposition used the output origin instead of the parent window"
    );
}

#[test]
fn reconfigure_popup() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let toplevel = Window::new(1);
    let (_, t_id) = f.create_toplevel(&comp, toplevel);

    let popup = Window::new(2);
    let (_, p_id) = f.create_popup(&comp, PopupBuilder::new(popup, toplevel, t_id).x(20).y(40));

    let new_dims = WindowDims {
        x: 40,
        y: 60,
        width: 80,
        height: 100,
    };
    f.reconfigure_window(popup, new_dims, true);
    f.run();
    f.run();
    f.assert_window_dimensions(popup, p_id, new_dims);
}

#[test]
fn reconfigure_popup_after_map() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let toplevel = Window::new(1);
    f.create_toplevel(&comp, toplevel);

    let popup = Window::new(2);
    let old_dims = WindowDims {
        x: 20,
        y: 40,
        width: 10,
        height: 10,
    };
    let new_dims = WindowDims {
        x: 40,
        y: 60,
        width: 80,
        height: 100,
    };

    let (buffer, surface) = comp.create_surface();
    let popup_data = WindowData {
        mapped: true,
        dims: old_dims,
        fullscreen: false,
    };
    f.new_window(popup, true, popup_data);
    f.satellite.map_window(popup);
    f.reconfigure_window(popup, new_dims, true);
    f.associate_window(&comp, popup, &surface);
    f.run();
    surface
        .send_request(Req::<WlSurface>::Attach {
            buffer: Some(buffer.obj.clone()),
            x: 0,
            y: 0,
        })
        .unwrap();
    f.run();
    let p_id = f.check_new_surface();
    f.testwl.configure_popup(p_id);
    f.run();
    f.assert_window_dimensions(popup, p_id, new_dims);
}

#[test]
fn drag_around_popup() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let toplevel = Window::new(1);
    let (_, t_id) = f.create_toplevel(&comp, toplevel);

    let popup = Window::new(2);
    let (_, p_id) = f.create_popup(&comp, PopupBuilder::new(popup, toplevel, t_id).x(0).y(0));

    let before_counter = f.connection().set_window_dims_counter;
    let mut new_dims = WindowDims {
        x: 0,
        y: 0,
        width: 10,
        height: 10,
    };
    for _ in 0..10 {
        new_dims.x += 5;
        new_dims.y += 5;
        f.reconfigure_window(popup, new_dims, true);
    }
    f.run();
    f.run();
    f.assert_window_dimensions(popup, p_id, new_dims);

    let after_counter = f.connection().set_window_dims_counter;
    assert_eq!(before_counter + 1, after_counter);
}

#[test]
fn reconfigure_toplevel() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let toplevel = Window::new(1);
    let (_, surface) = f.create_toplevel(&comp, toplevel);

    let mut dims = WindowDims {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
    };
    f.assert_window_dimensions(toplevel, surface, dims);

    dims.width = 80;
    dims.height = 120;
    let final_dims = dims;
    // A toplevel can be resized, but not change position
    dims.x = 20;
    dims.y = 20;
    f.reconfigure_window(toplevel, dims, false);
    f.run();
    f.run();

    f.assert_window_dimensions(toplevel, surface, final_dims);
}

type EventMatcher<'a, Event> = Box<dyn FnMut(&Event) -> bool + 'a>;
#[track_caller]
fn events_check<Event: std::fmt::Debug, const N: usize>(
    mut it: impl Iterator<Item = Event>,
    mut matchers: [EventMatcher<'_, Event>; N],
) {
    for (idx, matcher) in matchers.iter_mut().enumerate() {
        let item = it.next();
        if item.is_none() {
            panic!("event {idx} does not exist");
        }
        if !matcher(item.as_ref().unwrap()) {
            panic!("event {idx} was wrong ({item:?})");
        }
    }

    let mut remaining = it.peekable();
    if remaining.peek().is_some() {
        panic!("remaining events: {:?}", remaining.collect::<Vec<_>>());
    }
}

#[test]
fn tablet_smoke_test() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let seat = TestObject::<ZwpTabletSeatV2>::from_request(
        &comp.tablet_man.obj,
        zwp_tablet_manager_v2::Request::GetTabletSeat {
            seat: comp.seat.obj,
        },
    );
    // Not sure why exactly this requires 4 runs but it works so idk
    for _ in 0..4 {
        f.run();
    }

    let events = std::mem::take(&mut *seat.data.events.lock().unwrap()).into_iter();
    let (mut tab_id, mut tool_id, mut pad_id) = (None, None, None);
    events_check(
        events,
        [
            Box::new(|e| match e {
                zwp_tablet_seat_v2::Event::TabletAdded { id } => {
                    tab_id = Some(id.clone());
                    true
                }
                _ => false,
            }),
            Box::new(|e| match e {
                zwp_tablet_seat_v2::Event::ToolAdded { id } => {
                    tool_id = Some(id.clone());
                    true
                }
                _ => false,
            }),
            Box::new(|e| match e {
                zwp_tablet_seat_v2::Event::PadAdded { id } => {
                    pad_id = Some(id.clone());
                    true
                }
                _ => false,
            }),
        ],
    );
    let (tab_id, tool_id, pad_id) = (tab_id.unwrap(), tool_id.unwrap(), pad_id.unwrap());

    // For reasons beyond my mortal understanding, `id.object_data()` does not work properly.
    let tab_data = f.object_data(&tab_id);
    let tab_events = std::mem::take(&mut *tab_data.events.lock().unwrap()).into_iter();
    events_check(
        tab_events,
        [
            Box::new(|e| matches!(e, zwp_tablet_v2::Event::Name { name } if name == "tabby")),
            Box::new(|e| matches!(e, zwp_tablet_v2::Event::Done)),
        ],
    );

    let tool_data = f.object_data(&tool_id);
    let tool_events = std::mem::take(&mut *tool_data.events.lock().unwrap()).into_iter();
    events_check(
        tool_events,
        [
            Box::new(|e| {
                matches!(
                    e,
                    zwp_tablet_tool_v2::Event::Type {
                        tool_type: WEnum::Value(zwp_tablet_tool_v2::Type::Finger)
                    }
                )
            }),
            Box::new(|e| matches!(e, zwp_tablet_tool_v2::Event::Done)),
        ],
    );

    let pad_data = f.object_data(&pad_id);
    let pad_events = std::mem::take(&mut *pad_data.events.lock().unwrap()).into_iter();
    let mut group = None;
    events_check(
        pad_events,
        [
            Box::new(|e| matches!(e, zwp_tablet_pad_v2::Event::Buttons { buttons: 5 })),
            Box::new(|e| match e {
                zwp_tablet_pad_v2::Event::Group { pad_group } => {
                    group = Some(pad_group.clone());
                    true
                }
                _ => false,
            }),
            Box::new(|e| matches!(e, zwp_tablet_pad_v2::Event::Done)),
        ],
    );

    let group = group.unwrap();
    let g_data = f.object_data(&group);
    let g_events = std::mem::take(&mut *g_data.events.lock().unwrap()).into_iter();
    events_check(
        g_events,
        [
            Box::new(|e| {
                matches!(e,
                zwp_tablet_pad_group_v2::Event::Buttons { buttons } if buttons.is_empty())
            }),
            Box::new(|e| matches!(e, zwp_tablet_pad_group_v2::Event::Ring { .. })),
            Box::new(|e| matches!(e, zwp_tablet_pad_group_v2::Event::Strip { .. })),
            Box::new(|e| matches!(e, zwp_tablet_pad_group_v2::Event::Done)),
        ],
    )
}

#[test]
fn fullscreen_heuristic() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let (_, output) = f.new_output(0, 0);

    let window1 = Window::new(1);
    let (_, id) = f.create_toplevel(&comp, window1);
    f.testwl.move_surface_to_output(id, &output);
    f.run();

    let mut check_fullscreen = |id, override_redirect| {
        let window = Window::new(id);
        let (buffer, surface) = comp.create_surface();
        let data = WindowData {
            mapped: true,
            dims: WindowDims {
                x: 0,
                y: 0,
                // Outputs default to 1000x1000 in testwl
                width: 1000,
                height: 1000,
            },
            fullscreen: false,
        };
        f.new_window(window, override_redirect, data);
        f.map_window(&comp, window, &surface.obj, &buffer);
        f.run();
        let id = f.check_new_surface();
        let surface_data = f.testwl.get_surface_data(id).unwrap();
        assert!(
            surface_data.surface
                == f.testwl
                    .get_object::<s_proto::wl_surface::WlSurface>(id)
                    .unwrap()
        );

        let Some(testwl::SurfaceRole::Toplevel(toplevel_data)) = &surface_data.role else {
            panic!("Expected toplevel, got {:?}", surface_data.role);
        };

        assert!(
            toplevel_data
                .states
                .contains(&xdg_toplevel::State::Fullscreen),
            "states: {:?}",
            toplevel_data.states
        );
    };

    check_fullscreen(2, false);
    check_fullscreen(3, true);
}

#[test]
fn scaled_output_popup() {
    let (mut f, comp) = TestFixture::new_with_compositor();

    let (_, output) = f.new_output(0, 0);
    let scale = 2;
    output.scale(scale);
    output.done();
    f.run();
    f.run();

    let toplevel = Window::new(1);
    let (_, toplevel_id) = f.create_toplevel(&comp, toplevel);
    f.testwl.move_surface_to_output(toplevel_id, &output);
    f.run();

    let popup = Window::new(2);
    let builder = PopupBuilder::new(popup, toplevel, toplevel_id)
        .x(50)
        .y(50)
        .scale(scale);
    let initial_dims = builder.dims;
    let (_, popup_id) = f.create_popup(&comp, builder);
    f.testwl.move_surface_to_output(popup_id, &output);
    f.run();
    assert_eq!(
        initial_dims,
        f.connection().window(popup).dims,
        "X11 dimensions changed after configure"
    );
}

#[test]
fn fractional_scale_popup() {
    let mut f = TestFixture::new_pre_connect(|testwl| {
        testwl.enable_fractional_scale();
    });
    let comp = f.compositor();
    let (_, output) = f.new_output(0, 0);

    let toplevel = Window::new(1);
    let (_, toplevel_id) = f.create_toplevel(&comp, toplevel);
    let surface_data = f
        .testwl
        .get_surface_data(toplevel_id)
        .expect("No surface data");
    let fractional = surface_data
        .fractional
        .as_ref()
        .expect("No fractional scale for surface");

    fractional.preferred_scale(180); // 1.5 scale
    f.testwl.move_surface_to_output(toplevel_id, &output);
    f.run();
    f.run();

    let popup = Window::new(2);
    let builder = PopupBuilder::new(popup, toplevel, toplevel_id)
        .x(60)
        .y(60)
        .width(60)
        .height(60)
        .scale(1.5);
    let initial_dims = builder.dims;
    f.create_popup(&comp, builder);
    f.run();
    assert_eq!(
        initial_dims,
        f.connection().window(popup).dims,
        "X11 dimensions changed after configure"
    );
}

#[test]
fn scaled_output_small_popup() {
    let (mut f, comp) = TestFixture::new_with_compositor();

    let (_, output) = f.new_output(0, 0);
    output.scale(2);
    output.done();
    f.run();
    f.run();

    let toplevel = Window::new(1);
    let (_, toplevel_id) = f.create_toplevel(&comp, toplevel);
    f.testwl.move_surface_to_output(toplevel_id, &output);
    f.run();

    let popup = Window::new(2);
    let builder = PopupBuilder::new(popup, toplevel, toplevel_id)
        .x(50)
        .y(50)
        .width(1)
        .height(1)
        .check_size_and_pos(false);

    let (_, popup_id) = f.create_popup(&comp, builder);
    f.testwl.move_surface_to_output(popup_id, &output);
    f.run();

    let dims = f.connection().window(popup).dims;

    assert!(dims.width > 0);
    assert!(dims.height > 0);
}

#[test]
fn fractional_scale_small_popup() {
    let mut f = TestFixture::new_pre_connect(|testwl| {
        testwl.enable_fractional_scale();
    });
    let comp = f.compositor();

    let (_, output) = f.new_output(0, 0);
    let toplevel = Window::new(1);
    let (_, toplevel_id) = f.create_toplevel(&comp, toplevel);
    let data = f.testwl.get_surface_data(toplevel_id).unwrap();
    let fractional = data
        .fractional
        .as_ref()
        .expect("Missing fracitonal scale data");
    fractional.preferred_scale(180); // 1.5 scale
    f.testwl.move_surface_to_output(toplevel_id, &output);
    f.run();
    f.run();

    {
        let data = f.testwl.get_surface_data(toplevel_id).unwrap();
        let viewport = data.viewport.as_ref().expect("Missing viewport");
        assert_eq!(viewport.width, 67);
        assert_eq!(viewport.height, 67);
    }

    let popup = Window::new(2);
    let builder = PopupBuilder::new(popup, toplevel, toplevel_id)
        .width(1)
        .height(1)
        .check_size_and_pos(false);

    let (_, popup_id) = f.create_popup(&comp, builder);
    let dims = f.connection().window(popup).dims;
    assert!(dims.width > 0);
    assert!(dims.height > 0);

    let data = f
        .testwl
        .get_surface_data(popup_id)
        .expect("Missing popup data");
    let pos = &data.popup().positioner_state;
    assert_eq!(pos.size.unwrap(), testwl::Vec2 { x: 1, y: 1 });

    let dims = WindowDims {
        x: 0,
        y: 0,
        width: 2,
        height: 1,
    };
    f.reconfigure_window(popup, dims, true);
    f.run();
    f.run();

    let dims = f.connection().window(popup).dims;
    assert!(dims.width > 0);
    assert!(dims.height > 0);

    let data = f
        .testwl
        .get_surface_data(popup_id)
        .expect("Missing popup data");
    let pos = &data.popup().positioner_state;
    assert_eq!(pos.size.unwrap(), testwl::Vec2 { x: 1, y: 1 });
}

#[test]
fn toplevel_size_limits_scaled() {
    let (mut f, comp) = TestFixture::new_with_compositor();

    let (_, output) = f.new_output(0, 0);
    output.scale(2);
    output.done();
    f.run();
    f.run();

    let window = Window::new(1);
    let (buffer, surface) = comp.create_surface();
    let data = WindowData {
        mapped: true,
        dims: WindowDims {
            width: 50,
            height: 50,
            ..Default::default()
        },
        fullscreen: false,
    };
    f.new_window(window, false, data);
    f.satellite.set_size_hints(
        window,
        super::WmNormalHints {
            min_size: Some(WinSize {
                width: 20,
                height: 20,
            }),
            max_size: Some(WinSize {
                width: 100,
                height: 100,
            }),
        },
    );

    f.map_window(&comp, window, &surface.obj, &buffer);
    f.run();

    let id = f.check_new_surface();
    f.testwl.configure_toplevel(id, 50, 50, vec![]);
    f.run();

    f.testwl.move_surface_to_output(id, &output);
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    let toplevel = data.toplevel();
    assert_eq!(toplevel.min_size, Some(testwl::Vec2 { x: 10, y: 10 }));
    assert_eq!(toplevel.max_size, Some(testwl::Vec2 { x: 50, y: 50 }));

    f.satellite.set_size_hints(
        window,
        super::WmNormalHints {
            min_size: Some(WinSize {
                width: 40,
                height: 40,
            }),
            max_size: Some(WinSize {
                width: 200,
                height: 200,
            }),
        },
    );
    f.run();
    let data = f.testwl.get_surface_data(id).unwrap();
    let toplevel = data.toplevel();
    assert_eq!(toplevel.min_size, Some(testwl::Vec2 { x: 20, y: 20 }));
    assert_eq!(toplevel.max_size, Some(testwl::Vec2 { x: 100, y: 100 }));

    // test sizing with decorations
    f.testwl
        .force_decoration_mode(id, zxdg_toplevel_decoration_v1::Mode::ClientSide);
    f.testwl.configure_toplevel(id, 100, 100, vec![]);
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    let toplevel = data.toplevel();
    assert_eq!(toplevel.min_size, Some(testwl::Vec2 { x: 20, y: 45 }));
    assert_eq!(toplevel.max_size, Some(testwl::Vec2 { x: 100, y: 125 }));

    // Try unsetting size hints one after another
    f.satellite.set_size_hints(
        window,
        super::WmNormalHints {
            min_size: Some(WinSize {
                width: 40,
                height: 40,
            }),
            max_size: None,
        },
    );
    f.run();
    let data = f.testwl.get_surface_data(id).unwrap();
    let toplevel = data.toplevel();
    assert_eq!(toplevel.min_size, Some(testwl::Vec2 { x: 20, y: 45 }));
    assert_eq!(toplevel.max_size, None);

    f.satellite.set_size_hints(
        window,
        super::WmNormalHints {
            min_size: None,
            max_size: None,
        },
    );
    f.run();
    let data = f.testwl.get_surface_data(id).unwrap();
    let toplevel = data.toplevel();
    assert_eq!(toplevel.min_size, None);
    assert_eq!(toplevel.max_size, None);
}

#[test]
fn subpopup_positioning() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    TestObject::<WlPointer>::from_request(&comp.seat.obj, wl_seat::Request::GetPointer {});
    let win_toplevel = Window::new(1);
    let (_, id_toplevel) = f.create_toplevel(&comp, win_toplevel);

    f.testwl.move_pointer_to(id_toplevel, 0.0, 0.0);
    f.run();

    let win_popup = Window::new(2);
    let (_, id_popup) = f.create_popup(
        &comp,
        PopupBuilder::new(win_popup, win_toplevel, id_toplevel)
            .x(25)
            .y(25),
    );

    f.testwl.move_pointer_to(id_popup, 1.0, 1.0);
    f.run();

    let win_subpopup = Window::new(3);

    f.create_popup(
        &comp,
        PopupBuilder::new(win_subpopup, win_toplevel, id_toplevel)
            .x(50)
            .y(50),
    );

    let dims = f.connection().window(win_subpopup).dims;
    assert_eq!(dims.x, 50);
    assert_eq!(dims.y, 50);
}

#[test]
fn transient_for_toplevel() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let toplevel = Window::new(1);
    let (_, toplevel_id) = f.create_toplevel(&comp, toplevel);

    let sub_toplevel = Window::new(2);
    let (buffer, surface) = comp.create_surface();
    f.new_window(
        sub_toplevel,
        false,
        WindowData {
            mapped: true,
            dims: WindowDims {
                width: 50,
                height: 50,
                ..Default::default()
            },
            fullscreen: false,
        },
    );

    f.satellite.set_transient_for(sub_toplevel, toplevel);
    f.map_window(&comp, sub_toplevel, &surface.obj, &buffer);
    f.run();
    let id = f.check_new_surface();
    let toplevel_data = f.testwl.get_surface_data(toplevel_id).unwrap();
    let sub_data = f.testwl.get_surface_data(id).unwrap();
    assert_eq!(
        sub_data.toplevel().parent,
        Some(toplevel_data.toplevel().toplevel.clone())
    );
}

#[test]
fn touch_fractional_scale() {
    let mut f = TestFixture::new_pre_connect(|testwl| {
        testwl.enable_fractional_scale();
    });
    let comp = f.compositor();
    let (_, output) = f.new_output(0, 0);
    let touch = TestObject::<WlTouch>::from_request(&comp.seat.obj, wl_seat::Request::GetTouch {});
    f.run();

    let toplevel = Window::new(1);
    let (_, id) = f.create_toplevel(&comp, toplevel);
    f.testwl.move_surface_to_output(id, &output);

    let data = f.testwl.get_surface_data(id).unwrap();
    let server_surface = data.surface.clone();
    let fractional = data.fractional.as_ref().cloned().unwrap();

    let do_touch = |f: &mut TestFixture<_>, x, y| {
        f.testwl.touch().down(0, 0, &server_surface, 0, x, y);
        f.testwl.dispatch();
        f.run();
        f.run();
        let events = &mut touch.data.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        let event = events.pop().unwrap();
        let wl_touch::Event::Down { x, y, .. } = event else {
            panic!("Got unexpected event: {event:?}");
        };
        (x, y)
    };

    let (x, y) = do_touch(&mut f, 20.0, 40.0);
    assert_eq!(x, 20.0);
    assert_eq!(y, 40.0);

    fractional.preferred_scale(180); // 1.5 scale
    f.run();
    let (x, y) = do_touch(&mut f, 20.0, 40.0);
    assert_eq!(x, 20.0 * 1.5);
    assert_eq!(y, 40.0 * 1.5);
}

#[test]
fn tablet_tool_fractional_scale() {
    let mut f = TestFixture::new_pre_connect(|testwl| {
        testwl.enable_fractional_scale();
    });
    let comp = f.compositor();
    let (_, output) = f.new_output(0, 0);
    let toplevel = Window::new(1);
    let (_, id) = f.create_toplevel(&comp, toplevel);
    let surface_data = f.testwl.get_surface_data(id).unwrap();
    let fractional = surface_data.fractional.as_ref().cloned().unwrap();
    let server_surface = surface_data.surface.clone();
    f.testwl.move_surface_to_output(id, &output);

    let seat = TestObject::<ZwpTabletSeatV2>::from_request(
        &comp.tablet_man.obj,
        zwp_tablet_manager_v2::Request::GetTabletSeat {
            seat: comp.seat.obj,
        },
    );
    // Not sure why exactly this requires 4 runs but it works so idk
    for _ in 0..4 {
        f.run();
    }

    let mut tool = None;
    for event in seat.data.events.lock().unwrap().drain(..) {
        if let zwp_tablet_seat_v2::Event::ToolAdded { id } = event {
            tool = Some(id.clone());
            break;
        }
    }

    let client_tool = tool.expect("Didn't get tool");
    let client_data = f.object_data(&client_tool);
    let server_tool = f.testwl.tablet_tool().clone();
    let tablet = f.testwl.tablet().clone();

    let get_motion = || {
        let events = &mut *client_data.events.lock().unwrap();
        let event = events.pop();
        let Some(zwp_tablet_tool_v2::Event::Motion { x, y }) = event else {
            panic!("Didn't get motion event: {event:?}");
        };
        (x, y)
    };

    server_tool.proximity_in(0, &tablet, &server_surface);
    server_tool.motion(20.0, 40.0);
    f.testwl.dispatch();
    f.run();
    f.run();

    let (x, y) = get_motion();
    assert_eq!(x, 20.0);
    assert_eq!(y, 40.0);

    fractional.preferred_scale(180); // 1.5 scale
    server_tool.proximity_in(0, &tablet, &server_surface);
    server_tool.motion(20.0, 40.0);
    f.testwl.dispatch();
    f.run();
    f.run();

    let (x, y) = get_motion();
    assert_eq!(x, 20.0 * 1.5);
    assert_eq!(y, 40.0 * 1.5);
}

#[test]
fn output_updated_before_x_connection() {
    let mut f = EarlyTestFixture::new_early_with_options(SetupOptions::default());
    let comp = f.compositor();
    let (_, output) = f.new_output(-20, -20);

    let mut f = f.upgrade_connection(FakeXConnection::default());

    let window = Window::new(1);
    let (_, surface_id) = f.create_toplevel(&comp, window);
    f.testwl.move_surface_to_output(surface_id, &output);
    f.run();
    f.run();
    let data = &f.connection().windows[&window];
    assert_eq!(data.dims.x, 0);
    assert_eq!(data.dims.y, 0);
}

#[test]
fn quick_empty_data_offer() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    TestObject::<WlKeyboard>::from_request(&comp.seat.obj, wl_seat::Request::GetKeyboard {});
    let win = Window::new(1);
    let (_surface, _id) = f.create_toplevel(&comp, win);
    f.testwl.create_data_offer(vec![testwl::PasteData {
        mime_type: "text".to_string(),
        data: b"abc".to_vec(),
    }]);
    f.testwl.empty_data_offer();
    f.run();

    let selection = f.satellite.new_selection::<Clipboard>();
    assert!(selection.is_none());
}

#[test]
fn quick_destroy_window_with_serial() {
    let (mut f, comp) = TestFixture::new_with_compositor();

    let window = Window::new(1);
    let data = WindowData {
        mapped: true,
        dims: WindowDims {
            x: 0,
            y: 0,
            width: 50,
            height: 50,
        },
        fullscreen: false,
    };
    f.new_window(window, false, data);
    f.satellite.map_window(window);

    let (_, surface) = comp.create_surface();
    let xwl = TestObject::<XwaylandSurfaceV1>::from_request(
        &comp.shell.obj,
        Req::<XwaylandShellV1>::GetXwaylandSurface {
            surface: surface.clone(),
        },
    );

    let serial = f.surface_serial;
    let serial_lo = (serial & 0xFF) as u32;
    let serial_hi = ((serial >> 8) & 0xFF) as u32;
    xwl.send_request(Req::<XwaylandSurfaceV1>::SetSerial {
        serial_lo,
        serial_hi,
    })
    .unwrap();
    f.satellite
        .set_window_serial(window, [serial_lo, serial_hi]);
    f.satellite.unmap_window(window);
    f.satellite.destroy_window(window);
    f.run();

    let id = f.testwl.last_created_surface_id().unwrap();
    let surface_data = f.testwl.get_surface_data(id).unwrap();
    assert!(
        surface_data.role.is_none(),
        "Surface unexpectedly has role: {:?}",
        surface_data.role
    );
}

#[test]
fn scaled_pointer_lock_position_hint() {
    let mut f = TestFixture::new_pre_connect(|testwl| {
        testwl.enable_fractional_scale();
    });
    let comp = f.compositor();
    let pointer =
        TestObject::<WlPointer>::from_request(&comp.seat.obj, wl_seat::Request::GetPointer {});

    let (_, output) = f.new_output(0, 0);
    let win = Window::new(1);
    let (surface, id) = f.create_toplevel(&comp, win);
    let surface_data = f.testwl.get_surface_data(id).expect("No surface data");
    let fractional = surface_data
        .fractional
        .as_ref()
        .expect("No fractional scale for surface");
    fractional.preferred_scale(180); // 1.5 scale
    f.testwl.move_surface_to_output(id, &output);
    f.run();
    f.run();

    let locked_pointer = TestObject::<ZwpLockedPointerV1>::from_request(
        &comp.pointer_constraints.obj,
        zwp_pointer_constraints_v1::Request::LockPointer {
            surface: surface.obj.clone(),
            pointer: pointer.obj.clone(),
            region: None,
            lifetime: WEnum::Value(zwp_pointer_constraints_v1::Lifetime::Persistent),
        },
    );
    locked_pointer.set_cursor_position_hint(75.0, 75.0);
    f.run();
    f.run();

    let lock_data = f
        .testwl
        .locked_pointer()
        .expect("Missing locked pointer data");
    assert_eq!(lock_data.surface, id);
    assert_eq!(
        lock_data.cursor_hint,
        Some(testwl::Vec2f { x: 50.0, y: 50.0 })
    );
}

#[test]
fn integer_output_pointer_lock_position_hint() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let pointer =
        TestObject::<WlPointer>::from_request(&comp.seat.obj, wl_seat::Request::GetPointer {});

    let (_, output) = f.new_output(0, 0);
    output.scale(2);
    output.done();
    f.run();
    f.run();
    let win = Window::new(1);
    let (surface, id) = f.create_toplevel(&comp, win);
    f.testwl.move_surface_to_output(id, &output);
    f.run();
    f.run();

    let locked_pointer = TestObject::<ZwpLockedPointerV1>::from_request(
        &comp.pointer_constraints.obj,
        zwp_pointer_constraints_v1::Request::LockPointer {
            surface: surface.obj.clone(),
            pointer: pointer.obj.clone(),
            region: None,
            lifetime: WEnum::Value(zwp_pointer_constraints_v1::Lifetime::Persistent),
        },
    );
    locked_pointer.set_cursor_position_hint(100.0, 100.0);
    f.run();
    f.run();

    let lock_data = f
        .testwl
        .locked_pointer()
        .expect("Missing locked pointer data");
    assert_eq!(lock_data.surface, id);
    assert_eq!(
        lock_data.cursor_hint,
        Some(testwl::Vec2f { x: 50.0, y: 50.0 })
    );
}

#[test]
fn disconnected_output_rescaling() {
    let mut f = TestFixture::new_pre_connect(|testwl| {
        testwl.enable_fractional_scale();
    });
    let comp = f.compositor();
    let (_, output_main) = f.new_output(0, 0);
    let (_, output_ext) = f.new_output(1000, 0);

    let window = Window::new(1);
    let (_, id) = f.create_toplevel(&comp, window);

    let surface_data = f.testwl.get_surface_data(id).expect("No surface data");
    let fractional = surface_data
        .fractional
        .as_ref()
        .expect("No fractional scale for surface");
    fractional.preferred_scale(240); // 2.0 scale
    f.testwl.move_surface_to_output(id, &output_main);
    f.run();

    let surface_data = f.testwl.get_surface_data(id).expect("No surface data");
    let fractional = surface_data
        .fractional
        .as_ref()
        .expect("No fractional scale for surface");
    fractional.preferred_scale(180); // 1.5 scale
    f.testwl.move_surface_to_output(id, &output_ext);
    f.run();
    // Multiple monitors with different scaling will select the lowest scale across monitors
    assert_eq!(f.satellite.inner.new_scale, Some(1.5));

    f.remove_output(output_ext);
    let surface_data = f.testwl.get_surface_data(id).expect("No surface data");
    let fractional = surface_data
        .fractional
        .as_ref()
        .expect("No fractional scale for surface");
    fractional.preferred_scale(120); // 1.0 scale
    f.run();
    f.run();
    // An fractional scale change done while the surface is on a removed output is ignored
    assert_eq!(f.satellite.inner.new_scale, Some(1.5));

    f.testwl.move_surface_to_output(id, &output_main);
    let surface_data = f.testwl.get_surface_data(id).expect("No surface data");
    let fractional = surface_data
        .fractional
        .as_ref()
        .expect("No fractional scale for surface");
    fractional.preferred_scale(240); // 2.0 scale
    f.run();
    f.run();
    // After the output is disconnected, only the 2x scale output remains, so use that scale
    assert_eq!(f.satellite.inner.new_scale, Some(2.0));
}

#[track_caller]
fn check_window_and_viewport_size(
    f: &TestFixture<FakeXConnection>,
    window: Window,
    id: testwl::SurfaceId,
    width: u16,
    height: u16,
) {
    let dims = f.connection().window(window).dims;
    assert_eq!((dims.width, dims.height), (width, height), "X window size");
    let data = f.testwl.get_surface_data(id).unwrap();
    let viewport = data.viewport.as_ref().unwrap();
    assert_eq!(
        (viewport.width, viewport.height),
        (width as i32, height as i32),
        "viewport size"
    );
}

#[test]
fn client_side_decorations() {
    let (mut f, compositor) = TestFixture::new_with_compositor();
    let window = Window::new(1);
    let (_, id) = f.create_toplevel(&compositor, window);
    f.testwl
        .force_decoration_mode(id, zxdg_toplevel_decoration_v1::Mode::ClientSide);
    f.testwl.configure_toplevel(id, 100, 100, vec![]);
    f.run();

    check_window_and_viewport_size(&f, window, id, 100, 75);

    let subsurface_id = f.testwl.last_created_surface_id().unwrap();
    assert_ne!(subsurface_id, id);
    let data = f.testwl.get_surface_data(subsurface_id).unwrap();
    assert!(data.buffer.is_some());
    let Some(SurfaceRole::Subsurface(subsurface)) = &data.role else {
        panic!("surface was not a subsurface: {:?}", data.role);
    };
    assert_eq!(subsurface.position, testwl::Vec2 { x: 0, y: -25 });
    assert_eq!(subsurface.parent, id);
    let subsurface = subsurface.subsurface.clone();

    f.testwl
        .configure_toplevel(id, 100, 100, vec![xdg_toplevel::State::Fullscreen]);
    f.run();
    check_window_and_viewport_size(&f, window, id, 100, 100);

    let data = f.testwl.get_surface_data(subsurface_id).unwrap();
    assert!(data.buffer.is_none());

    // Make sure updating the window title does not draw the bar if it should not
    f.satellite
        .set_win_title(window, WmName::WmName("window".into()));
    f.run();
    let data = f.testwl.get_surface_data(subsurface_id).unwrap();
    assert!(data.buffer.is_none());

    f.testwl
        .force_decoration_mode(id, zxdg_toplevel_decoration_v1::Mode::ServerSide);
    f.testwl.configure_toplevel(id, 100, 100, vec![]);
    f.run();

    check_window_and_viewport_size(&f, window, id, 100, 100);
    assert!(f.testwl.get_surface_data(subsurface_id).is_none());
    assert!(!subsurface.is_alive());
}

#[test]
fn client_side_decorations_fractional_scale() {
    use super::decoration::DecorationsDataSatellite;

    let mut f = TestFixture::new_pre_connect(|testwl| {
        testwl.enable_fractional_scale();
    });
    let compositor = f.compositor();
    let (_, output) = f.new_output(0, 0);
    let window = Window::new(1);
    let (_, id) = f.create_toplevel(&compositor, window);
    f.testwl.move_surface_to_output(id, &output);
    f.testwl
        .force_decoration_mode(id, zxdg_toplevel_decoration_v1::Mode::ClientSide);
    f.run();

    let content_height = 100.0 - f64::from(DecorationsDataSatellite::TITLEBAR_HEIGHT);
    let scales: [(f64, u32); 2] = [(1.5, 180), (1.25, 150)];
    for (scale, preferred) in scales {
        let data = f.testwl.get_surface_data(id).unwrap();
        data.fractional
            .as_ref()
            .expect("Missing fractional scale")
            .preferred_scale(preferred);
        f.testwl.configure_toplevel(id, 100, 100, vec![]);
        f.run();
        f.run();

        let x_width = (100.0 * scale).floor() as u16;
        let x_height = (content_height * scale) as u16;
        let dims = f.connection().window(window).dims;
        assert_eq!(
            (dims.width, dims.height),
            (x_width, x_height),
            "X window size at scale {scale}"
        );
        let data = f.testwl.get_surface_data(id).unwrap();
        let viewport = data.viewport.as_ref().unwrap();
        assert_eq!(
            (viewport.width, viewport.height),
            (100, content_height as i32),
            "viewport size at scale {scale}"
        );
    }
}

#[test]
fn client_side_decorations_no_global() {
    let mut f = TestFixture::new_pre_connect(|testwl| {
        testwl.disable_decorations_global();
    });
    let compositor = f.compositor();
    let window = Window::new(1);
    let (buffer, surface) = compositor.create_surface();

    let data = WindowData {
        mapped: true,
        dims: WindowDims {
            x: 0,
            y: 0,
            width: 50,
            height: 50,
        },
        fullscreen: false,
    };

    f.new_window(window, false, data);
    f.map_window(&compositor, window, &surface.obj, &buffer);
    f.run();

    let surfaces = f.testwl.created_surfaces();
    assert_eq!(surfaces.len(), 2);
    let mut toplevel = None;
    let mut subsurface_parent = None;
    for id in surfaces {
        let data = f.testwl.get_surface_data(*id).unwrap();
        match data
            .role
            .as_ref()
            .expect("A surface was created without a role")
        {
            SurfaceRole::Toplevel(_) => {
                toplevel = Some(*id);
            }
            SurfaceRole::Subsurface(sub) => {
                assert_eq!(sub.position, testwl::Vec2 { x: 0, y: -25 });
                subsurface_parent = Some(sub.parent);
            }
            other => panic!("got surface with unexpected role: {other:?}"),
        }
    }

    assert_eq!(toplevel.unwrap(), subsurface_parent.unwrap());
}

#[test]
fn resize_decorations_on_reconfigure() {
    let (mut f, compositor) = TestFixture::new_with_compositor();
    let window = Window::new(1);
    let (_, id) = f.create_toplevel(&compositor, window);
    f.testwl
        .force_decoration_mode(id, zxdg_toplevel_decoration_v1::Mode::ClientSide);
    f.testwl.configure_toplevel(id, 100, 100, vec![]);
    f.run();

    check_window_and_viewport_size(&f, window, id, 100, 75);

    let subsurface_id = f.testwl.last_created_surface_id().unwrap();
    assert_ne!(subsurface_id, id);
    let data = f.testwl.get_surface_data(subsurface_id).unwrap();
    let buf_dims = f
        .testwl
        .get_buffer_dimensions(data.buffer.as_ref().expect("Missing buffer for subsurface"));
    assert_eq!(buf_dims, testwl::Vec2 { x: 100, y: 25 });
    assert!(
        matches!(data.role, Some(SurfaceRole::Subsurface(_))),
        "surface was not a subsurface: {:?}",
        data.role
    );

    // Weston seems to do this when a window is deactivated. Since the stored window data's height
    // is used as a fallback in this case, do not reapply the height reduction from the titlebar.
    f.testwl.configure_toplevel(id, 0, 0, vec![]);
    f.run();
    check_window_and_viewport_size(&f, window, id, 100, 75);

    let dims = WindowDims {
        x: 0,
        y: 0,
        width: 200,
        height: 200,
    };
    f.reconfigure_window(window, dims, false);
    f.run();
    f.run();

    let data = f.testwl.get_surface_data(subsurface_id).unwrap();
    let buf_dims = f
        .testwl
        .get_buffer_dimensions(data.buffer.as_ref().expect("Missing buffer for subsurface"));
    assert_eq!(buf_dims, testwl::Vec2 { x: 200, y: 25 });
}

#[test]
fn decorations_with_title_on_thin_window() {
    let (mut f, compositor) = TestFixture::new_with_compositor();
    let window = Window::new(1);
    let (_, id) = f.create_toplevel(&compositor, window);
    f.testwl
        .force_decoration_mode(id, zxdg_toplevel_decoration_v1::Mode::ClientSide);
    f.testwl.configure_toplevel(id, 1, 100, vec![]);
    f.run();

    // Asserts no panics occur with not enough space for even a single character of the title
    f.satellite
        .set_win_title(window, WmName::WmName("window".into()));
}

#[test]
fn decorations_max_height_int_max() {
    let (mut f, compositor) = TestFixture::new_with_compositor();
    let window = Window::new(1);
    let (_, id) = f.create_toplevel(&compositor, window);
    f.testwl
        .force_decoration_mode(id, zxdg_toplevel_decoration_v1::Mode::ClientSide);
    f.testwl.configure_toplevel(id, 1, 100, vec![]);
    f.run();

    // When calculating the Wayland max size from the X size hints, the titlebar height is added to
    // the height hint, but obviously this should not overflow the size hint.
    f.satellite.set_size_hints(
        window,
        super::WmNormalHints {
            min_size: None,
            max_size: Some(WinSize {
                width: i32::MAX,
                height: i32::MAX,
            }),
        },
    );
    f.run();

    let data = f.testwl.get_surface_data(id).unwrap();
    let toplevel = data.toplevel();
    assert_eq!(
        toplevel.max_size,
        Some(testwl::Vec2 {
            x: i32::MAX,
            y: i32::MAX
        })
    );
}

/// See Pointer::handle_event for an explanation.
#[test]
fn popup_pointer_motion_workaround() {}

fn assert_relative_motion<C: XConnection>(
    f: &mut TestFixture<C>,
    relative: &TestObject<ZwpRelativePointerV1>,
    scale: f64,
) {
    f.testwl
        .relative_pointer()
        .relative_motion(17, 23, 3.5, -2.25, 7.25, -6.5);
    // Host event -> satellite client dispatch -> XWayland client delivery.
    for _ in 0..8 {
        f.run();
        if !relative.data.events.lock().unwrap().is_empty() {
            break;
        }
    }
    let events = std::mem::take(&mut *relative.data.events.lock().unwrap());
    assert_eq!(events.len(), 1);
    let zwp_relative_pointer_v1::Event::RelativeMotion {
        utime_hi,
        utime_lo,
        dx,
        dy,
        dx_unaccel,
        dy_unaccel,
    } = events[0]
    else {
        panic!("Expected relative motion")
    };
    assert_eq!((utime_hi, utime_lo), (17, 23));
    // Wayland wl_fixed transport quantizes fractional deltas to 1/256.
    assert!((dx - 3.5 * scale).abs() < 1.0 / 256.0);
    assert!((dy - -2.25 * scale).abs() < 1.0 / 256.0);
    assert_eq!((dx_unaccel, dy_unaccel), (7.25, -6.5));
}

#[test]
fn relative_motion_uses_current_surface_scale() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let pointer =
        TestObject::<WlPointer>::from_request(&comp.seat.obj, wl_seat::Request::GetPointer {});
    let relative = TestObject::<ZwpRelativePointerV1>::from_request(
        &comp.relative_pointer_man.obj,
        zwp_relative_pointer_manager_v1::Request::GetRelativePointer {
            pointer: pointer.obj.clone(),
        },
    );
    let win = Window::new(1);
    let (_, id) = f.create_toplevel(&comp, win);
    f.run();
    f.testwl.move_pointer_to(id, 0.0, 0.0);
    f.run();
    let entity = f.satellite.windows[&win];
    for scale in [1.0, 2.0, 1.5] {
        // Change the focused surface after enter. A cached pointer-enter scale
        // would be stale and would fail the 2x and 1.5x assertions below.
        f.satellite
            .world
            .get::<&mut super::SurfaceScaleFactor>(entity)
            .unwrap()
            .0 = scale;
        assert_relative_motion(&mut f, &relative, scale);
    }
}

#[test]
fn relative_motion_follows_pointer_focus_and_no_focus() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let pointer =
        TestObject::<WlPointer>::from_request(&comp.seat.obj, wl_seat::Request::GetPointer {});
    let relative = TestObject::<ZwpRelativePointerV1>::from_request(
        &comp.relative_pointer_man.obj,
        zwp_relative_pointer_manager_v1::Request::GetRelativePointer {
            pointer: pointer.obj.clone(),
        },
    );
    let mut windows = Vec::new();
    for (number, scale) in [(1, 2.0), (2, 1.5)] {
        let win = Window::new(number);
        let (surface, id) = f.create_toplevel(&comp, win);
        let entity = f.satellite.windows[&win];
        f.satellite
            .world
            .get::<&mut super::SurfaceScaleFactor>(entity)
            .unwrap()
            .0 = scale;
        windows.push((surface, id, scale));
    }
    f.run();
    assert_relative_motion(&mut f, &relative, 1.0);
    for (_, id, scale) in &windows {
        f.testwl.move_pointer_to(*id, 0.0, 0.0);
        f.run();
        assert_relative_motion(&mut f, &relative, *scale);
    }
}

fn fixed_render_fixture() -> (TestFixture<FakeXConnection>, Compositor) {
    let (mut f, comp) = TestFixture::new_with_compositor();
    // Inject policy before outputs/surfaces, avoiding process-global environment
    // mutation while other fixtures run in parallel.
    f.satellite.fixed_render_scale = Some(2);
    f.satellite.current_scale = 2.0;
    f.satellite.new_scale = Some(2.0);
    (f, comp)
}

fn pump_fixed_render(f: &mut TestFixture<FakeXConnection>) {
    for _ in 0..4 {
        f.run();
    }
}

fn assert_fixed_output_size(
    output: &TestObject<WlOutput>,
    xdg: &TestObject<ZxdgOutputV1>,
    mode: (i32, i32),
    logical: (i32, i32),
) {
    let events = output.data.events.lock().unwrap();
    let modes: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            wl_output::Event::Mode { width, height, .. } => Some((*width, *height)),
            _ => None,
        })
        .collect();
    assert!(!modes.is_empty(), "No virtual mode advertised");
    assert!(
        modes.iter().all(|size| *size == mode),
        "Unexpected mode: {modes:?}"
    );
    let events = xdg.data.events.lock().unwrap();
    let sizes: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            zxdg_output_v1::Event::LogicalSize { width, height } => Some((*width, *height)),
            _ => None,
        })
        .collect();
    assert!(!sizes.is_empty(), "No virtual logical size advertised");
    assert!(
        sizes.iter().all(|size| *size == logical),
        "Unexpected logical size: {sizes:?}"
    );
}

#[test]
fn fixed_render_scale_rejects_invalid_configuration() {
    assert_eq!(super::parse_fixed_render_scale(None), Ok(None));
    for value in ["1", "2", "3", "4"] {
        assert_eq!(
            super::parse_fixed_render_scale(Some(value)),
            Ok(Some(value.parse().unwrap()))
        );
    }
    for value in ["", "0", "-1", "1.5", "5", "NaN", "inf"] {
        assert!(
            super::parse_fixed_render_scale(Some(value)).is_err(),
            "Accepted {value}"
        );
    }
}

#[test]
fn fixed_render_geometry_handles_event_order_and_later_host_mode_changes() {
    for logical_first in [false, true] {
        let (mut f, _) = fixed_render_fixture();
        let man = f.enable_xdg_output();
        let (output, host_output) = f.new_output(0, 0);
        let xdg = f.create_xdg_output(&man, output.obj.clone());
        let host_xdg = f.testwl.get_xdg_output(&host_output).unwrap();
        output.data.events.lock().unwrap().clear();
        xdg.data.events.lock().unwrap().clear();
        if logical_first {
            host_xdg.logical_size(2648, 1490);
            host_xdg.done();
            pump_fixed_render(&mut f);
        }
        host_output.mode(s_proto::wl_output::Mode::Current, 3840, 2160, 144000);
        host_output.scale(2);
        host_output.done();
        pump_fixed_render(&mut f);
        if !logical_first {
            assert!(
                !output
                    .data
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|event| matches!(event, wl_output::Event::Mode { .. }))
            );
            host_xdg.logical_size(2648, 1490);
            host_xdg.done();
            pump_fixed_render(&mut f);
        }
        assert_fixed_output_size(&output, &xdg, (5296, 2980), (5296, 2980));
        // A later physical-mode/host-integer-scale notification must not undo
        // the authoritative virtual monitor rectangle.
        host_output.mode(s_proto::wl_output::Mode::Current, 3840, 2160, 60000);
        host_output.mode(s_proto::wl_output::Mode::Preferred, 1920, 1080, 60000);
        host_output.scale(1);
        host_output.done();
        pump_fixed_render(&mut f);
        assert_fixed_output_size(&output, &xdg, (5296, 2980), (5296, 2980));
        assert_eq!(f.satellite.current_scale, 2.0);
    }
}

#[test]
fn fixed_render_geometry_preserves_rotated_logical_rectangle() {
    let (mut f, _) = fixed_render_fixture();
    let man = f.enable_xdg_output();
    let (output, host_output) = f.new_output(0, 0);
    let xdg = f.create_xdg_output(&man, output.obj.clone());
    let host_xdg = f.testwl.get_xdg_output(&host_output).unwrap();
    output.data.events.lock().unwrap().clear();
    xdg.data.events.lock().unwrap().clear();
    host_output.geometry(
        0,
        0,
        620,
        350,
        s_proto::wl_output::Subpixel::None,
        "test".into(),
        "portrait".into(),
        s_proto::wl_output::Transform::_90,
    );
    host_output.mode(s_proto::wl_output::Mode::Current, 3840, 2160, 144000);
    host_xdg.logical_size(1490, 2648);
    host_xdg.done();
    host_output.done();
    pump_fixed_render(&mut f);
    assert_fixed_output_size(&output, &xdg, (5296, 2980), (2980, 5296));
}

#[test]
fn fixed_render_mixed_dpi_outputs_share_render_scale_and_virtual_offsets() {
    let (mut f, comp) = fixed_render_fixture();
    let man = f.enable_xdg_output();
    let (output1, host1) = f.new_output(0, 0);
    let (output2, host2) = f.new_output(2648, 0);
    let xdg1 = f.create_xdg_output(&man, output1.obj.clone());
    let xdg2 = f.create_xdg_output(&man, output2.obj.clone());
    let host_xdg1 = f.testwl.get_xdg_output(&host1).unwrap();
    let host_xdg2 = f.testwl.get_xdg_output(&host2).unwrap();
    for events in [&output1.data.events, &output2.data.events] {
        events.lock().unwrap().clear();
    }
    for events in [&xdg1.data.events, &xdg2.data.events] {
        events.lock().unwrap().clear();
    }
    host1.mode(s_proto::wl_output::Mode::Current, 3840, 2160, 144000);
    host1.scale(2); // Host 145% rounds wl_output.scale to 2.
    host_xdg1.logical_position(0, 0);
    host_xdg1.logical_size(2648, 1490);
    host2.mode(s_proto::wl_output::Mode::Current, 1920, 1080, 60000);
    host2.scale(1); // Adjacent host 100% monitor.
    host_xdg2.logical_position(2648, 0);
    host_xdg2.logical_size(1920, 1080);
    host1.done();
    host2.done();
    host_xdg1.done();
    host_xdg2.done();
    pump_fixed_render(&mut f);
    assert_fixed_output_size(&output1, &xdg1, (5296, 2980), (5296, 2980));
    assert_fixed_output_size(&output2, &xdg2, (3840, 2160), (3840, 2160));
    assert_eq!(f.satellite.current_scale, 2.0);
    assert_eq!(f.satellite.new_scale, Some(2.0));
    for output in [&output1, &output2] {
        assert!(
            output
                .data
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, wl_output::Event::Scale { factor: 2 }))
        );
        assert!(
            !output
                .data
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, wl_output::Event::Scale { factor: 1 }))
        );
    }
    assert!(
        xdg2.data
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(
                event,
                zxdg_output_v1::Event::LogicalPosition { x: 5296, y: 0 }
            ))
    );
    let win = Window::new(1);
    let (_, id) = f.create_toplevel(&comp, win);
    for host in [&host1, &host2] {
        f.testwl.move_surface_to_output(id, host);
        pump_fixed_render(&mut f);
        let surface = f.satellite.windows[&win];
        assert_eq!(
            f.satellite
                .world
                .get::<&super::SurfaceScaleFactor>(surface)
                .unwrap()
                .0,
            2.0
        );
    }
    assert_eq!(f.connection().windows[&win].dims.x, 5296);
}

#[test]
fn fixed_render_geometry_normalizes_negative_virtual_origins() {
    let (mut f, _) = fixed_render_fixture();
    let man = f.enable_xdg_output();
    let (output1, host1) = f.new_output(-2648, -100);
    let (output2, host2) = f.new_output(0, 0);
    let xdg1 = f.create_xdg_output(&man, output1.obj.clone());
    let xdg2 = f.create_xdg_output(&man, output2.obj.clone());
    f.testwl.move_xdg_output(&host1, -2648, -100);
    f.testwl.move_xdg_output(&host2, 0, 0);
    pump_fixed_render(&mut f);
    assert!(
        xdg1.data
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, zxdg_output_v1::Event::LogicalPosition { x: 0, y: 0 }))
    );
    assert!(
        xdg2.data
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(
                event,
                zxdg_output_v1::Event::LogicalPosition { x: 5296, y: 200 }
            ))
    );
    // wl_output geometry arriving later may not override the xdg source.
    f.testwl.move_output(&host1, -2000, -50);
    f.testwl.move_output(&host2, 800, 20);
    pump_fixed_render(&mut f);
    let last_geometry = |output: &TestObject<WlOutput>| {
        output
            .data
            .events
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|event| match event {
                wl_output::Event::Geometry { x, y, .. } => Some((*x, *y)),
                _ => None,
            })
    };
    assert_eq!(last_geometry(&output1), Some((0, 0)));
    assert_eq!(last_geometry(&output2), Some((5296, 200)));
}

fn cursor_buffer(comp: &Compositor, width: i32, height: i32) -> TestObject<WlBuffer> {
    let fd = unsafe { BorrowedFd::borrow_raw(0) };
    let pool = TestObject::<WlShmPool>::from_request(
        &comp.shm.obj,
        Req::<WlShm>::CreatePool {
            fd,
            size: width * height * 4,
        },
    );
    TestObject::<WlBuffer>::from_request(
        &pool.obj,
        Req::<WlShmPool>::CreateBuffer {
            offset: 0,
            width,
            height,
            stride: width * 4,
            format: WEnum::Value(Format::Argb8888),
        },
    )
}

fn cursor_surface(comp: &Compositor) -> TestObject<WlSurface> {
    TestObject::from_request(&comp.compositor.obj, Req::<WlCompositor>::CreateSurface {})
}

#[test]
fn fixed_render_cursor_maps_image_hotspot_and_resize() {
    let (mut f, comp) = fixed_render_fixture();
    let pointer =
        TestObject::<WlPointer>::from_request(&comp.seat.obj, wl_seat::Request::GetPointer {});
    let surface = cursor_surface(&comp);
    let first = cursor_buffer(&comp, 72, 72);
    pointer.set_cursor(24, Some(&surface.obj), 24, 12);
    surface.attach(Some(&first.obj), 0, 0);
    surface.set_buffer_scale(1);
    surface.commit();
    pump_fixed_render(&mut f);
    let cursor = f.testwl.pointer_cursor().unwrap();
    assert_eq!(cursor.hotspot, testwl::Vec2 { x: 12, y: 6 });
    let data = f.testwl.get_surface_data(cursor.surface).unwrap();
    assert_eq!(data.buffer_scale, 1);
    assert_eq!(
        (
            data.viewport.as_ref().unwrap().width,
            data.viewport.as_ref().unwrap().height
        ),
        (36, 36)
    );
    let replacement = cursor_buffer(&comp, 95, 73);
    surface.attach(Some(&replacement.obj), 0, 0);
    surface.commit();
    pump_fixed_render(&mut f);
    let data = f.testwl.get_surface_data(cursor.surface).unwrap();
    assert_eq!(
        (
            data.viewport.as_ref().unwrap().width,
            data.viewport.as_ref().unwrap().height
        ),
        (48, 37)
    );
    assert_eq!(
        f.testwl.pointer_cursor().unwrap().hotspot,
        testwl::Vec2 { x: 12, y: 6 }
    );
    pointer.set_cursor(24, None, 0, 0);
    pump_fixed_render(&mut f);
    assert!(f.testwl.pointer_cursor().is_none());
    surface.destroy();
    pump_fixed_render(&mut f);
    assert!(f.testwl.get_surface_data(cursor.surface).is_none());
    assert_eq!(
        f.satellite
            .world
            .query::<&super::CursorSurface>()
            .iter()
            .count(),
        0
    );
}

#[test]
fn fixed_render_cursor_preserves_existing_buffer_scale_and_late_updates() {
    let (mut f, comp) = fixed_render_fixture();
    let pointer =
        TestObject::<WlPointer>::from_request(&comp.seat.obj, wl_seat::Request::GetPointer {});
    let surface = cursor_surface(&comp);
    let buffer = cursor_buffer(&comp, 72, 72);
    // A future XWayland may supply scale=2 and already divide its hotspot.
    // Even when the scale request follows set_cursor, the result is 12, not 6.
    pointer.set_cursor(24, Some(&surface.obj), 12, 6);
    surface.attach(Some(&buffer.obj), 0, 0);
    surface.set_buffer_scale(2);
    surface.commit();
    pump_fixed_render(&mut f);
    let cursor = f.testwl.pointer_cursor().unwrap();
    assert_eq!(cursor.hotspot, testwl::Vec2 { x: 12, y: 6 });
    let data = f.testwl.get_surface_data(cursor.surface).unwrap();
    assert_eq!(data.buffer_scale, 2);
    assert_eq!(
        (
            data.viewport.as_ref().unwrap().width,
            data.viewport.as_ref().unwrap().height
        ),
        (36, 36)
    );
    // The subsequent change must refresh active cursor coordinates as well.
    surface.set_buffer_scale(1);
    surface.commit();
    pump_fixed_render(&mut f);
    assert_eq!(
        f.testwl.pointer_cursor().unwrap().hotspot,
        testwl::Vec2 { x: 6, y: 3 }
    );
    surface.attach(None, 0, 0);
    surface.commit();
    pump_fixed_render(&mut f);
    let data = f.testwl.get_surface_data(cursor.surface).unwrap();
    assert!(data.buffer.is_none());
    assert_eq!(
        (
            data.viewport.as_ref().unwrap().width,
            data.viewport.as_ref().unwrap().height
        ),
        (-1, -1)
    );
}

#[test]
fn default_cursor_image_and_hotspot_are_not_transformed() {
    let (mut f, comp) = TestFixture::new_with_compositor();
    let pointer =
        TestObject::<WlPointer>::from_request(&comp.seat.obj, wl_seat::Request::GetPointer {});
    let surface = cursor_surface(&comp);
    let buffer = cursor_buffer(&comp, 72, 72);
    pointer.set_cursor(24, Some(&surface.obj), 24, 12);
    surface.attach(Some(&buffer.obj), 0, 0);
    surface.set_buffer_scale(1);
    surface.commit();
    pump_fixed_render(&mut f);
    let cursor = f.testwl.pointer_cursor().unwrap();
    assert_eq!(cursor.hotspot, testwl::Vec2 { x: 24, y: 12 });
    let data = f.testwl.get_surface_data(cursor.surface).unwrap();
    assert_eq!(
        (
            data.viewport.as_ref().unwrap().width,
            data.viewport.as_ref().unwrap().height
        ),
        (-1, -1)
    );
    assert_eq!(
        f.satellite
            .world
            .query::<&super::CursorSurface>()
            .iter()
            .count(),
        0
    );
}

#[test]
fn fixed_render_tablet_cursor_uses_same_geometry_and_hotspot_policy() {
    let (mut f, comp) = fixed_render_fixture();
    let seat = TestObject::<ZwpTabletSeatV2>::from_request(
        &comp.tablet_man.obj,
        zwp_tablet_manager_v2::Request::GetTabletSeat {
            seat: comp.seat.obj.clone(),
        },
    );
    pump_fixed_render(&mut f);
    let tool = seat
        .data
        .events
        .lock()
        .unwrap()
        .iter()
        .find_map(|event| match event {
            zwp_tablet_seat_v2::Event::ToolAdded { id } => Some(id.clone()),
            _ => None,
        })
        .expect("Missing tablet tool");
    let surface = cursor_surface(&comp);
    let buffer = cursor_buffer(&comp, 72, 72);
    tool.set_cursor(24, Some(&surface.obj), 24, 12);
    surface.attach(Some(&buffer.obj), 0, 0);
    surface.set_buffer_scale(1);
    surface.commit();
    pump_fixed_render(&mut f);
    let cursor = f.testwl.tablet_cursor().unwrap();
    assert_eq!(cursor.hotspot, testwl::Vec2 { x: 12, y: 6 });
    let data = f.testwl.get_surface_data(cursor.surface).unwrap();
    assert_eq!(
        (
            data.viewport.as_ref().unwrap().width,
            data.viewport.as_ref().unwrap().height
        ),
        (36, 36)
    );
    tool.set_cursor(24, None, 0, 0);
    pump_fixed_render(&mut f);
    assert!(f.testwl.tablet_cursor().is_none());
}

#[test]
fn fixed_render_superseded_cursor_updates_do_not_replace_active_cursor() {
    let (mut f, comp) = fixed_render_fixture();
    let pointer =
        TestObject::<WlPointer>::from_request(&comp.seat.obj, wl_seat::Request::GetPointer {});
    let old_surface = cursor_surface(&comp);
    let old_buffer = cursor_buffer(&comp, 72, 72);
    pointer.set_cursor(24, Some(&old_surface.obj), 24, 12);
    old_surface.attach(Some(&old_buffer.obj), 0, 0);
    old_surface.commit();
    pump_fixed_render(&mut f);
    let old_id = f.testwl.pointer_cursor().unwrap().surface;
    let new_surface = cursor_surface(&comp);
    let new_buffer = cursor_buffer(&comp, 96, 96);
    pointer.set_cursor(24, Some(&new_surface.obj), 32, 16);
    new_surface.attach(Some(&new_buffer.obj), 0, 0);
    new_surface.commit();
    pump_fixed_render(&mut f);
    let active = f.testwl.pointer_cursor().unwrap();
    assert_ne!(active.surface, old_id);
    assert_eq!(active.hotspot, testwl::Vec2 { x: 16, y: 8 });
    old_surface.set_buffer_scale(2);
    old_surface.commit();
    pump_fixed_render(&mut f);
    assert_eq!(f.testwl.pointer_cursor(), Some(active));
    old_surface.destroy();
    pump_fixed_render(&mut f);
    assert_eq!(f.testwl.pointer_cursor(), Some(active));
}

#[test]
fn ui_multiplier_rejects_invalid_configuration() {
    assert_eq!(super::parse_ui_multiplier(None, None), Ok(1.0));
    for value in ["1", "1.0344827586206897", "1.103448275862069"] {
        assert!(super::parse_ui_multiplier(Some(value), Some(2)).is_ok());
    }
    for value in ["", "0", "-1", "NaN", "inf", "-inf", "1e-320"] {
        assert!(
            super::parse_ui_multiplier(Some(value), Some(2)).is_err(),
            "Accepted {value}"
        );
    }
    assert!(super::parse_ui_multiplier(Some("1.1"), None).is_err());
}

#[test]
fn ui_multiplier_scales_windows_and_mixed_outputs_but_preserves_density_and_cursor() {
    for target_percent in [145.0, 150.0, 160.0] {
        let (mut f, comp) = fixed_render_fixture();
        f.satellite.ui_multiplier = target_percent / 145.0;
        let mapping = 2.0 / f.satellite.ui_multiplier;
        f.satellite.current_scale = mapping;
        f.satellite.new_scale = Some(mapping);
        let man = f.enable_xdg_output();
        let (output1, host1) = f.new_output(0, 0);
        let (output2, host2) = f.new_output(2648, 0);
        let xdg1 = f.create_xdg_output(&man, output1.obj.clone());
        let xdg2 = f.create_xdg_output(&man, output2.obj.clone());
        for events in [&output1.data.events, &output2.data.events] {
            events.lock().unwrap().clear();
        }
        for events in [&xdg1.data.events, &xdg2.data.events] {
            events.lock().unwrap().clear();
        }
        host1.mode(s_proto::wl_output::Mode::Current, 3840, 2160, 144000);
        host1.scale(2);
        let host_xdg1 = f.testwl.get_xdg_output(&host1).unwrap();
        host_xdg1.logical_size(2648, 1490);
        host_xdg1.logical_position(0, 0);
        host2.mode(s_proto::wl_output::Mode::Current, 1920, 1080, 60000);
        host2.scale(1);
        let host_xdg2 = f.testwl.get_xdg_output(&host2).unwrap();
        host_xdg2.logical_size(1920, 1080);
        host_xdg2.logical_position(2648, 0);
        host1.done();
        host2.done();
        host_xdg1.done();
        host_xdg2.done();
        pump_fixed_render(&mut f);
        let first_size = (
            (2648.0 * mapping).round() as i32,
            (1490.0 * mapping).round() as i32,
        );
        let second_size = (
            ((2648.0 + 1920.0) * mapping).round() as i32 - (2648.0 * mapping).round() as i32,
            (1080.0 * mapping).round() as i32,
        );
        assert_fixed_output_size(&output1, &xdg1, first_size, first_size);
        assert_fixed_output_size(&output2, &xdg2, second_size, second_size);
        assert_eq!(f.satellite.current_scale, mapping);
        assert_eq!(
            f.satellite.new_global_scale(),
            Some(2.0),
            "Xsettings must retain integer rendering density"
        );
        assert!(
            xdg2.data
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event,
            zxdg_output_v1::Event::LogicalPosition { x, y: 0 } if *x == first_size.0))
        );
        for output in [&output1, &output2] {
            assert!(
                output
                    .data
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|event| !matches!(event,
                wl_output::Event::Scale { factor } if *factor != 2))
            );
        }
        let pointer =
            TestObject::<WlPointer>::from_request(&comp.seat.obj, wl_seat::Request::GetPointer {});
        let relative = TestObject::<ZwpRelativePointerV1>::from_request(
            &comp.relative_pointer_man.obj,
            zwp_relative_pointer_manager_v1::Request::GetRelativePointer {
                pointer: pointer.obj.clone(),
            },
        );
        let win = Window::new(1);
        let (_, id) = f.create_toplevel(&comp, win);
        f.testwl.move_surface_to_output(id, &host1);
        f.testwl.move_pointer_to(id, 10.0, 20.0);
        pump_fixed_render(&mut f);
        assert_eq!(
            f.satellite
                .world
                .get::<&super::SurfaceScaleFactor>(f.satellite.windows[&win])
                .unwrap()
                .0,
            mapping
        );
        let width = f.connection().window(win).dims.width;
        assert_eq!(width, (100.0 * mapping) as u16);
        let data = f.testwl.get_surface_data(id).unwrap();
        let viewport = data.viewport.as_ref().unwrap();
        assert_eq!(viewport.width, 100);
        let events = pointer.data.events.lock().unwrap();
        let (x, y) = events
            .iter()
            .find_map(|event| match event {
                wayland_client::protocol::wl_pointer::Event::Enter {
                    surface_x,
                    surface_y,
                    ..
                } => Some((*surface_x, *surface_y)),
                _ => None,
            })
            .expect("Pointer enter not delivered");
        assert!((x - 10.0 * mapping).abs() < 1.0 / 128.0);
        assert!((y - 20.0 * mapping).abs() < 1.0 / 128.0);
        drop(events);
        assert_relative_motion(&mut f, &relative, mapping);
        // A 100-point Unity element renders B*100 X pixels and appears at
        // the requested physical percentage, independent of its window mapping.
        assert!((200.0 / mapping * 1.45 - target_percent).abs() < 1e-9);
        let cursor = cursor_surface(&comp);
        let buffer = cursor_buffer(&comp, 72, 72);
        pointer.set_cursor(24, Some(&cursor.obj), 24, 12);
        cursor.attach(Some(&buffer.obj), 0, 0);
        cursor.commit();
        pump_fixed_render(&mut f);
        let current = f.testwl.pointer_cursor().unwrap();
        assert_eq!(current.hotspot, testwl::Vec2 { x: 12, y: 6 });
        let data = f.testwl.get_surface_data(current.surface).unwrap();
        assert_eq!(
            data.viewport.as_ref().unwrap().width,
            36,
            "Cursor must follow desktop size, not app UI multiplier"
        );
    }
}

#[test]
fn ui_multiplier_rounds_shared_output_edges_and_position_only_updates() {
    let (mut f, _) = fixed_render_fixture();
    f.satellite.ui_multiplier = 150.0 / 145.0;
    let mapping = 2.0 / f.satellite.ui_multiplier;
    f.satellite.current_scale = mapping;
    let man = f.enable_xdg_output();
    let mut outputs = Vec::new();
    for origin in [-1000, 0, 1000, 2000] {
        let (output, host) = f.new_output(origin, 0);
        let xdg = f.create_xdg_output(&man, output.obj.clone());
        let host_xdg = f.testwl.get_xdg_output(&host).unwrap();
        host_xdg.logical_position(origin, 0);
        host_xdg.logical_size(1000, 800);
        host.mode(s_proto::wl_output::Mode::Current, 1450, 1160, 60000);
        host.done();
        host_xdg.done();
        pump_fixed_render(&mut f);
        outputs.push((output, xdg, host_xdg));
    }
    let latest_mode = |output: &TestObject<WlOutput>| {
        output
            .data
            .events
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|event| match event {
                wl_output::Event::Mode { width, height, .. } => Some((*width, *height)),
                _ => None,
            })
            .unwrap()
    };
    let latest_position = |xdg: &TestObject<ZxdgOutputV1>| {
        xdg.data
            .events
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|event| match event {
                zxdg_output_v1::Event::LogicalPosition { x, y } => Some((*x, *y)),
                _ => None,
            })
            .unwrap()
    };
    for pair in outputs.windows(2) {
        let left = latest_position(&pair[0].1).0;
        let width = latest_mode(&pair[0].0).0;
        let right = latest_position(&pair[1].1).0;
        assert_eq!(
            left + width,
            right,
            "Shared virtual monitor edge must be exact"
        );
    }
    assert_eq!(latest_mode(&outputs[2].0).0, 1934);
    // Moving an output without changing its size changes the rounding phase.
    outputs[2].2.logical_position(2000, 0);
    outputs[2].2.done();
    pump_fixed_render(&mut f);
    assert_eq!(latest_mode(&outputs[2].0).0, 1933);
}

#[test]
fn ui_multiplier_native_popup_sizes_survive_roundtrip_and_reposition_without_growth() {
    for target_percent in [145.0, 150.0, 160.0] {
        let (mut f, comp) = fixed_render_fixture();
        f.satellite.ui_multiplier = target_percent / 145.0;
        let mapping = 2.0 / f.satellite.ui_multiplier;
        f.satellite.current_scale = mapping;
        let toplevel = Window::new(1);
        let (_, t_id) = f.create_toplevel(&comp, toplevel);
        let popup = Window::new(2);
        let (_, p_id) = f.create_popup(
            &comp,
            PopupBuilder::new(popup, toplevel, t_id)
                .width(124)
                .height(88)
                .scale(mapping)
                .check_size_and_pos(false),
        );
        let check_size = |f: &TestFixture<FakeXConnection>, width: u16, height: u16| {
            let dims = f.connection().window(popup).dims;
            assert_eq!(
                (dims.width, dims.height),
                (width, height),
                "Native popup allocation shrank or grew at target{target_percent}"
            );
            let data = f.testwl.get_surface_data(p_id).unwrap();
            let logical = testwl::Vec2 {
                x: (f64::from(width) / mapping).ceil() as i32,
                y: (f64::from(height) / mapping).ceil() as i32,
            };
            assert_eq!(data.popup().positioner_state.size, Some(logical));
            let viewport = data.viewport.as_ref().unwrap();
            assert_eq!(
                (viewport.width, viewport.height),
                (logical.x, logical.y),
                "Popup positioner and allocated viewport must agree"
            );
        };
        check_size(&f, 124, 88);
        // Include tiny allocations, the two-row GTK menu range, odd sizes and
        // fractions that previously lost one native X pixel on each roundtrip.
        for width in [1, 97, 124, 255] {
            for height in [1, 2, 3, 63, 64, 84, 88, 97, 99, 101, 124, 125] {
                let current = f.connection().window(popup).dims;
                f.reconfigure_window(
                    popup,
                    WindowDims {
                        width,
                        height,
                        ..current
                    },
                    true,
                );
                pump_fixed_render(&mut f);
                check_size(&f, width, height);
                for _ in 0..3 {
                    f.testwl.configure_popup(p_id);
                    pump_fixed_render(&mut f);
                    check_size(&f, width, height);
                }
            }
        }
        // A compositor that genuinely chooses a different logical size must
        // still constrain the popup rather than being overridden by its request.
        f.testwl.configure_popup_size(p_id, 40, 30);
        pump_fixed_render(&mut f);
        let dims = f.connection().window(popup).dims;
        assert_eq!(
            (dims.width, dims.height),
            ((40.0 * mapping) as u16, (30.0 * mapping) as u16)
        );
    }
}
