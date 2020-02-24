// Copyright (C) 2019-2020 François Laignel <fengalin@free.fr>
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

//! An implementation of `Pad`s to run asynchronous processings.
//!
//! [`PadSink`] & [`PadSrc`] provide an asynchronous API to ease the development of `Element`s in
//! the `threadshare` GStreamer plugins framework.
//!
//! The diagram below shows how the [`PadSrc`] & [`PadSink`] and the related `struct`s integrate in
//! `ts` `Element`s.
//!
//! Note: [`PadSrc`] & [`PadSink`] only support `gst::PadMode::Push` at the moment.
//!
//! ```text
//!    ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓          ┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━
//!                    Element A               ┃          ┃          Element B
//!                                            ┃          ┃
//!                 ╭─────────────────╮        ┃          ┃       ╭──────────────────╮
//!                 │     PadSrc      │        ┃          ┃       │      PadSink     │
//!                 │     Handler     │        ┃          ┃       │      Handler     │
//!                 │─────────────────│        ┃          ┃       │──────────────────│
//!                 │ - src_activate* │     ╭──┸──╮    ╭──┸──╮    │ - sink_activate* │
//!                 │ - src_event*    │<────│     │<╌╌╌│     │───>│ - sink_chain*    │
//!                 │ - src_query     │<────│ gst │    │ gst │───>│ - sink_event*    │
//!                 │─────────────────│     │     │    │     │───>│ - sink_query     │
//!                 │ - task fn       │     │ Pad │    │ Pad │    ╰──────────────────╯
//!                 ╰─────────────────╯  ╭─>│     │╌╌╌>│     │─╮            │
//!           ╭───────╯      │           │  ╰──┰──╯    ╰──┰──╯ ╰───────╮    │
//!    ╭────────────╮   ╭────────╮ push* │     ┃          ┃          ╭─────────╮
//!    │ Pad Task ↺ │<──│ PadSrc │───────╯     ┃          ┃          │ PadSink │
//!    ╰────────────╯   ╰────────╯             ┃          ┃          ╰─────────╯
//!    ━━━━━━━━━━━━━━━━━━━━━━│━━━━━━━━━━━━━━━━━┛          ┗━━━━━━━━━━━━━━━│━━━━━━━━━━━━
//!                          ╰───────────────────╮      ╭─────────────────╯
//!                                          ╭──────────────╮
//!                                          │  PadContext  │
//!                                          │╭────────────╮│
//!                                          ││  Context ↺ ││
//!                                          ╰╰────────────╯╯
//! ```
//!
//! Asynchronous operations for both [`PadSrc`] in `Element A` and [`PadSink`] in `Element B` run on
//!  the same [`Context`], which can also be shared by other `Element`s or instances of the same
//! `Element`s in multiple `Pipeline`s.
//!
//! `Element A` & `Element B` can also be linked to non-threadshare `Element`s in which case, they
//! operate in a regular synchronous way.
//!
//! Note that only operations on the streaming thread (serialized events, buffers, serialized
//! queries) are handled from the `PadContext` and asynchronously, everything else operates
//! blocking.
//!
//! [`PadSink`]: struct.PadSink.html
//! [`PadSrc`]: struct.PadSrc.html
//! [`Context`]: ../executor/struct.Context.html

use either::Either;

use futures::future;
use futures::future::BoxFuture;
use futures::prelude::*;

use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_fixme, gst_log, gst_loggable_error, gst_warning};
use gst::{FlowError, FlowSuccess};

use std::fmt;
use std::marker::PhantomData;
use std::sync;
use std::sync::{Arc, Weak};

use super::executor::{self, Context, JoinHandle, TaskOutput};
use super::pad_context::{PadContext, PadContextWeak};
use super::task::Task;
use super::RUNTIME_CAT;

/// Errors related to [`PadSrc`] `Context` handling.
///
/// [`PadSrc`]: struct.PadSrc.html
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PadContextError {
    ActiveContext,
    ActiveTask,
}

impl fmt::Display for PadContextError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            PadContextError::ActiveContext => {
                write!(f, "The PadSrc is already operating on a Context")
            }
            PadContextError::ActiveTask => write!(f, "A task is still active"),
        }
    }
}

impl std::error::Error for PadContextError {}

#[inline]
fn event_ret_to_event_full_res(ret: bool, event: &gst::Event) -> Result<FlowSuccess, FlowError> {
    if ret {
        Ok(FlowSuccess::Ok)
    } else if let gst::EventView::Caps(_) = event.view() {
        Err(FlowError::NotNegotiated)
    } else {
        Err(FlowError::Error)
    }
}

#[inline]
fn event_to_event_full(
    ret: Either<bool, BoxFuture<'static, bool>>,
    event: gst::Event,
) -> Either<Result<FlowSuccess, FlowError>, BoxFuture<'static, Result<FlowSuccess, FlowError>>> {
    match ret {
        Either::Left(ret) => Either::Left(event_ret_to_event_full_res(ret, &event)),
        Either::Right(fut) => Either::Right(
            fut.map(move |ret| event_ret_to_event_full_res(ret, &event))
                .boxed(),
        ),
    }
}

