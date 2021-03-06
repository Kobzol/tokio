//! Windows-specific types for signal handling.
//!
//! This module is only defined on Windows and contains the primary `Event` type
//! for receiving notifications of events. These events are listened for via the
//! `SetConsoleCtrlHandler` function which receives events of the type
//! `CTRL_C_EVENT` and `CTRL_BREAK_EVENT`

#![cfg(windows)]

use std::convert::TryFrom;
use std::io;
use std::pin::Pin;
use std::sync::Once;
use std::task::{Context, Poll};

use futures_core::stream::Stream;
use tokio_reactor::Handle;
use tokio_sync::mpsc::{channel, Receiver};
use winapi::shared::minwindef::*;
use winapi::um::consoleapi::SetConsoleCtrlHandler;
use winapi::um::wincon::*;

use crate::registry::{globals, EventId, EventInfo, Init, Storage};

#[derive(Debug)]
pub(crate) struct OsStorage {
    ctrl_c: EventInfo,
    ctrl_break: EventInfo,
}

impl Init for OsStorage {
    fn init() -> Self {
        Self {
            ctrl_c: EventInfo::default(),
            ctrl_break: EventInfo::default(),
        }
    }
}

impl Storage for OsStorage {
    fn event_info(&self, id: EventId) -> Option<&EventInfo> {
        match DWORD::try_from(id) {
            Ok(CTRL_C_EVENT) => Some(&self.ctrl_c),
            Ok(CTRL_BREAK_EVENT) => Some(&self.ctrl_break),
            _ => None,
        }
    }

    fn for_each<'a, F>(&'a self, mut f: F)
    where
        F: FnMut(&'a EventInfo),
    {
        f(&self.ctrl_c);
        f(&self.ctrl_break);
    }
}

#[derive(Debug)]
pub(crate) struct OsExtraData {}

impl Init for OsExtraData {
    fn init() -> Self {
        Self {}
    }
}

/// Stream of events discovered via `SetConsoleCtrlHandler`.
///
/// This structure can be used to listen for events of the type `CTRL_C_EVENT`
/// and `CTRL_BREAK_EVENT`. The `Stream` trait is implemented for this struct
/// and will resolve for each notification received by the process. Note that
/// there are few limitations with this as well:
///
/// * A notification to this process notifies *all* `Event` streams for that
///   event type.
/// * Notifications to an `Event` stream **are coalesced** if they aren't
///   processed quickly enough. This means that if two notifications are
///   received back-to-back, then the stream may only receive one item about the
///   two notifications.
// FIXME: refactor and combine with unix::Signal
#[must_use = "streams do nothing unless polled"]
#[derive(Debug)]
pub(crate) struct Event {
    rx: Receiver<()>,
}

impl Event {
    /// Creates a new stream listening for the `CTRL_C_EVENT` events.
    ///
    /// This function will register a handler via `SetConsoleCtrlHandler` and
    /// deliver notifications to the returned stream.
    pub(crate) fn ctrl_c(handle: &Handle) -> io::Result<Self> {
        Event::new(CTRL_C_EVENT, handle)
    }

    /// Creates a new stream listening for the `CTRL_BREAK_EVENT` events.
    ///
    /// This function will register a handler via `SetConsoleCtrlHandler` and
    /// deliver notifications to the returned stream.
    fn ctrl_break_handle(handle: &Handle) -> io::Result<Self> {
        Event::new(CTRL_BREAK_EVENT, handle)
    }

    fn new(signum: DWORD, _handle: &Handle) -> io::Result<Self> {
        global_init()?;

        let (tx, rx) = channel(1);
        globals().register_listener(signum as EventId, tx);

        Ok(Event { rx })
    }
}

impl Stream for Event {
    type Item = ();

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

fn global_init() -> io::Result<()> {
    static INIT: Once = Once::new();

    let mut init = None;
    INIT.call_once(|| unsafe {
        let rc = SetConsoleCtrlHandler(Some(handler), TRUE);
        let ret = if rc == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        };

        init = Some(ret);
    });

    init.unwrap_or_else(|| Ok(()))
}

unsafe extern "system" fn handler(ty: DWORD) -> BOOL {
    let globals = globals();
    globals.record_event(ty as EventId);

    // According to https://docs.microsoft.com/en-us/windows/console/handlerroutine
    // the handler routine is always invoked in a new thread, thus we don't
    // have the same restrictions as in Unix signal handlers, meaning we can
    // go ahead and perform the broadcast here.
    if globals.broadcast() {
        TRUE
    } else {
        // No one is listening for this notification any more
        // let the OS fire the next (possibly the default) handler.
        FALSE
    }
}

/// Represents a stream which receives "ctrl-break" notifications sent to the process
/// via `SetConsoleCtrlHandler`.
///
/// A notification to this process notifies *all* streams listening to
/// this event. Moreover, the notifications **are coalesced** if they aren't processed
/// quickly enough. This means that if two notifications are received back-to-back,
/// then the stream may only receive one item about the two notifications.
#[must_use = "streams do nothing unless polled"]
#[derive(Debug)]
pub struct CtrlBreak {
    inner: Event,
}

impl CtrlBreak {
    /// Creates a new stream which receives "ctrl-break" notifications sent to the
    /// process.
    ///
    /// This function binds to the default reactor.
    pub fn new() -> io::Result<Self> {
        Self::with_handle(&Handle::default())
    }

    /// Creates a new stream which receives "ctrl-break" notifications sent to the
    /// process.
    ///
    /// This function binds to reactor specified by `handle`.
    pub fn with_handle(handle: &Handle) -> io::Result<Self> {
        Event::ctrl_break_handle(handle).map(|inner| Self { inner })
    }
}

impl Stream for CtrlBreak {
    type Item = ();

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner)
            .poll_next(cx)
            .map(|item| item.map(|_| ()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::future::FutureExt;
    use futures_util::stream::StreamExt;
    use std::future::Future;
    use std::time::Duration;
    use tokio_timer::Timeout;

    fn with_timeout<F: Future>(future: F) -> impl Future<Output = F::Output> {
        Timeout::new(future, Duration::from_secs(1)).map(|result| result.expect("timed out"))
    }

    #[tokio::test]
    async fn ctrl_c() {
        let ctrl_c = crate::CtrlC::new().expect("failed to create CtrlC");

        // Windows doesn't have a good programmatic way of sending events
        // like sending signals on Unix, so we'll stub out the actual OS
        // integration and test that our handling works.
        unsafe {
            super::handler(CTRL_C_EVENT);
        }

        let _ = with_timeout(ctrl_c.into_future()).await;
    }

    #[tokio::test]
    async fn ctrl_break() {
        let ctrl_break = super::CtrlBreak::new().expect("failed to create CtrlC");

        // Windows doesn't have a good programmatic way of sending events
        // like sending signals on Unix, so we'll stub out the actual OS
        // integration and test that our handling works.
        unsafe {
            super::handler(CTRL_BREAK_EVENT);
        }

        let _ = with_timeout(ctrl_break.into_future()).await;
    }
}
