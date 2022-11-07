// Copyright (C) 2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;

use std::collections::VecDeque;
use std::sync::Mutex;

use once_cell::sync::Lazy;

use super::boxes;
use super::Buffer;
use super::DeltaFrames;

/// Offset for the segment in non-single-stream variants.
const SEGMENT_OFFSET: gst::ClockTime = gst::ClockTime::from_seconds(60 * 60 * 1000);

/// Offset between UNIX epoch and Jan 1 1601 epoch in seconds.
/// 1601 = UNIX + UNIX_1601_OFFSET.
const UNIX_1601_OFFSET: u64 = 11_644_473_600;

/// Offset between NTP and UNIX epoch in seconds.
/// NTP = UNIX + NTP_UNIX_OFFSET.
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

/// Reference timestamp meta caps for NTP timestamps.
static NTP_CAPS: Lazy<gst::Caps> = Lazy::new(|| gst::Caps::builder("timestamp/x-ntp").build());

/// Reference timestamp meta caps for UNIX timestamps.
static UNIX_CAPS: Lazy<gst::Caps> = Lazy::new(|| gst::Caps::builder("timestamp/x-unix").build());

/// Returns the UTC time of the buffer in the UNIX epoch.
fn get_utc_time_from_buffer(buffer: &gst::BufferRef) -> Option<gst::ClockTime> {
    buffer
        .iter_meta::<gst::ReferenceTimestampMeta>()
        .find_map(|meta| {
            if meta.reference().can_intersect(&UNIX_CAPS) {
                Some(meta.timestamp())
            } else if meta.reference().can_intersect(&NTP_CAPS) {
                meta.timestamp().checked_sub(NTP_UNIX_OFFSET.seconds())
            } else {
                None
            }
        })
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "fmp4mux",
        gst::DebugColorFlags::empty(),
        Some("FMP4Mux Element"),
    )
});

const DEFAULT_FRAGMENT_DURATION: gst::ClockTime = gst::ClockTime::from_seconds(10);
const DEFAULT_HEADER_UPDATE_MODE: super::HeaderUpdateMode = super::HeaderUpdateMode::None;
const DEFAULT_WRITE_MFRA: bool = false;
const DEFAULT_WRITE_MEHD: bool = false;
const DEFAULT_INTERLEAVE_BYTES: Option<u64> = None;
const DEFAULT_INTERLEAVE_TIME: Option<gst::ClockTime> = Some(gst::ClockTime::from_mseconds(250));

#[derive(Debug, Clone)]
struct Settings {
    fragment_duration: gst::ClockTime,
    header_update_mode: super::HeaderUpdateMode,
    write_mfra: bool,
    write_mehd: bool,
    interleave_bytes: Option<u64>,
    interleave_time: Option<gst::ClockTime>,
    movie_timescale: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            fragment_duration: DEFAULT_FRAGMENT_DURATION,
            header_update_mode: DEFAULT_HEADER_UPDATE_MODE,
            write_mfra: DEFAULT_WRITE_MFRA,
            write_mehd: DEFAULT_WRITE_MEHD,
            interleave_bytes: DEFAULT_INTERLEAVE_BYTES,
            interleave_time: DEFAULT_INTERLEAVE_TIME,
            movie_timescale: 0,
        }
    }
}

#[derive(Debug)]
struct GopBuffer {
    buffer: gst::Buffer,
    pts: gst::ClockTime,
    dts: Option<gst::ClockTime>,
}

#[derive(Debug)]
struct Gop {
    // Running times
    start_pts: gst::ClockTime,
    start_dts: Option<gst::ClockTime>,
    earliest_pts: gst::ClockTime,
    // Once this is known to be the final earliest PTS/DTS
    final_earliest_pts: bool,
    // PTS plus duration of last buffer, or start of next GOP
    end_pts: gst::ClockTime,
    // Once this is known to be the final end PTS/DTS
    final_end_pts: bool,
    // DTS plus duration of last buffer, or start of next GOP
    end_dts: Option<gst::ClockTime>,

    // Buffer positions
    earliest_pts_position: gst::ClockTime,
    start_dts_position: Option<gst::ClockTime>,

    // Buffer, PTS running time, DTS running time
    buffers: Vec<GopBuffer>,
}

struct Stream {
    sinkpad: super::FMP4MuxPad,

    caps: gst::Caps,
    delta_frames: DeltaFrames,

    queued_gops: VecDeque<Gop>,
    fragment_filled: bool,

    // Difference between the first DTS and 0 in case of negative DTS
    dts_offset: Option<gst::ClockTime>,

    // Current position (DTS, or PTS for intra-only) to prevent
    // timestamps from going backwards when queueing new buffers
    current_position: gst::ClockTime,

    // Current UTC time in ONVIF mode to prevent timestamps from
    // going backwards when draining a fragment.
    // UNIX epoch.
    current_utc_time: gst::ClockTime,
}

#[derive(Default)]
struct State {
    streams: Vec<Stream>,

    // Created once we received caps and kept up to date with the caps,
    // sent as part of the buffer list for the first fragment.
    stream_header: Option<gst::Buffer>,

    sequence_number: u32,

    // Fragment tracking for mfra
    current_offset: u64,
    fragment_offsets: Vec<super::FragmentOffset>,

    // Start / end PTS of the whole stream
    earliest_pts: Option<gst::ClockTime>,
    end_pts: Option<gst::ClockTime>,

    // Start PTS of the current fragment
    fragment_start_pts: Option<gst::ClockTime>,
    // Additional timeout delay in case GOPs are bigger than the fragment duration
    timeout_delay: gst::ClockTime,

    // In ONVIF mode the UTC time corresponding to the beginning of the stream
    // UNIX epoch.
    start_utc_time: Option<gst::ClockTime>,
    end_utc_time: Option<gst::ClockTime>,

    sent_headers: bool,
}

#[derive(Default)]
pub(crate) struct FMP4Mux {
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl FMP4Mux {
    fn find_earliest_stream<'a>(
        &self,
        state: &'a mut State,
        timeout: bool,
    ) -> Result<Option<(usize, &'a mut Stream)>, gst::FlowError> {
        let mut earliest_stream = None;
        let mut all_have_data_or_eos = true;

        for (idx, stream) in state.streams.iter_mut().enumerate() {
            let buffer = match stream.sinkpad.peek_buffer() {
                Some(buffer) => buffer,
                None => {
                    if stream.sinkpad.is_eos() {
                        gst::trace!(CAT, obj: stream.sinkpad, "Stream is EOS");
                    } else {
                        all_have_data_or_eos = false;
                        gst::trace!(CAT, obj: stream.sinkpad, "Stream has no buffer");
                    }
                    continue;
                }
            };

            if stream.fragment_filled {
                gst::trace!(CAT, obj: stream.sinkpad, "Stream has current fragment filled");
                continue;
            }

            let segment = match stream
                .sinkpad
                .segment()
                .clone()
                .downcast::<gst::ClockTime>()
                .ok()
            {
                Some(segment) => segment,
                None => {
                    gst::error!(CAT, obj: stream.sinkpad, "Got buffer before segment");
                    return Err(gst::FlowError::Error);
                }
            };

            // If the stream has no valid running time, assume it's before everything else.
            let running_time = match segment.to_running_time(buffer.dts_or_pts()) {
                None => {
                    gst::trace!(CAT, obj: stream.sinkpad, "Stream has no valid running time");
                    if earliest_stream
                        .as_ref()
                        .map_or(true, |(_, _, earliest_running_time)| {
                            *earliest_running_time > gst::ClockTime::ZERO
                        })
                    {
                        earliest_stream = Some((idx, stream, gst::ClockTime::ZERO));
                    }
                    continue;
                }
                Some(running_time) => running_time,
            };

            gst::trace!(CAT, obj: stream.sinkpad, "Stream has running time {} queued", running_time);

            if earliest_stream
                .as_ref()
                .map_or(true, |(_idx, _stream, earliest_running_time)| {
                    *earliest_running_time > running_time
                })
            {
                earliest_stream = Some((idx, stream, running_time));
            }
        }

        if !timeout && !all_have_data_or_eos {
            gst::trace!(
                CAT,
                imp: self,
                "No timeout and not all streams have a buffer or are EOS"
            );
            Ok(None)
        } else if let Some((idx, stream, earliest_running_time)) = earliest_stream {
            gst::trace!(
                CAT,
                imp: self,
                "Stream {} is earliest stream with running time {}",
                stream.sinkpad.name(),
                earliest_running_time
            );
            Ok(Some((idx, stream)))
        } else {
            gst::trace!(CAT, imp: self, "No streams have data queued currently");
            Ok(None)
        }
    }

    // Queue incoming buffers as individual GOPs.
    fn queue_gops(
        &self,
        _idx: usize,
        stream: &mut Stream,
        segment: &gst::FormattedSegment<gst::ClockTime>,
        mut buffer: gst::Buffer,
    ) -> Result<(), gst::FlowError> {
        use gst::Signed::*;

        assert!(!stream.fragment_filled);

        gst::trace!(CAT, obj: stream.sinkpad, "Handling buffer {:?}", buffer);

        let delta_frames = stream.delta_frames;

        if delta_frames.requires_dts() && buffer.dts().is_none() {
            gst::error!(CAT, obj: stream.sinkpad, "Require DTS for video streams");
            return Err(gst::FlowError::Error);
        }

        if delta_frames.intra_only() && buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
            gst::error!(CAT, obj: stream.sinkpad, "Intra-only stream with delta units");
            return Err(gst::FlowError::Error);
        }

        let pts_position = buffer.pts().ok_or_else(|| {
            gst::error!(CAT, obj: stream.sinkpad, "Require timestamped buffers");
            gst::FlowError::Error
        })?;
        let duration = buffer.duration();
        let end_pts_position = duration.opt_add(pts_position).unwrap_or(pts_position);

        let mut pts = segment
            .to_running_time_full(pts_position)
            .ok_or_else(|| {
                gst::error!(CAT, obj: stream.sinkpad, "Couldn't convert PTS to running time");
                gst::FlowError::Error
            })?
            .positive()
            .unwrap_or_else(|| {
                gst::warning!(CAT, obj: stream.sinkpad, "Negative PTSs are not supported");
                gst::ClockTime::ZERO
            });

