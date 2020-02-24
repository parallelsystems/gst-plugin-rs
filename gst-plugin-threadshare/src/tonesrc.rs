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

use glib;
use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use glib::{glib_object_impl, glib_object_subclass};

use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_element_error, gst_error, gst_error_msg, gst_log, gst_trace};
use gst_audio;

use futures::future::BoxFuture;
use futures::lock::Mutex;
use futures::prelude::*;

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time;
use std::{i32, u32};

use either::Either;

use lazy_static::lazy_static;

use rand;

use muldiv::MulDiv;

use byte_slice_cast::*;

use crate::runtime::prelude::*;
use crate::runtime::{self, Context, JoinHandle, PadSrc, PadSrcRef};

const DEFAULT_CONTEXT: &'static str = "";
const DEFAULT_CONTEXT_WAIT: u32 = 0;
const DEFAULT_SAMPLES_PER_BUFFER: u32 = 160;
const DEFAULT_FREQ1: u32 = 0;
const DEFAULT_VOL1: i32 = 0;
const DEFAULT_FREQ2: u32 = 0;
const DEFAULT_VOL2: i32 = 0;
const DEFAULT_ON_TIME1: u32 = 1000;
const DEFAULT_ON_TIME2: u32 = 1000;
const DEFAULT_OFF_TIME1: u32 = 1000;
const DEFAULT_OFF_TIME2: u32 = 1000;
const DEFAULT_REPEAT: bool = false;

#[derive(Debug, Clone)]
struct Settings {
    context: String,
    context_wait: u32,
    samples_per_buffer: u32,
    tone_gen_settings: tonegen::ToneGenSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
            samples_per_buffer: DEFAULT_SAMPLES_PER_BUFFER,
            tone_gen_settings: Default::default(),
        }
    }
}

