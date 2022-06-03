// Copyright (C) 2022 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use once_cell::sync::Lazy;

use minidom::Element;

use std::collections::BTreeMap;
use std::sync::Mutex;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "onvifmetadataparse",
        gst::DebugColorFlags::empty(),
        Some("ONVIF Metadata Parser Element"),
    )
});

static NTP_CAPS: Lazy<gst::Caps> = Lazy::new(|| gst::Caps::builder("timestamp/x-ntp").build());
static UNIX_CAPS: Lazy<gst::Caps> = Lazy::new(|| gst::Caps::builder("timestamp/x-unix").build());
const NTP_UNIX_OFFSET: gst::ClockTime = gst::ClockTime::from_seconds(2_208_988_800);

#[derive(Clone, Debug)]
struct Settings {
    latency: gst::ClockTime,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            latency: gst::ClockTime::from_seconds(6),
        }
    }
}

#[derive(Default, Debug)]
struct State {
    // Initially queued buffers until we have a UTC time / PTS mapping
    pre_queued_buffers: Vec<gst::Buffer>,
    // Mapping of UTC time to PTS
    utc_time_pts_mapping: Option<(gst::ClockTime, gst::ClockTime)>,
    // UTC time -> XML
    queued_frames: BTreeMap<gst::ClockTime, Element>,
}

pub struct OnvifMetadataParse {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

impl OnvifMetadataParse {
    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        element: &super::OnvifMetadataParse,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::log!(CAT, obj: element, "Handling buffer {:?}", buffer);

        let settings = self.settings.lock().unwrap().clone();
        let mut state = self.state.lock().unwrap();

        // First we need to get an UTC/PTS mapping. We wait up to the latency
        // for that and otherwise error out.
        if state.utc_time_pts_mapping.is_none() {
            let pts = match buffer.pts() {
                Some(pts) => pts,
                None => {
                    gst::error!(CAT, obj: element, "Need buffers with PTS");
                    return Err(gst::FlowError::Error);
                }
            };

            let utc_time = buffer
                .iter_meta::<gst::ReferenceTimestampMeta>()
                .find_map(|meta| {
                    if meta.reference().can_intersect(&*NTP_CAPS) {
                        meta.timestamp().checked_sub(NTP_UNIX_OFFSET)
                    } else if meta.reference().can_intersect(&*UNIX_CAPS) {
                        Some(meta.timestamp())
                    } else {
                        None
                    }
                });

            if let Some(utc_time) = utc_time {
                let initial_pts = state
                    .pre_queued_buffers
                    .first()
                    .map(|b| b.pts().unwrap())
                    .unwrap_or(pts);
                let diff = pts.saturating_sub(initial_pts);
                let initial_utc_time = match utc_time.checked_sub(diff) {
                    Some(initial_utc_time) => initial_utc_time,
                    None => {
                        gst::error!(CAT, obj: element, "Can't calculate initial UTC time");
                        return Err(gst::FlowError::Error);
                    }
                };

                gst::info!(
                    CAT,
                    obj: element,
                    "Calculated initial UTC/PTS mapping: {}/{}",
                    initial_utc_time,
                    initial_pts
                );
                state.utc_time_pts_mapping = Some((initial_utc_time, initial_pts));
            } else {
                state.pre_queued_buffers.push(buffer);

                if let Some(front_pts) = state.pre_queued_buffers.first().map(|b| b.pts().unwrap())
                {
                    if pts.saturating_sub(front_pts) >= settings.latency {
                        gst::error!(
                            CAT,
                            obj: element,
                            "Received no UTC time in the first {}",
                            settings.latency
                        );
                        return Err(gst::FlowError::Error);
                    }
                }

                return Ok(gst::FlowSuccess::Ok);
            }
        }

        let pts = match buffer.pts() {
            Some(pts) => pts,
            None => {
                gst::error!(CAT, obj: element, "Need buffers with PTS");
                return Err(gst::FlowError::Error);
            }
        };

        self.queue(element, &mut state, &settings, buffer)?;
        let buffers = self.drain(element, &mut state, &settings, Some(pts))?;

        if let Some(buffers) = buffers {
            drop(state);
            self.srcpad.push_list(buffers)
        } else {
            Ok(gst::FlowSuccess::Ok)
        }
    }

