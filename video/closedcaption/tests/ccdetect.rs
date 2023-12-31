// Copyright (C) 2020 Matthew Waters <matthew@centricular.com>
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

use gst::prelude::*;

use std::sync::{Arc, Mutex};

use pretty_assertions::assert_eq;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsclosedcaption::plugin_register_static().unwrap();
    });
}

struct NotifyState {
    cc608_count: u32,
    cc708_count: u32,
}

impl Default for NotifyState {
    fn default() -> Self {
        NotifyState {
            cc608_count: 0,
            cc708_count: 0,
        }
    }
}

macro_rules! assert_push_data {
    ($h:expr, $state:expr, $data:expr, $ts:expr, $cc608_count:expr, $cc708_count:expr) => {
        let mut buf = gst::Buffer::from_mut_slice($data);
        buf.get_mut().unwrap().set_pts($ts);

        assert_eq!($h.push(buf), Ok(gst::FlowSuccess::Ok));
        {
            let state_guard = $state.lock().unwrap();
            assert_eq!(state_guard.cc608_count, $cc608_count);
            assert_eq!(state_guard.cc708_count, $cc708_count);
        }
    };
}

#[test]
fn test_have_cc_data_notify() {
    init();
    let valid_cc608_data = vec![0xfc, 0x80, 0x81];
    let invalid_cc608_data = vec![0xf8, 0x80, 0x81];
    let valid_cc708_data = vec![0xfe, 0x80, 0x81];
    let invalid_cc708_data = vec![0xfa, 0x80, 0x81];

    let mut h = gst_check::Harness::new("ccdetect");
    h.set_src_caps_str("closedcaption/x-cea-708,format=cc_data");
    h.set_sink_caps_str("closedcaption/x-cea-708,format=cc_data");
    h.get_element()
        .unwrap()
        .set_property("window", &(500_000_000u64))
        .unwrap();

    let state = Arc::new(Mutex::new(NotifyState::default()));
    let state_c = state.clone();
    h.get_element()
        .unwrap()
        .connect_notify(Some("cc608"), move |o, _pspec| {
            let mut state_guard = state_c.lock().unwrap();
            state_guard.cc608_count += 1;
            o.get_property("cc608").unwrap();
        });
    let state_c = state.clone();
    h.get_element()
        .unwrap()
        .connect_notify(Some("cc708"), move |o, _pspec| {
            let mut state_guard = state_c.lock().unwrap();
            state_guard.cc708_count += 1;
            o.get_property("cc708").unwrap();
        });

    /* valid cc608 data moves cc608 property to true */
    assert_push_data!(h, state, valid_cc608_data, 0.into(), 1, 0);

    /* invalid cc608 data moves cc608 property to false */
    assert_push_data!(h, state, invalid_cc608_data, 1_000_000_000.into(), 2, 0);

    /* valid cc708 data moves cc708 property to true */
    assert_push_data!(h, state, valid_cc708_data, 2_000_000_000.into(), 2, 1);

    /* invalid cc708 data moves cc708 property to false */
    assert_push_data!(h, state, invalid_cc708_data, 3_000_000_000.into(), 2, 2);
}

#[test]
fn test_cc_data_window() {
    init();
    let valid_cc608_data = vec![0xfc, 0x80, 0x81];
    let invalid_cc608_data = vec![0xf8, 0x80, 0x81];

    let mut h = gst_check::Harness::new("ccdetect");
    h.set_src_caps_str("closedcaption/x-cea-708,format=cc_data");
    h.set_sink_caps_str("closedcaption/x-cea-708,format=cc_data");
    h.get_element()
        .unwrap()
        .set_property("window", &500_000_000u64)
        .unwrap();

    let state = Arc::new(Mutex::new(NotifyState::default()));
    let state_c = state.clone();
    h.get_element()
        .unwrap()
        .connect_notify(Some("cc608"), move |_o, _pspec| {
            let mut state_guard = state_c.lock().unwrap();
            state_guard.cc608_count += 1;
        });
    let state_c = state.clone();
    h.get_element()
        .unwrap()
        .connect_notify(Some("cc708"), move |_o, _pspec| {
            let mut state_guard = state_c.lock().unwrap();
            state_guard.cc708_count += 1;
        });

    /* valid cc608 data moves cc608 property to true */
    assert_push_data!(h, state, valid_cc608_data.clone(), 0.into(), 1, 0);

    /* valid cc608 data moves within window */
    assert_push_data!(h, state, valid_cc608_data.clone(), 300_000_000.into(), 1, 0);

    /* invalid cc608 data before window expires, no change */
    assert_push_data!(
        h,
        state,
        invalid_cc608_data.clone(),
        600_000_000.into(),
        1,
        0
    );

    /* invalid cc608 data after window expires, cc608 changes to false */
    assert_push_data!(h, state, invalid_cc608_data, 1_000_000_000.into(), 2, 0);

    /* valid cc608 data before window expires, no change */
    assert_push_data!(
        h,
        state,
        valid_cc608_data.clone(),
        1_300_000_000.into(),
        2,
        0
    );

    /* valid cc608 data after window expires, property changes */
    assert_push_data!(h, state, valid_cc608_data, 1_600_000_000.into(), 3, 0);
}