/// A trait to define `handler`s for [`PadSrc`] callbacks.
///
/// *See the [`pad` module] documentation for a description of the model.*
///
/// [`PadSrc`]: struct.PadSrc.html
/// [`pad` module]: index.html
pub trait PadSrcHandler: Clone + Send + Sync + 'static {
    type ElementImpl: ElementImpl;

    fn src_activate(
        &self,
        pad: &PadSrcRef,
        _imp: &Self::ElementImpl,
        _element: &gst::Element,
    ) -> Result<(), gst::LoggableError> {
        let gst_pad = pad.gst_pad();
        if gst_pad.is_active() {
            gst_debug!(
                RUNTIME_CAT,
                obj: gst_pad,
                "Already activated in {:?} mode ",
                gst_pad.get_mode()
            );
            return Ok(());
        }

        gst_pad
            .activate_mode(gst::PadMode::Push, true)
            .map_err(|err| {
                gst_error!(
                    RUNTIME_CAT,
                    obj: gst_pad,
                    "Error in PadSink activate: {:?}",
                    err
                );
                gst_loggable_error!(RUNTIME_CAT, "Error in PadSink activate: {:?}", err)
            })
    }

    fn src_activatemode(
        &self,
        _pad: &PadSrcRef,
        _imp: &Self::ElementImpl,
        _element: &gst::Element,
        _mode: gst::PadMode,
        _active: bool,
    ) -> Result<(), gst::LoggableError> {
        Ok(())
    }

    fn src_event(
        &self,
        pad: &PadSrcRef,
        _imp: &Self::ElementImpl,
        element: &gst::Element,
        event: gst::Event,
    ) -> Either<bool, BoxFuture<'static, bool>> {
        if event.is_serialized() {
            let pad_weak = pad.downgrade();
            let element = element.clone();

            Either::Right(
                async move {
                    let pad = pad_weak.upgrade().expect("PadSrc no longer exists");
                    gst_log!(RUNTIME_CAT, obj: pad.gst_pad(), "Handling {:?}", event);

                    pad.gst_pad().event_default(Some(&element), event)
                }
                .boxed(),
            )
        } else {
            gst_log!(RUNTIME_CAT, obj: pad.gst_pad(), "Handling {:?}", event);
            Either::Left(pad.gst_pad().event_default(Some(element), event))
        }
    }

    fn src_event_full(
        &self,
        pad: &PadSrcRef,
        imp: &Self::ElementImpl,
        element: &gst::Element,
        event: gst::Event,
    ) -> Either<Result<FlowSuccess, FlowError>, BoxFuture<'static, Result<FlowSuccess, FlowError>>>
    {
        // default is to dispatch to `src_event`
        // (as implemented in `gst_pad_send_event_unchecked`)
        let event_clone = event.clone();
        event_to_event_full(self.src_event(pad, imp, element, event), event_clone)
    }

    fn src_query(
        &self,
        pad: &PadSrcRef,
        _imp: &Self::ElementImpl,
        element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        gst_log!(RUNTIME_CAT, obj: pad.gst_pad(), "Handling {:?}", query);
        if query.is_serialized() {
            // FIXME serialized queries should be handled with the dataflow
            // but we can't return a `Future` because we couldn't honor QueryRef's lifetime
            false
        } else {
            pad.gst_pad().query_default(Some(element), query)
        }
    }
}

#[derive(Default, Debug)]
pub struct PadSrcState {
    is_initialized: bool,
}

#[derive(Debug)]
struct PadSrcInner {
    state: sync::Mutex<PadSrcState>,
    gst_pad: gst::Pad,
    pad_context: sync::RwLock<Option<PadContext>>,
    task: Task,
}

impl PadSrcInner {
    fn new(gst_pad: gst::Pad) -> Self {
        if gst_pad.get_direction() != gst::PadDirection::Src {
            panic!("Wrong pad direction for PadSrc");
        }

        PadSrcInner {
            state: sync::Mutex::new(PadSrcState::default()),
            gst_pad,
            pad_context: sync::RwLock::new(None),
            task: Task::default(),
        }
    }

    fn has_pad_context(&self) -> bool {
        self.pad_context.read().unwrap().as_ref().is_some()
    }
}

impl Drop for PadSrcInner {
    fn drop(&mut self) {
        // Check invariant which can't be held automatically in `PadSrc`
        // because `drop` can't be `async`
        if self.has_pad_context() {
            panic!("Missing call to `PadSrc::unprepare`");
        }
    }
}

/// A [`PadSrc`] which can be moved in [`handler`]s functions and `Future`s.
///
/// Call [`upgrade`] to use the [`PadSrc`].
///
/// *See the [`pad` module] documentation for a description of the model.*
///
/// [`PadSrc`]: struct.PadSrc.html
/// [`handler`]: trait.PadSrcHandler.html
/// [`upgrade`]: struct.PadSrcWeak.html#method.upgrade
/// [`pad` module]: index.html
#[derive(Clone, Debug)]
pub struct PadSrcWeak(Weak<PadSrcInner>);

impl PadSrcWeak {
    pub fn upgrade(&self) -> Option<PadSrcRef<'_>> {
        self.0.upgrade().map(PadSrcRef::new)
    }
}

/// A [`PadSrc`] to be used in `Handler`s functions and `Future`s.
///
/// Call [`downgrade`] if you need to `clone` the [`PadSrc`].
///
/// *See the [`pad` module] documentation for a description of the model.*
///
/// [`PadSrc`]: struct.PadSrc.html
/// [`PadSrcWeak`]: struct.PadSrcWeak.html
/// [`downgrade`]: struct.PadSrcRef.html#method.downgrade
/// [`pad` module]: index.html
#[derive(Debug)]
pub struct PadSrcRef<'a> {
    strong: PadSrcStrong,
    phantom: PhantomData<&'a PadSrcStrong>,
}

