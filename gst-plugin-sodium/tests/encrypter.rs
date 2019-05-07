// encrypter.rs
//
// Copyright 2019 Jordan Petridis <jordan@centricular.com>
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to
// deal in the Software without restriction, including without limitation the
// rights to use, copy, modify, merge, publish, distribute, sublicense, and/or
// sell copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS
// IN THE SOFTWARE.
//
// SPDX-License-Identifier: MIT

#[macro_use]
extern crate lazy_static;
extern crate gstrssodium;

use glib::prelude::*;
use gst::prelude::*;

lazy_static! {
    static ref RECEIVER_PUBLIC: glib::Bytes = {
        let public = [
            28, 95, 33, 124, 28, 103, 80, 78, 7, 28, 234, 40, 226, 179, 253, 166, 169, 64, 78, 5,
            57, 92, 151, 179, 221, 89, 68, 70, 44, 225, 219, 19,
        ];

        glib::Bytes::from_owned(public)
    };
    static ref SENDER_PRIVATE: glib::Bytes = {
        let secret = [
            154, 227, 90, 239, 206, 184, 202, 234, 176, 161, 14, 91, 218, 98, 142, 13, 145, 223,
            210, 222, 224, 240, 98, 51, 142, 165, 255, 1, 159, 100, 242, 162,
        ];
        glib::Bytes::from_owned(secret)
    };
    static ref NONCE: glib::Bytes = {
        let nonce = [
            144, 187, 179, 230, 15, 4, 241, 15, 37, 133, 22, 30, 50, 106, 70, 159, 243, 218, 173,
            22, 18, 36, 4, 45,
        ];
        glib::Bytes::from_owned(nonce)
    };
}

fn init() {
    use std::sync::{Once, ONCE_INIT};
    static INIT: Once = ONCE_INIT;

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrssodium::plugin_register_static().unwrap();
        // set the nonce
        std::env::set_var("GST_SODIUM_ENCRYPT_NONCE", hex::encode(&*NONCE));
    });
}

#[test]
fn encrypt_file() {
    init();

    let input = include_bytes!("sample.mp3");
    let expected_output = include_bytes!("encrypted_sample.enc");

    let mut adapter = gst_base::UniqueAdapter::new();

    let enc = gst::ElementFactory::make("rssodiumencrypter", None).unwrap();
    enc.set_property("sender-key", &*SENDER_PRIVATE)
        .expect("failed to set property");
    enc.set_property("receiver-key", &*RECEIVER_PUBLIC)
        .expect("failed to set property");
    enc.set_property("block-size", &1024u32)
        .expect("failed to set property");

    let mut h = gst_check::Harness::new_with_element(&enc, None, None);
    h.add_element_src_pad(&enc.get_static_pad("src").expect("failed to get src pad"));
    h.add_element_sink_pad(&enc.get_static_pad("sink").expect("failed to get src pad"));
    h.set_src_caps_str("application/x-sodium-encrypted");

    let buf = gst::Buffer::from_mut_slice(Vec::from(&input[..]));

    assert_eq!(h.push(buf), Ok(gst::FlowSuccess::Ok));
    h.push_event(gst::Event::new_eos().build());

    println!("Pulling buffer...");
    while let Some(buf) = h.pull() {
        adapter.push(buf);
        if adapter.available() >= expected_output.len() {
            break;
        }
    }

    let buf = adapter
        .take_buffer(adapter.available())
        .expect("failed to take buffer");
    let map = buf.map_readable().expect("Couldn't map buffer readable");
    assert_eq!(map.as_ref(), expected_output.as_ref());
}

