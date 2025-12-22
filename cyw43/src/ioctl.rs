use core::cell::{Cell, RefCell};
use core::future::{Future, poll_fn};
use core::task::{Poll, Waker};

use embassy_sync::waitqueue::WakerRegistration;

use crate::consts::Ioctl;


#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IoctlType {
    Get = 0,
    Set = 2,
}

/// IOCTL Error wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum IoctlError {
    /// Generic IOCTL failure with status code.
    Status(core::num::NonZeroI32),
}

impl From<i32> for IoctlError {
    fn from(status: i32) -> Self {
        match core::num::NonZeroI32::new(status) {
            Some(n) => IoctlError::Status(n),
            // Fallback for 0, although logic shouldn't produce Err(0).
            None => IoctlError::Status(unsafe { core::num::NonZeroI32::new_unchecked(i32::MAX) }),
        }
    }
}

impl From<IoctlError> for i32 {
    fn from(e: IoctlError) -> i32 {
        match e {
            IoctlError::Status(n) => n.get(),
        }
    }
}

#[derive(Clone, Copy)]
pub struct PendingIoctl {
    pub buf: *mut [u8],
    pub kind: IoctlType,
    pub cmd: Ioctl,
    pub iface: u32,
}

#[derive(Clone, Copy)]
enum IoctlStateInner {
    Pending(PendingIoctl),
    Sent { buf: *mut [u8] },
    Done { result: Result<usize, IoctlError> },
}

struct Wakers {
    control: WakerRegistration,
    runner: WakerRegistration,
}

impl Wakers {
    const fn new() -> Self {
        Self {
            control: WakerRegistration::new(),
            runner: WakerRegistration::new(),
        }
    }
}

pub struct IoctlState {
    state: Cell<IoctlStateInner>,
    wakers: RefCell<Wakers>,
}

impl IoctlState {
    pub const fn new() -> Self {
        Self {
            state: Cell::new(IoctlStateInner::Done { result: Ok(0) }),
            wakers: RefCell::new(Wakers::new()),
        }
    }

    fn wake_control(&self) {
        self.wakers.borrow_mut().control.wake();
    }

    fn register_control(&self, waker: &Waker) {
        self.wakers.borrow_mut().control.register(waker);
    }

    fn wake_runner(&self) {
        self.wakers.borrow_mut().runner.wake();
    }

    fn register_runner(&self, waker: &Waker) {
        self.wakers.borrow_mut().runner.register(waker);
    }

    pub fn wait_complete(&self) -> impl Future<Output = Result<usize, IoctlError>> + '_ {
        poll_fn(|cx| {
            if let IoctlStateInner::Done { result } = self.state.get() {
                Poll::Ready(result)
            } else {
                self.register_control(cx.waker());
                Poll::Pending
            }
        })
    }

    pub fn wait_pending(&self) -> impl Future<Output = PendingIoctl> + '_ {
        poll_fn(|cx| {
            if let IoctlStateInner::Pending(pending) = self.state.get() {
                self.state.set(IoctlStateInner::Sent { buf: pending.buf });
                Poll::Ready(pending)
            } else {
                self.register_runner(cx.waker());
                Poll::Pending
            }
        })
    }

    pub fn cancel_ioctl(&self) {
        self.state.set(IoctlStateInner::Done { result: Ok(0) });
    }

    pub async fn do_ioctl(
        &self,
        kind: IoctlType,
        cmd: Ioctl,
        iface: u32,
        buf: &mut [u8],
    ) -> Result<usize, IoctlError> {
        self.state
            .set(IoctlStateInner::Pending(PendingIoctl { buf, kind, cmd, iface }));
        self.wake_runner();
        self.wait_complete().await
    }

    pub fn ioctl_done(&self, response: &[u8], result: Result<(), IoctlError>) {
        if let IoctlStateInner::Sent { buf } = self.state.get() {
            // Check that the buffer is valid!
            let buf = unsafe { &mut *buf };

            let result = match result {
                Ok(()) => {
                    let len = core::cmp::min(buf.len(), response.len());
                    buf[..len].copy_from_slice(&response[..len]);
                    Ok(len)
                },
                Err(e) => Err(e),
            };

            self.state.set(IoctlStateInner::Done { result });
            self.wake_control();
        } else {
            warn!("IOCTL Response but no pending Ioctl");
        }
    }
}