#[test]
fn test_have_cdp_notify() {
    init();
    let valid_cc608_data = vec![
        0x96, 0x69, /* cdp magic bytes */
        0x10, /* length of cdp packet */
        0x8f, /* framerate */
        0x43, /* flags */
        0x00, 0x00, /* sequence counter */
        0x72, /* cc_data byte header */
        0xe1, /* n cc_data triples with 0xe0 as reserved bits */
        0xfc, 0x80, 0x81, /* cc_data triple */
        0x74, /* cdp end of frame byte header */
        0x00, 0x00, /* sequence counter */
        0x60, /* checksum */
    ];
    let invalid_cc608_data = vec![
        0x96, 0x69, 0x10, 0x8f, 0x43, 0x00, 0x00, 0x72, 0xe1, 0xf8, 0x81, 0x82, 0x74, 0x00, 0x00,
        0x60,
    ];

    let mut h = gst_check::Harness::new("ccdetect");
    h.set_src_caps_str("closedcaption/x-cea-708,format=cdp");
    h.set_sink_caps_str("closedcaption/x-cea-708,format=cdp");
    h.get_element()
        .unwrap()
        .set_property("window", &500_000_000u64)
        .unwrap();

    let state = Arc::new(Mutex::new(NotifyState::default()));
    let state_c = state.clone();
    h.get_element()
        .unwrap()
        .connect_notify(Some("cc608"), move |_o, _pspec| {
            let mut state_guard = state_c.lock().unwrap();
            state_guard.cc608_count += 1;
        });
    let state_c = state.clone();
    h.get_element()
        .unwrap()
        .connect_notify(Some("cc708"), move |_o, _pspec| {
            let mut state_guard = state_c.lock().unwrap();
            state_guard.cc708_count += 1;
        });

    /* valid cc608 data moves cc608 property to true */
    assert_push_data!(h, state, valid_cc608_data, 0.into(), 1, 0);

    /* invalid cc608 data moves cc608 property to false */
    assert_push_data!(h, state, invalid_cc608_data, 1_000_000_000.into(), 2, 0);
}

