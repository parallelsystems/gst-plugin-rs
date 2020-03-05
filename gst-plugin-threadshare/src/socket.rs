// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
// Copyright (C) 2018 LEE Dongjun <redongjun@gmail.com>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

use either::Either;

use futures::future::{abortable, AbortHandle, Aborted, BoxFuture};
use futures::lock::Mutex;

use gst;
use gst::prelude::*;
use gst::{gst_debug, gst_error, gst_error_msg};

use lazy_static::lazy_static;

use std::io;
use std::sync::Arc;

use gio;
use gio::prelude::*;
use gio_sys as gio_ffi;
use gobject_sys as gobject_ffi;

#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};

#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket, IntoRawSocket, RawSocket};

lazy_static! {
    static ref SOCKET_CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-socket",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing Socket"),
    );
}

#[derive(Debug)]
pub struct Socket<T: SocketRead + 'static>(Arc<Mutex<SocketInner<T>>>);

pub trait SocketRead: Send + Unpin {
    const DO_TIMESTAMP: bool;

    fn read<'buf>(
        &self,
        buffer: &'buf mut [u8],
    ) -> BoxFuture<'buf, io::Result<(usize, Option<std::net::SocketAddr>)>>;
}

#[derive(PartialEq, Eq, Debug)]
enum SocketState {
    Paused,
    Prepared,
    Started,
    Unprepared,
}

#[derive(Debug)]
struct SocketInner<T: SocketRead + 'static> {
    state: SocketState,
    element: gst::Element,
    reader: T,
    buffer_pool: gst::BufferPool,
    clock: Option<gst::Clock>,
    base_time: Option<gst::ClockTime>,
    read_handle: Option<AbortHandle>,
}

impl<T: SocketRead + 'static> Socket<T> {
    pub fn new(element: &gst::Element, reader: T, buffer_pool: gst::BufferPool) -> Self {
        Socket(Arc::new(Mutex::new(SocketInner::<T> {
            state: SocketState::Unprepared,
            element: element.clone(),
            reader,
            buffer_pool,
            clock: None,
            base_time: None,
            read_handle: None,
        })))
    }

    pub async fn prepare(&self) -> Result<SocketStream<T>, ()> {
        // Null->Ready
        let mut inner = self.0.lock().await;
        if inner.state != SocketState::Unprepared {
            gst_debug!(SOCKET_CAT, obj: &inner.element, "Socket already prepared");
            return Ok(SocketStream::<T>::new(self));
        }
        gst_debug!(SOCKET_CAT, obj: &inner.element, "Preparing socket");

        inner.buffer_pool.set_active(true).map_err(|err| {
            gst_error!(SOCKET_CAT, obj: &inner.element, "Failed to prepare socket: {}", err);
        })?;
        inner.state = SocketState::Prepared;

        Ok(SocketStream::<T>::new(self))
    }

    pub async fn start(&self, clock: Option<gst::Clock>, base_time: Option<gst::ClockTime>) {
        // Paused->Playing
        let mut inner = self.0.lock().await;
        assert_ne!(SocketState::Unprepared, inner.state);
        if inner.state == SocketState::Started {
            gst_debug!(SOCKET_CAT, obj: &inner.element, "Socket already started");
            return;
        }

        gst_debug!(SOCKET_CAT, obj: &inner.element, "Starting socket");
        inner.clock = clock;
        inner.base_time = base_time;
        inner.state = SocketState::Started;
    }

    pub async fn pause(&self) {
        // Playing->Paused
        let mut inner = self.0.lock().await;
        assert_ne!(SocketState::Unprepared, inner.state);
        if inner.state != SocketState::Started {
            gst_debug!(SOCKET_CAT, obj: &inner.element, "Socket not started");
            return;
        }

        gst_debug!(SOCKET_CAT, obj: &inner.element, "Pausing socket");
        inner.clock = None;
        inner.base_time = None;
        inner.state = SocketState::Paused;
        if let Some(read_handle) = inner.read_handle.take() {
            read_handle.abort();
        }
    }

    pub async fn unprepare(&self) -> Result<(), ()> {
        // Ready->Null
        let mut inner = self.0.lock().await;
        assert_ne!(SocketState::Started, inner.state);
        if inner.state == SocketState::Unprepared {
            gst_debug!(SOCKET_CAT, obj: &inner.element, "Socket already unprepared");
            return Ok(());
        }

        inner.buffer_pool.set_active(false).map_err(|err| {
            gst_error!(SOCKET_CAT, obj: &inner.element, "Failed to unprepare socket: {}", err);
        })?;
        inner.state = SocketState::Unprepared;

        Ok(())
    }
}

