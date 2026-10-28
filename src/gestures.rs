//! Touchpad pinch on Wayland.
//!
//! winit 0.30 doesn't expose pointer gestures on Linux, so we bind `zwp_pointer_gestures_v1`
//! on winit's own `wl_display` with a separate event queue, dispatched on a background
//! thread (libwayland supports several readers on one connection). Can go away once we
//! move to winit 0.31, which handles this itself.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use wayland_backend::sys::client::Backend;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_pointer::WlPointer, wl_registry::WlRegistry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::wp::pointer_gestures::zv1::client::{
    zwp_pointer_gesture_pinch_v1::{self, ZwpPointerGesturePinchV1},
    zwp_pointer_gestures_v1::ZwpPointerGesturesV1,
};

#[derive(Debug, Clone, Copy)]
pub enum Pinch {
    Begin,
    /// `scale` is cumulative since `Begin` (1.0 = unchanged, >1 = fingers spreading).
    Update { scale: f64 },
    End,
}

struct State {
    tx: Sender<Pinch>,
    wake: Arc<dyn Fn() + Send + Sync>,
    gestures: ZwpPointerGesturesV1,
    pointer: Option<(WlPointer, ZwpPointerGesturePinchV1)>,
}

impl State {
    fn send(&self, p: Pinch) {
        let _ = self.tx.send(p);
        (self.wake)();
    }
}

/// Starts listening for pinches. `display` is the raw `wl_display*` winit is using.
/// Returns false if the compositor doesn't offer pointer gestures.
pub fn start(display: *mut c_void, tx: Sender<Pinch>, wake: Arc<dyn Fn() + Send + Sync>) -> bool {
    // SAFETY: winit owns the display and keeps it alive for the whole run of the app.
    let backend = unsafe { Backend::from_foreign_display(display.cast()) };
    let conn = Connection::from_backend(backend);
    let Ok((globals, mut queue)) = registry_queue_init::<State>(&conn) else { return false };
    let qh = queue.handle();
    let Ok(gestures) = globals.bind::<ZwpPointerGesturesV1, _, _>(&qh, 1..=3, ()) else {
        log::info!("compositor has no pointer gestures; pinch zoom disabled");
        return false;
    };
    let Ok(_seat) = globals.bind::<wl_seat::WlSeat, _, _>(&qh, 1..=7, ()) else { return false };
    let mut state = State { tx, wake, gestures, pointer: None };
    std::thread::Builder::new()
        .name("wayland-gestures".into())
        .spawn(move || {
            while queue.blocking_dispatch(&mut state).is_ok() {}
        })
        .is_ok()
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &WlRegistry, _: <WlRegistry as wayland_client::Proxy>::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(state: &mut Self, seat: &wl_seat::WlSeat, event: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = event {
            if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                let pointer = seat.get_pointer(qh, ());
                let pinch = state.gestures.get_pinch_gesture(&pointer, qh, ());
                state.pointer = Some((pointer, pinch));
            }
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    // winit handles the pointer itself; this one only exists to hang gestures off.
    fn event(_: &mut Self, _: &WlPointer, _: <WlPointer as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwpPointerGesturesV1, ()> for State {
    fn event(_: &mut Self, _: &ZwpPointerGesturesV1, _: <ZwpPointerGesturesV1 as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwpPointerGesturePinchV1, ()> for State {
    fn event(state: &mut Self, _: &ZwpPointerGesturePinchV1, event: zwp_pointer_gesture_pinch_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            zwp_pointer_gesture_pinch_v1::Event::Begin { .. } => state.send(Pinch::Begin),
            zwp_pointer_gesture_pinch_v1::Event::Update { scale, .. } => state.send(Pinch::Update { scale }),
            zwp_pointer_gesture_pinch_v1::Event::End { .. } => state.send(Pinch::End),
            _ => {}
        }
    }
}