        let mut end_pts = segment
            .to_running_time_full(end_pts_position)
            .ok_or_else(|| {
                gst::error!(CAT, obj: stream.sinkpad, "Couldn't convert end PTS to running time");
                gst::FlowError::Error
            })?
            .positive()
            .unwrap_or_else(|| {
                gst::warning!(CAT, obj: stream.sinkpad, "Negative PTSs are not supported");
                gst::ClockTime::ZERO
            });

        // Enforce monotonically increasing PTS for intra-only streams
        if !delta_frames.requires_dts() {
            if pts < stream.current_position {
                gst::warning!(
                    CAT,
                    obj: stream.sinkpad,
                    "Decreasing PTS {} < {}",
                    pts,
                    stream.current_position,
                );
                pts = stream.current_position;
            } else {
                stream.current_position = pts;
            }
            end_pts = std::cmp::max(end_pts, pts);
        }

        let (dts_position, dts, end_dts) = if !delta_frames.requires_dts() {
            (None, None, None)
        } else {
            // Negative DTS are handled via the dts_offset and by having negative composition time
            // offsets in the `trun` box. The smallest DTS here is shifted to zero.
            let dts_position = buffer.dts().expect("not DTS");
            let end_dts_position = duration.opt_add(dts_position).unwrap_or(dts_position);

            let signed_dts = segment.to_running_time_full(dts_position).ok_or_else(|| {
                gst::error!(CAT, obj: stream.sinkpad, "Couldn't convert DTS to running time");
                gst::FlowError::Error
            })?;
            let mut dts = match signed_dts {
                Positive(dts) => {
                    if let Some(dts_offset) = stream.dts_offset {
                        dts + dts_offset
                    } else {
                        dts
                    }
                }
                Negative(dts) => {
                    if stream.dts_offset.is_none() {
                        stream.dts_offset = Some(dts);
                    }

                    let dts_offset = stream.dts_offset.unwrap();
                    if dts > dts_offset {
                        gst::warning!(CAT, obj: stream.sinkpad, "DTS before first DTS");
                        gst::ClockTime::ZERO
                    } else {
                        dts_offset - dts
                    }
                }
            };

            let signed_end_dts =
                segment
                    .to_running_time_full(end_dts_position)
                    .ok_or_else(|| {
                        gst::error!(
                            CAT,
                            obj: stream.sinkpad,
                            "Couldn't convert end DTS to running time"
                        );
                        gst::FlowError::Error
                    })?;
            let mut end_dts = match signed_end_dts {
                Positive(dts) => {
                    if let Some(dts_offset) = stream.dts_offset {
                        dts + dts_offset
                    } else {
                        dts
                    }
                }
                Negative(dts) => {
                    if stream.dts_offset.is_none() {
                        stream.dts_offset = Some(dts);
                    }

                    let dts_offset = stream.dts_offset.unwrap();
                    if dts > dts_offset {
                        gst::warning!(CAT, obj: stream.sinkpad, "End DTS before first DTS");
                        gst::ClockTime::ZERO
                    } else {
                        dts_offset - dts
                    }
                }
            };

            // Enforce monotonically increasing DTS for intra-only streams
            // NOTE: PTS stays the same so this will cause a bigger PTS/DTS difference
            // FIXME: Is this correct?
            if dts < stream.current_position {
                gst::warning!(
                    CAT,
                    obj: stream.sinkpad,
                    "Decreasing DTS {} < {}",
                    dts,
                    stream.current_position,
                );
                dts = stream.current_position;
            } else {
                stream.current_position = dts;
            }
            end_dts = std::cmp::max(end_dts, dts);

            (Some(dts_position), Some(dts), Some(end_dts))
        };

        // If this is a multi-stream element then we need to update the PTS/DTS positions according
        // to the output segment, specifically to re-timestamp them with the running time and
        // adjust for the segment shift to compensate for negative DTS.
        let aggregator = self.obj();
        let class = aggregator.class();
        let (pts_position, dts_position) = if class.as_ref().variant.is_single_stream() {
            (pts_position, dts_position)
        } else {
            let pts_position = pts + SEGMENT_OFFSET;
            let dts_position = dts.map(|dts| {
                dts + SEGMENT_OFFSET - stream.dts_offset.unwrap_or(gst::ClockTime::ZERO)
            });

            let buffer = buffer.make_mut();
            buffer.set_pts(pts_position);
            buffer.set_dts(dts_position);

            (pts_position, dts_position)
        };

        if !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
            gst::debug!(
                CAT,
                obj: stream.sinkpad,
                "Starting new GOP at PTS {} DTS {} (DTS offset {})",
                pts,
                dts.display(),
                stream.dts_offset.display(),
            );

            let gop = Gop {
                start_pts: pts,
                start_dts: dts,
                start_dts_position: if !delta_frames.requires_dts() {
                    None
                } else {
                    dts_position
                },
                earliest_pts: pts,
                earliest_pts_position: pts_position,
                final_earliest_pts: !delta_frames.requires_dts(),
                end_pts,
                end_dts,
                final_end_pts: false,
                buffers: vec![GopBuffer { buffer, pts, dts }],
            };
            stream.queued_gops.push_front(gop);