impl<'a> PadSrcRef<'a> {
    fn new(inner_arc: Arc<PadSrcInner>) -> Self {
        PadSrcRef {
            strong: PadSrcStrong(inner_arc),
            phantom: PhantomData,
        }
    }

    pub fn gst_pad(&self) -> &gst::Pad {
        self.strong.gst_pad()
    }

    pub fn pad_context(&self) -> PadContextWeak {
        self.strong.pad_context()
    }

    /// Spawns `future` using current [`PadContext`].
    ///
    /// # Panics
    ///
    /// This function panics if the `PadSrc` is not prepared.
    ///
    /// [`PadContext`]: ../struct.PadContext.html
    pub fn spawn<Fut>(&self, future: Fut) -> JoinHandle<Fut::Output>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        self.strong.spawn(future)
    }

    pub fn downgrade(&self) -> PadSrcWeak {
        self.strong.downgrade()
    }

    pub async fn push(&self, buffer: gst::Buffer) -> Result<FlowSuccess, FlowError> {
        self.strong.push(buffer).await
    }

    pub async fn push_list(&self, list: gst::BufferList) -> Result<FlowSuccess, FlowError> {
        self.strong.push_list(list).await
    }

    pub async fn push_event(&self, event: gst::Event) -> bool {
        self.strong.push_event(event).await
    }

    /// `Start` the `Pad` `task`.
    ///
    /// The `Task` will loop on the provided `func`.
    /// The execution occurs on the `Task`'s context.
    pub fn start_task<F, Fut>(&self, func: F)
    where
        F: (FnMut() -> Fut) + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.strong.start_task(func);
    }

    /// Pauses the `Started` `Pad` `Task`.
    pub fn pause_task(&self) {
        self.strong.pause_task();
    }

    /// Stops the `Started` `Pad` `Task`.
    pub fn stop_task(&self) {
        self.strong.stop_task();
    }

    fn activate_mode_hook(
        &self,
        mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        // Important: don't panic here as the hook is used without `catch_panic_pad_function`
        // in the default `activatemode` handling
        gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "ActivateMode {:?}, {}", mode, active);

        if mode == gst::PadMode::Pull {
            gst_error!(RUNTIME_CAT, obj: self.gst_pad(), "Pull mode not supported by PadSrc");
            return Err(gst_loggable_error!(
                RUNTIME_CAT,
                "Pull mode not supported by PadSrc"
            ));
        }

        if !active {
            self.strong.state.lock().unwrap().is_initialized = false;
        }

        Ok(())
    }
}

#[derive(Debug)]
struct PadSrcStrong(Arc<PadSrcInner>);

impl PadSrcStrong {
    fn new(gst_pad: gst::Pad) -> Self {
        PadSrcStrong(Arc::new(PadSrcInner::new(gst_pad)))
    }

    #[inline]
    fn gst_pad(&self) -> &gst::Pad {
        &self.0.gst_pad
    }

