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

use futures::future::BoxFuture;
use futures::lock::Mutex as FutMutex;
use futures::prelude::*;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_log, gst_trace};
use gst_audio;

use once_cell::sync::Lazy;

use std::i32;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time;

use crate::runtime::prelude::*;
use crate::runtime::{Context, PadSrc, PadSrcRef, PadSrcWeak, Task};

use rand;

use muldiv::MulDiv;

use byte_slice_cast::*;

const DEFAULT_CONTEXT: &'static str = "";
const DEFAULT_CONTEXT_WAIT: gst::ClockTime = gst::ClockTime::ZERO;
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
    context_wait: gst::ClockTime,
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

struct ToneSrcPadState {
    need_initial_events: bool,
    need_segment: bool,
    sample_offset: u64,
    tone_gen: Option<(tonegen::ToneGen, tonegen::ToneGenSettings)>,
    last_time: Option<gst::ClockTime>,
    interval: Option<tokio::time::Interval>,
}

impl Default for ToneSrcPadState {
    fn default() -> Self {
        Self {
            need_initial_events: true,
            need_segment: true,
            sample_offset: 0,
            tone_gen: None,
            last_time: None,
            interval: None,
        }
    }
}

#[derive(Debug, Default)]
struct ToneSrcPadHandlerInner {
    state: FutMutex<ToneSrcPadState>,
}

#[derive(Clone, Debug, Default)]
struct ToneSrcPadHandler(Arc<ToneSrcPadHandlerInner>);

impl ToneSrcPadHandler {
    async fn reset_state(&self) {
        *self.0.state.lock().await = Default::default();
    }

    async fn set_need_segment(&self) {
        self.0.state.lock().await.need_segment = true;
    }

    async fn push_prelude(&self, pad: &PadSrcRef<'_>, _element: &super::ToneSrc) {
        let mut state = self.0.state.lock().await;
        if state.need_initial_events {
            gst_debug!(CAT, obj: pad.gst_pad(), "Pushing initial events");

            let stream_id = format!("{:08x}{:08x}", rand::random::<u32>(), rand::random::<u32>());
            let stream_start_evt = gst::event::StreamStart::builder(&stream_id)
                .group_id(gst::GroupId::next())
                .build();
            pad.push_event(stream_start_evt).await;

            pad.push_event(gst::event::Caps::new(&pad.gst_pad().pad_template_caps()))
                .await;

            state.need_initial_events = false;
        }

        if state.need_segment {
            let segment_evt =
                gst::event::Segment::new(&gst::FormattedSegment::<gst::format::Time>::new());
            pad.push_event(segment_evt).await;

            state.need_segment = false;
        }
    }

