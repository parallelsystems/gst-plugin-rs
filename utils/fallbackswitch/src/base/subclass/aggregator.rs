// Copyright (C) 2017-2019 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use super::super::ffi;

use glib::prelude::*;
use glib::subclass::prelude::*;
use glib::translate::*;

use gst::subclass::prelude::*;

use std::ptr;

use super::super::Aggregator;
use super::super::AggregatorPad;

pub trait AggregatorImpl: AggregatorImplExt + ElementImpl {
    fn flush(&self, aggregator: &Self::Type) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.parent_flush(aggregator)
    }

    fn clip(
        &self,
        aggregator: &Self::Type,
        aggregator_pad: &AggregatorPad,
        buffer: gst::Buffer,
    ) -> Option<gst::Buffer> {
        self.parent_clip(aggregator, aggregator_pad, buffer)
    }

    fn finish_buffer(
        &self,
        aggregator: &Self::Type,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.parent_finish_buffer(aggregator, buffer)
    }

    fn sink_event(
        &self,
        aggregator: &Self::Type,
        aggregator_pad: &AggregatorPad,
        event: gst::Event,
    ) -> bool {
        self.parent_sink_event(aggregator, aggregator_pad, event)
    }

    fn sink_event_pre_queue(
        &self,
        aggregator: &Self::Type,
        aggregator_pad: &AggregatorPad,
        event: gst::Event,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.parent_sink_event_pre_queue(aggregator, aggregator_pad, event)
    }

    fn sink_query(
        &self,
        aggregator: &Self::Type,
        aggregator_pad: &AggregatorPad,
        query: &mut gst::QueryRef,
    ) -> bool {
        self.parent_sink_query(aggregator, aggregator_pad, query)
    }

    fn sink_query_pre_queue(
        &self,
        aggregator: &Self::Type,
        aggregator_pad: &AggregatorPad,
        query: &mut gst::QueryRef,
    ) -> bool {
        self.parent_sink_query_pre_queue(aggregator, aggregator_pad, query)
    }

    fn src_event(&self, aggregator: &Self::Type, event: gst::Event) -> bool {
        self.parent_src_event(aggregator, event)
    }

    fn src_query(&self, aggregator: &Self::Type, query: &mut gst::QueryRef) -> bool {
        self.parent_src_query(aggregator, query)
    }

    fn src_activate(
        &self,
        aggregator: &Self::Type,
        mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        self.parent_src_activate(aggregator, mode, active)
    }

    fn aggregate(
        &self,
        aggregator: &Self::Type,
        timeout: bool,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.parent_aggregate(aggregator, timeout)
    }

    fn start(&self, aggregator: &Self::Type) -> Result<(), gst::ErrorMessage> {
        self.parent_start(aggregator)
    }

    fn stop(&self, aggregator: &Self::Type) -> Result<(), gst::ErrorMessage> {
        self.parent_stop(aggregator)
    }

    fn get_next_time(&self, aggregator: &Self::Type) -> gst::ClockTime {
        self.parent_get_next_time(aggregator)
    }

    fn create_new_pad(
        &self,
        aggregator: &Self::Type,
        templ: &gst::PadTemplate,
        req_name: Option<&str>,
        caps: Option<&gst::Caps>,
    ) -> Option<AggregatorPad> {
        self.parent_create_new_pad(aggregator, templ, req_name, caps)
    }

    fn update_src_caps(
        &self,
        aggregator: &Self::Type,
        caps: &gst::Caps,
    ) -> Result<gst::Caps, gst::FlowError> {
        self.parent_update_src_caps(aggregator, caps)
    }

    fn fixate_src_caps(&self, aggregator: &Self::Type, caps: gst::Caps) -> gst::Caps {
        self.parent_fixate_src_caps(aggregator, caps)
    }

    fn negotiated_src_caps(
        &self,
        aggregator: &Self::Type,
        caps: &gst::Caps,
    ) -> Result<(), gst::LoggableError> {
        self.parent_negotiated_src_caps(aggregator, caps)
    }

    fn negotiate(&self, aggregator: &Self::Type) -> bool {
        self.parent_negotiate(aggregator)
    }
}

pub trait AggregatorImplExt: ObjectSubclass {
    fn parent_flush(&self, aggregator: &Self::Type) -> Result<gst::FlowSuccess, gst::FlowError>;

