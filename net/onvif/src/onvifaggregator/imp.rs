use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;
use gst_base::AGGREGATOR_FLOW_NEED_DATA;
use once_cell::sync::Lazy;
use std::sync::Mutex;

// Offset in nanoseconds from midnight 01-01-1900 (prime epoch) to
// midnight 01-01-1970 (UNIX epoch)
const PRIME_EPOCH_OFFSET: gst::ClockTime = gst::ClockTime::from_seconds(2_208_988_800);

#[derive(Default)]
struct State {
    // FIFO of MetaFrames
    meta_frames: Vec<gst::Buffer>,
    // We may store the next buffer we output here while waiting
    // for a future buffer, when we need one to calculate its duration
    current_media_buffer: Option<gst::Buffer>,
}

pub struct OnvifAggregator {
    // Input media stream, can be anything with a reference timestamp meta
    media_sink_pad: gst_base::AggregatorPad,
    // Input metadata stream, must be complete VideoAnalytics XML documents
    // as output by onvifdepay
    meta_sink_pad: gst_base::AggregatorPad,
    state: Mutex<State>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "onvifaggregator",
        gst::DebugColorFlags::empty(),
        Some("ONVIF metadata / video aggregator"),
    )
});

static NTP_CAPS: Lazy<gst::Caps> = Lazy::new(|| gst::Caps::builder("timestamp/x-ntp").build());
static UNIX_CAPS: Lazy<gst::Caps> = Lazy::new(|| gst::Caps::builder("timestamp/x-unix").build());

#[glib::object_subclass]
impl ObjectSubclass for OnvifAggregator {
    const NAME: &'static str = "GstOnvifAggregator";
    type Type = super::OnvifAggregator;
    type ParentType = gst_base::Aggregator;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("media").unwrap();
        let media_sink_pad =
            gst::PadBuilder::<gst_base::AggregatorPad>::from_template(&templ, Some("media"))
                .build();

        let templ = klass.pad_template("meta").unwrap();
        let meta_sink_pad =
            gst::PadBuilder::<gst_base::AggregatorPad>::from_template(&templ, Some("meta")).build();

        Self {
            media_sink_pad,
            meta_sink_pad,
            state: Mutex::default(),
        }
    }
}

impl ObjectImpl for OnvifAggregator {
    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.media_sink_pad).unwrap();
        obj.add_pad(&self.meta_sink_pad).unwrap();
    }
}

impl GstObjectImpl for OnvifAggregator {}

impl ElementImpl for OnvifAggregator {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "ONVIF metadata aggregator",
                "Aggregator",
                "ONVIF metadata aggregator",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let media_caps = gst::Caps::new_any();
            let media_sink_pad_template = gst::PadTemplate::with_gtype(
                "media",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &media_caps,
                gst_base::AggregatorPad::static_type(),
            )
            .unwrap();

            let meta_caps = gst::Caps::builder("application/x-onvif-metadata")
                .field("parsed", true)
                .field("encoding", "utf8")
                .build();

            let meta_sink_pad_template = gst::PadTemplate::with_gtype(
                "meta",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &meta_caps,
                gst_base::AggregatorPad::static_type(),
            )
            .unwrap();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &media_caps,
            )
            .unwrap();

            vec![
                media_sink_pad_template,
                meta_sink_pad_template,
                src_pad_template,
            ]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn request_new_pad(
        &self,
        element: &Self::Type,
        _templ: &gst::PadTemplate,
        _name: Option<String>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        gst::error!(
            CAT,
            obj: element,
            "onvifaggregator doesn't expose request pads"
        );

        None
    }

    fn release_pad(&self, element: &Self::Type, _pad: &gst::Pad) {
        gst::error!(
            CAT,
            obj: element,
            "onvifaggregator doesn't expose request pads"
        );
    }
}