    async fn timeout(
        &self,
        _pad: &PadSrcRef<'_>,
        element: &super::ToneSrc,
        buffer_pool: &gst::BufferPool,
        start_time: gst::ClockTime,
    ) -> Result<gst::Buffer, gst::FlowError> {
        let settings = {
            let this = ToneSrc::from_instance(element);
            this.settings.lock().unwrap().clone()
        };

        let mut buffer = match buffer_pool.acquire_buffer(None) {
            Err(err) => return Err(err),
            Ok(buffer) => buffer,
        };

        let mut state = self.0.state.lock().await;

        if state.interval.is_none() {
            let timeout = gst::ClockTime::SECOND
                .mul_div_floor(settings.samples_per_buffer as u64, 8000)
                .unwrap()
                .nseconds();
            state.interval = Some(tokio::time::interval(time::Duration::from_nanos(timeout)));
        }

        state.interval.as_mut().unwrap().tick().await;

        {
            let buffer = buffer.get_mut().unwrap();

            match &mut state.tone_gen {
                &mut Some((_, ref old_settings)) if *old_settings == settings.tone_gen_settings => {
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

            let timestamp = start_time
                + gst::ClockTime::SECOND
                    .mul_div_floor(state.sample_offset, 8000)
                    .unwrap();
            state.sample_offset += samples as u64;
            buffer.set_pts(timestamp);
            buffer.set_size((2 * samples) as usize);

            let now = gst::util_get_timestamp();
            if let Some(last_time) = state.last_time {
                let expected_distance = gst::ClockTime::SECOND
                    .mul_div_floor(settings.samples_per_buffer as u64, 8000)
                    .unwrap();

                if now - last_time > expected_distance + 5 * gst::ClockTime::MSECOND
                    || now - last_time < expected_distance - 5 * gst::ClockTime::MSECOND
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

        Ok(buffer)
    }

    async fn push_buffer(
        &self,
        pad: &PadSrcRef<'_>,
        element: &super::ToneSrc,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", buffer);

        self.push_prelude(pad, element).await;

        pad.push(buffer).await
    }
}

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
                q.set(true, gst::ClockTime::from_nseconds(0), gst::ClockTime::NONE);
                true
            }
            QueryView::Scheduling(ref mut q) => {
                q.set(gst::SchedulingFlags::SEQUENTIAL, 1, -1, 0);
                q.add_scheduling_modes(&[gst::PadMode::Push]);
                true
            }
            QueryView::Caps(ref mut q) => {
                let caps = pad.gst_pad().pad_template_caps();
                let result = q
                    .filter()
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
        tonesrc: &ToneSrc,
        _element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", event);

        let ret = match event.view() {
            EventView::FlushStart(..) => tonesrc.task.flush_start().is_ok(),
            EventView::FlushStop(..) => tonesrc.task.flush_stop().is_ok(),
            EventView::Reconfigure(..) => true,
            EventView::Latency(..) => true,
            _ => false,
        };

        if ret {
            gst_log!(CAT, obj: pad.gst_pad(), "Handled {:?}", event);
        } else {
            gst_log!(CAT, obj: pad.gst_pad(), "Didn't handle {:?}", event);
        }

        ret
    }
}

pub struct ToneSrc {
    src_pad: PadSrc,
    src_pad_handler: ToneSrcPadHandler,
    task: Task,
    settings: StdMutex<Settings>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "ts-tonesrc",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing tone source"),
    )
});

struct ToneSrcTask {
    element: super::ToneSrc,
    src_pad: PadSrcWeak,
    src_pad_handler: ToneSrcPadHandler,
    buffer_pool: gst::BufferPool,
    start_time: Option<gst::ClockTime>,
}

impl ToneSrcTask {
    fn new(
        element: &super::ToneSrc,
        src_pad: &PadSrc,
        src_pad_handler: &ToneSrcPadHandler,
        buffer_pool: gst::BufferPool,
    ) -> Self {
        ToneSrcTask {
            element: element.clone(),
            src_pad: src_pad.downgrade(),
            src_pad_handler: src_pad_handler.clone(),
            buffer_pool,
            start_time: gst::ClockTime::NONE,
        }
    }
}

impl TaskImpl for ToneSrcTask {
    fn start(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst_log!(CAT, obj: &self.element, "Starting task");

            let clock = self.element.clock();
            if clock != Some(gst::SystemClock::obtain()) {
                return Err(gst::error_msg!(
                    gst::LibraryError::Settings,
                    ["Only works if the system clock is used"]
                ));
            }
            let clock = clock.unwrap();

            self.start_time = Some(clock.time().unwrap() - self.element.base_time().unwrap());

            gst_log!(CAT, obj: &self.element, "Task started");
            Ok(())
        }
        .boxed()
    }

    fn iterate(&mut self) -> BoxFuture<'_, Result<(), gst::FlowError>> {
        async move {
            let pad = self.src_pad.upgrade().expect("PadSrc no longer exists");

            let res = match self
                .src_pad_handler
                .timeout(
                    &pad,
                    &self.element,
                    &self.buffer_pool,
                    self.start_time.unwrap(),
                )
                .await
            {
                Ok(buffer) => {
                    self.src_pad_handler
                        .push_buffer(&pad, &self.element, buffer)
                        .await
                }
                Err(err) => Err(err),
            };

            match res {
                Ok(_) => gst_log!(CAT, obj: &self.element, "Successfully pushed buffer"),
                Err(gst::FlowError::Flushing) => gst_debug!(CAT, obj: &self.element, "Flushing"),
                Err(gst::FlowError::Eos) => {
                    gst_debug!(CAT, obj: &self.element, "EOS");
                    pad.push_event(gst::event::Eos::new()).await;
                }
                Err(err) => {
                    gst_error!(CAT, obj: &self.element, "Got error {}", err);
                    gst::element_error!(
                        self.element,
                        gst::StreamError::Failed,
                        ("Internal data stream error"),
                        ["streaming stopped, reason {}", err]
                    );
                }
            }

            res.map(drop)
        }
        .boxed()
    }

    fn stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst_log!(CAT, obj: &self.element, "Stopping task");
            let _ = self.buffer_pool.set_active(false);
            self.src_pad_handler.reset_state().await;
            gst_log!(CAT, obj: &self.element, "Task stopped");
            Ok(())
        }
        .boxed()
    }

    fn flush_stop(&mut self) -> BoxFuture<'_, Result<(), gst::ErrorMessage>> {
        async move {
            gst_log!(CAT, obj: &self.element, "Stopping task flush");
            self.src_pad_handler.set_need_segment().await;
            gst_log!(CAT, obj: &self.element, "Stopped task flush");
            Ok(())
        }
        .boxed()
    }
}