            if let Some(prev_gop) = stream.queued_gops.get_mut(1) {
                gst::debug!(
                    CAT,
                    obj: stream.sinkpad,
                    "Updating previous GOP starting at PTS {} to end PTS {} DTS {}",
                    prev_gop.earliest_pts,
                    pts,
                    dts.display(),
                );

                prev_gop.end_pts = std::cmp::max(prev_gop.end_pts, pts);
                prev_gop.end_dts = std::cmp::max(prev_gop.end_dts, dts);

                if !delta_frames.requires_dts() {
                    prev_gop.final_end_pts = true;
                }

                if !prev_gop.final_earliest_pts {
                    // Don't bother logging this for intra-only streams as it would be for every
                    // single buffer.
                    if delta_frames.requires_dts() {
                        gst::debug!(
                            CAT,
                            obj: stream.sinkpad,
                            "Previous GOP has final earliest PTS at {}",
                            prev_gop.earliest_pts
                        );
                    }

                    prev_gop.final_earliest_pts = true;
                    if let Some(prev_prev_gop) = stream.queued_gops.get_mut(2) {
                        prev_prev_gop.final_end_pts = true;
                    }
                }
            }
        } else if let Some(gop) = stream.queued_gops.front_mut() {
            assert!(!delta_frames.intra_only());

            gop.end_pts = std::cmp::max(gop.end_pts, end_pts);
            gop.end_dts = gop.end_dts.opt_max(end_dts);
            gop.buffers.push(GopBuffer { buffer, pts, dts });

            if delta_frames.requires_dts() {
                let dts = dts.unwrap();

                if gop.earliest_pts > pts && !gop.final_earliest_pts {
                    gst::debug!(
                        CAT,
                        obj: stream.sinkpad,
                        "Updating current GOP earliest PTS from {} to {}",
                        gop.earliest_pts,
                        pts
                    );
                    gop.earliest_pts = pts;
                    gop.earliest_pts_position = pts_position;

                    if let Some(prev_gop) = stream.queued_gops.get_mut(1) {
                        if prev_gop.end_pts < pts {
                            gst::debug!(
                                CAT,
                                obj: stream.sinkpad,
                                "Updating previous GOP starting PTS {} end time from {} to {}",
                                pts,
                                prev_gop.end_pts,
                                pts
                            );
                            prev_gop.end_pts = pts;
                        }
                    }
                }

                let gop = stream.queued_gops.front_mut().unwrap();

                // The earliest PTS is known when the current DTS is bigger or equal to the first
                // PTS that was observed in this GOP. If there was another frame later that had a
                // lower PTS then it wouldn't be possible to display it in time anymore, i.e. the
                // stream would be invalid.
                if gop.start_pts <= dts && !gop.final_earliest_pts {
                    gst::debug!(
                        CAT,
                        obj: stream.sinkpad,
                        "GOP has final earliest PTS at {}",
                        gop.earliest_pts
                    );
                    gop.final_earliest_pts = true;

                    if let Some(prev_gop) = stream.queued_gops.get_mut(1) {
                        prev_gop.final_end_pts = true;
                    }
                }
            }
        } else {
            gst::warning!(
                CAT,
                obj: stream.sinkpad,
                "Waiting for keyframe at the beginning of the stream"
            );
        }

        if let Some((prev_gop, first_gop)) = Option::zip(
            stream.queued_gops.iter().find(|gop| gop.final_end_pts),
            stream.queued_gops.back(),
        ) {
            gst::debug!(
                CAT,
                obj: stream.sinkpad,
                "Queued full GOPs duration updated to {}",
                prev_gop.end_pts.saturating_sub(first_gop.earliest_pts),
            );
        }

        gst::debug!(
            CAT,
            obj: stream.sinkpad,
            "Queued duration updated to {}",
            Option::zip(stream.queued_gops.front(), stream.queued_gops.back())
                .map(|(end, start)| end.end_pts.saturating_sub(start.start_pts))
                .unwrap_or(gst::ClockTime::ZERO)
        );

        Ok(())
    }

    #[allow(clippy::type_complexity)]
    fn drain_buffers(
        &self,
        state: &mut State,
        settings: &Settings,
        timeout: bool,
        at_eos: bool,
    ) -> Result<
        (
            // Drained streams
            Vec<(super::FragmentHeaderStream, VecDeque<Buffer>)>,
            // Minimum earliest PTS position of all streams
            Option<gst::ClockTime>,
            // Minimum earliest PTS of all streams
            Option<gst::ClockTime>,
            // Minimum start DTS position of all streams (if any stream has DTS)
            Option<gst::ClockTime>,
            // End PTS of this drained fragment, i.e. start PTS of the next fragment
            Option<gst::ClockTime>,
        ),
        gst::FlowError,
    > {
        let mut drained_streams = Vec::with_capacity(state.streams.len());

        let mut min_earliest_pts_position = None;
        let mut min_earliest_pts = None;
        let mut min_start_dts_position = None;
        let mut fragment_end_pts = None;

        // The first stream decides how much can be dequeued, if anything at all.
        //
        // All complete GOPs (or at EOS everything) up to the fragment duration will be dequeued
        // but on timeout in live pipelines it might happen that the first stream does not have a
        // complete GOP queued. In that case nothing is dequeued for any of the streams and the
        // timeout is advanced by 1s until at least one complete GOP can be dequeued.
        //
        // If the first stream is already EOS then the next stream that is not EOS yet will be
        // taken in its place.
        let fragment_start_pts = state.fragment_start_pts.unwrap();
        gst::info!(
            CAT,
            imp: self,
            "Starting to drain at {}",
            fragment_start_pts
        );

        for (idx, stream) in state.streams.iter_mut().enumerate() {
            let stream_settings = stream.sinkpad.imp().settings.lock().unwrap().clone();

            assert!(
                timeout
                    || at_eos
                    || stream.sinkpad.is_eos()
                    || stream.queued_gops.get(1).map(|gop| gop.final_earliest_pts) == Some(true)
            );

            // Drain all complete GOPs until at most one fragment duration was dequeued for the
            // first stream, or until the dequeued duration of the first stream.
            let mut gops = Vec::with_capacity(stream.queued_gops.len());
            let dequeue_end_pts =
                fragment_end_pts.unwrap_or(fragment_start_pts + settings.fragment_duration);
            gst::trace!(
                CAT,
                obj: stream.sinkpad,
                "Draining up to end PTS {} / duration {}",
                dequeue_end_pts,
                dequeue_end_pts - fragment_start_pts
            );

            while let Some(gop) = stream.queued_gops.back() {
                // If this GOP is not complete then we can't pop it yet.
                //
                // If there was no complete GOP at all yet then it might be bigger than the
                // fragment duration. In this case we might not be able to handle the latency
                // requirements in a live pipeline.
                if !gop.final_end_pts && !at_eos && !stream.sinkpad.is_eos() {
                    break;
                }

                // If this GOP starts after the fragment end then don't dequeue it yet unless this is
                // the first stream and no GOPs were dequeued at all yet. This would mean that the
                // GOP is bigger than the fragment duration.
                if !at_eos
                    && gop.end_pts > dequeue_end_pts
                    && (fragment_end_pts.is_some() || !gops.is_empty())
                {
                    break;
                }

                gops.push(stream.queued_gops.pop_back().unwrap());
            }
            stream.fragment_filled = false;

            // If we don't have a next fragment start PTS then this is the first stream as above.
            if fragment_end_pts.is_none() {
                if let Some(last_gop) = gops.last() {
                    // Dequeued something so let's take the end PTS of the last GOP
                    fragment_end_pts = Some(last_gop.end_pts);
                    gst::info!(
                        CAT,
                        obj: stream.sinkpad,
                        "Draining up to PTS {} for this fragment",
                        last_gop.end_pts,
                    );
                } else {
                    // If nothing was dequeued for the first stream then this is OK if we're at
                    // EOS: we just consider the next stream as first stream then.
                    if at_eos || stream.sinkpad.is_eos() {
                        // This is handled below generally if nothing was dequeued
                    } else {
                        // Otherwise this can only really happen on timeout in live pipelines.
                        assert!(timeout);

                        gst::warning!(
                            CAT,
                            obj: stream.sinkpad,
                            "Don't have a complete GOP for the first stream on timeout in a live pipeline",
                        );

                        // In this case we advance the timeout by 1s and hope that things are
                        // better then.
                        return Err(gst_base::AGGREGATOR_FLOW_NEED_DATA);
                    }
                }
            }

            if gops.is_empty() {
                gst::info!(
                    CAT,
                    obj: stream.sinkpad,
                    "Draining no buffers",
                );

                drained_streams.push((
                    super::FragmentHeaderStream {
                        caps: stream.caps.clone(),
                        start_time: None,
                        delta_frames: stream.delta_frames,
                        trak_timescale: stream_settings.trak_timescale,
                    },
                    VecDeque::new(),
                ));

                continue;
            }

            assert!(fragment_end_pts.is_some());

            let first_gop = gops.first().unwrap();
            let last_gop = gops.last().unwrap();
            let earliest_pts = first_gop.earliest_pts;
            let earliest_pts_position = first_gop.earliest_pts_position;
            let start_dts = first_gop.start_dts;
            let start_dts_position = first_gop.start_dts_position;
            let end_pts = last_gop.end_pts;
            let dts_offset = stream.dts_offset;

            if min_earliest_pts.opt_gt(earliest_pts).unwrap_or(true) {
                min_earliest_pts = Some(earliest_pts);
            }
            if min_earliest_pts_position
                .opt_gt(earliest_pts_position)
                .unwrap_or(true)
            {
                min_earliest_pts_position = Some(earliest_pts_position);
            }
            if let Some(start_dts_position) = start_dts_position {
                if min_start_dts_position
                    .opt_gt(start_dts_position)
                    .unwrap_or(true)
                {
                    min_start_dts_position = Some(start_dts_position);
                }
            }

            gst::info!(
                CAT,
                obj: stream.sinkpad,
                "Draining {} worth of buffers starting at PTS {} DTS {}, DTS offset {}",
                end_pts.saturating_sub(earliest_pts),
                earliest_pts,
                start_dts.display(),
                dts_offset.display(),
            );

            if let Some((prev_gop, first_gop)) = Option::zip(
                stream.queued_gops.iter().find(|gop| gop.final_end_pts),
                stream.queued_gops.back(),
            ) {
                gst::debug!(
                    CAT,
                    obj: stream.sinkpad,
                    "Queued full GOPs duration updated to {}",
                    prev_gop.end_pts.saturating_sub(first_gop.earliest_pts),
                );
            }

            gst::debug!(
                CAT,
                obj: stream.sinkpad,
                "Queued duration updated to {}",
                Option::zip(stream.queued_gops.front(), stream.queued_gops.back())
                    .map(|(end, start)| end.end_pts.saturating_sub(start.start_pts))
                    .unwrap_or(gst::ClockTime::ZERO)
            );

            let start_time = if !stream.delta_frames.requires_dts() {
                earliest_pts
            } else {
                start_dts.unwrap()
            };

            let mut buffers = VecDeque::with_capacity(gops.iter().map(|g| g.buffers.len()).sum());

            for gop in gops {
                let mut gop_buffers = gop.buffers.into_iter().peekable();
                while let Some(buffer) = gop_buffers.next() {
                    let timestamp = if !stream.delta_frames.requires_dts() {
                        buffer.pts
                    } else {
                        buffer.dts.unwrap()
                    };

                    let end_timestamp = match gop_buffers.peek() {
                        Some(buffer) => {
                            if !stream.delta_frames.requires_dts() {
                                buffer.pts
                            } else {
                                buffer.dts.unwrap()
                            }
                        }
                        None => {
                            if !stream.delta_frames.requires_dts() {
                                gop.end_pts
                            } else {
                                gop.end_dts.unwrap()
                            }
                        }
                    };

                    // Timestamps are enforced to monotonically increase when queueing buffers
                    let duration = end_timestamp
                        .checked_sub(timestamp)
                        .expect("Timestamps going backwards");

                    let composition_time_offset = if !stream.delta_frames.requires_dts() {
                        None
                    } else {
                        let pts = buffer.pts;
                        let dts = buffer.dts.unwrap();

                        if pts > dts {
                            Some(i64::try_from((pts - dts).nseconds()).map_err(|_| {
                                gst::error!(CAT, obj: stream.sinkpad, "Too big PTS/DTS difference");
                                gst::FlowError::Error
                            })?)
                        } else {
                            let diff = i64::try_from((dts - pts).nseconds()).map_err(|_| {
                                gst::error!(CAT, obj: stream.sinkpad, "Too big PTS/DTS difference");
                                gst::FlowError::Error
                            })?;
                            Some(-diff)
                        }
                    };

                    buffers.push_back(Buffer {
                        idx,
                        buffer: buffer.buffer,
                        timestamp,
                        duration,
                        composition_time_offset,
                    });
                }
            }

            drained_streams.push((
                super::FragmentHeaderStream {
                    caps: stream.caps.clone(),
                    start_time: Some(start_time),
                    delta_frames: stream.delta_frames,
                    trak_timescale: stream_settings.trak_timescale,
                },
                buffers,
            ));
        }

        Ok((
            drained_streams,
            min_earliest_pts_position,
            min_earliest_pts,
            min_start_dts_position,
            fragment_end_pts,
        ))
    }

    fn preprocess_drained_streams_onvif(
        &self,
        state: &mut State,
        drained_streams: &mut [(super::FragmentHeaderStream, VecDeque<Buffer>)],
    ) -> Result<Option<gst::ClockTime>, gst::FlowError> {
        let aggregator = self.obj();
        if aggregator.class().as_ref().variant != super::Variant::ONVIF {
            return Ok(None);
        }

        let mut max_end_utc_time = None;

        let calculate_pts = |buffer: &Buffer| -> gst::ClockTime {
            let composition_time_offset = buffer.composition_time_offset.unwrap_or(0);
            if composition_time_offset > 0 {
                buffer.timestamp + (composition_time_offset as u64).nseconds()
            } else {
                buffer
                    .timestamp
                    .checked_sub(((-composition_time_offset) as u64).nseconds())
                    .unwrap()
            }
        };

        // If this is the first fragment then allow the first buffers to not have a reference
        // timestamp meta and backdate them
        if state.stream_header.is_none() {
            for (idx, (_, drain_buffers)) in drained_streams.iter_mut().enumerate() {
                let (buffer_idx, utc_time, buffer) =
                    match drain_buffers.iter().enumerate().find_map(|(idx, buffer)| {
                        get_utc_time_from_buffer(&buffer.buffer)
                            .map(|timestamp| (idx, timestamp, buffer))
                    }) {
                        None => {
                            gst::error!(
                                CAT,
                                obj: state.streams[idx].sinkpad,
                                "No reference timestamp set on any buffers in the first fragment",
                            );
                            return Err(gst::FlowError::Error);
                        }
                        Some(res) => res,
                    };

                // Now do the backdating
                if buffer_idx > 0 {
                    let utc_time_pts = calculate_pts(buffer);

                    for buffer in drain_buffers.iter_mut().take(buffer_idx) {
                        let buffer_pts = calculate_pts(buffer);
                        let buffer_pts_diff = if utc_time_pts >= buffer_pts {
                            (utc_time_pts - buffer_pts).nseconds() as i64
                        } else {
                            -((buffer_pts - utc_time_pts).nseconds() as i64)
                        };
                        let buffer_utc_time = if buffer_pts_diff >= 0 {
                            utc_time
                                .checked_sub((buffer_pts_diff as u64).nseconds())
                                .unwrap()
                        } else {
                            utc_time
                                .checked_add(((-buffer_pts_diff) as u64).nseconds())
                                .unwrap()
                        };

                        let buffer = buffer.buffer.make_mut();
                        gst::ReferenceTimestampMeta::add(
                            buffer,
                            &UNIX_CAPS,
                            buffer_utc_time,
                            gst::ClockTime::NONE,
                        );
                    }
                }
            }
        }

        // Calculate the minimum across all streams and remember that
        if state.start_utc_time.is_none() {
            let mut start_utc_time = None;

            for (idx, (_, drain_buffers)) in drained_streams.iter().enumerate() {
                for buffer in drain_buffers {
                    let utc_time = match get_utc_time_from_buffer(&buffer.buffer) {
                        None => {
                            gst::error!(
                                CAT,
                                obj: state.streams[idx].sinkpad,
                                "No reference timestamp set on all buffers"
                            );
                            return Err(gst::FlowError::Error);
                        }
                        Some(utc_time) => utc_time,
                    };

                    if start_utc_time.is_none() || start_utc_time > Some(utc_time) {
                        start_utc_time = Some(utc_time);
                    }
                }
            }

            gst::debug!(
                CAT,
                imp: self,
                "Configuring start UTC time {}",
                start_utc_time.unwrap()
            );
            state.start_utc_time = start_utc_time;
        }

        // Update all buffer timestamps based on the UTC time and offset to the start UTC time
        let start_utc_time = state.start_utc_time.unwrap();
        for (idx, (stream, drain_buffers)) in drained_streams.iter_mut().enumerate() {
            let mut start_time = None;

            for buffer in drain_buffers.iter_mut() {
                let utc_time = match get_utc_time_from_buffer(&buffer.buffer) {
                    None => {
                        gst::error!(
                            CAT,
                            obj: state.streams[idx].sinkpad,
                            "No reference timestamp set on all buffers"
                        );
                        return Err(gst::FlowError::Error);
                    }
                    Some(utc_time) => utc_time,
                };

                // Convert PTS UTC time to DTS
                let mut utc_time_dts =
                    if let Some(composition_time_offset) = buffer.composition_time_offset {
                        if composition_time_offset >= 0 {
                            utc_time
                                .checked_sub((composition_time_offset as u64).nseconds())
                                .unwrap()
                        } else {
                            utc_time
                                .checked_add(((-composition_time_offset) as u64).nseconds())
                                .unwrap()
                        }
                    } else {
                        utc_time
                    };

                // Enforce monotonically increasing timestamps
                if utc_time_dts < state.streams[idx].current_utc_time {
                    gst::warning!(
                        CAT,
                        obj: state.streams[idx].sinkpad,
                        "Decreasing UTC DTS timestamp for buffer {} < {}",
                        utc_time_dts,
                        state.streams[idx].current_utc_time,
                    );
                    utc_time_dts = state.streams[idx].current_utc_time;
                } else {
                    state.streams[idx].current_utc_time = utc_time_dts;
                }

                let timestamp = utc_time_dts.checked_sub(start_utc_time).unwrap();

                gst::trace!(
                    CAT,
                    obj: state.streams[idx].sinkpad,
                    "Updating buffer timestamp from {} to relative UTC DTS time {} / absolute DTS time {}, UTC PTS time {}",
                    buffer.timestamp,
                    timestamp,
                    utc_time_dts,
                    utc_time,
                );

                buffer.timestamp = timestamp;
                if start_time.is_none() || start_time > Some(buffer.timestamp) {
                    start_time = Some(buffer.timestamp);
                }
            }

            // Update durations for all buffers except for the last in the fragment unless all
            // have the same duration anyway
            let mut common_duration = Ok(None);
            let mut drain_buffers_iter = drain_buffers.iter_mut().peekable();
            while let Some(buffer) = drain_buffers_iter.next() {
                let next_timestamp = drain_buffers_iter.peek().map(|b| b.timestamp);

                if let Some(next_timestamp) = next_timestamp {
                    let duration = next_timestamp.saturating_sub(buffer.timestamp);
                    if common_duration == Ok(None) {
                        common_duration = Ok(Some(duration));
                    } else if common_duration != Ok(Some(duration)) {
                        common_duration = Err(());
                    }

                    gst::trace!(
                        CAT,
                        obj: state.streams[idx].sinkpad,
                        "Updating buffer with timestamp {} duration from {} to relative UTC duration {}",
                        buffer.timestamp,
                        buffer.duration,
                        duration,
                    );

                    buffer.duration = duration;
                } else if let Ok(Some(common_duration)) = common_duration {
                    gst::trace!(
                        CAT,
                        obj: state.streams[idx].sinkpad,
                        "Updating last buffer with timestamp {} duration from {} to common relative UTC duration {}",
                        buffer.timestamp,
                        buffer.duration,
                        common_duration,
                    );

                    buffer.duration = common_duration;
                } else {
                    gst::trace!(
                        CAT,
                        obj: state.streams[idx].sinkpad,
                        "Keeping last buffer with timestamp {} duration at {}",
                        buffer.timestamp,
                        buffer.duration,
                    );
                }

                let end_utc_time = start_utc_time + buffer.timestamp + buffer.duration;
                if max_end_utc_time.is_none() || max_end_utc_time < Some(end_utc_time) {
                    max_end_utc_time = Some(end_utc_time);
                }
            }

            if let Some(start_time) = start_time {
                gst::debug!(CAT, obj: state.streams[idx].sinkpad, "Fragment starting at UTC time {}", start_time);
                *stream.start_time.as_mut().unwrap() = start_time;
            } else {
                assert!(stream.start_time.is_none());
            }
        }

        Ok(max_end_utc_time)
    }

    #[allow(clippy::type_complexity)]
    fn interleave_buffers(
        &self,
        settings: &Settings,
        mut drained_streams: Vec<(super::FragmentHeaderStream, VecDeque<Buffer>)>,
    ) -> Result<(Vec<Buffer>, Vec<super::FragmentHeaderStream>), gst::FlowError> {
        let mut interleaved_buffers =
            Vec::with_capacity(drained_streams.iter().map(|(_, bufs)| bufs.len()).sum());
        while let Some((_idx, (_, bufs))) =
            drained_streams
                .iter_mut()
                .enumerate()
                .min_by(|(a_idx, (_, a)), (b_idx, (_, b))| {
                    let (a, b) = match (a.front(), b.front()) {
                        (None, None) => return std::cmp::Ordering::Equal,
                        (None, _) => return std::cmp::Ordering::Greater,
                        (_, None) => return std::cmp::Ordering::Less,
                        (Some(a), Some(b)) => (a, b),
                    };

                    match a.timestamp.cmp(&b.timestamp) {
                        std::cmp::Ordering::Equal => a_idx.cmp(b_idx),
                        cmp => cmp,
                    }
                })
        {
            let start_time = match bufs.front() {
                None => {
                    // No more buffers now
                    break;
                }
                Some(buf) => buf.timestamp,
            };
            let mut current_end_time = start_time;
            let mut dequeued_bytes = 0;

            while settings
                .interleave_bytes
                .opt_ge(dequeued_bytes)
                .unwrap_or(true)
                && settings
                    .interleave_time
                    .opt_ge(current_end_time.saturating_sub(start_time))
                    .unwrap_or(true)
            {
                if let Some(buffer) = bufs.pop_front() {
                    current_end_time = buffer.timestamp + buffer.duration;
                    dequeued_bytes += buffer.buffer.size() as u64;
                    interleaved_buffers.push(buffer);
                } else {
                    // No buffers left in this stream, go to next stream
                    break;
                }
            }
        }

        // All buffers should be consumed now
        assert!(drained_streams.iter().all(|(_, bufs)| bufs.is_empty()));

        let streams = drained_streams
            .into_iter()
            .map(|(stream, _)| stream)
            .collect::<Vec<_>>();

        Ok((interleaved_buffers, streams))
    }

    fn drain(
        &self,
        state: &mut State,
        settings: &Settings,
        timeout: bool,
        at_eos: bool,
        upstream_events: &mut Vec<(super::FMP4MuxPad, gst::Event)>,
    ) -> Result<(Option<gst::Caps>, Option<gst::BufferList>), gst::FlowError> {
        if at_eos {
            gst::info!(CAT, imp: self, "Draining at EOS");
        } else if timeout {
            gst::info!(CAT, imp: self, "Draining at timeout");
        } else {
            for stream in &state.streams {
                if !stream.fragment_filled && !stream.sinkpad.is_eos() {
                    return Ok((None, None));
                }
            }
        }

        // Collect all buffers and their timing information that are to be drained right now.
        let (
            mut drained_streams,
            min_earliest_pts_position,
            min_earliest_pts,
            min_start_dts_position,
            fragment_end_pts,
        ) = self.drain_buffers(state, settings, timeout, at_eos)?;

        // Remove all GAP buffers before processing them further
        for (stream, buffers) in &mut drained_streams {
            buffers.retain(|buf| {
                !buf.buffer.flags().contains(gst::BufferFlags::GAP)
                    || !buf.buffer.flags().contains(gst::BufferFlags::DROPPABLE)
                    || buf.buffer.size() != 0
            });

            if buffers.is_empty() {
                stream.start_time = None;
            }
        }

        // For ONVIF, replace all timestamps with timestamps based on UTC times.
        let max_end_utc_time =
            self.preprocess_drained_streams_onvif(state, &mut drained_streams)?;

        // Create header now if it was not created before and return the caps
        let mut caps = None;
        if state.stream_header.is_none() {
            let (_, new_caps) = self.update_header(state, settings, false)?.unwrap();
            caps = Some(new_caps);
        }

        // Interleave buffers according to the settings into a single vec
        let (mut interleaved_buffers, streams) =
            self.interleave_buffers(settings, drained_streams)?;

        let mut buffer_list = None;
        if interleaved_buffers.is_empty() {
            assert!(at_eos);
        } else {
            // If there are actual buffers to output then create headers as needed and create a
            // bufferlist for all buffers that have to be output.
            let min_earliest_pts_position = min_earliest_pts_position.unwrap();
            let min_earliest_pts = min_earliest_pts.unwrap();
            let fragment_end_pts = fragment_end_pts.unwrap();

            let mut fmp4_header = None;
            if !state.sent_headers {
                let mut buffer = state.stream_header.as_ref().unwrap().copy();
                {
                    let buffer = buffer.get_mut().unwrap();

                    buffer.set_pts(min_earliest_pts_position);
                    buffer.set_dts(min_start_dts_position);

                    // Header is DISCONT|HEADER
                    buffer.set_flags(gst::BufferFlags::DISCONT | gst::BufferFlags::HEADER);
                }

                fmp4_header = Some(buffer);

                state.sent_headers = true;
            }

            // TODO: Write prft boxes before moof
            // TODO: Write sidx boxes before moof and rewrite once offsets are known

            if state.sequence_number == 0 {
                state.sequence_number = 1;
            }
            let sequence_number = state.sequence_number;
            state.sequence_number += 1;
            let (mut fmp4_fragment_header, moof_offset) =
                boxes::create_fmp4_fragment_header(super::FragmentHeaderConfiguration {
                    variant: self.obj().class().as_ref().variant,
                    sequence_number,
                    streams: streams.as_slice(),
                    buffers: interleaved_buffers.as_slice(),
                })
                .map_err(|err| {
                    gst::error!(
                        CAT,
                        imp: self,
                        "Failed to create FMP4 fragment header: {}",
                        err
                    );
                    gst::FlowError::Error
                })?;

            {
                let buffer = fmp4_fragment_header.get_mut().unwrap();
                buffer.set_pts(min_earliest_pts_position);
                buffer.set_dts(min_start_dts_position);
                buffer.set_duration(fragment_end_pts.checked_sub(min_earliest_pts));

                // Fragment header is HEADER
                buffer.set_flags(gst::BufferFlags::HEADER);

                // Copy metas from the first actual buffer to the fragment header. This allows
                // getting things like the reference timestamp meta or the timecode meta to identify
                // the fragment.
                let _ = interleaved_buffers[0].buffer.copy_into(
                    buffer,
                    gst::BufferCopyFlags::META,
                    0,
                    None,
                );
            }

            let moof_offset = state.current_offset
                + fmp4_header.as_ref().map(|h| h.size()).unwrap_or(0) as u64
                + moof_offset;

            let buffers_len = interleaved_buffers.len();
            for (idx, buffer) in interleaved_buffers.iter_mut().enumerate() {
                // Fix up buffer flags, all other buffers are DELTA_UNIT
                let buffer_ref = buffer.buffer.make_mut();
                buffer_ref.unset_flags(gst::BufferFlags::all());
                buffer_ref.set_flags(gst::BufferFlags::DELTA_UNIT);

                // Set the marker flag for the last buffer of the segment
                if idx == buffers_len - 1 {
                    buffer_ref.set_flags(gst::BufferFlags::MARKER);
                }
            }

            buffer_list = Some(
                fmp4_header
                    .into_iter()
                    .chain(Some(fmp4_fragment_header))
                    .chain(interleaved_buffers.into_iter().map(|buffer| buffer.buffer))
                    .inspect(|b| {
                        state.current_offset += b.size() as u64;
                    })
                    .collect::<gst::BufferList>(),
            );

            // Write mfra only for the main stream, and if there are no buffers for the main stream
            // in this segment then don't write anything.
            if let Some(super::FragmentHeaderStream {
                start_time: Some(start_time),
                ..
            }) = streams.get(0)
            {
                state.fragment_offsets.push(super::FragmentOffset {
                    time: *start_time,
                    offset: moof_offset,
                });
            }

            state.end_pts = Some(fragment_end_pts);
            state.end_utc_time = max_end_utc_time;

            // Update for the start PTS of the next fragment
            gst::info!(
                CAT,
                imp: self,
                "Starting new fragment at {}",
                fragment_end_pts,
            );
            state.fragment_start_pts = Some(fragment_end_pts);

            let fku_time = fragment_end_pts + settings.fragment_duration;
            let max_position = state
                .streams
                .iter()
                .map(|s| s.current_position)
                .max()
                .unwrap();

            let fku_time = if max_position > fku_time {
                gst::warning!(
                    CAT,
                    imp: self,
                    "Sending force-keyunit event late for running time {} at {}",
                    fku_time,
                    max_position,
                );
                None
            } else {
                gst::debug!(
                    CAT,
                    imp: self,
                    "Sending force-keyunit event for running time {}",
                    fku_time,
                );
                Some(fku_time)
            };

            let fku = gst_video::UpstreamForceKeyUnitEvent::builder()
                .running_time(fku_time)
                .all_headers(true)
                .build();

            for stream in &state.streams {
                upstream_events.push((stream.sinkpad.clone(), fku.clone()));
            }

            // Reset timeout delay now that we've output an actual fragment
            state.timeout_delay = gst::ClockTime::ZERO;
        }

        if settings.write_mfra && at_eos {
            gst::debug!(CAT, imp: self, "Writing mfra box");
            match boxes::create_mfra(&streams[0].caps, &state.fragment_offsets) {
                Ok(mut mfra) => {
                    {
                        let mfra = mfra.get_mut().unwrap();
                        // mfra is HEADER|DELTA_UNIT like other boxes
                        mfra.set_flags(gst::BufferFlags::HEADER | gst::BufferFlags::DELTA_UNIT);
                    }

                    if buffer_list.is_none() {
                        buffer_list = Some(gst::BufferList::new_sized(1));
                    }
                    buffer_list.as_mut().unwrap().get_mut().unwrap().add(mfra);
                }
                Err(err) => {
                    gst::error!(CAT, imp: self, "Failed to create mfra box: {}", err);
                }
            }
        }

        // TODO: Write edit list at EOS
        // TODO: Rewrite bitrates at EOS

        Ok((caps, buffer_list))
    }

    fn create_streams(&self, state: &mut State) -> Result<(), gst::FlowError> {
        for pad in self
            .obj()
            .sink_pads()
            .into_iter()
            .map(|pad| pad.downcast::<super::FMP4MuxPad>().unwrap())
        {
            let caps = match pad.current_caps() {
                Some(caps) => caps,
                None => {
                    gst::warning!(CAT, obj: pad, "Skipping pad without caps");
                    continue;
                }
            };

            gst::info!(CAT, obj: pad, "Configuring caps {:?}", caps);

            let s = caps.structure(0).unwrap();

            let mut delta_frames = DeltaFrames::IntraOnly;
            match s.name() {
                "video/x-h264" | "video/x-h265" => {
                    if !s.has_field_with_type("codec_data", gst::Buffer::static_type()) {
                        gst::error!(CAT, obj: pad, "Received caps without codec_data");
                        return Err(gst::FlowError::NotNegotiated);
                    }
                    delta_frames = DeltaFrames::Bidirectional;
                }
                "video/x-vp9" => {
                    if !s.has_field_with_type("colorimetry", str::static_type()) {
                        gst::error!(CAT, obj: pad, "Received caps without colorimetry");
                        return Err(gst::FlowError::NotNegotiated);
                    }
                    delta_frames = DeltaFrames::PredictiveOnly;
                }
                "image/jpeg" => (),
                "audio/mpeg" => {
                    if !s.has_field_with_type("codec_data", gst::Buffer::static_type()) {
                        gst::error!(CAT, obj: pad, "Received caps without codec_data");
                        return Err(gst::FlowError::NotNegotiated);
                    }
                }
                "audio/x-opus" => {
                    if let Some(header) = s
                        .get::<gst::ArrayRef>("streamheader")
                        .ok()
                        .and_then(|a| a.get(0).and_then(|v| v.get::<gst::Buffer>().ok()))
                    {
                        if gst_pbutils::codec_utils_opus_parse_header(&header, None).is_err() {
                            gst::error!(CAT, obj: pad, "Received invalid Opus header");
                            return Err(gst::FlowError::NotNegotiated);
                        }
                    } else if gst_pbutils::codec_utils_opus_parse_caps(&caps, None).is_err() {
                        gst::error!(CAT, obj: pad, "Received invalid Opus caps");
                        return Err(gst::FlowError::NotNegotiated);
                    }
                }
                "audio/x-alaw" | "audio/x-mulaw" => (),
                "audio/x-adpcm" => (),
                "application/x-onvif-metadata" => (),
                _ => unreachable!(),
            }

            state.streams.push(Stream {
                sinkpad: pad,
                caps,
                delta_frames,
                queued_gops: VecDeque::new(),
                fragment_filled: false,
                dts_offset: None,
                current_position: gst::ClockTime::ZERO,
                current_utc_time: gst::ClockTime::ZERO,
            });
        }

        if state.streams.is_empty() {
            gst::error!(CAT, imp: self, "No streams available");
            return Err(gst::FlowError::Error);
        }

        // Sort video streams first and then audio streams and then metadata streams, and each group by pad name.
        state.streams.sort_by(|a, b| {
            let order_of_caps = |caps: &gst::CapsRef| {
                let s = caps.structure(0).unwrap();

                if s.name().starts_with("video/") {
                    0
                } else if s.name().starts_with("audio/") {
                    1
                } else if s.name().starts_with("application/x-onvif-metadata") {
                    2
                } else {
                    unimplemented!();
                }
            };

            let st_a = order_of_caps(&a.caps);
            let st_b = order_of_caps(&b.caps);

            if st_a == st_b {
                return a.sinkpad.name().cmp(&b.sinkpad.name());
            }

            st_a.cmp(&st_b)
        });

        Ok(())
    }

    fn update_header(
        &self,
        state: &mut State,
        settings: &Settings,
        at_eos: bool,
    ) -> Result<Option<(gst::BufferList, gst::Caps)>, gst::FlowError> {
        let aggregator = self.obj();
        let class = aggregator.class();
        let variant = class.as_ref().variant;

        if settings.header_update_mode == super::HeaderUpdateMode::None && at_eos {
            return Ok(None);
        }

        assert!(!at_eos || state.streams.iter().all(|s| s.queued_gops.is_empty()));

        let duration = if variant == super::Variant::ONVIF {
            state
                .end_utc_time
                .opt_checked_sub(state.start_utc_time)
                .ok()
                .flatten()
        } else {
            state
                .end_pts
                .opt_checked_sub(state.earliest_pts)
                .ok()
                .flatten()
        };

        let streams = state
            .streams
            .iter()
            .map(|s| super::HeaderStream {
                trak_timescale: s.sinkpad.imp().settings.lock().unwrap().trak_timescale,
                delta_frames: s.delta_frames,
                caps: s.caps.clone(),
            })
            .collect::<Vec<_>>();

        let mut buffer = boxes::create_fmp4_header(super::HeaderConfiguration {
            variant,
            update: at_eos,
            movie_timescale: settings.movie_timescale,
            streams,
            write_mehd: settings.write_mehd,
            duration: if at_eos { duration } else { None },
            start_utc_time: state
                .start_utc_time
                .map(|unix| unix.nseconds() / 100 + UNIX_1601_OFFSET * 10_000_000),
        })
        .map_err(|err| {
            gst::error!(CAT, imp: self, "Failed to create FMP4 header: {}", err);
            gst::FlowError::Error
        })?;

        {
            let buffer = buffer.get_mut().unwrap();

            // No timestamps

            // Header is DISCONT|HEADER
            buffer.set_flags(gst::BufferFlags::DISCONT | gst::BufferFlags::HEADER);
        }

        // Remember stream header for later
        state.stream_header = Some(buffer.clone());

        let variant = match variant {
            super::Variant::ISO | super::Variant::DASH | super::Variant::ONVIF => "iso-fragmented",
            super::Variant::CMAF => "cmaf",
        };
        let caps = gst::Caps::builder("video/quicktime")
            .field("variant", variant)
            .field("streamheader", gst::Array::new([&buffer]))
            .build();

        let mut list = gst::BufferList::new_sized(1);
        {
            let list = list.get_mut().unwrap();
            list.add(buffer);
        }

        Ok(Some((list, caps)))
    }
}