    fn parent_clip(
        &self,
        aggregator: &Self::Type,
        aggregator_pad: &AggregatorPad,
        buffer: gst::Buffer,
    ) -> Option<gst::Buffer>;

    fn parent_finish_buffer(
        &self,
        aggregator: &Self::Type,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError>;

    fn parent_sink_event(
        &self,
        aggregator: &Self::Type,
        aggregator_pad: &AggregatorPad,
        event: gst::Event,
    ) -> bool;

    fn parent_sink_event_pre_queue(
        &self,
        aggregator: &Self::Type,
        aggregator_pad: &AggregatorPad,
        event: gst::Event,
    ) -> Result<gst::FlowSuccess, gst::FlowError>;

    fn parent_sink_query(
        &self,
        aggregator: &Self::Type,
        aggregator_pad: &AggregatorPad,
        query: &mut gst::QueryRef,
    ) -> bool;

    fn parent_sink_query_pre_queue(
        &self,
        aggregator: &Self::Type,
        aggregator_pad: &AggregatorPad,
        query: &mut gst::QueryRef,
    ) -> bool;

    fn parent_src_event(&self, aggregator: &Self::Type, event: gst::Event) -> bool;

    fn parent_src_query(&self, aggregator: &Self::Type, query: &mut gst::QueryRef) -> bool;

    fn parent_src_activate(
        &self,
        aggregator: &Self::Type,
        mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError>;

    fn parent_aggregate(
        &self,
        aggregator: &Self::Type,
        timeout: bool,
    ) -> Result<gst::FlowSuccess, gst::FlowError>;

    fn parent_start(&self, aggregator: &Self::Type) -> Result<(), gst::ErrorMessage>;

    fn parent_stop(&self, aggregator: &Self::Type) -> Result<(), gst::ErrorMessage>;

    fn parent_get_next_time(&self, aggregator: &Self::Type) -> gst::ClockTime;

    fn parent_create_new_pad(
        &self,
        aggregator: &Self::Type,
        templ: &gst::PadTemplate,
        req_name: Option<&str>,
        caps: Option<&gst::Caps>,
    ) -> Option<AggregatorPad>;

    fn parent_update_src_caps(
        &self,
        aggregator: &Self::Type,
        caps: &gst::Caps,
    ) -> Result<gst::Caps, gst::FlowError>;

    fn parent_fixate_src_caps(&self, aggregator: &Self::Type, caps: gst::Caps) -> gst::Caps;

    fn parent_negotiated_src_caps(
        &self,
        aggregator: &Self::Type,
        caps: &gst::Caps,
    ) -> Result<(), gst::LoggableError>;

    fn parent_negotiate(&self, aggregator: &Self::Type) -> bool;
}

impl<T: AggregatorImpl> AggregatorImplExt for T {
    fn parent_flush(&self, aggregator: &Self::Type) -> Result<gst::FlowSuccess, gst::FlowError> {
        unsafe {
            let data = T::type_data();
            let parent_class = data.as_ref().get_parent_class() as *mut ffi::GstAggregatorClass;
            (*parent_class)
                .flush
                .map(|f| {
                    from_glib(f(aggregator
                        .unsafe_cast_ref::<Aggregator>()
                        .to_glib_none()
                        .0))
                })
                .unwrap_or(gst::FlowReturn::Ok)
                .into_result()
        }
    }

    fn parent_clip(
        &self,
        aggregator: &Self::Type,
        aggregator_pad: &AggregatorPad,
        buffer: gst::Buffer,
    ) -> Option<gst::Buffer> {
        unsafe {
            let data = T::type_data();
            let parent_class = data.as_ref().get_parent_class() as *mut ffi::GstAggregatorClass;
            match (*parent_class).clip {
                None => Some(buffer),
                Some(ref func) => from_glib_full(func(
                    aggregator.unsafe_cast_ref::<Aggregator>().to_glib_none().0,
                    aggregator_pad.to_glib_none().0,
                    buffer.into_ptr(),
                )),
            }
        }
    }

    fn parent_finish_buffer(
        &self,
        aggregator: &Self::Type,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        unsafe {
            let data = T::type_data();
            let parent_class = data.as_ref().get_parent_class() as *mut ffi::GstAggregatorClass;
            let f = (*parent_class)
                .finish_buffer
                .expect("Missing parent function `finish_buffer`");
            gst::FlowReturn::from_glib(f(
                aggregator.unsafe_cast_ref::<Aggregator>().to_glib_none().0,
                buffer.into_ptr(),
            ))
            .into_result()
        }
    }

    fn parent_sink_event(
        &self,
        aggregator: &Self::Type,
        aggregator_pad: &AggregatorPad,
        event: gst::Event,
    ) -> bool {
        unsafe {
            let data = T::type_data();
            let parent_class = data.as_ref().get_parent_class() as *mut ffi::GstAggregatorClass;
            let f = (*parent_class)
                .sink_event
                .expect("Missing parent function `sink_event`");
            from_glib(f(
                aggregator.unsafe_cast_ref::<Aggregator>().to_glib_none().0,
                aggregator_pad.to_glib_none().0,
                event.into_ptr(),
            ))
        }
    }

    fn parent_sink_event_pre_queue(
        &self,
        aggregator: &Self::Type,
        aggregator_pad: &AggregatorPad,
        event: gst::Event,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        unsafe {
            let data = T::type_data();
            let parent_class = data.as_ref().get_parent_class() as *mut ffi::GstAggregatorClass;
            let f = (*parent_class)
                .sink_event_pre_queue
                .expect("Missing parent function `sink_event_pre_queue`");
            gst::FlowReturn::from_glib(f(
                aggregator.unsafe_cast_ref::<Aggregator>().to_glib_none().0,
                aggregator_pad.to_glib_none().0,
                event.into_ptr(),
            ))
            .into_result()
        }
    }

    fn parent_sink_query(
        &self,
        aggregator: &Self::Type,
        aggregator_pad: &AggregatorPad,
        query: &mut gst::QueryRef,
    ) -> bool {
        unsafe {
            let data = T::type_data();
            let parent_class = data.as_ref().get_parent_class() as *mut ffi::GstAggregatorClass;
            let f = (*parent_class)
                .sink_query
                .expect("Missing parent function `sink_query`");
            from_glib(f(
                aggregator.unsafe_cast_ref::<Aggregator>().to_glib_none().0,
                aggregator_pad.to_glib_none().0,
                query.as_mut_ptr(),
            ))
        }
    }

    fn parent_sink_query_pre_queue(
        &self,
        aggregator: &Self::Type,
        aggregator_pad: &AggregatorPad,
        query: &mut gst::QueryRef,
    ) -> bool {
        unsafe {
            let data = T::type_data();
            let parent_class = data.as_ref().get_parent_class() as *mut ffi::GstAggregatorClass;
            let f = (*parent_class)
                .sink_query_pre_queue
                .expect("Missing parent function `sink_query`");
            from_glib(f(
                aggregator.unsafe_cast_ref::<Aggregator>().to_glib_none().0,
                aggregator_pad.to_glib_none().0,
                query.as_mut_ptr(),
            ))
        }
    }

    fn parent_src_event(&self, aggregator: &Self::Type, event: gst::Event) -> bool {
        unsafe {
            let data = T::type_data();
            let parent_class = data.as_ref().get_parent_class() as *mut ffi::GstAggregatorClass;
            let f = (*parent_class)
                .src_event
                .expect("Missing parent function `src_event`");
            from_glib(f(
                aggregator.unsafe_cast_ref::<Aggregator>().to_glib_none().0,
                event.into_ptr(),
            ))
        }
    }

    fn parent_src_query(&self, aggregator: &Self::Type, query: &mut gst::QueryRef) -> bool {
        unsafe {
            let data = T::type_data();
            let parent_class = data.as_ref().get_parent_class() as *mut ffi::GstAggregatorClass;
            let f = (*parent_class)
                .src_query
                .expect("Missing parent function `src_query`");
            from_glib(f(
                aggregator.unsafe_cast_ref::<Aggregator>().to_glib_none().0,
                query.as_mut_ptr(),
            ))
        }
    }

    fn parent_src_activate(
        &self,
        aggregator: &Self::Type,
        mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        unsafe {
            let data = T::type_data();
            let parent_class = data.as_ref().get_parent_class() as *mut ffi::GstAggregatorClass;
            match (*parent_class).src_activate {
                None => Ok(()),
                Some(f) => gst::result_from_gboolean!(
                    f(
                        aggregator.unsafe_cast_ref::<Aggregator>().to_glib_none().0,
                        mode.to_glib(),
                        active.to_glib()
                    ),
                    gst::CAT_RUST,
                    "Parent function `src_activate` failed"
                ),
            }
        }
    }

    fn parent_aggregate(
        &self,
        aggregator: &Self::Type,
        timeout: bool,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        unsafe {
            let data = T::type_data();
            let parent_class = data.as_ref().get_parent_class() as *mut ffi::GstAggregatorClass;
            let f = (*parent_class)
                .aggregate
                .expect("Missing parent function `aggregate`");
            gst::FlowReturn::from_glib(f(
                aggregator.unsafe_cast_ref::<Aggregator>().to_glib_none().0,
                timeout.to_glib(),
            ))
            .into_result()
        }
    }

    fn parent_start(&self, aggregator: &Self::Type) -> Result<(), gst::ErrorMessage> {
        unsafe {
            let data = T::type_data();
            let parent_class = data.as_ref().get_parent_class() as *mut ffi::GstAggregatorClass;
            (*parent_class)
                .start
                .map(|f| {
                    if from_glib(f(aggregator
                        .unsafe_cast_ref::<Aggregator>()
                        .to_glib_none()
                        .0))
                    {
                        Ok(())
                    } else {
                        Err(gst::error_msg!(
                            gst::CoreError::Failed,
                            ["Parent function `start` failed"]
                        ))
                    }
                })
                .unwrap_or(Ok(()))
        }
    }

    fn parent_stop(&self, aggregator: &Self::Type) -> Result<(), gst::ErrorMessage> {
        unsafe {
            let data = T::type_data();
            let parent_class = data.as_ref().get_parent_class() as *mut ffi::GstAggregatorClass;
            (*parent_class)
                .stop
                .map(|f| {
                    if from_glib(f(aggregator
                        .unsafe_cast_ref::<Aggregator>()
                        .to_glib_none()
                        .0))
                    {
                        Ok(())
                    } else {
                        Err(gst::error_msg!(
                            gst::CoreError::Failed,
                            ["Parent function `stop` failed"]
                        ))
                    }
                })
                .unwrap_or(Ok(()))
        }
    }

    fn parent_get_next_time(&self, aggregator: &Self::Type) -> gst::ClockTime {
        unsafe {
            let data = T::type_data();
            let parent_class = data.as_ref().get_parent_class() as *mut ffi::GstAggregatorClass;
            (*parent_class)
                .get_next_time
                .map(|f| {
                    from_glib(f(aggregator
                        .unsafe_cast_ref::<Aggregator>()
                        .to_glib_none()
                        .0))
                })
                .unwrap_or(gst::CLOCK_TIME_NONE)
        }
    }

    fn parent_create_new_pad(
        &self,
        aggregator: &Self::Type,
        templ: &gst::PadTemplate,
        req_name: Option<&str>,
        caps: Option<&gst::Caps>,
    ) -> Option<AggregatorPad> {
        unsafe {
            let data = T::type_data();
            let parent_class = data.as_ref().get_parent_class() as *mut ffi::GstAggregatorClass;
            let f = (*parent_class)
                .create_new_pad
                .expect("Missing parent function `create_new_pad`");
            from_glib_full(f(
                aggregator.unsafe_cast_ref::<Aggregator>().to_glib_none().0,
                templ.to_glib_none().0,
                req_name.to_glib_none().0,
                caps.to_glib_none().0,
            ))
        }
    }

    fn parent_update_src_caps(
        &self,
        aggregator: &Self::Type,
        caps: &gst::Caps,
    ) -> Result<gst::Caps, gst::FlowError> {
        unsafe {
            let data = T::type_data();
            let parent_class = data.as_ref().get_parent_class() as *mut ffi::GstAggregatorClass;
            let f = (*parent_class)
                .update_src_caps
                .expect("Missing parent function `update_src_caps`");

            let mut out_caps = ptr::null_mut();
            gst::FlowReturn::from_glib(f(
                aggregator.unsafe_cast_ref::<Aggregator>().to_glib_none().0,
                caps.as_mut_ptr(),
                &mut out_caps,
            ))
            .into_result_value(|| from_glib_full(out_caps))
        }
    }

    fn parent_fixate_src_caps(&self, aggregator: &Self::Type, caps: gst::Caps) -> gst::Caps {
        unsafe {
            let data = T::type_data();
            let parent_class = data.as_ref().get_parent_class() as *mut ffi::GstAggregatorClass;

            let f = (*parent_class)
                .fixate_src_caps
                .expect("Missing parent function `fixate_src_caps`");
            from_glib_full(f(
                aggregator.unsafe_cast_ref::<Aggregator>().to_glib_none().0,
                caps.into_ptr(),
            ))
        }
    }

    fn parent_negotiated_src_caps(
        &self,
        aggregator: &Self::Type,
        caps: &gst::Caps,
    ) -> Result<(), gst::LoggableError> {
        unsafe {
            let data = T::type_data();
            let parent_class = data.as_ref().get_parent_class() as *mut ffi::GstAggregatorClass;
            (*parent_class)
                .negotiated_src_caps
                .map(|f| {
                    gst::result_from_gboolean!(
                        f(
                            aggregator.unsafe_cast_ref::<Aggregator>().to_glib_none().0,
                            caps.to_glib_none().0
                        ),
                        gst::CAT_RUST,
                        "Parent function `negotiated_src_caps` failed"
                    )
                })
                .unwrap_or(Ok(()))
        }
    }

    fn parent_negotiate(&self, aggregator: &Self::Type) -> bool {
        unsafe {
            let data = T::type_data();
            let parent_class = data.as_ref().get_parent_class() as *mut ffi::GstAggregatorClass;
            (*parent_class)
                .negotiate
                .map(|f| {
                    from_glib(f(aggregator
                        .unsafe_cast_ref::<Aggregator>()
                        .to_glib_none()
                        .0))
                })
                .unwrap_or(true)
        }
    }
}

unsafe impl<T: AggregatorImpl> IsSubclassable<T> for Aggregator
where
    <T as ObjectSubclass>::Instance: PanicPoison,
{
    fn override_vfuncs(klass: &mut glib::Class<Self>) {
        <gst::Element as IsSubclassable<T>>::override_vfuncs(klass);
        let klass = klass.as_mut();
        klass.flush = Some(aggregator_flush::<T>);
        klass.clip = Some(aggregator_clip::<T>);
        klass.finish_buffer = Some(aggregator_finish_buffer::<T>);
        klass.sink_event = Some(aggregator_sink_event::<T>);
        klass.sink_query = Some(aggregator_sink_query::<T>);
        klass.src_event = Some(aggregator_src_event::<T>);
        klass.src_query = Some(aggregator_src_query::<T>);
        klass.src_activate = Some(aggregator_src_activate::<T>);
        klass.aggregate = Some(aggregator_aggregate::<T>);
        klass.start = Some(aggregator_start::<T>);
        klass.stop = Some(aggregator_stop::<T>);
        klass.get_next_time = Some(aggregator_get_next_time::<T>);
        klass.create_new_pad = Some(aggregator_create_new_pad::<T>);
        klass.update_src_caps = Some(aggregator_update_src_caps::<T>);
        klass.fixate_src_caps = Some(aggregator_fixate_src_caps::<T>);
        klass.negotiated_src_caps = Some(aggregator_negotiated_src_caps::<T>);
        {
            klass.sink_event_pre_queue = Some(aggregator_sink_event_pre_queue::<T>);
            klass.sink_query_pre_queue = Some(aggregator_sink_query_pre_queue::<T>);
            klass.negotiate = Some(aggregator_negotiate::<T>);
        }
    }
}

unsafe extern "C" fn aggregator_flush<T: AggregatorImpl>(
    ptr: *mut ffi::GstAggregator,
) -> gst::ffi::GstFlowReturn
where
    T::Instance: PanicPoison,
{
    let instance = &*(ptr as *mut T::Instance);
    let imp = instance.get_impl();
    let wrap: Borrowed<Aggregator> = from_glib_borrow(ptr);

    gst::panic_to_error!(&wrap, &instance.panicked(), gst::FlowReturn::Error, {
        imp.flush(wrap.unsafe_cast_ref()).into()
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_clip<T: AggregatorImpl>(
    ptr: *mut ffi::GstAggregator,
    aggregator_pad: *mut ffi::GstAggregatorPad,
    buffer: *mut gst::ffi::GstBuffer,
) -> *mut gst::ffi::GstBuffer
where
    T::Instance: PanicPoison,
{
    let instance = &*(ptr as *mut T::Instance);
    let imp = instance.get_impl();
    let wrap: Borrowed<Aggregator> = from_glib_borrow(ptr);

    let ret = gst::panic_to_error!(&wrap, &instance.panicked(), None, {
        imp.clip(
            wrap.unsafe_cast_ref(),
            &from_glib_borrow(aggregator_pad),
            from_glib_full(buffer),
        )
    });

    ret.map(|r| r.into_ptr()).unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn aggregator_finish_buffer<T: AggregatorImpl>(
    ptr: *mut ffi::GstAggregator,
    buffer: *mut gst::ffi::GstBuffer,
) -> gst::ffi::GstFlowReturn
where
    T::Instance: PanicPoison,
{
    let instance = &*(ptr as *mut T::Instance);
    let imp = instance.get_impl();
    let wrap: Borrowed<Aggregator> = from_glib_borrow(ptr);

    gst::panic_to_error!(&wrap, &instance.panicked(), gst::FlowReturn::Error, {
        imp.finish_buffer(wrap.unsafe_cast_ref(), from_glib_full(buffer))
            .into()
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_sink_event<T: AggregatorImpl>(
    ptr: *mut ffi::GstAggregator,
    aggregator_pad: *mut ffi::GstAggregatorPad,
    event: *mut gst::ffi::GstEvent,
) -> glib::ffi::gboolean
where
    T::Instance: PanicPoison,
{
    let instance = &*(ptr as *mut T::Instance);
    let imp = instance.get_impl();
    let wrap: Borrowed<Aggregator> = from_glib_borrow(ptr);

    gst::panic_to_error!(wrap, &instance.panicked(), false, {
        imp.sink_event(
            wrap.unsafe_cast_ref(),
            &from_glib_borrow(aggregator_pad),
            from_glib_full(event),
        )
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_sink_event_pre_queue<T: AggregatorImpl>(
    ptr: *mut ffi::GstAggregator,
    aggregator_pad: *mut ffi::GstAggregatorPad,
    event: *mut gst::ffi::GstEvent,
) -> gst::ffi::GstFlowReturn
where
    T::Instance: PanicPoison,
{
    let instance = &*(ptr as *mut T::Instance);
    let imp = instance.get_impl();
    let wrap: Borrowed<Aggregator> = from_glib_borrow(ptr);

    gst::panic_to_error!(&wrap, &instance.panicked(), gst::FlowReturn::Error, {
        imp.sink_event_pre_queue(
            wrap.unsafe_cast_ref(),
            &from_glib_borrow(aggregator_pad),
            from_glib_full(event),
        )
        .into()
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_sink_query<T: AggregatorImpl>(
    ptr: *mut ffi::GstAggregator,
    aggregator_pad: *mut ffi::GstAggregatorPad,
    query: *mut gst::ffi::GstQuery,
) -> glib::ffi::gboolean
where
    T::Instance: PanicPoison,
{
    let instance = &*(ptr as *mut T::Instance);
    let imp = instance.get_impl();
    let wrap: Borrowed<Aggregator> = from_glib_borrow(ptr);

    gst::panic_to_error!(&wrap, &instance.panicked(), false, {
        imp.sink_query(
            wrap.unsafe_cast_ref(),
            &from_glib_borrow(aggregator_pad),
            gst::QueryRef::from_mut_ptr(query),
        )
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_sink_query_pre_queue<T: AggregatorImpl>(
    ptr: *mut ffi::GstAggregator,
    aggregator_pad: *mut ffi::GstAggregatorPad,
    query: *mut gst::ffi::GstQuery,
) -> glib::ffi::gboolean
where
    T::Instance: PanicPoison,
{
    let instance = &*(ptr as *mut T::Instance);
    let imp = instance.get_impl();
    let wrap: Borrowed<Aggregator> = from_glib_borrow(ptr);

    gst::panic_to_error!(&wrap, &instance.panicked(), false, {
        imp.sink_query_pre_queue(
            wrap.unsafe_cast_ref(),
            &from_glib_borrow(aggregator_pad),
            gst::QueryRef::from_mut_ptr(query),
        )
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_src_event<T: AggregatorImpl>(
    ptr: *mut ffi::GstAggregator,
    event: *mut gst::ffi::GstEvent,
) -> glib::ffi::gboolean
where
    T::Instance: PanicPoison,
{
    let instance = &*(ptr as *mut T::Instance);
    let imp = instance.get_impl();
    let wrap: Borrowed<Aggregator> = from_glib_borrow(ptr);

    gst::panic_to_error!(&wrap, &instance.panicked(), false, {
        imp.src_event(wrap.unsafe_cast_ref(), from_glib_full(event))
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_src_query<T: AggregatorImpl>(
    ptr: *mut ffi::GstAggregator,
    query: *mut gst::ffi::GstQuery,
) -> glib::ffi::gboolean
where
    T::Instance: PanicPoison,
{
    let instance = &*(ptr as *mut T::Instance);
    let imp = instance.get_impl();
    let wrap: Borrowed<Aggregator> = from_glib_borrow(ptr);

    gst::panic_to_error!(&wrap, &instance.panicked(), false, {
        imp.src_query(wrap.unsafe_cast_ref(), gst::QueryRef::from_mut_ptr(query))
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_src_activate<T: AggregatorImpl>(
    ptr: *mut ffi::GstAggregator,
    mode: gst::ffi::GstPadMode,
    active: glib::ffi::gboolean,
) -> glib::ffi::gboolean
where
    T::Instance: PanicPoison,
{
    let instance = &*(ptr as *mut T::Instance);
    let imp = instance.get_impl();
    let wrap: Borrowed<Aggregator> = from_glib_borrow(ptr);

    gst::panic_to_error!(&wrap, &instance.panicked(), false, {
        match imp.src_activate(wrap.unsafe_cast_ref(), from_glib(mode), from_glib(active)) {
            Ok(()) => true,
            Err(err) => {
                err.log_with_object(&*wrap);
                false
            }
        }
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_aggregate<T: AggregatorImpl>(
    ptr: *mut ffi::GstAggregator,
    timeout: glib::ffi::gboolean,
) -> gst::ffi::GstFlowReturn
where
    T::Instance: PanicPoison,
{
    let instance = &*(ptr as *mut T::Instance);
    let imp = instance.get_impl();
    let wrap: Borrowed<Aggregator> = from_glib_borrow(ptr);

    gst::panic_to_error!(&wrap, &instance.panicked(), gst::FlowReturn::Error, {
        imp.aggregate(wrap.unsafe_cast_ref(), from_glib(timeout))
            .into()
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_start<T: AggregatorImpl>(
    ptr: *mut ffi::GstAggregator,
) -> glib::ffi::gboolean
where
    T::Instance: PanicPoison,
{
    let instance = &*(ptr as *mut T::Instance);
    let imp = instance.get_impl();
    let wrap: Borrowed<Aggregator> = from_glib_borrow(ptr);

    gst::panic_to_error!(&wrap, &instance.panicked(), false, {
        match imp.start(wrap.unsafe_cast_ref()) {
            Ok(()) => true,
            Err(err) => {
                wrap.post_error_message(err);
                false
            }
        }
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_stop<T: AggregatorImpl>(
    ptr: *mut ffi::GstAggregator,
) -> glib::ffi::gboolean
where
    T::Instance: PanicPoison,
{
    let instance = &*(ptr as *mut T::Instance);
    let imp = instance.get_impl();
    let wrap: Borrowed<Aggregator> = from_glib_borrow(ptr);

    gst::panic_to_error!(&wrap, &instance.panicked(), false, {
        match imp.stop(wrap.unsafe_cast_ref()) {
            Ok(()) => true,
            Err(err) => {
                wrap.post_error_message(err);
                false
            }
        }
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_get_next_time<T: AggregatorImpl>(
    ptr: *mut ffi::GstAggregator,
) -> gst::ffi::GstClockTime
where
    T::Instance: PanicPoison,
{
    let instance = &*(ptr as *mut T::Instance);
    let imp = instance.get_impl();
    let wrap: Borrowed<Aggregator> = from_glib_borrow(ptr);

    gst::panic_to_error!(&wrap, &instance.panicked(), gst::CLOCK_TIME_NONE, {
        imp.get_next_time(wrap.unsafe_cast_ref())
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_create_new_pad<T: AggregatorImpl>(
    ptr: *mut ffi::GstAggregator,
    templ: *mut gst::ffi::GstPadTemplate,
    req_name: *const libc::c_char,
    caps: *const gst::ffi::GstCaps,
) -> *mut ffi::GstAggregatorPad
where
    T::Instance: PanicPoison,
{
    let instance = &*(ptr as *mut T::Instance);
    let imp = instance.get_impl();
    let wrap: Borrowed<Aggregator> = from_glib_borrow(ptr);

    gst::panic_to_error!(&wrap, &instance.panicked(), None, {
        let req_name: Borrowed<Option<glib::GString>> = from_glib_borrow(req_name);

        imp.create_new_pad(
            wrap.unsafe_cast_ref(),
            &from_glib_borrow(templ),
            req_name.as_ref().as_ref().map(|s| s.as_str()),
            Option::<gst::Caps>::from_glib_borrow(caps)
                .as_ref()
                .as_ref(),
        )
    })
    .to_glib_full()
}

unsafe extern "C" fn aggregator_update_src_caps<T: AggregatorImpl>(
    ptr: *mut ffi::GstAggregator,
    caps: *mut gst::ffi::GstCaps,
    res: *mut *mut gst::ffi::GstCaps,
) -> gst::ffi::GstFlowReturn
where
    T::Instance: PanicPoison,
{
    let instance = &*(ptr as *mut T::Instance);
    let imp = instance.get_impl();
    let wrap: Borrowed<Aggregator> = from_glib_borrow(ptr);

    *res = ptr::null_mut();

    gst::panic_to_error!(&wrap, &instance.panicked(), gst::FlowReturn::Error, {
        match imp.update_src_caps(wrap.unsafe_cast_ref(), &from_glib_borrow(caps)) {
            Ok(res_caps) => {
                *res = res_caps.into_ptr();
                gst::FlowReturn::Ok
            }
            Err(err) => err.into(),
        }
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_fixate_src_caps<T: AggregatorImpl>(
    ptr: *mut ffi::GstAggregator,
    caps: *mut gst::ffi::GstCaps,
) -> *mut gst::ffi::GstCaps
where
    T::Instance: PanicPoison,
{
    let instance = &*(ptr as *mut T::Instance);
    let imp = instance.get_impl();
    let wrap: Borrowed<Aggregator> = from_glib_borrow(ptr);

    gst::panic_to_error!(&wrap, &instance.panicked(), gst::Caps::new_empty(), {
        imp.fixate_src_caps(wrap.unsafe_cast_ref(), from_glib_full(caps))
    })
    .into_ptr()
}

unsafe extern "C" fn aggregator_negotiated_src_caps<T: AggregatorImpl>(
    ptr: *mut ffi::GstAggregator,
    caps: *mut gst::ffi::GstCaps,
) -> glib::ffi::gboolean
where
    T::Instance: PanicPoison,
{
    let instance = &*(ptr as *mut T::Instance);
    let imp = instance.get_impl();
    let wrap: Borrowed<Aggregator> = from_glib_borrow(ptr);

    gst::panic_to_error!(&wrap, &instance.panicked(), false, {
        match imp.negotiated_src_caps(wrap.unsafe_cast_ref(), &from_glib_borrow(caps)) {
            Ok(()) => true,
            Err(err) => {
                err.log_with_object(&*wrap);
                false
            }
        }
    })
    .to_glib()
}

unsafe extern "C" fn aggregator_negotiate<T: AggregatorImpl>(
    ptr: *mut ffi::GstAggregator,
) -> glib::ffi::gboolean
where
    T::Instance: PanicPoison,
{
    let instance = &*(ptr as *mut T::Instance);
    let imp = instance.get_impl();
    let wrap: Borrowed<Aggregator> = from_glib_borrow(ptr);

    gst::panic_to_error!(&wrap, &instance.panicked(), false, {
        imp.negotiate(wrap.unsafe_cast_ref())
    })
    .to_glib()
}