    #[inline]
    fn pad_context_priv(&self) -> sync::RwLockReadGuard<'_, Option<PadContext>> {
        self.0.pad_context.read().unwrap()
    }

    #[inline]
    fn pad_context(&self) -> PadContextWeak {
        self.pad_context_priv()
            .as_ref()
            .expect("PadContext not initialized")
            .downgrade()
    }

    #[inline]
    fn spawn<Fut>(&self, future: Fut) -> JoinHandle<Fut::Output>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        let pad_ctx = self.pad_context_priv();
        pad_ctx
            .as_ref()
            .expect("PadContext not initialized")
            .spawn(future)
    }

    #[inline]
    fn downgrade(&self) -> PadSrcWeak {
        PadSrcWeak(Arc::downgrade(&self.0))
    }

    fn push_prelude(
        &self,
    ) -> Result<FlowSuccess, FlowError> {
        let state = self.state.lock().unwrap();
        if !state.is_initialized || self.gst_pad().check_reconfigure() {
            drop(state);
            if !self.push_pad_context_event() {
                return Err(FlowError::Error);
            }

            let mut state = self.state.lock().unwrap();
            if !state.is_initialized {
                // Get rid of reconfigure flag
                self.gst_pad().check_reconfigure();
                state.is_initialized = true;
            }
        }

        Ok(FlowSuccess::Ok)
    }

    #[inline]
    fn push_pad_context_event(&self) -> bool {
        let pad_ctx = self.pad_context_priv();
        let pad_ctx = pad_ctx.as_ref().unwrap();
        gst_log!(
            RUNTIME_CAT,
            obj: self.gst_pad(),
            "Pushing PadContext Event {}",
            pad_ctx,
        );

        let ret = self.gst_pad().push_event(pad_ctx.new_sticky_event());
        if !ret {
            gst_error!(RUNTIME_CAT,
                obj: self.gst_pad(),
                "Failed to push PadContext sticky event to PadSrc",
            );
        }

        ret
    }

    fn drain_pending_tasks(&self) -> Option<impl Future<Output = TaskOutput>> {
        self.pad_context_priv()
            .as_ref()
            .unwrap()
            .drain_pending_tasks()
    }

    #[inline]
    async fn push(&self, buffer: gst::Buffer) -> Result<FlowSuccess, FlowError> {
        gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "Pushing {:?}", buffer);

        self.push_prelude()?;

        let success = self.gst_pad().push(buffer).map_err(|err| {
            gst_error!(RUNTIME_CAT,
                obj: self.gst_pad(),
                "Failed to push Buffer to PadSrc: {:?}",
                err,
            );
            err
        })?;

        if let Some(pending_tasks) = self.drain_pending_tasks() {
            gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "Processing pending tasks (push)");
            pending_tasks.await?;
        }

        Ok(success)
    }

    #[inline]
    async fn push_list(&self, list: gst::BufferList) -> Result<FlowSuccess, FlowError> {
        gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "Pushing {:?}", list);

        self.push_prelude()?;

        let success = self.gst_pad().push_list(list).map_err(|err| {
            gst_error!(
                RUNTIME_CAT,
                obj: self.gst_pad(),
                "Failed to push BufferList to PadSrc: {:?} ({})",
                err,
                self.pad_context_priv().as_ref().unwrap(),
            );
            err
        })?;

        if let Some(pending_tasks) = self.drain_pending_tasks() {
            gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "Processing pending tasks (push_list)");
            pending_tasks.await?;
        }

        Ok(success)
    }

    #[inline]
    async fn push_event(&self, event: gst::Event) -> bool {
        let was_handled = if PadContext::is_pad_context_event(&event) {
            // Push our own PadContext
            if !self.push_pad_context_event() {
                return false;
            }

            // Get rid of reconfigure flag
            self.gst_pad().check_reconfigure();
            self.0.state.lock().unwrap().is_initialized = true;

            true
        } else {
            gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "Pushing {:?}", event);

            if self.push_prelude().is_err() {
                return false;
            }
            self.gst_pad().push_event(event)
        };

        if let Some(pending_tasks) = self.drain_pending_tasks() {
            gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "Processing pending tasks (push_event)");
            if pending_tasks.await.is_err() {
                return false;
            }
        }

        was_handled
    }

    #[inline]
    fn start_task<F, Fut>(&self, func: F)
    where
        F: (FnMut() -> Fut) + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.0.task.start(func);
    }

    #[inline]
    fn pause_task(&self) {
        self.0.task.pause();
    }

    #[inline]
    async fn stop_task(&self) {
        self.0.task.stop();
    }
}

/// The `PadSrc` which `Element`s must own.
///
/// Call [`downgrade`] if you need to `clone` the `PadSrc`.
///
/// *See the [`pad` module] documentation for a description of the model.*
///
/// [`downgrade`]: struct.PadSrc.html#method.downgrade
/// [`pad` module]: index.html
#[derive(Debug)]
pub struct PadSrc(PadSrcStrong);

impl PadSrc {
    pub fn new(gst_pad: gst::Pad) -> Self {
        let pad = gst_pad.clone();
        let this = PadSrc(PadSrcStrong::new(gst_pad));

        let this_weak = this.downgrade();
        pad.set_activatemode_function(move |gst_pad, _parent, mode, active| {
            // Important: don't panic here as we operate without `catch_panic_pad_function`
            // because we may not know which element the PadSrc is associated to yet
            this_weak
                .upgrade()
                .ok_or_else(|| {
                    gst_error!(RUNTIME_CAT, obj: gst_pad, "PadSrc no longer exists");
                    gst_loggable_error!(RUNTIME_CAT, "PadSrc no longer exists")
                })?
                .activate_mode_hook(mode, active)
        });

        this
    }

    pub fn new_from_template(templ: &gst::PadTemplate, name: Option<&str>) -> Self {
        Self::new(gst::Pad::new_from_template(templ, name))
    }

