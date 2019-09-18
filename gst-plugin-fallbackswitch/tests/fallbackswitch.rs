// Copyright (C) 2019 Sebastian Dröge <sebastian@centricular.com>
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

#[macro_use]
extern crate gstreamer as gst;
use gst::prelude::*;
extern crate gstreamer_app as gst_app;
extern crate gstreamer_check as gst_check;

extern crate gstfallbackswitch;

#[macro_use]
extern crate lazy_static;

lazy_static! {
    static ref TEST_CAT: gst::DebugCategory = gst::DebugCategory::new(
        "fallbackswitch-test",
        gst::DebugColorFlags::empty(),
        Some("fallbackswitch test"),
    );
}

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstfallbackswitch::plugin_register_static().expect("gstfallbackswitch test");
    });
}

macro_rules! assert_fallback_buffer {
    ($buffer:expr, $ts:expr) => {
        assert_eq!($buffer.get_pts(), $ts);
        assert_eq!($buffer.get_size(), 160 * 120 * 4);
    };
}

macro_rules! assert_buffer {
    ($buffer:expr, $ts:expr) => {
        assert_eq!($buffer.get_pts(), $ts);
        assert_eq!($buffer.get_size(), 320 * 240 * 4);
    };
}

#[test]
fn test_no_fallback_no_drops() {
    let pipeline = setup_pipeline(None);

    push_buffer(&pipeline, 0 * gst::SECOND);
    set_time(&pipeline, 0 * gst::SECOND);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, 0 * gst::SECOND);

    push_buffer(&pipeline, 1 * gst::SECOND);
    set_time(&pipeline, 1 * gst::SECOND);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, 1 * gst::SECOND);

    push_buffer(&pipeline, 2 * gst::SECOND);
    set_time(&pipeline, 2 * gst::SECOND);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, 2 * gst::SECOND);

    push_eos(&pipeline);
    wait_eos(&pipeline);

    stop_pipeline(pipeline);
}

#[test]
fn test_no_drops_live() {
    test_no_drops(true);
}

#[test]
fn test_no_drops_not_live() {
    test_no_drops(false);
}

fn test_no_drops(live: bool) {
    let pipeline = setup_pipeline(Some(live));

    push_buffer(&pipeline, 0 * gst::SECOND);
    push_fallback_buffer(&pipeline, 0 * gst::SECOND);
    set_time(&pipeline, 0 * gst::SECOND);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, 0 * gst::SECOND);

    push_fallback_buffer(&pipeline, 1 * gst::SECOND);
    push_buffer(&pipeline, 1 * gst::SECOND);
    set_time(&pipeline, 1 * gst::SECOND);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, 1 * gst::SECOND);

    push_buffer(&pipeline, 2 * gst::SECOND);
    push_fallback_buffer(&pipeline, 2 * gst::SECOND);
    set_time(&pipeline, 2 * gst::SECOND);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, 2 * gst::SECOND);

    // EOS on the fallback should not be required
    push_eos(&pipeline);
    wait_eos(&pipeline);

    stop_pipeline(pipeline);
}

#[test]
fn test_no_drops_but_no_fallback_frames_live() {
    test_no_drops_but_no_fallback_frames(true);
}

#[test]
fn test_no_drops_but_no_fallback_frames_not_live() {
    test_no_drops_but_no_fallback_frames(false);
}

fn test_no_drops_but_no_fallback_frames(live: bool) {
    let pipeline = setup_pipeline(Some(live));

    push_buffer(&pipeline, 0 * gst::SECOND);
    // +10ms needed here because the immediate timeout will be always at running time 0, but
    // aggregator also adds the latency to it so we end up at 10ms instead.
    set_time(&pipeline, 0 * gst::SECOND + 10 * gst::MSECOND);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, 0 * gst::SECOND);

    push_buffer(&pipeline, 1 * gst::SECOND);
    set_time(&pipeline, 1 * gst::SECOND);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, 1 * gst::SECOND);

    push_buffer(&pipeline, 2 * gst::SECOND);
    set_time(&pipeline, 2 * gst::SECOND);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, 2 * gst::SECOND);

    // EOS on the fallback should not be required
    push_eos(&pipeline);
    wait_eos(&pipeline);

    stop_pipeline(pipeline);
}

#[test]
fn test_short_drop_live() {
    test_short_drop(true);
}

#[test]
fn test_short_drop_not_live() {
    test_short_drop(false);
}

