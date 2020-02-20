// Copyright (C) 2019 Mathieu Duponchelle <mathieu@centricular.com>
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

use std::thread;

use glib;
use glib::prelude::*;

use gst;
use gst_check;

use gstthreadshare;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstthreadshare::plugin_register_static().expect("gstthreadshare udpsrc test");
    });
}

#[test]
fn test_client_management() {
    init();

    let h = gst_check::Harness::new("ts-udpsink");
    let udpsink = h.get_element().unwrap();

    let clients = udpsink
        .get_property("clients")
        .unwrap()
        .get::<String>()
        .unwrap()
        .unwrap();

    assert_eq!(clients, "127.0.0.1:5000");

    udpsink.emit("add", &[&"192.168.1.1", &57]).unwrap();
    let clients = udpsink
        .get_property("clients")
        .unwrap()
        .get::<String>()
        .unwrap()
        .unwrap();
    assert_eq!(clients, "127.0.0.1:5000,192.168.1.1:57");

    /* Adding a client twice is not supported */
    udpsink.emit("add", &[&"192.168.1.1", &57]).unwrap();
    let clients = udpsink
        .get_property("clients")
        .unwrap()
        .get::<String>()
        .unwrap()
        .unwrap();
    assert_eq!(clients, "127.0.0.1:5000,192.168.1.1:57");

    udpsink.emit("remove", &[&"192.168.1.1", &57]).unwrap();
    let clients = udpsink
        .get_property("clients")
        .unwrap()
        .get::<String>()
        .unwrap()
        .unwrap();
    assert_eq!(clients, "127.0.0.1:5000");

    /* Removing a non-existing client should not be a problem */
    udpsink.emit("remove", &[&"192.168.1.1", &57]).unwrap();
    let clients = udpsink
        .get_property("clients")
        .unwrap()
        .get::<String>()
        .unwrap()
        .unwrap();
    assert_eq!(clients, "127.0.0.1:5000");

    /* While the default host:address client is listed in clients,
     * it can't be removed with the remove signal */
    udpsink.emit("remove", &[&"127.0.0.1", &5000]).unwrap();
    let clients = udpsink
        .get_property("clients")
        .unwrap()
        .get::<String>()
        .unwrap()
        .unwrap();
    assert_eq!(clients, "127.0.0.1:5000");

    /* It is however possible to remove the default client by setting
     * host to None */
    let host: Option<String> = None;
    udpsink.set_property("host", &host).unwrap();
    let clients = udpsink
        .get_property("clients")
        .unwrap()
        .get::<String>()
        .unwrap()
        .unwrap();
    assert_eq!(clients, "");

    /* The client properties is writable too */
    udpsink
        .set_property("clients", &"127.0.0.1:5000,192.168.1.1:57")
        .unwrap();
    let clients = udpsink
        .get_property("clients")
        .unwrap()
        .get::<String>()
        .unwrap()
        .unwrap();
    assert_eq!(clients, "127.0.0.1:5000,192.168.1.1:57");

    udpsink.emit("clear", &[]).unwrap();
    let clients = udpsink
        .get_property("clients")
        .unwrap()
        .get::<String>()
        .unwrap()
        .unwrap();
    assert_eq!(clients, "");
}

#[test]
fn test_chain() {
    init();

    let mut h = gst_check::Harness::new("ts-udpsink");
    h.set_src_caps_str(&"foo/bar");

    thread::spawn(move || {
        use std::net;
        use std::time;

        thread::sleep(time::Duration::from_millis(50));

        let socket = net::UdpSocket::bind("127.0.0.1:5000").unwrap();
        let mut buf = [0; 5];
        let (amt, _) = socket.recv_from(&mut buf).unwrap();

        assert!(amt == 4);
        assert!(buf == [42, 43, 44, 45, 0]);
    });

    let buf = gst::Buffer::from_slice(&[42, 43, 44, 45]);
    assert!(h.push(buf) == Ok(gst::FlowSuccess::Ok));
}