impl OnvifAggregator {
    fn consume_meta(
        &self,
        state: &mut State,
        element: &super::OnvifAggregator,
        end: gst::ClockTime,
    ) -> Result<bool, gst::FlowError> {
        while let Some(buffer) = self.meta_sink_pad.peek_buffer() {
            let meta_ts = self.lookup_reference_timestamp(&buffer).ok_or_else(|| {
                gst::element_error!(
                    element,
                    gst::ResourceError::Read,
                    ["Parsed metadata buffer should hold reference timestamp"]
                );

                gst::FlowError::Error
            })?;

            if meta_ts <= end {
                let buffer = self.meta_sink_pad.pop_buffer().unwrap();
                state.meta_frames.push(buffer);
            } else {
                return Ok(true);
            }
        }

        Ok(self.meta_sink_pad.is_eos())
    }

    fn lookup_reference_timestamp(&self, buffer: &gst::Buffer) -> Option<gst::ClockTime> {
        for meta in buffer.iter_meta::<gst::ReferenceTimestampMeta>() {
            if meta.reference().is_subset(&NTP_CAPS) {
                return Some(meta.timestamp());
            } else if meta.reference().is_subset(&UNIX_CAPS) {
                return Some(meta.timestamp() + PRIME_EPOCH_OFFSET);
            }
        }

        None
    }

    fn media_buffer_duration(
        &self,
        element: &super::OnvifAggregator,
        current_media_buffer: &gst::Buffer,
        timeout: bool,
    ) -> Option<gst::ClockTime> {
        match current_media_buffer.duration() {
            Some(duration) => {
                gst::log!(
                    CAT,
                    obj: element,
                    "Current media buffer has a duration, using it: {}",
                    duration
                );
                Some(duration)
            }
            None => {
                if let Some(next_buffer) = self.media_sink_pad.peek_buffer() {
                    match next_buffer.pts().zip(current_media_buffer.pts()) {
                        Some((next_pts, current_pts)) => {
                            let duration = next_pts.saturating_sub(current_pts);

                            gst::log!(
                                CAT,
                                obj: element,
                                "calculated duration for current media buffer from next buffer: {}",
                                duration
                            );

                            Some(duration)
                        }
                        None => {
                            gst::log!(
                                CAT,
                                obj: element,
                                "could not calculate duration for current media buffer"
                            );
                            Some(gst::ClockTime::from_nseconds(0))
                        }
                    }
                } else if timeout {
                    gst::log!(
                        CAT,
                        obj: element,
                        "could not calculate duration for current media buffer"
                    );
                    Some(gst::ClockTime::from_nseconds(0))
                } else {
                    gst::trace!(
                        CAT,
                        obj: element,
                        "No next buffer to peek at yet to calculate duration"
                    );
                    None
                }
            }
        }
    }

    fn consume_media(
        &self,
        state: &mut State,
        element: &super::OnvifAggregator,
        timeout: bool,
    ) -> Result<Option<gst::Buffer>, gst::FlowError> {
        if let Some(current_media_buffer) = state
            .current_media_buffer
            .take()
            .or_else(|| self.media_sink_pad.pop_buffer())
        {
            if let Some(current_media_start) =
                self.lookup_reference_timestamp(&current_media_buffer)
            {
                match self.media_buffer_duration(element, &current_media_buffer, timeout) {
                    Some(duration) => {
                        let end = current_media_start + duration;

                        if self.consume_meta(state, element, end)? {
                            Ok(Some(current_media_buffer))
                        } else {
                            state.current_media_buffer = Some(current_media_buffer);
                            Ok(None)
                        }
                    }
                    None => {
                        state.current_media_buffer = Some(current_media_buffer);
                        Ok(None)
                    }
                }
            } else {
                Ok(Some(current_media_buffer))
            }
        } else {
            Ok(None)
        }
    }
}

impl AggregatorImpl for OnvifAggregator {
    fn aggregate(
        &self,
        element: &Self::Type,
        timeout: bool,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::trace!(CAT, obj: element, "aggregate, timeout: {}", timeout);

        let mut state = self.state.lock().unwrap();

        if let Some(mut buffer) = self.consume_media(&mut state, element, timeout)? {
            let mut buflist = gst::BufferList::new();

            {
                let buflist_mut = buflist.get_mut().unwrap();

                for frame in state.meta_frames.drain(..) {
                    buflist_mut.add(frame);
                }
            }

            drop(state);

            {
                let buf = buffer.make_mut();
                let mut meta = gst::meta::CustomMeta::add(buf, "OnvifXMLFrameMeta").unwrap();

                let s = meta.mut_structure();
                s.set("frames", buflist);
            }

            let position = buffer.pts().opt_add(
                buffer
                    .duration()
                    .unwrap_or_else(|| gst::ClockTime::from_nseconds(0)),
            );

            gst::log!(CAT, obj: element, "Updating position: {:?}", position);

            element.set_position(position);

            self.finish_buffer(element, buffer)
        } else if self.media_sink_pad.is_eos() {
            Err(gst::FlowError::Eos)
        } else {
            Err(AGGREGATOR_FLOW_NEED_DATA)
        }
    }