fn test_short_drop(live: bool) {
    let pipeline = setup_pipeline(Some(live));

    push_buffer(&pipeline, 0 * gst::SECOND);
    push_fallback_buffer(&pipeline, 0 * gst::SECOND);
    set_time(&pipeline, 0 * gst::SECOND);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, 0 * gst::SECOND);

    // A timeout at 1s will get rid of the fallback buffer
    // but not output anything
    push_fallback_buffer(&pipeline, 1 * gst::SECOND);
    // Time out the fallback buffer at +10ms
    set_time(&pipeline, 1 * gst::SECOND + 10 * gst::MSECOND);

    push_fallback_buffer(&pipeline, 2 * gst::SECOND);
    push_buffer(&pipeline, 2 * gst::SECOND);
    set_time(&pipeline, 2 * gst::SECOND);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, 2 * gst::SECOND);

    push_eos(&pipeline);
    push_fallback_eos(&pipeline);
    wait_eos(&pipeline);

    stop_pipeline(pipeline);
}

#[test]
fn test_long_drop_and_eos_live() {
    test_long_drop_and_eos(true);
}

#[test]
fn test_long_drop_and_eos_not_live() {
    test_long_drop_and_eos(false);
}

fn test_long_drop_and_eos(live: bool) {
    let pipeline = setup_pipeline(Some(live));

    // Produce the first frame
    push_buffer(&pipeline, 0 * gst::SECOND);
    push_fallback_buffer(&pipeline, 0 * gst::SECOND);
    set_time(&pipeline, 0 * gst::SECOND);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, 0 * gst::SECOND);

    // Produce a second frame but only from the fallback source
    push_fallback_buffer(&pipeline, 1 * gst::SECOND);
    set_time(&pipeline, 1 * gst::SECOND + 10 * gst::MSECOND);

    // Produce a third frame but only from the fallback source
    push_fallback_buffer(&pipeline, 2 * gst::SECOND);
    set_time(&pipeline, 2 * gst::SECOND + 10 * gst::MSECOND);

    // Produce a fourth frame but only from the fallback source
    // This should be output now
    push_fallback_buffer(&pipeline, 3 * gst::SECOND);
    set_time(&pipeline, 3 * gst::SECOND + 10 * gst::MSECOND);
    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer, 3 * gst::SECOND);

    // Produce a fifth frame but only from the fallback source
    // This should be output now
    push_fallback_buffer(&pipeline, 4 * gst::SECOND);
    set_time(&pipeline, 4 * gst::SECOND + 10 * gst::MSECOND);
    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer, 4 * gst::SECOND);

    // Wait for EOS to arrive at appsink
    push_eos(&pipeline);
    push_fallback_eos(&pipeline);
    wait_eos(&pipeline);

    stop_pipeline(pipeline);
}

#[test]
fn test_long_drop_and_recover_live() {
    test_long_drop_and_recover(true);
}

#[test]
fn test_long_drop_and_recover_not_live() {
    test_long_drop_and_recover(false);
}

fn test_long_drop_and_recover(live: bool) {
    let pipeline = setup_pipeline(Some(live));

    // Produce the first frame
    push_buffer(&pipeline, 0 * gst::SECOND);
    push_fallback_buffer(&pipeline, 0 * gst::SECOND);
    set_time(&pipeline, 0 * gst::SECOND);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, 0 * gst::SECOND);

    // Produce a second frame but only from the fallback source
    push_fallback_buffer(&pipeline, 1 * gst::SECOND);
    set_time(&pipeline, 1 * gst::SECOND + 10 * gst::MSECOND);

    // Produce a third frame but only from the fallback source
    push_fallback_buffer(&pipeline, 2 * gst::SECOND);
    set_time(&pipeline, 2 * gst::SECOND + 10 * gst::MSECOND);

    // Produce a fourth frame but only from the fallback source
    // This should be output now
    push_fallback_buffer(&pipeline, 3 * gst::SECOND);
    set_time(&pipeline, 3 * gst::SECOND + 10 * gst::MSECOND);
    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer, 3 * gst::SECOND);

    // Produce a fifth frame but only from the fallback source
    // This should be output now
    push_fallback_buffer(&pipeline, 4 * gst::SECOND);
    set_time(&pipeline, 4 * gst::SECOND + 10 * gst::MSECOND);
    let buffer = pull_buffer(&pipeline);
    assert_fallback_buffer!(buffer, 4 * gst::SECOND);

    // Produce a sixth frame from the normal source
    push_buffer(&pipeline, 5 * gst::SECOND);
    push_fallback_buffer(&pipeline, 5 * gst::SECOND);
    set_time(&pipeline, 5 * gst::SECOND);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, 5 * gst::SECOND);

    // Produce a seventh frame from the normal source but no fallback.
    // This should still be output immediately
    push_buffer(&pipeline, 6 * gst::SECOND);
    set_time(&pipeline, 6 * gst::SECOND);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, 6 * gst::SECOND);

    // Produce a eight frame from the normal source
    push_buffer(&pipeline, 7 * gst::SECOND);
    push_fallback_buffer(&pipeline, 7 * gst::SECOND);
    set_time(&pipeline, 7 * gst::SECOND);
    let buffer = pull_buffer(&pipeline);
    assert_buffer!(buffer, 7 * gst::SECOND);

    // Wait for EOS to arrive at appsink
    push_eos(&pipeline);
    push_fallback_eos(&pipeline);
    wait_eos(&pipeline);

    stop_pipeline(pipeline);
}