impl ToneSrc {
    fn prepare(&self, element: &super::ToneSrc) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Preparing");

        let settings = self.settings.lock().unwrap().clone();
        let context =
            Context::acquire(&settings.context, settings.context_wait.into()).map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to acquire Context: {}", err]
                )
            })?;

        let caps = self.src_pad.gst_pad().pad_template_caps();
        let pool = gst::BufferPool::new();
        let mut config = pool.config();
        config.set_params(Some(&caps), 2 * settings.samples_per_buffer, 0, 0);
        pool.set_config(config).map_err(|_| {
            gst::error_msg!(
                gst::ResourceError::Settings,
                ["Failed to configure buffer pool"]
            )
        })?;
        pool.set_active(true).map_err(|_| {
            gst::error_msg!(
                gst::ResourceError::Settings,
                ["Failed to activate buffer pool"]
            )
        })?;

        self.task
            .prepare(
                ToneSrcTask::new(element, &self.src_pad, &self.src_pad_handler, pool),
                context,
            )
            .map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Error preparing Task: {:?}", err]
                )
            })?;

        gst_debug!(CAT, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &super::ToneSrc) {
        gst_debug!(CAT, obj: element, "Unpreparing");

        self.task.unprepare().unwrap();

        gst_debug!(CAT, obj: element, "Unprepared");
    }

    fn stop(&self, element: &super::ToneSrc) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Stopping");
        self.task.stop()?;
        gst_debug!(CAT, obj: element, "Stopped");
        Ok(())
    }

    fn start(&self, element: &super::ToneSrc) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Starting");
        self.task.start()?;
        gst_debug!(CAT, obj: element, "Started");
        Ok(())
    }

    fn pause(&self, element: &super::ToneSrc) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Pausing");
        self.task.pause()?;
        gst_debug!(CAT, obj: element, "Paused");
        Ok(())
    }
}

#[glib::object_subclass]
impl ObjectSubclass for ToneSrc {
    const NAME: &'static str = "RsTsToneSrc";
    type ParentType = gst::Element;
    type Type = super::ToneSrc;

    fn with_class(klass: &Self::Class) -> Self {
        let src_pad_handler = ToneSrcPadHandler::default();

        Self {
            src_pad: PadSrc::new(
                gst::Pad::from_template(&klass.pad_template("src").unwrap(), Some("src")),
                src_pad_handler.clone(),
            ),
            src_pad_handler,
            task: Task::default(),
            settings: StdMutex::new(Settings::default()),
        }
    }
}