#[glib::object_subclass]
impl ObjectSubclass for FMP4Mux {
    const NAME: &'static str = "GstFMP4Mux";
    type Type = super::FMP4Mux;
    type ParentType = gst_base::Aggregator;
    type Class = Class;
}

impl ObjectImpl for FMP4Mux {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                // TODO: Add chunk-duration property separate from fragment-size
                glib::ParamSpecUInt64::builder("fragment-duration")
                    .nick("Fragment Duration")
                    .blurb("Duration for each FMP4 fragment")
                    .default_value(DEFAULT_FRAGMENT_DURATION.nseconds())
                    .mutable_ready()
                    .build(),
                glib::ParamSpecEnum::builder::<super::HeaderUpdateMode>("header-update-mode", DEFAULT_HEADER_UPDATE_MODE)
                    .nick("Header update mode")
                    .blurb("Mode for updating the header at the end of the stream")
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("write-mfra")
                    .nick("Write mfra box")
                    .blurb("Write fragment random access box at the end of the stream")
                    .default_value(DEFAULT_WRITE_MFRA)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecBoolean::builder("write-mehd")
                    .nick("Write mehd box")
                    .blurb("Write movie extends header box with the duration at the end of the stream (needs a header-update-mode enabled)")
                    .default_value(DEFAULT_WRITE_MFRA)
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("interleave-bytes")
                    .nick("Interleave Bytes")
                    .blurb("Interleave between streams in bytes")
                    .default_value(DEFAULT_INTERLEAVE_BYTES.unwrap_or(0))
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt64::builder("interleave-time")
                    .nick("Interleave Time")
                    .blurb("Interleave between streams in nanoseconds")
                    .default_value(DEFAULT_INTERLEAVE_TIME.map(gst::ClockTime::nseconds).unwrap_or(u64::MAX))
                    .mutable_ready()
                    .build(),
                glib::ParamSpecUInt::builder("movie-timescale")
                    .nick("Movie Timescale")
                    .blurb("Timescale to use for the movie (units per second, 0 is automatic)")
                    .mutable_ready()
                    .build(),
            ]
        });

        &PROPERTIES
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "fragment-duration" => {
                let mut settings = self.settings.lock().unwrap();
                let fragment_duration = value.get().expect("type checked upstream");
                if settings.fragment_duration != fragment_duration {
                    settings.fragment_duration = fragment_duration;
                    drop(settings);
                    self.obj().set_latency(fragment_duration, None);
                }
            }

            "header-update-mode" => {
                let mut settings = self.settings.lock().unwrap();
                settings.header_update_mode = value.get().expect("type checked upstream");
            }

            "write-mfra" => {
                let mut settings = self.settings.lock().unwrap();
                settings.write_mfra = value.get().expect("type checked upstream");
            }

            "write-mehd" => {
                let mut settings = self.settings.lock().unwrap();
                settings.write_mehd = value.get().expect("type checked upstream");
            }

            "interleave-bytes" => {
                let mut settings = self.settings.lock().unwrap();
                settings.interleave_bytes = match value.get().expect("type checked upstream") {
                    0 => None,
                    v => Some(v),
                };
            }

            "interleave-time" => {
                let mut settings = self.settings.lock().unwrap();
                settings.interleave_time = match value.get().expect("type checked upstream") {
                    Some(gst::ClockTime::ZERO) | None => None,
                    v => v,
                };
            }

            "movie-timescale" => {
                let mut settings = self.settings.lock().unwrap();
                settings.movie_timescale = value.get().expect("type checked upstream");
            }

            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "fragment-duration" => {
                let settings = self.settings.lock().unwrap();
                settings.fragment_duration.to_value()
            }

            "header-update-mode" => {
                let settings = self.settings.lock().unwrap();
                settings.header_update_mode.to_value()
            }

            "write-mfra" => {
                let settings = self.settings.lock().unwrap();
                settings.write_mfra.to_value()
            }

            "write-mehd" => {
                let settings = self.settings.lock().unwrap();
                settings.write_mehd.to_value()
            }

            "interleave-bytes" => {
                let settings = self.settings.lock().unwrap();
                settings.interleave_bytes.unwrap_or(0).to_value()
            }

            "interleave-time" => {
                let settings = self.settings.lock().unwrap();
                settings.interleave_time.to_value()
            }

            "movie-timescale" => {
                let settings = self.settings.lock().unwrap();
                settings.movie_timescale.to_value()
            }

            _ => unimplemented!(),
        }
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        let class = obj.class();
        for templ in class.pad_template_list().filter(|templ| {
            templ.presence() == gst::PadPresence::Always
                && templ.direction() == gst::PadDirection::Sink
        }) {
            let sinkpad =
                gst::PadBuilder::<gst_base::AggregatorPad>::from_template(&templ, Some("sink"))
                    .flags(gst::PadFlags::ACCEPT_INTERSECT)
                    .build();

            obj.add_pad(&sinkpad).unwrap();
        }

        obj.set_latency(Settings::default().fragment_duration, None);
    }
}