struct Pipeline {
    pipeline: gst::Pipeline,
    clock_join_handle: Option<std::thread::JoinHandle<()>>,
}

impl std::ops::Deref for Pipeline {
    type Target = gst::Pipeline;

    fn deref(&self) -> &gst::Pipeline {
        &self.pipeline
    }
}

fn setup_pipeline(with_live_fallback: Option<bool>) -> Pipeline {
    init();

    gst_debug!(TEST_CAT, "Setting up pipeline");

    let clock = gst_check::TestClock::new();
    clock.set_time(0 * gst::SECOND);
    let pipeline = gst::Pipeline::new(None);

    // Running time 0 in our pipeline is going to be clock time 1s. All
    // clock ids before 1s are used for signalling to our clock advancing
    // thread.
    pipeline.use_clock(Some(&clock));
    pipeline.set_base_time(1 * gst::SECOND);
    pipeline.set_start_time(gst::CLOCK_TIME_NONE);

    let src = gst::ElementFactory::make("appsrc", Some("src"))
        .unwrap()
        .downcast::<gst_app::AppSrc>()
        .unwrap();
    src.set_property("is-live", &true).unwrap();
    src.set_property("format", &gst::Format::Time).unwrap();
    src.set_property("min-latency", &(10i64)).unwrap();
    src.set_property(
        "caps",
        &gst::Caps::builder("video/x-raw")
            .field("format", &"ARGB")
            .field("width", &320)
            .field("height", &240)
            .field("framerate", &gst::Fraction::new(1, 1))
            .build(),
    )
    .unwrap();

    let switch = gst::ElementFactory::make("fallbackswitch", Some("switch")).unwrap();
    switch.set_property("timeout", &(3 * gst::SECOND)).unwrap();

    let sink = gst::ElementFactory::make("appsink", Some("sink"))
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    sink.set_property("sync", &false).unwrap();

    let queue = gst::ElementFactory::make("queue", None).unwrap();

    pipeline
        .add_many(&[src.upcast_ref(), &switch, &queue, &sink.upcast_ref()])
        .unwrap();
    src.link_pads(Some("src"), &switch, Some("sink")).unwrap();
    switch.link_pads(Some("src"), &queue, Some("sink")).unwrap();
    queue.link_pads(Some("src"), &sink, Some("sink")).unwrap();

    if let Some(live) = with_live_fallback {
        let fallback_src = gst::ElementFactory::make("appsrc", Some("fallback-src"))
            .unwrap()
            .downcast::<gst_app::AppSrc>()
            .unwrap();
        fallback_src.set_property("is-live", &live).unwrap();
        fallback_src
            .set_property("format", &gst::Format::Time)
            .unwrap();
        fallback_src.set_property("min-latency", &(10i64)).unwrap();
        fallback_src
            .set_property(
                "caps",
                &gst::Caps::builder("video/x-raw")
                    .field("format", &"ARGB")
                    .field("width", &160)
                    .field("height", &120)
                    .field("framerate", &gst::Fraction::new(1, 1))
                    .build(),
            )
            .unwrap();

        pipeline.add(&fallback_src).unwrap();

        fallback_src
            .link_pads(Some("src"), &switch, Some("fallback_sink"))
            .unwrap();
    }

    pipeline.set_state(gst::State::Playing).unwrap();

    let clock_clone = clock.clone();
    let clock_join_handle = std::thread::spawn(move || {
        let clock = clock_clone;

        loop {
            while let Some(clock_id) = clock.peek_next_pending_id().and_then(|clock_id| {
                // Process if the clock ID is in the past or now
                if clock.get_time() >= clock_id.get_time() {
                    Some(clock_id)
                } else {
                    None
                }
            }) {
                gst_debug!(TEST_CAT, "Processing clock ID at {}", clock_id.get_time());
                if let Some(clock_id) = clock.process_next_clock_id() {
                    gst_debug!(TEST_CAT, "Processed clock ID at {}", clock_id.get_time());
                    if clock_id.get_time() == 0.into() {
                        gst_debug!(TEST_CAT, "Stopping clock thread");
                        return;
                    }
                }
            }

            // Sleep for 5ms as long as we have pending clock IDs that are in the future
            // at the top of the queue. We don't want to do a busy loop here.
            while clock.peek_next_pending_id().iter().any(|clock_id| {
                // Sleep if the clock ID is in the future
                clock.get_time() < clock_id.get_time()
            }) {
                use std::{thread, time};

                thread::sleep(time::Duration::from_millis(10));
            }

            // Otherwise if there are none (or they are ready now) wait until there are
            // clock ids again
            let _ = clock.wait_for_next_pending_id();
        }
    });

    Pipeline {
        pipeline,
        clock_join_handle: Some(clock_join_handle),
    }
}

