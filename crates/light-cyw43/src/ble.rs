//! The short-range radio's host stack: advertising, a connection, and a way to measure what the
//! link will actually carry.
//!
//! WHAT IS HERE AND WHAT IS NOT. The stack itself is a dependency -- advertising, connections and
//! the attribute protocol are a large, well-served body of work, and the framework consumes it
//! rather than writing it. What is here is the hosting: giving it a controller built from the
//! part's own driver, somewhere static to keep its resources, and a pass of its runner from the
//! loop that owns the radio, because this framework has no executor to give it.
//!
//! WHICH VERSION OF THE STACK IS NOT A PREFERENCE. It and the part's driver must resolve to one
//! version of the protocol crate between them; two are distinct types and the driver's transport
//! then cannot satisfy the stack's bound at all. See the manifest for the arithmetic.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use bt_hci::controller::ExternalController;
use light_core::StaticCell;
use trouble_host::prelude::*;

use crate::{noop_waker, place_task, TaskStorage};

/// How many commands the controller bridge may have outstanding. The stack issues them one at a
/// time during bring-up; this is headroom, not a throughput knob.
const HCI_SLOTS: usize = 4;
/// One connection: this part is updated and played by one thing at a time, and every connection
/// carries buffers whose cost is paid whether or not anyone connects.
const CONNECTIONS: usize = 1;
/// The attribute protocol's own channel, and room beside it.
const CHANNELS: usize = 2;

type Controller = ExternalController<cyw43::bluetooth::BtDriver<'static>, HCI_SLOTS>;

/// The stack's own memory. Static because the stack borrows it for as long as it lives, and far too
/// large for the application core's stack -- see the note on the wireless side's packet pool.
static RESOURCES: StaticCell<HostResources<DefaultPacketPool, CONNECTIONS, CHANNELS>> = StaticCell::new();
static STACK: StaticCell<Stack<'static, Controller, DefaultPacketPool>> = StaticCell::new();
static BLE_TASK: StaticCell<TaskStorage> = StaticCell::new();

/// The short-range radio, hosting a stack and advertising.
pub struct Ble {
        peripheral: Peripheral<'static, Controller, DefaultPacketPool>,
        /// The stack's own work, driven a pass at a time like every other future here.
        task: Pin<&'static mut dyn Future<Output = ()>>,
        //   HELD, NOT HANDED BACK. A connection ends when it is dropped, so returning one to a
        // caller that has nowhere to keep it would disconnect the moment they looked at it. Keeping
        // it here also keeps the stack's types out of every board that uses this.
        connection: Option<Connection<'static, DefaultPacketPool>>,
}

impl Ble {
        /// Host a stack on the controller, using `address` as the one this device answers to.
        ///
        /// Takes the part's short-range handle, because from here on the stack owns the
        /// conversation with it: the raw question-and-answer used to read the address at bring-up
        /// cannot share a controller with a stack that is also reading its events.
        pub fn new(bt: cyw43::bluetooth::BtDriver<'static>, address: [u8; 6]) -> Self {
                let controller: Controller = ExternalController::new(bt);
                //   BUILT WHERE IT LIVES, not built here and moved: the same reason the wireless
                // side's state is. These resources are kilobytes and the core's stack is eight
                let resources = RESOURCES.init_with(HostResources::new);
                //   NO ADDRESS IS SET, and that is the fix rather than an omission. The obvious
                // thing is to hand the stack the address the part reported at bring-up -- but the
                // only kind that can be set here is a RANDOM one, and a static random address is
                // required to have the top two bits of its first byte set. A public address does
                // not, and this part's does not, so setting it produced a controller that accepted
                // every advertising command and then put nothing on the air: `advertise` returned
                // Ok, `accept` waited the full thirty seconds, and a scanner two feet away heard
                // silence. Left unset, the stack advertises as the part's own public address, which
                // is the device's real identity anyway.
                let _ = address;
                let stack = STACK.init(trouble_host::new(controller, resources));
                let Host { peripheral, runner, .. } = stack.build();
                Self { task: place_task(&BLE_TASK, async move { run(runner).await }), peripheral, connection: None }
        }

        /// Give the stack its pass of the runtime loop.
        pub fn poll(&mut self) {
                let waker = noop_waker();
                let mut cx = Context::from_waker(&waker);
                let _ = self.task.as_mut().poll(&mut cx);
        }

        /// Advertise under `name` and wait for something to connect, or give up.
        ///
        /// Blocking, like the wireless side's join: there is nothing for the rest of the firmware
        /// to do while a radio waits to be found that it would not have to be woken for anyway.
        ///   `pump` MUST DRIVE THE PART'S OWN BUS, and nothing works without it. The short-range
        /// side shares one serial bus with the wireless side, and it is the part driver's task that
        /// carries packets over it -- so a stack polled on its own composes perfect commands that
        /// never leave the chip. Blocking here is what makes it this call's problem: the loop that
        /// would otherwise give the driver its pass is the one waiting on this.
        pub fn accept(&mut self, name: &str, timeout_us: u64, pump: &mut dyn FnMut()) -> bool {
                let Ble { peripheral, task, connection } = self;
                let waker = noop_waker();
                let mut cx = Context::from_waker(&waker);

                //   the advertisement carries the name, because a device nobody can identify in a
                // list of nearby radios is a device nobody connects to
                let mut adv_data = [0u8; 31];
                let Ok(len) = AdStructure::encode_slice(
                        &[AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED), AdStructure::CompleteLocalName(name.as_bytes())],
                        &mut adv_data[..],
                ) else {
                        //   the advertisement is thirty-one bytes and a long name does not fit; that
                        // is the caller's to shorten rather than something to announce without
                        return false;
                };

                let work = async {
                        let advertiser = peripheral
                                .advertise(&Default::default(), Advertisement::ConnectableScannableUndirected { adv_data: &adv_data[..len], scan_data: &[] })
                                .await
                                .ok()?;
                        advertiser.accept().await.ok()
                };

                let deadline = crate::now_us().saturating_add(timeout_us);
                let mut work = core::pin::pin!(work);
                loop {
                        pump();
                        let _ = task.as_mut().poll(&mut cx);
                        if let Poll::Ready(v) = work.as_mut().poll(&mut cx) {
                                *connection = v;
                                return connection.is_some();
                        }
                        if crate::now_us() >= deadline {
                                return false;
                        }
                }
        }

        /// Whether something is connected right now.
        pub fn connected(&self) -> bool {
                self.connection.as_ref().is_some_and(|c| c.is_connected())
        }
}

/// The stack's runner, wrapped so its answer is nothing and it can be named for a static.
///
///   IT IS NOT SUPPOSED TO RETURN, so the interesting case is the one where it does -- and
/// discarding that was expensive. A runner that gives up leaves a stack which accepts every request
/// and puts nothing on the air: `advertise` succeeds, `accept` waits out its whole deadline, and a
/// scanner beside the board hears nothing, with no line anywhere saying why. Said out loud, once,
/// because after it stops there is nothing further to report.
async fn run(mut runner: Runner<'static, Controller, DefaultPacketPool>) {
        match runner.run().await {
                Ok(()) => light_core::warn!("radio: the short-range stack stopped of its own accord"),
                Err(e) => light_core::warn!("radio: the short-range stack stopped ({e:?})"),
        }
}