impl ObjectImpl for ToneSrc {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpec::new_string(
                    "context",
                    "Context",
                    "Context name to share threads with",
                    Some(DEFAULT_CONTEXT),
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_uint(
                    "context-wait",
                    "Context Wait",
                    "Throttle poll loop to run at most once every this many ms",
                    0,
                    1000,
                    DEFAULT_CONTEXT_WAIT.mseconds() as u32,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_uint(
                    "samples-per-buffer",
                    "Samples Per Buffer",
                    "Number of samples per output buffer",
                    1,
                    u32::MAX,
                    DEFAULT_SAMPLES_PER_BUFFER,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_uint(
                    "freq1",
                    "Frequency 1",
                    "Frequency of first telephony tone component",
                    0,
                    4000,
                    DEFAULT_FREQ1,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_int(
                    "vol1",
                    "Volume 1",
                    "Volume of first telephony tone component",
                    -50,
                    0,
                    DEFAULT_VOL1,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_uint(
                    "freq2",
                    "Frequency 2",
                    "Frequency of second telephony tone component",
                    0,
                    4000,
                    DEFAULT_FREQ2,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_int(
                    "vol2",
                    "Volume 2",
                    "Volume of second telephony tone component",
                    -50,
                    0,
                    DEFAULT_VOL2,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_uint(
                    "on-time1",
                    "On Time 1",
                    "Time of the first period when the tone signal is present",
                    0,
                    u32::MAX,
                    DEFAULT_ON_TIME1,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_uint(
                    "off-time1",
                    "Off Time 1",
                    "Time of the first period when the tone signal is off",
                    0,
                    u32::MAX,
                    DEFAULT_OFF_TIME1,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_uint(
                    "on-time2",
                    "On Time 2",
                    "Time of the second period when the tone signal is present",
                    0,
                    u32::MAX,
                    DEFAULT_ON_TIME2,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_uint(
                    "off-time2",
                    "Off Time 2",
                    "Time of the second period when the tone signal is off",
                    0,
                    u32::MAX,
                    DEFAULT_OFF_TIME2,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_boolean(
                    "repeat",
                    "Repeat",
                    "Whether to repeat specified tone indefinitly",
                    DEFAULT_REPEAT,
                    glib::ParamFlags::READWRITE,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        _obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        let mut settings = self.settings.lock().unwrap();
        match pspec.name() {
            "context" => {
                settings.context = value
                    .get::<Option<String>>()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            "context-wait" => {
                settings.context_wait = gst::ClockTime::from_mseconds(
                    value.get::<u32>().expect("type checked upstream").into(),
                );
            }
            "samples-per-buffer" => {
                settings.samples_per_buffer = value.get().unwrap();
            }
            "freq1" => {
                settings.tone_gen_settings.freq1 = value.get().unwrap();
            }
            "vol1" => {
                settings.tone_gen_settings.vol1 = value.get().unwrap();
            }

            "freq2" => {
                settings.tone_gen_settings.freq2 = value.get().unwrap();
            }
            "vol2" => {
                settings.tone_gen_settings.vol2 = value.get().unwrap();
            }
            "on-time1" => {
                settings.tone_gen_settings.on_time1 = value.get().unwrap();
            }
            "off-time1" => {
                settings.tone_gen_settings.off_time1 = value.get().unwrap();
            }
            "on-time2" => {
                settings.tone_gen_settings.on_time2 = value.get().unwrap();
            }
            "off-time2" => {
                settings.tone_gen_settings.off_time2 = value.get().unwrap();
            }
            "repeat" => {
                settings.tone_gen_settings.repeat = value.get().unwrap();
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "context" => settings.context.to_value(),
            "context-wait" => (settings.context_wait.mseconds() as u32).to_value(),
            "samples-per-buffer" => settings.samples_per_buffer.to_value(),
            "freq1" => settings.tone_gen_settings.freq1.to_value(),
            "vol1" => settings.tone_gen_settings.vol1.to_value(),
            "freq2" => settings.tone_gen_settings.freq2.to_value(),
            "vol2" => settings.tone_gen_settings.vol2.to_value(),
            "on-time1" => settings.tone_gen_settings.on_time1.to_value(),
            "off-time1" => settings.tone_gen_settings.off_time1.to_value(),
            "on-time2" => settings.tone_gen_settings.on_time2.to_value(),
            "off-time2" => settings.tone_gen_settings.off_time2.to_value(),
            "repeat" => settings.tone_gen_settings.repeat.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(self.src_pad.gst_pad()).unwrap();

        crate::set_element_flags(obj, gst::ElementFlags::SOURCE);
    }
}

impl ElementImpl for ToneSrc {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Thread-sharing tone source",
                "Source/Generic",
                "Thread-sharing tone source",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
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

            vec![src_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                self.prepare(element).map_err(|err| {
                    element.post_error_message(err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::PlayingToPaused => {
                self.pause(element).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::ReadyToNull => {
                self.unprepare(element);
            }
            _ => (),
        }

        let mut success = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::ReadyToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToPlaying => {
                self.start(element).map_err(|_| gst::StateChangeError)?;
            }
            gst::StateChange::PlayingToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PausedToReady => {
                self.stop(element).map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        Ok(success)
    }
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