    fn queue(
        &self,
        element: &super::OnvifMetadataParse,
        state: &mut State,
        _settings: &Settings,
        buffer: gst::Buffer,
    ) -> Result<(), gst::FlowError> {
        let State {
            ref mut pre_queued_buffers,
            ref mut queued_frames,
            ..
        } = &mut *state;

        for buffer in pre_queued_buffers.drain(..).chain(std::iter::once(buffer)) {
            let map = buffer.map_readable().map_err(|_| {
                gst::element_error!(
                    element,
                    gst::ResourceError::Read,
                    ["Failed to map buffer readable"]
                );

                gst::FlowError::Error
            })?;

            let utf8 = std::str::from_utf8(map.as_slice()).map_err(|err| {
                gst::element_error!(
                    element,
                    gst::StreamError::Format,
                    ["Failed to decode buffer as UTF-8: {}", err]
                );

                gst::FlowError::Error
            })?;

            let root = utf8.parse::<Element>().map_err(|err| {
                gst::element_error!(
                    element,
                    gst::ResourceError::Read,
                    ["Failed to parse buffer as XML: {}", err]
                );

                gst::FlowError::Error
            })?;

            if let Some(analytics) =
                root.get_child("VideoAnalytics", "http://www.onvif.org/ver10/schema")
            {
                for el in analytics.children() {
                    if el.is("Frame", "http://www.onvif.org/ver10/schema") {
                        let timestamp = el.attr("UtcTime").ok_or_else(|| {
                            gst::element_error!(
                                element,
                                gst::ResourceError::Read,
                                ["Frame element has no UtcTime attribute"]
                            );

                            gst::FlowError::Error
                        })?;

                        let dt =
                            chrono::DateTime::parse_from_rfc3339(timestamp).map_err(|err| {
                                gst::element_error!(
                                    element,
                                    gst::ResourceError::Read,
                                    ["Failed to parse UtcTime {}: {}", timestamp, err]
                                );

                                gst::FlowError::Error
                            })?;

                        let dt_unix_ns = gst::ClockTime::from_nseconds(dt.timestamp_nanos() as u64);

                        gst::trace!(
                            CAT,
                            obj: element,
                            "Queueing frame with UTC time {}",
                            dt_unix_ns
                        );

                        let xml = queued_frames.entry(dt_unix_ns).or_insert_with(|| {
                            Element::builder("VideoAnalytics", "http://www.onvif.org/ver10/schema")
                                .prefix(Some("tt".into()), "http://www.onvif.org/ver10/schema")
                                .unwrap()
                                .build()
                        });

                        xml.append_child(el.clone());
                    }
                }
            }
        }

        Ok(())
    }

    fn drain(
        &self,
        element: &super::OnvifMetadataParse,
        state: &mut State,
        settings: &Settings,
        pts: Option<gst::ClockTime>,
    ) -> Result<Option<gst::BufferList>, gst::FlowError> {
        let State {
            ref mut queued_frames,
            utc_time_pts_mapping,
            ..
        } = &mut *state;

        let utc_time_pts_mapping = match utc_time_pts_mapping {
            Some(utc_time_pts_mapping) => utc_time_pts_mapping,
            None => return Ok(None),
        };

        let utc_time_to_pts = |utc_time: gst::ClockTime| {
            if utc_time < utc_time_pts_mapping.0 {
                let diff = utc_time_pts_mapping.0 - utc_time;
                utc_time_pts_mapping.1.checked_sub(diff)
            } else {
                let diff = utc_time - utc_time_pts_mapping.0;
                Some(utc_time_pts_mapping.1 + diff)
            }
        };

        let mut buffers = Vec::new();

        while !queued_frames.is_empty() {
            let utc_time = *queued_frames.iter().next().unwrap().0;
            let frame_pts = match utc_time_to_pts(utc_time) {
                Some(frame_pts) => frame_pts,
                None => {
                    gst::warning!(CAT, obj: element, "UTC time {} outside segment", utc_time);
                    gst::ClockTime::ZERO
                }
            };

            // Not at EOS and not above the latency yet
            if pts.map_or(false, |pts| {
                pts.saturating_sub(frame_pts) < settings.latency
            }) {
                break;
            }

            let frame = queued_frames.remove(&utc_time).unwrap();

            gst::trace!(
                CAT,
                obj: element,
                "Dequeueing frame with UTC time {} / PTS {}",
                utc_time,
                frame_pts
            );

            let xml = Element::builder("MetadataStream", "http://www.onvif.org/ver10/schema")
                .prefix(Some("tt".into()), "http://www.onvif.org/ver10/schema")
                .unwrap()
                .append(frame)
                .build();

            let mut vec = Vec::new();
            if let Err(err) = xml.write_to_decl(&mut vec) {
                gst::error!(CAT, obj: element, "Can't serialize XML element: {}", err);
                continue;
            }

            let mut buffer = gst::Buffer::from_mut_slice(vec);
            let buffer_ref = buffer.get_mut().unwrap();
            buffer_ref.set_pts(frame_pts);

            gst::ReferenceTimestampMeta::add(
                buffer_ref,
                &*UNIX_CAPS,
                utc_time,
                gst::ClockTime::NONE,
            );

            buffers.push(buffer);
        }

        buffers.sort_by_key(|b| b.pts());

        if !buffers.is_empty() {
            let mut buffer_list = gst::BufferList::new_sized(buffers.len());
            let buffer_list_ref = buffer_list.get_mut().unwrap();
            buffer_list_ref.extend(buffers);

            Ok(Some(buffer_list))
        } else {
            Ok(None)
        }
    }