#[test]
fn test_malformed_cdp_notify() {
    init();
    let too_short = vec![0x96, 0x69];
    let wrong_magic = vec![
        0x00, 0x00, 0x10, 0x8f, 0x43, 0x00, 0x00, 0x72, 0xe1, 0xfc, 0x81, 0x82, 0x74, 0x00, 0x00,
        0x60,
    ];
    let length_too_long = vec![
        0x96, 0x69, 0x20, 0x8f, 0x43, 0x00, 0x00, 0x72, 0xe1, 0xfc, 0x81, 0x82, 0x74, 0x00, 0x00,
        0x60,
    ];
    let length_too_short = vec![
        0x96, 0x69, 0x00, 0x8f, 0x43, 0x00, 0x00, 0x72, 0xe1, 0xfc, 0x81, 0x82, 0x74, 0x00, 0x00,
        0x60,
    ];
    let wrong_cc_data_header_byte = vec![
        0x96, 0x69, 0x10, 0x8f, 0x43, 0x00, 0x00, 0xff, 0xe1, 0xfc, 0x81, 0x82, 0x74, 0x00, 0x00,
        0x60,
    ];
    let big_cc_count = vec![
        0x96, 0x69, 0x10, 0x8f, 0x43, 0x00, 0x00, 0x72, 0xef, 0xfc, 0x81, 0x82, 0x74, 0x00, 0x00,
        0x60,
    ];
    let wrong_cc_count_reserved_bits = vec![
        0x96, 0x69, 0x10, 0x8f, 0x43, 0x00, 0x00, 0x72, 0x01, 0xfc, 0x81, 0x82, 0x74, 0x00, 0x00,
        0x60,
    ];
    let cc608_after_cc708 = vec![
        0x96, 0x69, 0x13, 0x8f, 0x43, 0x00, 0x00, 0x72, 0xe2, 0xfe, 0x81, 0x82, 0xfc, 0x83, 0x84,
        0x74, 0x00, 0x00, 0x60,
    ];

    let mut h = gst_check::Harness::new("ccdetect");
    h.set_src_caps_str("closedcaption/x-cea-708,format=cdp");
    h.set_sink_caps_str("closedcaption/x-cea-708,format=cdp");
    h.get_element()
        .unwrap()
        .set_property("window", &0u64)
        .unwrap();

    let state = Arc::new(Mutex::new(NotifyState::default()));
    let state_c = state.clone();
    h.get_element()
        .unwrap()
        .connect_notify(Some("cc608"), move |_o, _pspec| {
            let mut state_guard = state_c.lock().unwrap();
            state_guard.cc608_count += 1;
        });
    let state_c = state.clone();
    h.get_element()
        .unwrap()
        .connect_notify(Some("cc708"), move |_o, _pspec| {
            let mut state_guard = state_c.lock().unwrap();
            state_guard.cc708_count += 1;
        });

    /* all invalid data does not change properties */
    assert_push_data!(h, state, too_short, 0.into(), 0, 0);
    assert_push_data!(h, state, wrong_magic, 1_000.into(), 0, 0);
    assert_push_data!(h, state, length_too_long, 2_000.into(), 0, 0);
    assert_push_data!(h, state, length_too_short, 3_000.into(), 0, 0);
    assert_push_data!(h, state, wrong_cc_data_header_byte, 4_000.into(), 0, 0);
    assert_push_data!(h, state, big_cc_count, 5_000.into(), 0, 0);
    assert_push_data!(h, state, wrong_cc_count_reserved_bits, 6_000.into(), 0, 0);
    assert_push_data!(h, state, cc608_after_cc708, 7_000.into(), 0, 0);
}

#[test]
fn test_gap_events() {
    init();
    let valid_cc608_data = vec![0xfc, 0x80, 0x81];

    let mut h = gst_check::Harness::new("ccdetect");
    h.set_src_caps_str("closedcaption/x-cea-708,format=cc_data");
    h.set_sink_caps_str("closedcaption/x-cea-708,format=cc_data");
    h.get_element()
        .unwrap()
        .set_property("window", &500_000_000u64)
        .unwrap();

    let state = Arc::new(Mutex::new(NotifyState::default()));
    let state_c = state.clone();
    h.get_element()
        .unwrap()
        .connect_notify(Some("cc608"), move |_o, _pspec| {
            let mut state_guard = state_c.lock().unwrap();
            state_guard.cc608_count += 1;
        });
    let state_c = state.clone();
    h.get_element()
        .unwrap()
        .connect_notify(Some("cc708"), move |_o, _pspec| {
            let mut state_guard = state_c.lock().unwrap();
            state_guard.cc708_count += 1;
        });

    /* valid cc608 data moves cc608 property to true */
    assert_push_data!(h, state, valid_cc608_data, 0.into(), 1, 0);

    /* pushing gap event within the window changes nothing */
    assert_eq!(
        h.push_event(gst::event::Gap::builder(100_000_000.into(), 1.into()).build()),
        true
    );

    {
        let state_guard = state.lock().unwrap();
        assert_eq!(state_guard.cc608_count, 1);
        assert_eq!(state_guard.cc708_count, 0);
    }

    /* pushing gap event outside the window moves cc608 property to false */
    assert_eq!(
        h.push_event(gst::event::Gap::builder(1_000_000_000.into(), 1.into()).build()),
        true
    );

    {
        let state_guard = state.lock().unwrap();
        assert_eq!(state_guard.cc608_count, 2);
        assert_eq!(state_guard.cc708_count, 0);
    }
}