impl<T: SocketRead + Unpin + 'static> Clone for Socket<T> {
    fn clone(&self) -> Self {
        Socket::<T>(self.0.clone())
    }
}

pub type SocketStreamItem =
    Result<(gst::Buffer, Option<std::net::SocketAddr>), Either<gst::FlowError, io::Error>>;

#[derive(Debug)]
pub struct SocketStream<T: SocketRead + 'static> {
    socket: Socket<T>,
    mapped_buffer: Option<gst::MappedBuffer<gst::buffer::Writable>>,
}

impl<T: SocketRead + 'static> SocketStream<T> {
    fn new(socket: &Socket<T>) -> Self {
        SocketStream {
            socket: socket.clone(),
            mapped_buffer: None,
        }
    }

    // Implementing `next` as an `async fn` instead of a `Stream` because of the `async` `Mutex`
    // See https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/merge_requests/204#note_322774
    #[allow(clippy::should_implement_trait)]
    pub async fn next(&mut self) -> Option<SocketStreamItem> {
        // take the mapped_buffer before locking the socket so as to please the mighty borrow checker
        let read_fut = {
            let mut inner = self.socket.0.lock().await;
            if inner.state != SocketState::Started {
                gst_debug!(SOCKET_CAT, obj: &inner.element, "DataQueue is not Started");
                return None;
            }

            gst_debug!(SOCKET_CAT, obj: &inner.element, "Trying to read data");
            if self.mapped_buffer.is_none() {
                match inner.buffer_pool.acquire_buffer(None) {
                    Ok(buffer) => {
                        self.mapped_buffer = Some(buffer.into_mapped_buffer_writable().unwrap());
                    }
                    Err(err) => {
                        gst_debug!(SOCKET_CAT, obj: &inner.element, "Failed to acquire buffer {:?}", err);
                        return Some(Err(Either::Left(err)));
                    }
                }
            }

            let (read_fut, abort_handle) = abortable(
                inner
                    .reader
                    .read(self.mapped_buffer.as_mut().unwrap().as_mut_slice()),
            );
            inner.read_handle = Some(abort_handle);

            read_fut
        };

        match read_fut.await {
            Ok(Ok((len, saddr))) => {
                let inner = self.socket.0.lock().await;

                let dts = if T::DO_TIMESTAMP {
                    let time = inner.clock.as_ref().unwrap().get_time();
                    let running_time = time - inner.base_time.unwrap();
                    gst_debug!(SOCKET_CAT, obj: &inner.element, "Read {} bytes at {} (clock {})", len, running_time, time);
                    running_time
                } else {
                    gst_debug!(SOCKET_CAT, obj: &inner.element, "Read {} bytes", len);
                    gst::CLOCK_TIME_NONE
                };

                let mut buffer = self.mapped_buffer.take().unwrap().into_buffer();
                {
                    let buffer = buffer.get_mut().unwrap();
                    if len < buffer.get_size() {
                        buffer.set_size(len);
                    }
                    buffer.set_dts(dts);
                }

                Some(Ok((buffer, saddr)))
            }
            Ok(Err(err)) => {
                gst_debug!(SOCKET_CAT, obj: &self.socket.0.lock().await.element, "Read error {:?}", err);

                Some(Err(Either::Right(err)))
            }
            Err(Aborted) => {
                gst_debug!(SOCKET_CAT, obj: &self.socket.0.lock().await.element, "Read Aborted");

                None
            }
        }
    }
}

// Send/Sync struct for passing around a gio::Socket
// and getting the raw fd from it
//
// gio::Socket is not Send/Sync as it's generally unsafe
// to access it from multiple threads. Getting the underlying raw
// fd is safe though, as is receiving/sending from two different threads
#[derive(Debug)]
pub struct GioSocketWrapper {
    socket: *mut gio_ffi::GSocket,
}

unsafe impl Send for GioSocketWrapper {}
unsafe impl Sync for GioSocketWrapper {}

impl GioSocketWrapper {
    pub fn new(socket: &gio::Socket) -> Self {
        use glib::translate::*;

        Self {
            socket: socket.to_glib_full(),
        }
    }