    fn sink_event(
        &self,
        pad: &gst::Pad,
        element: &super::OnvifMetadataParse,
        event: gst::Event,
    ) -> bool {
        gst::log!(CAT, obj: pad, "Handling event {:?}", event);

        match event.view() {
            gst::EventView::Segment(_) | gst::EventView::Eos(_) => {
                let settings = self.settings.lock().unwrap().clone();
                let mut state = self.state.lock().unwrap();
                let buffers = self
                    .drain(element, &mut state, &settings, None)
                    .ok()
                    .flatten();
                state.pre_queued_buffers.clear();
                state.utc_time_pts_mapping = None;
                state.queued_frames.clear();
                drop(state);

                if let Some(buffers) = buffers {
                    if let Err(err) = self.srcpad.push_list(buffers) {
                        gst::error!(CAT, obj: element, "Failed to drain frames: {}", err);
                    }
                }

                pad.event_default(Some(element), event)
            }
            gst::EventView::FlushStop(_) => {
                let mut state = self.state.lock().unwrap();
                state.pre_queued_buffers.clear();
                state.queued_frames.clear();
                state.utc_time_pts_mapping = None;
                drop(state);
                pad.event_default(Some(element), event)
            }
            gst::EventView::Caps(_) => {
                let caps = self.srcpad.pad_template_caps();
                self.srcpad
                    .push_event(gst::event::Caps::builder(&caps).build())
            }
            _ => pad.event_default(Some(element), event),
        }
    }

    fn sink_query(
        &self,
        pad: &gst::Pad,
        element: &super::OnvifMetadataParse,
        query: &mut gst::QueryRef,
    ) -> bool {
        gst::log!(CAT, obj: pad, "Handling query {:?}", query);

        match query.view_mut() {
            gst::QueryViewMut::Caps(q) => {
                let caps = pad.pad_template_caps();
                let res = if let Some(filter) = q.filter() {
                    filter.intersect_with_mode(&caps, gst::CapsIntersectMode::First)
                } else {
                    caps
                };

                q.set_result(&res);

                true
            }
            gst::QueryViewMut::AcceptCaps(q) => {
                let caps = q.caps();
                let res = caps.can_intersect(&pad.pad_template_caps());
                q.set_result(res);

                true
            }
            _ => pad.query_default(Some(element), query),
        }
    }

    fn src_event(
        &self,
        pad: &gst::Pad,
        element: &super::OnvifMetadataParse,
        event: gst::Event,
    ) -> bool {
        gst::log!(CAT, obj: pad, "Handling event {:?}", event);

        match event.view() {
            gst::EventView::FlushStop(_) => {
                let mut state = self.state.lock().unwrap();
                state.pre_queued_buffers.clear();
                state.queued_frames.clear();
                state.utc_time_pts_mapping = None;
                drop(state);
                pad.event_default(Some(element), event)
            }
            _ => pad.event_default(Some(element), event),
        }
    }