impl GstObjectImpl for FMP4Mux {}

impl ElementImpl for FMP4Mux {
    fn request_new_pad(
        &self,
        templ: &gst::PadTemplate,
        name: Option<&str>,
        caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let state = self.state.lock().unwrap();
        if state.stream_header.is_some() {
            gst::error!(
                CAT,
                imp: self,
                "Can't request new pads after header was generated"
            );
            return None;
        }

        self.parent_request_new_pad(templ, name, caps)
    }
}

impl AggregatorImpl for FMP4Mux {
    fn next_time(&self) -> Option<gst::ClockTime> {
        let state = self.state.lock().unwrap();
        state.fragment_start_pts.opt_add(state.timeout_delay)
    }

    fn sink_query(
        &self,
        aggregator_pad: &gst_base::AggregatorPad,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryViewMut;

        gst::trace!(CAT, obj: aggregator_pad, "Handling query {:?}", query);

        match query.view_mut() {
            QueryViewMut::Caps(q) => {
                let allowed_caps = aggregator_pad
                    .current_caps()
                    .unwrap_or_else(|| aggregator_pad.pad_template_caps());

                if let Some(filter_caps) = q.filter() {
                    let res = filter_caps
                        .intersect_with_mode(&allowed_caps, gst::CapsIntersectMode::First);
                    q.set_result(&res);
                } else {
                    q.set_result(&allowed_caps);
                }

                true
            }
            _ => self.parent_sink_query(aggregator_pad, query),
        }
    }

