// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
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

extern crate glib;
use glib::prelude::*;

use gst;
use gst_audio;
use gst_check;

use gstthreadshare;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstthreadshare::plugin_register_static().expect("gstthreadshare tonesrc test");
    });
}

#[test]
fn test_push() {
    init();

    let mut h = gst_check::Harness::new("ts-tonesrc");

    {
        let tonesrc = h.get_element().unwrap();
        tonesrc.set_property("context", &"test-push").unwrap();
    }

    h.use_systemclock();
    h.play();

    for _ in 0..3 {
        let buffer = h.pull().unwrap();
        assert_eq!(buffer.get_size(), 320);
    }

    let caps = gst::Caps::new_simple(
        "audio/x-raw",
        &[
            ("format", &gst_audio::AUDIO_FORMAT_S16.to_string()),
            ("layout", &"interleaved"),
            ("rate", &8000i32),
            ("channels", &1i32),
        ],
    );

    let mut n_events = 0;
    loop {
        use gst::EventView;

        let event = h.pull_event().unwrap();
        match event.view() {
            EventView::StreamStart(..) => {
                assert_eq!(n_events, 0);
            }
            EventView::Caps(ev) => {
                assert_eq!(n_events, 1);
                let event_caps = ev.get_caps();
                assert_eq!(caps.as_ref(), event_caps);
            }
            EventView::Segment(..) => {
                assert_eq!(n_events, 2);
                break;
            }
            _ => (),
        }
        n_events += 1;
    }
    assert!(n_events >= 2);
}