    fn src_query(&self, aggregator: &Self::Type, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        match query.view_mut() {
            QueryViewMut::Position(..)
            | QueryViewMut::Duration(..)
            | QueryViewMut::Uri(..)
            | QueryViewMut::Caps(..)
            | QueryViewMut::Allocation(..) => self.media_sink_pad.peer_query(query),
            QueryViewMut::AcceptCaps(q) => {
                let caps = q.caps_owned();
                let class = aggregator.class();
                let templ = class.pad_template("media").unwrap();
                let templ_caps = templ.caps();

                q.set_result(caps.is_subset(templ_caps));

                true
            }
            _ => self.parent_src_query(aggregator, query),
        }
    }

    fn sink_event(
        &self,
        aggregator: &Self::Type,
        aggregator_pad: &gst_base::AggregatorPad,
        event: gst::Event,
    ) -> bool {
        use gst::EventView;

        match event.view() {
            EventView::Caps(e) => {
                if aggregator_pad.upcast_ref::<gst::Pad>() == &self.media_sink_pad {
                    gst::info!(CAT, obj: aggregator, "Pushing caps {}", e.caps());
                    aggregator.set_src_caps(&e.caps_owned());
                }

                true
            }
            EventView::Segment(e) => {
                if aggregator_pad.upcast_ref::<gst::Pad>() == &self.media_sink_pad {
                    aggregator.update_segment(e.segment());
                }
                self.parent_sink_event(aggregator, aggregator_pad, event)
            }
            _ => self.parent_sink_event(aggregator, aggregator_pad, event),
        }
    }

    fn sink_query(
        &self,
        aggregator: &Self::Type,
        aggregator_pad: &gst_base::AggregatorPad,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryViewMut;

        match query.view_mut() {
            QueryViewMut::Position(..)
            | QueryViewMut::Duration(..)
            | QueryViewMut::Uri(..)
            | QueryViewMut::Allocation(..) => {
                if aggregator_pad == &self.media_sink_pad {
                    let srcpad = aggregator.src_pad();
                    srcpad.peer_query(query)
                } else {
                    self.parent_sink_query(aggregator, aggregator_pad, query)
                }
            }
            QueryViewMut::Caps(q) => {
                if aggregator_pad == &self.media_sink_pad {
                    let srcpad = aggregator.src_pad();
                    srcpad.peer_query(query)
                } else {
                    let filter = q.filter_owned();
                    let class = aggregator.class();
                    let templ = class.pad_template("meta").unwrap();
                    let templ_caps = templ.caps();

                    if let Some(filter) = filter {
                        q.set_result(
                            &filter.intersect_with_mode(templ_caps, gst::CapsIntersectMode::First),
                        );
                    } else {
                        q.set_result(templ_caps);
                    }

                    true
                }
            }
            QueryViewMut::AcceptCaps(q) => {
                if aggregator_pad.upcast_ref::<gst::Pad>() == &self.media_sink_pad {
                    let srcpad = aggregator.src_pad();
                    srcpad.peer_query(query);
                } else {
                    let caps = q.caps_owned();
                    let class = aggregator.class();
                    let templ = class.pad_template("meta").unwrap();
                    let templ_caps = templ.caps();

                    q.set_result(caps.is_subset(templ_caps));
                }

                true
            }
            _ => self.parent_src_query(aggregator, query),
        }
    }

    fn next_time(&self, aggregator: &Self::Type) -> Option<gst::ClockTime> {
        aggregator.simple_get_next_time()
    }

    fn negotiate(&self, _aggregator: &Self::Type) -> bool {
        true
    }
}