    fn sink_event_pre_queue(
        &self,
        aggregator_pad: &gst_base::AggregatorPad,
        mut event: gst::Event,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        use gst::EventView;

        gst::trace!(CAT, obj: aggregator_pad, "Handling event {:?}", event);

        match event.view() {
            EventView::Segment(ev) => {
                if ev.segment().format() != gst::Format::Time {
                    gst::warning!(
                        CAT,
                        obj: aggregator_pad,
                        "Received non-TIME segment, replacing with default TIME segment"
                    );
                    let segment = gst::FormattedSegment::<gst::ClockTime>::new();
                    event = gst::event::Segment::builder(&segment)
                        .seqnum(event.seqnum())
                        .build();
                }
                self.parent_sink_event_pre_queue(aggregator_pad, event)
            }
            _ => self.parent_sink_event_pre_queue(aggregator_pad, event),
        }
    }

    fn sink_event(&self, aggregator_pad: &gst_base::AggregatorPad, event: gst::Event) -> bool {
        use gst::EventView;

        gst::trace!(CAT, obj: aggregator_pad, "Handling event {:?}", event);

        match event.view() {
            EventView::Segment(ev) => {
                // Already fixed-up above to always be a TIME segment
                let segment = ev
                    .segment()
                    .clone()
                    .downcast::<gst::ClockTime>()
                    .expect("non-TIME segment");
                gst::info!(CAT, obj: aggregator_pad, "Received segment {:?}", segment);

                // Only forward the segment event verbatim if this is a single stream variant.
                // Otherwise we have to produce a default segment and re-timestamp all buffers
                // with their running time.
                let aggregator = self.obj();
                let class = aggregator.class();
                if class.as_ref().variant.is_single_stream() {
                    aggregator.update_segment(&segment);
                }

                self.parent_sink_event(aggregator_pad, event)
            }
            EventView::Tag(_ev) => {
                // TODO: Maybe store for putting into the headers of the next fragment?

                self.parent_sink_event(aggregator_pad, event)
            }
            _ => self.parent_sink_event(aggregator_pad, event),
        }
    }

    fn src_query(&self, query: &mut gst::QueryRef) -> bool {
        use gst::QueryViewMut;

        gst::trace!(CAT, imp: self, "Handling query {:?}", query);

        match query.view_mut() {
            QueryViewMut::Seeking(q) => {
                // We can't really handle seeking, it would break everything
                q.set(false, gst::ClockTime::ZERO, gst::ClockTime::NONE);
                true
            }
            _ => self.parent_src_query(query),
        }
    }

    fn src_event(&self, event: gst::Event) -> bool {
        use gst::EventView;

        gst::trace!(CAT, imp: self, "Handling event {:?}", event);

        match event.view() {
            EventView::Seek(_ev) => false,
            _ => self.parent_src_event(event),
        }
    }

    fn flush(&self) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut state = self.state.lock().unwrap();