    fn src_query(
        &self,
        pad: &gst::Pad,
        element: &super::OnvifMetadataParse,
        query: &mut gst::QueryRef,
    ) -> bool {
        gst::log!(CAT, obj: pad, "Handling query {:?}", query);

        match query.view_mut() {
            gst::QueryViewMut::Caps(q) => {
                let caps = pad.pad_template_caps();
                let res = if let Some(filter) = q.filter() {
                    filter.intersect_with_mode(&caps, gst::CapsIntersectMode::First)
                } else {
                    caps
                };

                q.set_result(&res);

                true
            }
            gst::QueryViewMut::AcceptCaps(q) => {
                let caps = q.caps();
                let res = caps.can_intersect(&pad.pad_template_caps());
                q.set_result(res);

                true
            }
            gst::QueryViewMut::Latency(q) => {
                let mut upstream_query = gst::query::Latency::new();

                let ret = self.sinkpad.peer_query(&mut upstream_query);

                if ret {
                    let (live, mut min, mut max) = upstream_query.result();

                    let settings = self.settings.lock().unwrap();
                    min += settings.latency;
                    max = max.map(|max| max + settings.latency);

                    q.set(live, min, max);

                    gst::debug!(
                        CAT,
                        obj: pad,
                        "Latency query response: live {} min {} max {}",
                        live,
                        min,
                        max.display()
                    );
                }

                ret
            }
            _ => pad.query_default(Some(element), query),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for OnvifMetadataParse {
    const NAME: &'static str = "OnvifMetadataParse";
    type Type = super::OnvifMetadataParse;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                OnvifMetadataParse::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |parse, element| parse.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                OnvifMetadataParse::catch_panic_pad_function(
                    parent,
                    || false,
                    |parse, element| parse.sink_event(pad, element, event),
                )
            })
            .query_function(|pad, parent, query| {
                OnvifMetadataParse::catch_panic_pad_function(
                    parent,
                    || false,
                    |parse, element| parse.sink_query(pad, element, query),
                )
            })
            .flags(gst::PadFlags::PROXY_ALLOCATION)
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .event_function(|pad, parent, event| {
                OnvifMetadataParse::catch_panic_pad_function(
                    parent,
                    || false,
                    |parse, element| parse.src_event(pad, element, event),
                )
            })
            .query_function(|pad, parent, query| {
                OnvifMetadataParse::catch_panic_pad_function(
                    parent,
                    || false,
                    |parse, element| parse.src_query(pad, element, query),
                )
            })
            .flags(gst::PadFlags::PROXY_ALLOCATION)
            .flags(gst::PadFlags::FIXED_CAPS)
            .build();

        Self {
            srcpad,
            sinkpad,
            settings: Mutex::default(),
            state: Mutex::default(),
        }
    }
}

impl ObjectImpl for OnvifMetadataParse {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![glib::ParamSpecUInt64::new(
                "latency",
                "Latency",
                "Maximum latency to introduce for reordering metadata",
                0,
                u64::MAX,
                Settings::default().latency.nseconds(),
                glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
            )]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(
        &self,
        obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "latency" => {
                self.settings.lock().unwrap().latency = value.get().expect("type checked upstream");

                let _ = obj.post_message(gst::message::Latency::builder().src(obj).build());
            }
            _ => unimplemented!(),
        };
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "latency" => self.settings.lock().unwrap().latency.to_value(),
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }
}

impl GstObjectImpl for OnvifMetadataParse {}

impl ElementImpl for OnvifMetadataParse {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "ONVIF Metadata Parser",
                "Metadata/Parser",
                "Parses ONVIF Timed XML Metadata",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::builder("application/x-onvif-metadata")
                .field("encoding", "utf8")
                .field("parsed", true)
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let caps = gst::Caps::builder("application/x-onvif-metadata")
                .field("encoding", "utf8")
                .build();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst::trace!(CAT, obj: element, "Changing state {:?}", transition);

        if transition == gst::StateChange::PausedToReady {
            let mut state = self.state.lock().unwrap();
            *state = State::default();
        }

        let ret = self.parent_change_state(element, transition)?;

        if transition == gst::StateChange::ReadyToPaused {
            let mut state = self.state.lock().unwrap();
            *state = State::default();
        }

        Ok(ret)
    }
}