static PROPERTIES: [subclass::Property; 12] = [
    subclass::Property("context", |name| {
        glib::ParamSpec::string(
            name,
            "Context",
            "Context name to share threads with",
            Some(DEFAULT_CONTEXT),
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("context-wait", |name| {
        glib::ParamSpec::uint(
            name,
            "Context Wait",
            "Throttle poll loop to run at most once every this many ms",
            0,
            1000,
            DEFAULT_CONTEXT_WAIT,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("samples-per-buffer", |name| {
        glib::ParamSpec::uint(
            name,
            "Samples Per Buffer",
            "Number of samples per output buffer",
            1,
            u32::MAX,
            DEFAULT_SAMPLES_PER_BUFFER,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("freq1", |name| {
        glib::ParamSpec::uint(
            name,
            "Frequency 1",
            "Frequency of first telephony tone component",
            0,
            4000,
            DEFAULT_FREQ1,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("vol1", |name| {
        glib::ParamSpec::int(
            name,
            "Volume 1",
            "Volume of first telephony tone component",
            -50,
            0,
            DEFAULT_VOL1,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("freq2", |name| {
        glib::ParamSpec::uint(
            name,
            "Frequency 2",
            "Frequency of second telephony tone component",
            0,
            4000,
            DEFAULT_FREQ2,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("vol2", |name| {
        glib::ParamSpec::int(
            name,
            "Volume 2",
            "Volume of second telephony tone component",
            -50,
            0,
            DEFAULT_VOL2,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("on-time1", |name| {
        glib::ParamSpec::uint(
            name,
            "On Time 1",
            "Time of the first period when the tone signal is present",
            0,
            u32::MAX,
            DEFAULT_ON_TIME1,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("off-time1", |name| {
        glib::ParamSpec::uint(
            name,
            "Off Time 1",
            "Time of the first period when the tone signal is off",
            0,
            u32::MAX,
            DEFAULT_OFF_TIME1,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("on-time2", |name| {
        glib::ParamSpec::uint(
            name,
            "On Time 2",
            "Time of the second period when the tone signal is present",
            0,
            u32::MAX,
            DEFAULT_ON_TIME2,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("off-time2", |name| {
        glib::ParamSpec::uint(
            name,
            "Off Time 2",
            "Time of the second period when the tone signal is off",
            0,
            u32::MAX,
            DEFAULT_OFF_TIME2,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("repeat", |name| {
        glib::ParamSpec::boolean(
            name,
            "Repeat",
            "Whether to repeat specified tone indefinitly",
            DEFAULT_REPEAT,
            glib::ParamFlags::READWRITE,
        )
    }),
];

#[derive(Debug, Default)]
struct ToneSrcPadHandlerInner {
    flush_join_handle: StdMutex<Option<JoinHandle<Result<(), ()>>>>,
}

#[derive(Clone, Debug, Default)]
struct ToneSrcPadHandler(Arc<ToneSrcPadHandlerInner>);

impl PadSrcHandler for ToneSrcPadHandler {
    type ElementImpl = ToneSrc;

    fn src_query(
        &self,
        pad: &PadSrcRef,
        _tonesrc: &ToneSrc,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", query);

        let ret = match query.view_mut() {
            QueryView::Latency(ref mut q) => {
                q.set(true, 0.into(), gst::CLOCK_TIME_NONE);
                true
            }
            QueryView::Scheduling(ref mut q) => {
                q.set(gst::SchedulingFlags::SEQUENTIAL, 1, -1, 0);
                q.add_scheduling_modes(&[gst::PadMode::Push]);
                true
            }
            QueryView::Caps(ref mut q) => {
                let caps = pad.gst_pad().get_pad_template_caps().unwrap();
                let result = q
                    .get_filter()
                    .map(|f| f.intersect_with_mode(&caps, gst::CapsIntersectMode::First))
                    .unwrap_or(caps.clone());
                q.set_result(&result);

                true
            }
            _ => false,
        };

        if ret {
            gst_log!(CAT, obj: pad.gst_pad(), "Handled {:?}", query);
        } else {
            gst_log!(CAT, obj: pad.gst_pad(), "Didn't handle {:?}", query);
        }

        ret
    }

    fn src_event(
        &self,
        pad: &PadSrcRef,
        _tonesrc: &ToneSrc,
        element: &gst::Element,
        event: gst::Event,
    ) -> Either<bool, BoxFuture<'static, bool>> {
        use gst::EventView;

        gst_log!(CAT, obj: pad.gst_pad(), "Handling event {:?}", event);

        let ret = match event.view() {
            EventView::FlushStart(..) => {
                let mut flush_join_handle = self.0.flush_join_handle.lock().unwrap();
                if flush_join_handle.is_none() {
                    let element = element.clone();
                    let pad_weak = pad.downgrade();

                    *flush_join_handle = Some(pad.spawn(async move {
                        let res = ToneSrc::from_instance(&element).stop(&element).await;
                        let pad = pad_weak.upgrade().unwrap();
                        if res.is_ok() {
                            gst_debug!(CAT, obj: pad.gst_pad(), "FlushStart complete");
                        } else {
                            gst_debug!(CAT, obj: pad.gst_pad(), "FlushStart failed");
                        }

                        res
                    }));
                } else {
                    gst_debug!(CAT, obj: pad.gst_pad(), "FlushStart ignored: previous Flush in progress");
                }

                true
            }
            EventView::FlushStop(..) => {
                let (ret, state, pending) = element.get_state(0.into());
                if ret == Ok(gst::StateChangeSuccess::Success) && state == gst::State::Playing
                    || ret == Ok(gst::StateChangeSuccess::Async) && pending == gst::State::Playing
                {
                    let element = element.clone();
                    let inner_weak = Arc::downgrade(&self.0);
                    let pad_weak = pad.downgrade();

                    let fut = async move {
                        let mut ret = false;

                        let pad = pad_weak.upgrade().unwrap();
                        let inner_weak = inner_weak.upgrade().unwrap();
                        let flush_join_handle = inner_weak.flush_join_handle.lock().unwrap().take();
                        if let Some(flush_join_handle) = flush_join_handle {
                            if let Ok(Ok(())) = flush_join_handle.await {
                                ret = ToneSrc::from_instance(&element)
                                    .start(&element)
                                    .await
                                    .is_ok();
                                gst_debug!(CAT, obj: pad.gst_pad(), "FlushStop complete");
                            } else {
                                gst_debug!(CAT, obj: pad.gst_pad(), "FlushStop aborted: FlushStart failed");
                            }
                        } else {
                            gst_debug!(CAT, obj: pad.gst_pad(), "FlushStop ignored: no Flush in progress");
                        }

                        ret
                    }
                    .boxed();

                    return Either::Right(fut);
                }
                true
            }
            EventView::Reconfigure(..) => true,
            EventView::Latency(..) => true,
            _ => false,
        };

        if ret {
            gst_log!(CAT, obj: pad.gst_pad(), "Handled event {:?}", event);
        } else {
            gst_log!(CAT, obj: pad.gst_pad(), "Didn't handle event {:?}", event);
        }

        Either::Left(ret)
    }
}

impl ToneSrcPadHandler {
    async fn start_task(&self, pad: PadSrcRef<'_>, element: &gst::Element) {
        let element = element.clone();
        let pad_weak = pad.downgrade();

        pad.start_task(move || {
            let element = element.clone();
            let pad_weak = pad_weak.clone();

            async move {
                let pad = pad_weak.upgrade().expect("PadSrc no longer exists");

                if ToneSrc::from_instance(&element)
                    .timeout(&element)
                    .await
                    .is_err()
                {
                    pad.pause_task().await;
                    return;
                }
            }
        })
        .await;
    }
}

struct State {
    need_initial_events: bool,
    buffer_pool: Option<gst::BufferPool>,
    sample_offset: u64,
    start_time: gst::ClockTime,
    tone_gen: Option<(tonegen::ToneGen, tonegen::ToneGenSettings)>,
    last_time: Option<gst::ClockTime>,
    interval: Option<tokio::time::Interval>,
}

impl Default for State {
    fn default() -> State {
        State {
            need_initial_events: true,
            buffer_pool: None,
            sample_offset: 0,
            start_time: gst::CLOCK_TIME_NONE,
            tone_gen: None,
            last_time: None,
            interval: None,
        }
    }
}

struct ToneSrc {
    src_pad: PadSrc,
    src_pad_handler: ToneSrcPadHandler,
    state: Mutex<State>,
    settings: StdMutex<Settings>,
}

lazy_static! {
    static ref CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-tonesrc",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing tone source"),
    );
}

impl ToneSrc {
    async fn timeout(&self, element: &gst::Element) -> Result<(), ()> {
        let mut events = Vec::new();
        let mut state = self.state.lock().await;

        if state.interval.is_none() {
            let settings = self.settings.lock().unwrap();
            let timeout = gst::SECOND
                .mul_div_floor(settings.samples_per_buffer as u64, 8000)
                .unwrap()
                .unwrap();
            state.interval = Some(tokio::time::interval(time::Duration::from_nanos(timeout)));
        }

        state.interval.as_mut().unwrap().tick().await;

        if state.need_initial_events {
            gst_debug!(CAT, obj: element, "Pushing initial events");

            let stream_id = format!("{:08x}{:08x}", rand::random::<u32>(), rand::random::<u32>());
            events.push(gst::Event::new_stream_start(&stream_id).build());

            events.push(
                gst::Event::new_caps(&self.src_pad.gst_pad().get_pad_template_caps().unwrap())
                    .build(),
            );
            events.push(
                gst::Event::new_segment(&gst::FormattedSegment::<gst::format::Time>::new()).build(),
            );

            state.need_initial_events = false;
        }

        let buffer_pool = match state.buffer_pool {
            Some(ref pool) => pool.clone(),
            None => return Err(()),
        };

        drop(state);

        for event in events {
            self.src_pad.push_event(event).await;
        }

        let res = {
            match buffer_pool.acquire_buffer(None) {
                Err(err) => Err(err),
                Ok(mut buffer) => {
                    {
                        let buffer = buffer.get_mut().unwrap();

                        let settings = self.settings.lock().unwrap().clone();
                        let mut state = self.state.lock().await;

                        match &mut state.tone_gen {
                            &mut Some((_, ref old_settings))
                                if *old_settings == settings.tone_gen_settings =>
                            {
                                ()
                            }
                            &mut Some((ref mut tone_gen, ref mut old_settings)) => {
                                tone_gen.update(&settings.tone_gen_settings);
                                *old_settings = settings.tone_gen_settings.clone();
                            }
                            tone_gen => {
                                *tone_gen = Some((
                                    tonegen::ToneGen::new(&settings.tone_gen_settings),
                                    settings.tone_gen_settings,
                                ));
                            }
                        }

                        let samples = {
                            let mut data = buffer.map_writable().unwrap();
                            let data = data.as_mut_slice_of::<i16>().unwrap();
                            let tone_gen = state.tone_gen.as_mut().unwrap();
                            tone_gen.0.generate(data)
                        };

                        let timestamp = state.start_time
                            + gst::SECOND
                                .mul_div_floor(state.sample_offset, 8000)
                                .unwrap();
                        state.sample_offset += samples as u64;
                        buffer.set_pts(timestamp);
                        buffer.set_size((2 * samples) as usize);

                        let now = gst::util_get_timestamp();
                        if let Some(last_time) = state.last_time {
                            let expected_distance = gst::SECOND
                                .mul_div_floor(settings.samples_per_buffer as u64, 8000)
                                .unwrap();

                            if now - last_time > expected_distance + 5 * gst::MSECOND
                                || now - last_time < expected_distance - 5 * gst::MSECOND
                            {
                                gst_error!(
                                    CAT,
                                    obj: element,
                                    "Distance between last and current output too high/low: got {}, expected {}",
                                    now - last_time, expected_distance,
                                );
                            } else {
                                gst_trace!(
                                    CAT,
                                    obj: element,
                                    "Distance between last and current output normal: got {}, expected {}",
                                    now - last_time,
                                    expected_distance,
                                );
                            }
                        }
                        state.last_time = Some(now);
                    }

                    gst_log!(CAT, obj: element, "Forwarding buffer {:?}", buffer);
                    self.src_pad.push(buffer).await.map(|_| ())
                }
            }
        };

        match res {
            Ok(_) => {
                gst_log!(CAT, obj: element, "Successfully pushed item");
                Ok(())
            }
            Err(gst::FlowError::Flushing) | Err(gst::FlowError::Eos) => {
                gst_debug!(CAT, obj: element, "EOS");
                Err(())
            }
            Err(err) => {
                gst_error!(CAT, obj: element, "Got error {}", err);
                gst_element_error!(
                    element,
                    gst::StreamError::Failed,
                    ("Internal data stream error"),
                    ["streaming stopped, reason {}", err]
                );
                Err(())
            }
        }
    }

    async fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Preparing");

        let context = {
            let settings = self.settings.lock().unwrap();
            Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to acquire Context: {}", err]
                )
            })?
        };

        self.src_pad
            .prepare(context, &ToneSrcPadHandler::default())
            .await
            .map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Error preparing src_pads: {:?}", err]
                )
            })?;

        gst_debug!(CAT, obj: element, "Prepared");

        Ok(())
    }

    async fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(CAT, obj: element, "Unpreparing");

        let mut state = self.state.lock().await;

        self.src_pad.stop_task().await;
        let _ = self.src_pad.unprepare().await;

        *state = State::default();

        gst_debug!(CAT, obj: element, "Unprepared");

        Ok(())
    }

    async fn start(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Starting");

        let clock = element.get_clock();
        if clock != Some(gst::SystemClock::obtain()) {
            return Err(gst_error_msg!(
                gst::LibraryError::Settings,
                ["Only works if the system clock is used"]
            ));
        }
        let clock = clock.unwrap();

        let samples_per_buffer = self.settings.lock().unwrap().samples_per_buffer;
        let mut state = self.state.lock().await;

        let State {
            ref mut buffer_pool,
            ref mut start_time,
            ..
        } = *state;

        let caps = self.src_pad.gst_pad().get_pad_template_caps().unwrap();
        let pool = gst::BufferPool::new();
        let mut config = pool.get_config();
        config.set_params(Some(&caps), 2 * samples_per_buffer, 0, 0);
        pool.set_config(config).map_err(|_| {
            gst_error_msg!(
                gst::ResourceError::Settings,
                ["Failed to configure buffer pool"]
            )
        })?;
        pool.set_active(true).map_err(|_| {
            gst_error_msg!(
                gst::ResourceError::Settings,
                ["Failed to activate buffer pool"]
            )
        })?;
        *buffer_pool = Some(pool);

        *start_time = clock.get_time() - element.get_base_time();

        self.src_pad_handler
            .start_task(self.src_pad.as_ref(), element)
            .await;

        gst_debug!(CAT, obj: element, "Started");

        Ok(())
    }

    async fn stop(&self, element: &gst::Element) -> Result<(), ()> {
        gst_debug!(CAT, obj: element, "Stopping");
        let mut state = self.state.lock().await;

        self.src_pad.pause_task().await;

        if let Some(pool) = state.buffer_pool.take() {
            let _ = pool.set_active(false);
        }

        gst_debug!(CAT, obj: element, "Stopped");

        Ok(())
    }
}

impl ObjectSubclass for ToneSrc {
    const NAME: &'static str = "RsTsToneSrc";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Thread-sharing tone source",
            "Source/Generic",
            "Thread-sharing tone source",
            "Sebastian Dröge <sebastian@centricular.com>",
        );

        let caps = gst::Caps::new_simple(
            "audio/x-raw",
            &[
                ("format", &gst_audio::AUDIO_FORMAT_S16.to_string()),
                ("layout", &"interleaved"),
                ("rate", &8000i32),
                ("channels", &1i32),
            ],
        );

        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        klass.install_properties(&PROPERTIES);
    }

    fn new() -> Self {
        unreachable!()
    }

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("src").unwrap();
        let src_pad = PadSrc::new_from_template(&templ, Some("src"));

        Self {
            src_pad: src_pad,
            src_pad_handler: ToneSrcPadHandler::default(),
            state: Mutex::new(State::default()),
            settings: StdMutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for ToneSrc {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        let mut settings = self.settings.lock().unwrap();
        match *prop {
            subclass::Property("context", ..) => {
                settings.context = value.get().unwrap().unwrap_or_else(|| "".into());
            }
            subclass::Property("context-wait", ..) => {
                settings.context_wait = value.get_some().unwrap();
            }
            subclass::Property("samples-per-buffer", ..) => {
                settings.samples_per_buffer = value.get_some().unwrap();
            }
            subclass::Property("freq1", ..) => {
                settings.tone_gen_settings.freq1 = value.get_some().unwrap();
            }
            subclass::Property("vol1", ..) => {
                settings.tone_gen_settings.vol1 = value.get_some().unwrap();
            }

            subclass::Property("freq2", ..) => {
                settings.tone_gen_settings.freq2 = value.get_some().unwrap();
            }
            subclass::Property("vol2", ..) => {
                settings.tone_gen_settings.vol2 = value.get_some().unwrap();
            }
            subclass::Property("on-time1", ..) => {
                settings.tone_gen_settings.on_time1 = value.get_some().unwrap();
            }
            subclass::Property("off-time1", ..) => {
                settings.tone_gen_settings.off_time1 = value.get_some().unwrap();
            }
            subclass::Property("on-time2", ..) => {
                settings.tone_gen_settings.on_time2 = value.get_some().unwrap();
            }
            subclass::Property("off-time2", ..) => {
                settings.tone_gen_settings.off_time2 = value.get_some().unwrap();
            }
            subclass::Property("repeat", ..) => {
                settings.tone_gen_settings.repeat = value.get_some().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        let settings = self.settings.lock().unwrap();
        match *prop {
            subclass::Property("context", ..) => {
                Ok(settings.context.to_value())
            }
            subclass::Property("context-wait", ..) => {
                Ok(settings.context_wait.to_value())
            }
            subclass::Property("samples-per-buffer", ..) => {
                Ok(settings.samples_per_buffer.to_value())
            }
            subclass::Property("freq1", ..) => {
                Ok(settings.tone_gen_settings.freq1.to_value())
            }
            subclass::Property("vol1", ..) => {
                Ok(settings.tone_gen_settings.vol1.to_value())
            }
            subclass::Property("freq2", ..) => {
                Ok(settings.tone_gen_settings.freq2.to_value())
            }
            subclass::Property("vol2", ..) => {
                Ok(settings.tone_gen_settings.vol2.to_value())
            }
            subclass::Property("on-time1", ..) => {
                Ok(settings.tone_gen_settings.on_time1.to_value())
            }
            subclass::Property("off-time1", ..) => {
                Ok(settings.tone_gen_settings.off_time1.to_value())
            }
            subclass::Property("on-time2", ..) => {
                Ok(settings.tone_gen_settings.on_time2.to_value())
            }
            subclass::Property("off-time2", ..) => {
                Ok(settings.tone_gen_settings.off_time2.to_value())
            }
            subclass::Property("repeat", ..) => {
                Ok(settings.tone_gen_settings.repeat.to_value())
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(self.src_pad.gst_pad()).unwrap();

        super::set_element_flags(element, gst::ElementFlags::SOURCE);
    }
}

impl ElementImpl for ToneSrc {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                runtime::executor::block_on(self.prepare(element)).map_err(|err| {
                    element.post_error_message(&err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PlayingToPaused => {
                runtime::executor::block_on(self.stop(element))
                    .map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                runtime::executor::block_on(self.unprepare(element))
                    .map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        let mut ret = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::ReadyToPaused => {
                ret = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToPlaying => {
                runtime::executor::block_on(self.start(element))
                    .map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::PlayingToPaused => {
                ret = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToReady => {
                let mut state = runtime::executor::block_on(self.state.lock());
                state.need_initial_events = true;
            }
            _ => (),
        }

        Ok(ret)
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "ts-tonesrc",
        gst::Rank::None,
        ToneSrc::get_type(),
    )
}

mod tonegen {
    use super::*;
    use std::os::raw::c_void;
    use std::ptr;

    #[repr(C)]
    struct ToneGenDescriptor(c_void);
    #[repr(C)]
    struct ToneGenState(c_void);

    pub struct ToneGen(ptr::NonNull<ToneGenState>, ptr::NonNull<ToneGenDescriptor>);

    #[derive(Debug, PartialEq, Eq, Clone)]
    pub struct ToneGenSettings {
        pub freq1: u32,
        pub vol1: i32,
        pub freq2: u32,
        pub vol2: i32,
        pub on_time1: u32,
        pub off_time1: u32,
        pub on_time2: u32,
        pub off_time2: u32,
        pub repeat: bool,
    }

    impl Default for ToneGenSettings {
        fn default() -> Self {
            Self {
                freq1: DEFAULT_FREQ1,
                vol1: DEFAULT_VOL1,
                freq2: DEFAULT_FREQ2,
                vol2: DEFAULT_VOL2,
                on_time1: DEFAULT_ON_TIME1,
                off_time1: DEFAULT_OFF_TIME1,
                on_time2: DEFAULT_ON_TIME2,
                off_time2: DEFAULT_OFF_TIME2,
                repeat: DEFAULT_REPEAT,
            }
        }
    }

    extern "C" {
        fn tone_gen_descriptor_init(
            ptr: *mut ToneGenDescriptor,
            freq1: i32,
            vol1: i32,
            freq2: i32,
            vol2: i32,
            on_time1: i32,
            off_time1: i32,
            on_time2: i32,
            off_time2: i32,
            repeat: i32,
        ) -> *mut ToneGenDescriptor;
        fn tone_gen_descriptor_free(ptr: *mut ToneGenDescriptor);

        fn tone_gen_init(ptr: *mut ToneGenState, desc: *mut ToneGenDescriptor)
            -> *mut ToneGenState;
        fn tone_gen_free(ptr: *mut ToneGenState);

        fn tone_gen(ptr: *mut ToneGenState, amp: *mut i16, max_samples: i32) -> i32;
    }

    impl ToneGen {
        pub fn new(settings: &ToneGenSettings) -> Self {
            unsafe {
                let ptr = ptr::NonNull::new(tone_gen_descriptor_init(
                    ptr::null_mut(),
                    settings.freq1 as i32,
                    settings.vol1,
                    settings.freq2 as i32,
                    settings.vol2,
                    settings.on_time1 as i32,
                    settings.off_time1 as i32,
                    settings.on_time2 as i32,
                    settings.off_time2 as i32,
                    if settings.repeat { 1 } else { 0 },
                ))
                .unwrap();
                let ptr2 = ptr::NonNull::new(tone_gen_init(ptr::null_mut(), ptr.as_ptr())).unwrap();

                ToneGen(ptr2, ptr)
            }
        }

        pub fn update(&mut self, settings: &ToneGenSettings) {
            unsafe {
                let ptr = ptr::NonNull::new(tone_gen_descriptor_init(
                    self.1.as_ptr(),
                    settings.freq1 as i32,
                    settings.vol1,
                    settings.freq2 as i32,
                    settings.vol2,
                    settings.on_time1 as i32,
                    settings.off_time1 as i32,
                    settings.on_time2 as i32,
                    settings.off_time2 as i32,
                    if settings.repeat { 1 } else { 0 },
                ))
                .unwrap();
                self.1 = ptr;

                let ptr2 =
                    ptr::NonNull::new(tone_gen_init(self.0.as_ptr(), self.1.as_ptr())).unwrap();
                self.0 = ptr2;
            }
        }

        pub fn generate(&mut self, amp: &mut [i16]) -> i32 {
            unsafe { tone_gen(self.0.as_ptr(), amp.as_mut_ptr(), amp.len() as i32) }
        }
    }

    impl Drop for ToneGen {
        fn drop(&mut self) {
            unsafe {
                tone_gen_descriptor_free(self.1.as_ptr());
                tone_gen_free(self.0.as_ptr());
            }
        }
    }

    unsafe impl Send for ToneGen {}
}