        for stream in &mut state.streams {
            stream.queued_gops.clear();
            stream.dts_offset = None;
            stream.current_position = gst::ClockTime::ZERO;
            stream.current_utc_time = gst::ClockTime::ZERO;
            stream.fragment_filled = false;
        }

        state.current_offset = 0;
        state.fragment_offsets.clear();

        drop(state);

        self.parent_flush()
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        gst::trace!(CAT, imp: self, "Stopping");

        let _ = self.parent_stop();

        *self.state.lock().unwrap() = State::default();

        Ok(())
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        gst::trace!(CAT, imp: self, "Starting");

        self.parent_start()?;

        // For non-single-stream variants configure a default segment that allows for negative
        // DTS so that we can correctly re-timestamp buffers with their running times.
        let aggregator = self.obj();
        let class = aggregator.class();
        if !class.as_ref().variant.is_single_stream() {
            let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
            segment.set_start(SEGMENT_OFFSET);
            segment.set_position(SEGMENT_OFFSET);
            aggregator.update_segment(&segment);
        }

        *self.state.lock().unwrap() = State::default();

        Ok(())
    }

    fn negotiate(&self) -> bool {
        true
    }

    fn aggregate(&self, timeout: bool) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = self.settings.lock().unwrap().clone();

        let mut upstream_events = vec![];

        let all_eos;
        let (caps, buffers) = {
            let mut state = self.state.lock().unwrap();

            // Create streams
            if state.streams.is_empty() {
                self.create_streams(&mut state)?;
            }

            // Queue buffers from all streams that are not filled for the current fragment yet
            //
            // Always take a buffer from the stream with the earliest queued buffer to keep the
            // fill-level at all sinkpads in sync.
            let fragment_start_pts = state.fragment_start_pts;

            while let Some((idx, stream)) = self.find_earliest_stream(&mut state, timeout)? {
                // Can only happen if the stream was flushed in the meantime
                let buffer = match stream.sinkpad.pop_buffer() {
                    Some(buffer) => buffer,
                    None => continue,
                };

                // Can only happen if the stream was flushed in the meantime
                let segment = match stream
                    .sinkpad
                    .segment()
                    .clone()
                    .downcast::<gst::ClockTime>()
                    .ok()
                {
                    Some(segment) => segment,
                    None => {
                        gst::error!(CAT, obj: stream.sinkpad, "Got buffer before segment");
                        return Err(gst::FlowError::Error);
                    }
                };

                // Queue up the buffer and update GOP tracking state
                self.queue_gops(idx, stream, &segment, buffer)?;

                // Check if this stream is filled enough now.
                if let Some((queued_end_pts, fragment_start_pts)) = Option::zip(
                    stream
                        .queued_gops
                        .iter()
                        .find(|gop| gop.final_end_pts)
                        .map(|gop| gop.end_pts),
                    fragment_start_pts,
                ) {
                    if queued_end_pts.saturating_sub(fragment_start_pts)
                        >= settings.fragment_duration
                    {
                        gst::debug!(CAT, obj: stream.sinkpad, "Stream queued enough data for this fragment");
                        stream.fragment_filled = true;
                    }
                }
            }

            // Calculate the earliest PTS after queueing input if we can now.
            if state.earliest_pts.is_none() {
                let mut earliest_pts = None;

                for stream in &state.streams {
                    let stream_earliest_pts = match stream.queued_gops.back() {
                        None => {
                            earliest_pts = None;
                            break;
                        }
                        Some(oldest_gop) => {
                            if !timeout && !oldest_gop.final_earliest_pts {
                                earliest_pts = None;
                                break;
                            }

                            oldest_gop.earliest_pts
                        }
                    };

                    if earliest_pts.opt_gt(stream_earliest_pts).unwrap_or(true) {
                        earliest_pts = Some(stream_earliest_pts);
                    }
                }

                if let Some(earliest_pts) = earliest_pts {
                    gst::info!(CAT, imp: self, "Got earliest PTS {}", earliest_pts);
                    state.earliest_pts = Some(earliest_pts);
                    state.fragment_start_pts = Some(earliest_pts);

                    let fku_time = earliest_pts + settings.fragment_duration;
                    let max_position = state
                        .streams
                        .iter()
                        .map(|s| s.current_position)
                        .max()
                        .unwrap();

                    let fku_time = if max_position > fku_time {
                        gst::warning!(
                            CAT,
                            imp: self,
                            "Sending first force-keyunit event late for running time {} at {}",
                            fku_time,
                            max_position,
                        );
                        None
                    } else {
                        gst::debug!(
                            CAT,
                            imp: self,
                            "Sending first force-keyunit event for running time {}",
                            fku_time,
                        );
                        Some(fku_time)
                    };

                    let fku = gst_video::UpstreamForceKeyUnitEvent::builder()
                        .running_time(fku_time)
                        .all_headers(true)
                        .build();

                    for stream in &mut state.streams {
                        upstream_events.push((stream.sinkpad.clone(), fku.clone()));

                        // Check if this stream is filled enough now.
                        if let Some(queued_end_pts) = stream
                            .queued_gops
                            .iter()
                            .find(|gop| gop.final_end_pts)
                            .map(|gop| gop.end_pts)
                        {
                            if queued_end_pts.saturating_sub(earliest_pts)
                                >= settings.fragment_duration
                            {
                                gst::debug!(CAT, obj: stream.sinkpad, "Stream queued enough data for this fragment");
                                stream.fragment_filled = true;
                            }
                        }
                    }
                }
            }

            all_eos = state.streams.iter().all(|stream| stream.sinkpad.is_eos());
            if all_eos {
                gst::debug!(CAT, imp: self, "All streams are EOS now");
            }

            // If enough GOPs were queued, drain and create the output fragment
            match self.drain(
                &mut state,
                &settings,
                timeout,
                all_eos,
                &mut upstream_events,
            ) {
                Ok(res) => res,
                Err(gst_base::AGGREGATOR_FLOW_NEED_DATA) => {
                    gst::element_imp_warning!(
                        self,
                        gst::StreamError::Format,
                        ["Longer GOPs than fragment duration"]
                    );
                    state.timeout_delay += 1.seconds();

                    drop(state);
                    for (sinkpad, event) in upstream_events {
                        sinkpad.push_event(event);
                    }
                    return Err(gst_base::AGGREGATOR_FLOW_NEED_DATA);
                }
                Err(err) => return Err(err),
            }
        };

        for (sinkpad, event) in upstream_events {
            sinkpad.push_event(event);
        }

        if let Some(caps) = caps {
            gst::debug!(CAT, imp: self, "Setting caps on source pad: {:?}", caps);
            self.obj().set_src_caps(&caps);
        }

        if let Some(buffers) = buffers {
            gst::trace!(CAT, imp: self, "Pushing buffer list {:?}", buffers);
            self.obj().finish_buffer_list(buffers)?;
        }

        if all_eos {
            gst::debug!(CAT, imp: self, "Doing EOS handling");

            if settings.header_update_mode != super::HeaderUpdateMode::None {
                let updated_header =
                    self.update_header(&mut self.state.lock().unwrap(), &settings, true);
                match updated_header {
                    Ok(Some((buffer_list, caps))) => {
                        match settings.header_update_mode {
                            super::HeaderUpdateMode::None => unreachable!(),
                            super::HeaderUpdateMode::Rewrite => {
                                let mut q = gst::query::Seeking::new(gst::Format::Bytes);
                                if self.obj().src_pad().peer_query(&mut q) && q.result().0 {
                                    let aggregator = self.obj();

                                    aggregator.set_src_caps(&caps);

                                    // Seek to the beginning with a default bytes segment
                                    aggregator
                                        .update_segment(
                                            &gst::FormattedSegment::<gst::format::Bytes>::new(),
                                        );

                                    if let Err(err) = aggregator.finish_buffer_list(buffer_list) {
                                        gst::error!(
                                            CAT,
                                            imp: self,
                                            "Failed pushing updated header buffer downstream: {:?}",
                                            err,
                                        );
                                    }
                                } else {
                                    gst::error!(
                                        CAT,
                                        imp: self,
                                        "Can't rewrite header because downstream is not seekable"
                                    );
                                }
                            }
                            super::HeaderUpdateMode::Update => {
                                let aggregator = self.obj();

                                aggregator.set_src_caps(&caps);
                                if let Err(err) = aggregator.finish_buffer_list(buffer_list) {
                                    gst::error!(
                                        CAT,
                                        imp: self,
                                        "Failed pushing updated header buffer downstream: {:?}",
                                        err,
                                    );
                                }
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        gst::error!(
                            CAT,
                            imp: self,
                            "Failed to generate updated header: {:?}",
                            err
                        );
                    }
                }
            }

            // Need to output new headers if started again after EOS
            self.state.lock().unwrap().sent_headers = false;

            Err(gst::FlowError::Eos)
        } else {
            Ok(gst::FlowSuccess::Ok)
        }
    }
}

#[repr(C)]
pub(crate) struct Class {
    parent: gst_base::ffi::GstAggregatorClass,
    variant: super::Variant,
}

unsafe impl ClassStruct for Class {
    type Type = FMP4Mux;
}

impl std::ops::Deref for Class {
    type Target = glib::Class<gst_base::Aggregator>;

    fn deref(&self) -> &Self::Target {
        unsafe { &*(&self.parent as *const _ as *const _) }
    }
}

unsafe impl<T: FMP4MuxImpl> IsSubclassable<T> for super::FMP4Mux {
    fn class_init(class: &mut glib::Class<Self>) {
        Self::parent_class_init::<T>(class);

        let class = class.as_mut();
        class.variant = T::VARIANT;
    }
}

pub(crate) trait FMP4MuxImpl: AggregatorImpl {
    const VARIANT: super::Variant;
}

#[derive(Default)]
pub(crate) struct ISOFMP4Mux;

#[glib::object_subclass]
impl ObjectSubclass for ISOFMP4Mux {
    const NAME: &'static str = "GstISOFMP4Mux";
    type Type = super::ISOFMP4Mux;
    type ParentType = super::FMP4Mux;
}

impl ObjectImpl for ISOFMP4Mux {}

impl GstObjectImpl for ISOFMP4Mux {}