    pub fn as_socket(&self) -> gio::Socket {
        unsafe {
            use glib::translate::*;

            from_glib_none(self.socket)
        }
    }

    #[cfg(unix)]
    pub fn set_tos(&self, qos_dscp: i32) -> Result<(), glib::error::Error> {
        use libc::{IPPROTO_IP, IPPROTO_IPV6, IPV6_TCLASS, IP_TOS};

        let tos = (qos_dscp & 0x3f) << 2;

        let socket = self.as_socket();

        socket.set_option(IPPROTO_IP, IP_TOS, tos)?;

        if socket.get_family() == gio::SocketFamily::Ipv6 {
            socket.set_option(IPPROTO_IPV6, IPV6_TCLASS, tos)?;
        }

        Ok(())
    }

    #[cfg(not(unix))]
    pub fn set_tos(&self, qos_dscp: i32) -> Result<(), Error> {
        Ok(())
    }

    #[cfg(unix)]
    pub fn get<T: FromRawFd>(&self) -> T {
        unsafe { FromRawFd::from_raw_fd(libc::dup(gio_ffi::g_socket_get_fd(self.socket))) }
    }

    #[cfg(windows)]
    pub fn get<T: FromRawSocket>(&self) -> T {
        unsafe {
            FromRawSocket::from_raw_socket(
                dup_socket(gio_ffi::g_socket_get_fd(self.socket) as _) as _
            )
        }
    }
}

impl Clone for GioSocketWrapper {
    fn clone(&self) -> Self {
        Self {
            socket: unsafe { gobject_ffi::g_object_ref(self.socket as *mut _) as *mut _ },
        }
    }
}

impl Drop for GioSocketWrapper {
    fn drop(&mut self) {
        unsafe {
            gobject_ffi::g_object_unref(self.socket as *mut _);
        }
    }
}

#[cfg(windows)]
unsafe fn dup_socket(socket: usize) -> usize {
    use std::mem;
    use winapi::shared::ws2def;
    use winapi::um::processthreadsapi;
    use winapi::um::winsock2;

    let mut proto_info = mem::MaybeUninit::uninit();
    let ret = winsock2::WSADuplicateSocketA(
        socket,
        processthreadsapi::GetCurrentProcessId(),
        proto_info.as_mut_ptr(),
    );
    assert_eq!(ret, 0);
    let mut proto_info = proto_info.assume_init();

    let socket = winsock2::WSASocketA(
        ws2def::AF_INET,
        ws2def::SOCK_DGRAM,
        ws2def::IPPROTO_UDP as i32,
        &mut proto_info,
        0,
        0,
    );

    assert_ne!(socket, winsock2::INVALID_SOCKET);

    socket
}

pub fn wrap_socket(socket: &tokio::net::UdpSocket) -> Result<GioSocketWrapper, gst::ErrorMessage> {
    #[cfg(unix)]
    unsafe {
        let fd = libc::dup(socket.as_raw_fd());

        // This is unsafe because it allows us to share the fd between the socket and the
        // GIO socket below, but safety of this is the job of the application
        struct FdConverter(RawFd);
        impl IntoRawFd for FdConverter {
            fn into_raw_fd(self) -> RawFd {
                self.0
            }
        }

        let fd = FdConverter(fd);

        let gio_socket = gio::Socket::new_from_fd(fd).map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::OpenWrite,
                ["Failed to create wrapped GIO socket: {}", err]
            )
        })?;
        Ok(GioSocketWrapper::new(&gio_socket))
    }
    #[cfg(windows)]
    unsafe {
        // FIXME: Needs https://github.com/tokio-rs/tokio/pull/806
        // and https://github.com/carllerche/mio/pull/859
        let fd = unreachable!(); //dup_socket(socket.as_raw_socket() as _) as _;

        // This is unsafe because it allows us to share the fd between the socket and the
        // GIO socket below, but safety of this is the job of the application
        struct SocketConverter(RawSocket);
        impl IntoRawSocket for SocketConverter {
            fn into_raw_socket(self) -> RawSocket {
                self.0
            }
        }

        let fd = SocketConverter(fd);

        let gio_socket = gio::Socket::new_from_socket(fd).map_err(|err| {
            gst_error_msg!(
                gst::ResourceError::OpenWrite,
                ["Failed to create wrapped GIO socket: {}", err]
            )
        })?;

        Ok(GioSocketWrapper::new(&gio_socket))
    }
}