#[test]
fn test_querries() {
    init();

    let input = include_bytes!("sample.mp3");
    let expected_output = include_bytes!("encrypted_sample.enc");

    let mut adapter = gst_base::UniqueAdapter::new();

    let enc = gst::ElementFactory::make("rssodiumencrypter", None).unwrap();
    enc.set_property("sender-key", &*SENDER_PRIVATE)
        .expect("failed to set property");
    enc.set_property("receiver-key", &*RECEIVER_PUBLIC)
        .expect("failed to set property");
    enc.set_property("block-size", &1024u32)
        .expect("failed to set property");

    let mut h = gst_check::Harness::new_with_element(&enc, None, None);
    h.add_element_src_pad(&enc.get_static_pad("src").expect("failed to get src pad"));
    h.add_element_sink_pad(&enc.get_static_pad("sink").expect("failed to get src pad"));
    h.set_src_caps_str("application/x-sodium-encrypted");

    let buf = gst::Buffer::from_mut_slice(Vec::from(&input[..]));

    assert_eq!(h.push(buf), Ok(gst::FlowSuccess::Ok));
    h.push_event(gst::Event::new_eos().build());

    // Query position at 0
    let q1 = enc.query_position::<gst::format::Bytes>().unwrap();
    assert_eq!(gst::format::Bytes(Some(0)), q1);

    // Query position after a buffer pull
    let buf1 = h.pull().unwrap();
    let s1 = buf1.get_size() as u64;
    let q1 = enc.query_position::<gst::format::Bytes>().unwrap();
    assert_eq!(gst::format::Bytes(Some(s1)), q1);
    adapter.push(buf1);

    // Query position after 2 buffer pulls
    let buf2 = h.pull().unwrap();
    let s2 = buf2.get_size() as u64;
    let q2 = enc.query_position::<gst::format::Bytes>().unwrap();
    // query pos == b1 + b2 len()
    assert_eq!(gst::format::Bytes(Some(s1 + s2)), q2);
    adapter.push(buf2);

    while let Some(buf) = h.pull() {
        adapter.push(buf);
        if adapter.available() >= expected_output.len() {
            break;
        }
    }

    // Query position at eos
    let q_eos = enc.query_position::<gst::format::Bytes>().unwrap();
    assert_eq!(
        gst::format::Bytes(Some(expected_output.len() as u64)),
        q_eos
    );
    assert_eq!(gst::format::Bytes(Some(adapter.available() as u64)), q_eos);
}

#[test]
fn test_state_changes() {
    init();

    // NullToReady without keys provided
    {
        let enc = gst::ElementFactory::make("rssodiumencrypter", None).unwrap();
        assert!(enc.change_state(gst::StateChange::NullToReady).is_err());

        // Set only receiver key
        let enc = gst::ElementFactory::make("rssodiumencrypter", None).unwrap();
        enc.set_property("receiver-key", &*RECEIVER_PUBLIC)
            .expect("failed to set property");
        assert!(enc.change_state(gst::StateChange::NullToReady).is_err());

        // Set only sender key
        let enc = gst::ElementFactory::make("rssodiumencrypter", None).unwrap();
        enc.set_property("sender-key", &*SENDER_PRIVATE)
            .expect("failed to set property");
        assert!(enc.change_state(gst::StateChange::NullToReady).is_err());
    }

    // NullToReady
    {
        let enc = gst::ElementFactory::make("rssodiumencrypter", None).unwrap();
        enc.set_property("sender-key", &*SENDER_PRIVATE)
            .expect("failed to set property");
        enc.set_property("receiver-key", &*RECEIVER_PUBLIC)
            .expect("failed to set property");
        assert!(enc.change_state(gst::StateChange::NullToReady).is_ok());
    }

    // ReadyToNull
    {
        let enc = gst::ElementFactory::make("rssodiumencrypter", None).unwrap();
        enc.set_property("sender-key", &*SENDER_PRIVATE)
            .expect("failed to set property");
        enc.set_property("receiver-key", &*RECEIVER_PUBLIC)
            .expect("failed to set property");
        assert!(enc.change_state(gst::StateChange::NullToReady).is_ok());
    }
}