impl ElementImpl for ISOFMP4Mux {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "ISOFMP4Mux",
                "Codec/Muxer",
                "ISO fragmented MP4 muxer",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("video/quicktime")
                    .field("variant", "iso-fragmented")
                    .build(),
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::with_gtype(
                "sink_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &[
                    gst::Structure::builder("video/x-h264")
                        .field("stream-format", gst::List::new(["avc", "avc3"]))
                        .field("alignment", "au")
                        .field("width", gst::IntRange::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("video/x-h265")
                        .field("stream-format", gst::List::new(["hvc1", "hev1"]))
                        .field("alignment", "au")
                        .field("width", gst::IntRange::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("video/x-vp9")
                        .field("profile", gst::List::new(["0", "1", "2", "3"]))
                        .field("chroma-format", gst::List::new(["4:2:0", "4:2:2", "4:4:4"]))
                        .field("bit-depth-luma", gst::List::new([8u32, 10u32, 12u32]))
                        .field("bit-depth-chroma", gst::List::new([8u32, 10u32, 12u32]))
                        .field("width", gst::IntRange::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("audio/mpeg")
                        .field("mpegversion", 4i32)
                        .field("stream-format", "raw")
                        .field("channels", gst::IntRange::new(1, u16::MAX as i32))
                        .field("rate", gst::IntRange::new(1, i32::MAX))
                        .build(),
                    gst::Structure::builder("audio/x-opus")
                        .field("channel-mapping-family", gst::IntRange::new(0i32, 255))
                        .field("channels", gst::IntRange::new(1i32, 8))
                        .field("rate", gst::IntRange::new(1, i32::MAX))
                        .build(),
                ]
                .into_iter()
                .collect::<gst::Caps>(),
                super::FMP4MuxPad::static_type(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl AggregatorImpl for ISOFMP4Mux {}

impl FMP4MuxImpl for ISOFMP4Mux {
    const VARIANT: super::Variant = super::Variant::ISO;
}

#[derive(Default)]
pub(crate) struct CMAFMux;

#[glib::object_subclass]
impl ObjectSubclass for CMAFMux {
    const NAME: &'static str = "GstCMAFMux";
    type Type = super::CMAFMux;
    type ParentType = super::FMP4Mux;
}

impl ObjectImpl for CMAFMux {}

impl GstObjectImpl for CMAFMux {}

impl ElementImpl for CMAFMux {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "CMAFMux",
                "Codec/Muxer",
                "CMAF fragmented MP4 muxer",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("video/quicktime")
                    .field("variant", "cmaf")
                    .build(),
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::with_gtype(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &[
                    gst::Structure::builder("video/x-h264")
                        .field("stream-format", gst::List::new(["avc", "avc3"]))
                        .field("alignment", "au")
                        .field("width", gst::IntRange::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("video/x-h265")
                        .field("stream-format", gst::List::new(["hvc1", "hev1"]))
                        .field("alignment", "au")
                        .field("width", gst::IntRange::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("audio/mpeg")
                        .field("mpegversion", 4i32)
                        .field("stream-format", "raw")
                        .field("channels", gst::IntRange::new(1, u16::MAX as i32))
                        .field("rate", gst::IntRange::new(1, i32::MAX))
                        .build(),
                ]
                .into_iter()
                .collect::<gst::Caps>(),
                super::FMP4MuxPad::static_type(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl AggregatorImpl for CMAFMux {}

impl FMP4MuxImpl for CMAFMux {
    const VARIANT: super::Variant = super::Variant::CMAF;
}

#[derive(Default)]
pub(crate) struct DASHMP4Mux;

#[glib::object_subclass]
impl ObjectSubclass for DASHMP4Mux {
    const NAME: &'static str = "GstDASHMP4Mux";
    type Type = super::DASHMP4Mux;
    type ParentType = super::FMP4Mux;
}

impl ObjectImpl for DASHMP4Mux {}

impl GstObjectImpl for DASHMP4Mux {}

impl ElementImpl for DASHMP4Mux {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "DASHMP4Mux",
                "Codec/Muxer",
                "DASH fragmented MP4 muxer",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("video/quicktime")
                    .field("variant", "iso-fragmented")
                    .build(),
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::with_gtype(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &[
                    gst::Structure::builder("video/x-h264")
                        .field("stream-format", gst::List::new(["avc", "avc3"]))
                        .field("alignment", "au")
                        .field("width", gst::IntRange::<i32>::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::<i32>::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("video/x-h265")
                        .field("stream-format", gst::List::new(["hvc1", "hev1"]))
                        .field("alignment", "au")
                        .field("width", gst::IntRange::<i32>::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::<i32>::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("video/x-vp9")
                        .field("profile", gst::List::new(["0", "1", "2", "3"]))
                        .field("chroma-format", gst::List::new(["4:2:0", "4:2:2", "4:4:4"]))
                        .field("bit-depth-luma", gst::List::new([8u32, 10u32, 12u32]))
                        .field("bit-depth-chroma", gst::List::new([8u32, 10u32, 12u32]))
                        .field("width", gst::IntRange::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("audio/mpeg")
                        .field("mpegversion", 4i32)
                        .field("stream-format", "raw")
                        .field("channels", gst::IntRange::<i32>::new(1, u16::MAX as i32))
                        .field("rate", gst::IntRange::<i32>::new(1, i32::MAX))
                        .build(),
                    gst::Structure::builder("audio/x-opus")
                        .field("channel-mapping-family", gst::IntRange::new(0i32, 255))
                        .field("channels", gst::IntRange::new(1i32, 8))
                        .field("rate", gst::IntRange::new(1, i32::MAX))
                        .build(),
                ]
                .into_iter()
                .collect::<gst::Caps>(),
                super::FMP4MuxPad::static_type(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl AggregatorImpl for DASHMP4Mux {}

impl FMP4MuxImpl for DASHMP4Mux {
    const VARIANT: super::Variant = super::Variant::DASH;
}

#[derive(Default)]
pub(crate) struct ONVIFFMP4Mux;

#[glib::object_subclass]
impl ObjectSubclass for ONVIFFMP4Mux {
    const NAME: &'static str = "GstONVIFFMP4Mux";
    type Type = super::ONVIFFMP4Mux;
    type ParentType = super::FMP4Mux;
}

impl ObjectImpl for ONVIFFMP4Mux {}

impl GstObjectImpl for ONVIFFMP4Mux {}

impl ElementImpl for ONVIFFMP4Mux {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "ONVIFFMP4Mux",
                "Codec/Muxer",
                "ONVIF fragmented MP4 muxer",
                "Sebastian Dröge <sebastian@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &gst::Caps::builder("video/quicktime")
                    .field("variant", "iso-fragmented")
                    .build(),
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::with_gtype(
                "sink_%u",
                gst::PadDirection::Sink,
                gst::PadPresence::Request,
                &[
                    gst::Structure::builder("video/x-h264")
                        .field("stream-format", gst::List::new(["avc", "avc3"]))
                        .field("alignment", "au")
                        .field("width", gst::IntRange::<i32>::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::<i32>::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("video/x-h265")
                        .field("stream-format", gst::List::new(["hvc1", "hev1"]))
                        .field("alignment", "au")
                        .field("width", gst::IntRange::<i32>::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::<i32>::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("image/jpeg")
                        .field("width", gst::IntRange::<i32>::new(1, u16::MAX as i32))
                        .field("height", gst::IntRange::<i32>::new(1, u16::MAX as i32))
                        .build(),
                    gst::Structure::builder("audio/mpeg")
                        .field("mpegversion", 4i32)
                        .field("stream-format", "raw")
                        .field("channels", gst::IntRange::<i32>::new(1, u16::MAX as i32))
                        .field("rate", gst::IntRange::<i32>::new(1, i32::MAX))
                        .build(),
                    gst::Structure::builder("audio/x-alaw")
                        .field("channels", gst::IntRange::<i32>::new(1, 2))
                        .field("rate", gst::IntRange::<i32>::new(1, i32::MAX))
                        .build(),
                    gst::Structure::builder("audio/x-mulaw")
                        .field("channels", gst::IntRange::<i32>::new(1, 2))
                        .field("rate", gst::IntRange::<i32>::new(1, i32::MAX))
                        .build(),
                    gst::Structure::builder("audio/x-adpcm")
                        .field("layout", "g726")
                        .field("channels", 1i32)
                        .field("rate", 8000i32)
                        .field("bitrate", gst::List::new([16000i32, 24000, 32000, 40000]))
                        .build(),
                    gst::Structure::builder("application/x-onvif-metadata")
                        .field("parsed", true)
                        .build(),
                ]
                .into_iter()
                .collect::<gst::Caps>(),
                super::FMP4MuxPad::static_type(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl AggregatorImpl for ONVIFFMP4Mux {}

impl FMP4MuxImpl for ONVIFFMP4Mux {
    const VARIANT: super::Variant = super::Variant::ONVIF;
}

#[derive(Default, Clone)]
struct PadSettings {
    trak_timescale: u32,
}

#[derive(Default)]
pub(crate) struct FMP4MuxPad {
    settings: Mutex<PadSettings>,
}

#[glib::object_subclass]
impl ObjectSubclass for FMP4MuxPad {
    const NAME: &'static str = "GstFMP4MuxPad";
    type Type = super::FMP4MuxPad;
    type ParentType = gst_base::AggregatorPad;
}

impl ObjectImpl for FMP4MuxPad {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![glib::ParamSpecUInt::builder("trak-timescale")
                .nick("Track Timescale")
                .blurb("Timescale to use for the track (units per second, 0 is automatic)")
                .mutable_ready()
                .build()]
        });

        &PROPERTIES
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "trak-timescale" => {
                let mut settings = self.settings.lock().unwrap();
                settings.trak_timescale = value.get().expect("type checked upstream");
            }

            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "trak-timescale" => {
                let settings = self.settings.lock().unwrap();
                settings.trak_timescale.to_value()
            }

            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for FMP4MuxPad {}

impl PadImpl for FMP4MuxPad {}

impl AggregatorPadImpl for FMP4MuxPad {
    fn flush(&self, aggregator: &gst_base::Aggregator) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mux = aggregator.downcast_ref::<super::FMP4Mux>().unwrap();
        let mut mux_state = mux.imp().state.lock().unwrap();

        for stream in &mut mux_state.streams {
            if stream.sinkpad == *self.obj() {
                stream.queued_gops.clear();
                stream.dts_offset = None;
                stream.current_position = gst::ClockTime::ZERO;
                stream.current_utc_time = gst::ClockTime::ZERO;
                stream.fragment_filled = false;
                break;
            }
        }

        drop(mux_state);

        self.parent_flush(aggregator)
    }
}