    pub fn as_ref(&self) -> PadSrcRef<'_> {
        PadSrcRef::new(Arc::clone(&(self.0).0))
    }

    pub fn gst_pad(&self) -> &gst::Pad {
        self.0.gst_pad()
    }

    pub fn downgrade(&self) -> PadSrcWeak {
        self.0.downgrade()
    }

    pub fn check_reconfigure(&self) -> bool {
        self.gst_pad().check_reconfigure()
    }

    pub fn pad_context(&self) -> PadContextWeak {
        self.0.pad_context()
    }

    /// Spawns `future` using current [`PadContext`].
    ///
    /// # Panics
    ///
    /// This function panics if the `PadSrc` is not prepared.
    ///
    /// [`PadContext`]: ../struct.PadContext.html
    pub fn spawn<Fut>(&self, future: Fut) -> JoinHandle<Fut::Output>
    where
        Fut: Future + Send + 'static,
        Fut::Output: Send + 'static,
    {
        self.0.spawn(future)
    }

    fn init_pad_functions<H: PadSrcHandler>(&self, handler: &H) {
        let handler_clone = handler.clone();
        let this_weak = self.downgrade();
        self.gst_pad()
            .set_activate_function(move |gst_pad, parent| {
                let handler = handler_clone.clone();
                let this_weak = this_weak.clone();
                H::ElementImpl::catch_panic_pad_function(
                    parent,
                    || {
                        gst_error!(RUNTIME_CAT, obj: gst_pad, "Panic in PadSrc activate");
                        Err(gst_loggable_error!(RUNTIME_CAT, "Panic in PadSrc activate"))
                    },
                    move |imp, element| {
                        let this_ref = this_weak.upgrade().expect("PadSrc no longer exists");
                        handler.src_activate(&this_ref, imp, element)
                    },
                )
            });

        let handler_clone = handler.clone();
        let this_weak = self.downgrade();
        self.gst_pad()
            .set_activatemode_function(move |gst_pad, parent, mode, active| {
                let handler = handler_clone.clone();
                let this_weak = this_weak.clone();
                H::ElementImpl::catch_panic_pad_function(
                    parent,
                    || {
                        gst_error!(RUNTIME_CAT, obj: gst_pad, "Panic in PadSrc activatemode");
                        Err(gst_loggable_error!(
                            RUNTIME_CAT,
                            "Panic in PadSrc activatemode"
                        ))
                    },
                    move |imp, element| {
                        let this_ref = this_weak.upgrade().expect("PadSrc no longer exists");
                        this_ref.activate_mode_hook(mode, active)?;
                        handler.src_activatemode(&this_ref, imp, element, mode, active)
                    },
                )
            });

        // No need to `set_event_function` since `set_event_full_function`
        // overrides it and dispatches to `src_event` when necessary
        let handler_clone = handler.clone();
        let this_weak = self.downgrade();
        self.gst_pad()
            .set_event_full_function(move |_gst_pad, parent, event| {
                let handler = handler_clone.clone();
                let this_weak = this_weak.clone();
                H::ElementImpl::catch_panic_pad_function(
                    parent,
                    || Err(FlowError::Error),
                    move |imp, element| {
                        let this_ref = this_weak.upgrade().expect("PadSrc no longer exists");
                        match handler.src_event_full(&this_ref, imp, &element, event) {
                            Either::Left(res) => res,
                            Either::Right(_fut) => {
                                // See these threads:
                                // https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/merge_requests/240#note_378446
                                // https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/merge_requests/240#note_378454
                                unimplemented!("Future handling in src_event*");
                            }
                        }
                    },
                )
            });

        let handler_clone = handler.clone();
        let this_weak = self.downgrade();
        self.gst_pad()
            .set_query_function(move |_gst_pad, parent, query| {
                let handler = handler_clone.clone();
                let this_weak = this_weak.clone();
                H::ElementImpl::catch_panic_pad_function(
                    parent,
                    || false,
                    move |imp, element| {
                        let this_ref = this_weak.upgrade().expect("PadSrc no longer exists");
                        if !query.is_serialized() {
                            handler.src_query(&this_ref, imp, &element, query)
                        } else {
                            gst_fixme!(RUNTIME_CAT, obj: this_ref.gst_pad(), "Serialized Query not supported");
                            false
                        }
                    },
                )
            });
    }

    pub fn prepare<H: PadSrcHandler>(
        &self,
        context: Context,
        handler: &H,
    ) -> Result<(), PadContextError> {
        let _state = self.state.lock().unwrap();
        gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "Preparing");

        if (self.0).0.has_pad_context() {
            return Err(PadContextError::ActiveContext);
        }

        (self.0)
            .0
            .task
            .prepare(context.clone())
            .map_err(|_| PadContextError::ActiveTask)?;

        *(self.0).0.pad_context.write().unwrap() = Some(PadContext::new(context.clone()));

        self.init_pad_functions(handler);

        Ok(())
    }

    /// Releases the resources held by this `PadSrc`.
    pub async fn unprepare(&self) -> Result<(), PadContextError> {
        let _state = self.lock_state().await;
        gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "Unpreparing");

        (self.0)
            .0
            .task
            .unprepare()
            .await
            .map_err(|_| PadContextError::ActiveTask)?;

        self.gst_pad()
            .set_activatemode_function(move |_gst_pad, _parent, _mode, _active| {
                Err(gst_loggable_error!(RUNTIME_CAT, "PadSrc unprepared"))
            });
        self.gst_pad()
            .set_event_function(move |_gst_pad, _parent, _event| false);
        self.gst_pad()
            .set_event_full_function(move |_gst_pad, _parent, _event| Err(FlowError::Error));
        self.gst_pad()
            .set_query_function(move |_gst_pad, _parent, _query| false);

        *(self.0).0.pad_context.write().unwrap() = None;

        Ok(())
    }

    pub async fn push(&self, buffer: gst::Buffer) -> Result<FlowSuccess, FlowError> {
        self.0.push(buffer).await
    }

    pub async fn push_list(&self, list: gst::BufferList) -> Result<FlowSuccess, FlowError> {
        self.0.push_list(list).await
    }

    pub async fn push_event(&self, event: gst::Event) -> bool {
        self.0.push_event(event).await
    }

    /// `Start` the `Pad` `task`.
    ///
    /// The `Task` will loop on the provided `func`.
    /// The execution occurs on the `Task`'s context.
    pub async fn start_task<F, Fut>(&self, func: F)
    where
        F: (FnMut() -> Fut) + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.0.start_task(func).await;
    }

    /// Pauses the `Started` `Pad` `task`.
    pub async fn pause_task(&self) -> BoxFuture<'static, ()> {
        self.0.pause_task().await
    }

    pub async fn stop_task(&self) {
        self.0.stop_task().await;
    }
}