fn push_buffer(pipeline: &Pipeline, time: gst::ClockTime) {
    let src = pipeline
        .get_by_name("src")
        .unwrap()
        .downcast::<gst_app::AppSrc>()
        .unwrap();
    let mut buffer = gst::Buffer::with_size(320 * 240 * 4).unwrap();
    {
        let buffer = buffer.get_mut().unwrap();
        buffer.set_pts(time);
    }
    src.push_buffer(buffer).unwrap();
}

fn push_fallback_buffer(pipeline: &Pipeline, time: gst::ClockTime) {
    let src = pipeline
        .get_by_name("fallback-src")
        .unwrap()
        .downcast::<gst_app::AppSrc>()
        .unwrap();
    let mut buffer = gst::Buffer::with_size(160 * 120 * 4).unwrap();
    {
        let buffer = buffer.get_mut().unwrap();
        buffer.set_pts(time);
    }
    src.push_buffer(buffer).unwrap();
}

fn push_eos(pipeline: &Pipeline) {
    let src = pipeline
        .get_by_name("src")
        .unwrap()
        .downcast::<gst_app::AppSrc>()
        .unwrap();
    src.end_of_stream().unwrap();
}

fn push_fallback_eos(pipeline: &Pipeline) {
    let src = pipeline
        .get_by_name("fallback-src")
        .unwrap()
        .downcast::<gst_app::AppSrc>()
        .unwrap();
    src.end_of_stream().unwrap();
}

fn pull_buffer(pipeline: &Pipeline) -> gst::Buffer {
    let sink = pipeline
        .get_by_name("sink")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    let sample = sink.pull_sample().unwrap();
    sample.get_buffer_owned().unwrap()
}

fn set_time(pipeline: &Pipeline, time: gst::ClockTime) {
    let clock = pipeline
        .get_clock()
        .unwrap()
        .downcast::<gst_check::TestClock>()
        .unwrap();

    gst_debug!(TEST_CAT, "Setting time to {}", time);
    clock.set_time(gst::SECOND + time);
}

fn wait_eos(pipeline: &Pipeline) {
    let sink = pipeline
        .get_by_name("sink")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    // FIXME: Ideally without a sleep
    loop {
        use std::{thread, time};

        if sink.is_eos() {
            gst_debug!(TEST_CAT, "Waited for EOS");
            break;
        }
        thread::sleep(time::Duration::from_millis(10));
    }
}

fn stop_pipeline(mut pipeline: Pipeline) {
    pipeline.set_state(gst::State::Null).unwrap();

    let clock = pipeline
        .get_clock()
        .unwrap()
        .downcast::<gst_check::TestClock>()
        .unwrap();

    // Signal shutdown to the clock thread
    let clock_id = clock.new_single_shot_id(0.into()).unwrap();
    let _ = clock_id.wait();

    let switch = pipeline.get_by_name("switch").unwrap();
    let switch_weak = switch.downgrade();
    drop(switch);
    let pipeline_weak = pipeline.downgrade();

    pipeline.clock_join_handle.take().unwrap().join().unwrap();
    drop(pipeline);

    assert!(switch_weak.upgrade().is_none());
    assert!(pipeline_weak.upgrade().is_none());
}