/// A trait to define `handler`s for [`PadSink`] callbacks.
///
/// *See the [`pad` module] documentation for a description of the model.*
///
/// [`PadSink`]: struct.PadSink.html
/// [`pad` module]: index.html
pub trait PadSinkHandler: Clone + Send + Sync + 'static {
    type ElementImpl: ElementImpl;

    fn sink_activate(
        &self,
        pad: &PadSinkRef,
        _imp: &Self::ElementImpl,
        _element: &gst::Element,
    ) -> Result<(), gst::LoggableError> {
        let gst_pad = pad.gst_pad();
        if gst_pad.is_active() {
            gst_debug!(
                RUNTIME_CAT,
                obj: gst_pad,
                "Already activated in {:?} mode ",
                gst_pad.get_mode()
            );
            return Ok(());
        }

        gst_pad
            .activate_mode(gst::PadMode::Push, true)
            .map_err(|err| {
                gst_error!(
                    RUNTIME_CAT,
                    obj: gst_pad,
                    "Error in PadSink activate: {:?}",
                    err
                );
                gst_loggable_error!(RUNTIME_CAT, "Error in PadSink activate: {:?}", err)
            })
    }

    fn sink_activatemode(
        &self,
        _pad: &PadSinkRef,
        _imp: &Self::ElementImpl,
        _element: &gst::Element,
        _mode: gst::PadMode,
        _active: bool,
    ) -> Result<(), gst::LoggableError> {
        Ok(())
    }

    fn sink_chain(
        &self,
        _pad: &PadSinkRef,
        _imp: &Self::ElementImpl,
        _element: &gst::Element,
        _buffer: gst::Buffer,
    ) -> BoxFuture<'static, Result<FlowSuccess, FlowError>> {
        future::err(FlowError::NotSupported).boxed()
    }

    fn sink_chain_list(
        &self,
        _pad: &PadSinkRef,
        _imp: &Self::ElementImpl,
        _element: &gst::Element,
        _buffer_list: gst::BufferList,
    ) -> BoxFuture<'static, Result<FlowSuccess, FlowError>> {
        future::err(FlowError::NotSupported).boxed()
    }

    fn sink_event(
        &self,
        pad: &PadSinkRef,
        _imp: &Self::ElementImpl,
        element: &gst::Element,
        event: gst::Event,
    ) -> Either<bool, BoxFuture<'static, bool>> {
        if event.is_serialized() {
            let pad_weak = pad.downgrade();
            let element = element.clone();

            Either::Right(
                async move {
                    let pad = pad_weak.upgrade().expect("PadSink no longer exists");
                    gst_log!(RUNTIME_CAT, obj: pad.gst_pad(), "Handling {:?}", event);

                    pad.gst_pad().event_default(Some(&element), event)
                }
                .boxed(),
            )
        } else {
            gst_log!(RUNTIME_CAT, obj: pad.gst_pad(), "Handling {:?}", event);
            Either::Left(pad.gst_pad().event_default(Some(element), event))
        }
    }

    fn sink_event_full(
        &self,
        pad: &PadSinkRef,
        imp: &Self::ElementImpl,
        element: &gst::Element,
        event: gst::Event,
    ) -> Either<Result<FlowSuccess, FlowError>, BoxFuture<'static, Result<FlowSuccess, FlowError>>>
    {
        // default is to dispatch to `sink_event`
        // (as implemented in `gst_pad_send_event_unchecked`)
        let event_clone = event.clone();
        event_to_event_full(self.sink_event(pad, imp, element, event), event_clone)
    }

    fn sink_query(
        &self,
        pad: &PadSinkRef,
        _imp: &Self::ElementImpl,
        element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        gst_log!(RUNTIME_CAT, obj: pad.gst_pad(), "Handling {:?}", query);
        if query.is_serialized() {
            // FIXME serialized queries should be handled with the dataflow
            // but we can't return a `Future` because we couldn't honor QueryRef's lifetime
            false
        } else {
            pad.gst_pad().query_default(Some(element), query)
        }
    }
}

#[derive(Debug)]
struct PadSinkInner {
    gst_pad: gst::Pad,
    pad_context: sync::RwLock<Option<PadContextWeak>>,
}

impl PadSinkInner {
    fn new(gst_pad: gst::Pad) -> Self {
        if gst_pad.get_direction() != gst::PadDirection::Sink {
            panic!("Wrong pad direction for PadSink");
        }

        PadSinkInner {
            gst_pad,
            pad_context: sync::RwLock::new(None),
        }
    }
}

/// A [`PadSink`] which can be moved in `Handler`s functions and `Future`s.
///
/// Call [`upgrade`] to use the [`PadSink`].
///
/// *See the [`pad` module] documentation for a description of the model.*
///
/// [`PadSink`]: struct.PadSink.html
/// [`upgrade`]: struct.PadSinkWeak.html#method.upgrade
/// [`pad` module]: index.html
#[derive(Clone, Debug)]
pub struct PadSinkWeak(Weak<PadSinkInner>);

impl PadSinkWeak {
    pub fn upgrade(&self) -> Option<PadSinkRef<'_>> {
        self.0.upgrade().map(PadSinkRef::new)
    }
}

/// A [`PadSink`] to be used in [`handler`]s functions and `Future`s.
///
/// Call [`downgrade`] if you need to `clone` the [`PadSink`].
///
/// *See the [`pad` module] documentation for a description of the model.*
///
/// [`PadSink`]: struct.PadSink.html
/// [`handler`]: trait.PadSinkHandler.html
/// [`downgrade`]: struct.PadSinkRef.html#method.downgrade
/// [`pad` module]: index.html
#[derive(Debug)]
pub struct PadSinkRef<'a> {
    strong: PadSinkStrong,
    phantom: PhantomData<&'a PadSrcStrong>,
}

impl<'a> PadSinkRef<'a> {
    fn new(inner_arc: Arc<PadSinkInner>) -> Self {
        PadSinkRef {
            strong: PadSinkStrong(inner_arc),
            phantom: PhantomData,
        }
    }

    pub fn gst_pad(&self) -> &gst::Pad {
        self.strong.gst_pad()
    }

    pub fn pad_context(&self) -> Option<PadContextWeak> {
        self.strong.pad_context()
    }

    pub fn downgrade(&self) -> PadSinkWeak {
        self.strong.downgrade()
    }

    fn activate_mode_hook(
        &self,
        mode: gst::PadMode,
        active: bool,
    ) -> Result<(), gst::LoggableError> {
        // Important: don't panic here as the hook is used without `catch_panic_pad_function`
        // in the default `activatemode` handling
        gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "ActivateMode {:?}, {}", mode, active);

        if mode == gst::PadMode::Pull {
            gst_error!(RUNTIME_CAT, obj: self.gst_pad(), "Pull mode not supported by PadSink");
            return Err(gst_loggable_error!(
                RUNTIME_CAT,
                "Pull mode not supported by PadSink"
            ));
        }

        Ok(())
    }

    fn handle_future(
        &self,
        fut: impl Future<Output = Result<FlowSuccess, FlowError>> + Send + 'static,
    ) -> Result<FlowSuccess, FlowError> {
        if Context::is_context_thread() {
            match self.pad_context().as_ref() {
                Some(pad_ctx_weak) => {
                    pad_ctx_weak
                        .upgrade()
                        .expect("PadContext no longer exists")
                        .add_pending_task(fut.map(|res| res.map(drop)));
                    Ok(FlowSuccess::Ok)
                }
                None => {
                    // This can happen when an upstream element forwards the PadContext sticky event
                    // after the StreamStart event. While the upstream element should be fixed,
                    // we have no other solution but blocking the `Context`.
                    // See https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/issues/94
                    gst_warning!(
                        RUNTIME_CAT,
                        obj: self.gst_pad(),
                        "Operating on a Context without a PadContext. An upstream element should be fixed.",
                    );
                    // Note: we don't use `crate::runtime::executor::block_on` here
                    // because `Context::is_context_thread()` is checked in the `if`
                    // statement above.
                    futures::executor::block_on(fut)
                }
            }
        } else {
            // Not on a context thread: execute the Future immediately.
            //
            // - If there is no PadContext, we don't have any other options.
            // - If there is a PadContext, it means that we received it from
            //   an upstream element, but there is at least one non-ts element
            //   operating on another thread in between, so we can't take
            //   advantage of the task queue.
            //
            // Note: we don't use `crate::runtime::executor::block_on` here
            // because `Context::is_context_thread()` is checked in the `if`
            // statement above.
            futures::executor::block_on(fut)
        }
    }
}

#[derive(Debug)]
struct PadSinkStrong(Arc<PadSinkInner>);

impl PadSinkStrong {
    fn new(gst_pad: gst::Pad) -> Self {
        PadSinkStrong(Arc::new(PadSinkInner::new(gst_pad)))
    }

    fn gst_pad(&self) -> &gst::Pad {
        &self.0.gst_pad
    }

    fn pad_context(&self) -> Option<PadContextWeak> {
        self.0.pad_context.read().unwrap().clone()
    }

    fn downgrade(&self) -> PadSinkWeak {
        PadSinkWeak(Arc::downgrade(&self.0))
    }
}

/// The `PadSink` which `Element`s must own.
///
/// Call [`downgrade`] if you need to `clone` the `PadSink`.
///
/// *See the [`pad` module] documentation for a description of the model.*
///
/// [`downgrade`]: struct.PadSink.html#method.downgrade
/// [`pad` module]: index.html
#[derive(Debug)]
pub struct PadSink(PadSinkStrong);

impl PadSink {
    pub fn new(gst_pad: gst::Pad) -> Self {
        let pad = gst_pad.clone();
        let this = PadSink(PadSinkStrong::new(gst_pad));

        let this_weak = this.downgrade();
        pad.set_activatemode_function(move |gst_pad, _parent, mode, active| {
            // Important: don't panic here as we operate without `catch_panic_pad_function`
            // because we may not know which element the PadSrc is associated to yet
            this_weak
                .upgrade()
                .ok_or_else(|| {
                    gst_error!(RUNTIME_CAT, obj: gst_pad, "PadSink no longer exists");
                    gst_loggable_error!(RUNTIME_CAT, "PadSink no longer exists")
                })?
                .activate_mode_hook(mode, active)
        });

        this
    }

    pub fn new_from_template(templ: &gst::PadTemplate, name: Option<&str>) -> Self {
        Self::new(gst::Pad::new_from_template(templ, name))
    }

    pub fn gst_pad(&self) -> &gst::Pad {
        self.0.gst_pad()
    }

    pub fn pad_context(&self) -> Option<PadContextWeak> {
        self.0.pad_context()
    }

    pub fn downgrade(&self) -> PadSinkWeak {
        self.0.downgrade()
    }

    fn init_pad_functions<H: PadSinkHandler>(&self, handler: &H) {
        let handler_clone = handler.clone();
        let this_weak = self.downgrade();
        self.gst_pad()
            .set_activate_function(move |gst_pad, parent| {
                let handler = handler_clone.clone();
                let this_weak = this_weak.clone();
                H::ElementImpl::catch_panic_pad_function(
                    parent,
                    || {
                        gst_error!(RUNTIME_CAT, obj: gst_pad, "Panic in PadSink activate");
                        Err(gst_loggable_error!(
                            RUNTIME_CAT,
                            "Panic in PadSink activate"
                        ))
                    },
                    move |imp, element| {
                        let this_ref = this_weak.upgrade().expect("PadSink no longer exists");
                        handler.sink_activate(&this_ref, imp, element)
                    },
                )
            });

        let handler_clone = handler.clone();
        let this_weak = self.downgrade();
        self.gst_pad()
            .set_activatemode_function(move |gst_pad, parent, mode, active| {
                let handler = handler_clone.clone();
                let this_weak = this_weak.clone();
                H::ElementImpl::catch_panic_pad_function(
                    parent,
                    || {
                        gst_error!(RUNTIME_CAT, obj: gst_pad, "Panic in PadSink activatemode");
                        Err(gst_loggable_error!(
                            RUNTIME_CAT,
                            "Panic in PadSink activatemode"
                        ))
                    },
                    move |imp, element| {
                        let this_ref = this_weak.upgrade().expect("PadSink no longer exists");
                        this_ref.activate_mode_hook(mode, active)?;

                        handler.sink_activatemode(&this_ref, imp, element, mode, active)
                    },
                )
            });

        let handler_clone = handler.clone();
        let this_weak = self.downgrade();
        self.gst_pad()
            .set_chain_function(move |_gst_pad, parent, buffer| {
                let handler = handler_clone.clone();
                let this_weak = this_weak.clone();
                H::ElementImpl::catch_panic_pad_function(
                    parent,
                    || Err(FlowError::Error),
                    move |imp, element| {
                        let this_ref = this_weak.upgrade().expect("PadSink no longer exists");
                        let chain_fut = handler.sink_chain(&this_ref, imp, &element, buffer);
                        this_ref.handle_future(chain_fut)
                    },
                )
            });

        let handler_clone = handler.clone();
        let this_weak = self.downgrade();
        self.gst_pad()
            .set_chain_list_function(move |_gst_pad, parent, list| {
                let handler = handler_clone.clone();
                let this_weak = this_weak.clone();
                H::ElementImpl::catch_panic_pad_function(
                    parent,
                    || Err(FlowError::Error),
                    move |imp, element| {
                        let this_ref = this_weak.upgrade().expect("PadSink no longer exists");
                        let chain_list_fut =
                            handler.sink_chain_list(&this_ref, imp, &element, list);
                        this_ref.handle_future(chain_list_fut)
                    },
                )
            });

        // No need to `set_event_function` since `set_event_full_function`
        // overrides it and dispatches to `sink_event` when necessary
        let handler_clone = handler.clone();
        let this_weak = self.downgrade();
        self.gst_pad()
            .set_event_full_function(move |gst_pad, parent, event| {
                let handler = handler_clone.clone();
                let this_weak = this_weak.clone();
                H::ElementImpl::catch_panic_pad_function(
                    parent,
                    || Err(FlowError::Error),
                    move |imp, element| {
                        let this_ref = this_weak.upgrade().expect("PadSink no longer exists");
                        if let Some(received_pc) = PadContext::check_pad_context_event(&event) {
                            gst_log!(RUNTIME_CAT, obj: gst_pad, "Received {:?}", received_pc);
                            *this_ref.strong.0.pad_context.write().unwrap() = Some(received_pc);
                        }

                        match handler.sink_event_full(&this_ref, imp, &element, event) {
                            Either::Left(ret) => ret,
                            Either::Right(fut) => this_ref.handle_future(fut),
                        }
                    },
                )
            });

        let handler_clone = handler.clone();
        let this_weak = self.downgrade();
        self.gst_pad()
            .set_query_function(move |_gst_pad, parent, query| {
                let handler = handler_clone.clone();
                let this_weak = this_weak.clone();
                H::ElementImpl::catch_panic_pad_function(
                    parent,
                    || false,
                    move |imp, element| {
                        let this_ref = this_weak.upgrade().expect("PadSink no longer exists");
                        if !query.is_serialized() {
                            handler.sink_query(&this_ref, imp, &element, query)
                        } else {
                            gst_fixme!(RUNTIME_CAT, obj: this_ref.gst_pad(), "Serialized Query not supported");
                            false
                        }
                    },
                )
            });
    }

    pub async fn prepare<H: PadSinkHandler>(&self, handler: &H) {
        gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "Preparing");
        self.init_pad_functions(handler);
    }

    /// Releases the resources held by this `PadSink`.
    pub async fn unprepare(&self) {
        gst_log!(RUNTIME_CAT, obj: self.gst_pad(), "Unpreparing");

        self.gst_pad()
            .set_chain_function(move |_gst_pad, _parent, _buffer| Err(FlowError::Error));
        self.gst_pad()
            .set_chain_list_function(move |_gst_pad, _parent, _list| Err(FlowError::Error));
        self.gst_pad()
            .set_event_function(move |_gst_pad, _parent, _event| false);
        self.gst_pad()
            .set_event_full_function(move |_gst_pad, _parent, _event| Err(FlowError::Error));
        self.gst_pad()
            .set_query_function(move |_gst_pad, _parent, _query| false);
    }
}
