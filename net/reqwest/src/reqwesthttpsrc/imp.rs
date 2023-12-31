// Copyright (C) 2016-2018 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::u64;

use futures::future;
use futures::prelude::*;
use reqwest::{Client, Response, StatusCode};
use tokio::runtime;
use url::Url;

use once_cell::sync::Lazy;

use glib::subclass;
use glib::subclass::prelude::*;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_trace, gst_warning};
use gst_base::prelude::*;
use gst_base::subclass::prelude::*;

const DEFAULT_LOCATION: Option<Url> = None;
const DEFAULT_USER_AGENT: &str = concat!(
    "GStreamer reqwesthttpsrc ",
    env!("CARGO_PKG_VERSION"),
    "-",
    env!("COMMIT_ID")
);
const DEFAULT_IS_LIVE: bool = false;
const DEFAULT_TIMEOUT: u32 = 15;
const DEFAULT_COMPRESS: bool = false;
const DEFAULT_IRADIO_MODE: bool = true;
const DEFAULT_KEEP_ALIVE: bool = true;

#[derive(Debug, Clone)]
struct Settings {
    location: Option<Url>,
    user_agent: String,
    user_id: Option<String>,
    user_pw: Option<String>,
    timeout: u32,
    compress: bool,
    extra_headers: Option<gst::Structure>,
    cookies: Vec<String>,
    iradio_mode: bool,
    keep_alive: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            location: DEFAULT_LOCATION,
            user_agent: DEFAULT_USER_AGENT.into(),
            user_id: None,
            user_pw: None,
            timeout: DEFAULT_TIMEOUT,
            compress: DEFAULT_COMPRESS,
            extra_headers: None,
            cookies: Vec::new(),
            iradio_mode: DEFAULT_IRADIO_MODE,
            keep_alive: DEFAULT_KEEP_ALIVE,
        }
    }
}

static PROPERTIES: [subclass::Property; 11] = [
    subclass::Property("location", |name| {
        glib::ParamSpec::string(
            name,
            "Location",
            "URL to read from",
            None,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("user-agent", |name| {
        glib::ParamSpec::string(
            name,
            "User-Agent",
            "Value of the User-Agent HTTP request header field",
            DEFAULT_USER_AGENT.into(),
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("is-live", |name| {
        glib::ParamSpec::boolean(
            name,
            "Is Live",
            "Act like a live source",
            DEFAULT_IS_LIVE,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("user-id", |name| {
        glib::ParamSpec::string(
            name,
            "User-id",
            "HTTP location URI user id for authentication",
            None,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("user-pw", |name| {
        glib::ParamSpec::string(
            name,
            "User-pw",
            "HTTP location URI user password for authentication",
            None,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("timeout", |name| {
        glib::ParamSpec::uint(
            name,
            "Timeout",
            "Value in seconds to timeout a blocking I/O (0 = No timeout).",
            0,
            3600,
            DEFAULT_TIMEOUT,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("compress", |name| {
        glib::ParamSpec::boolean(
            name,
            "Compress",
            "Allow compressed content encodings",
            DEFAULT_COMPRESS,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("extra-headers", |name| {
        glib::ParamSpec::boxed(
            name,
            "Extra Headers",
            "Extra headers to append to the HTTP request",
            gst::Structure::static_type(),
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("cookies", |name| {
        glib::ParamSpec::boxed(
            name,
            "Cookies",
            "HTTP request cookies",
            Vec::<String>::static_type(),
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("iradio-mode", |name| {
        glib::ParamSpec::boolean(
            name,
            "I-Radio Mode",
            "Enable internet radio mode (ask server to send shoutcast/icecast metadata interleaved with the actual stream data",
            DEFAULT_IRADIO_MODE,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("keep-alive", |name| {
        glib::ParamSpec::boolean(
            name,
            "Keep Alive",
            "Use HTTP persistent connections",
            DEFAULT_KEEP_ALIVE,
            glib::ParamFlags::READWRITE,
        )
    }),
];

const REQWEST_CLIENT_CONTEXT: &str = "gst.reqwest.client";

#[derive(Clone, Debug, glib::GBoxed)]
#[gboxed(type_name = "ReqwestClientContext")]
struct ClientContext(Arc<ClientContextInner>);

#[derive(Debug)]
struct ClientContextInner {
    client: Client,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum State {
    Stopped,
    Started {
        uri: Url,
        response: Option<Response>,
        seekable: bool,
        position: u64,
        size: Option<u64>,
        start: u64,
        stop: Option<u64>,
        caps: Option<gst::Caps>,
        tags: Option<gst::TagList>,
    },
}

impl Default for State {
    fn default() -> Self {
        State::Stopped
    }
}

#[derive(Debug)]
pub struct ReqwestHttpSrc {
    client: Mutex<Option<ClientContext>>,
    external_client: Mutex<Option<ClientContext>>,
    settings: Mutex<Settings>,
    state: Mutex<State>,
    canceller: Mutex<Option<future::AbortHandle>>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "reqwesthttpsrc",
        gst::DebugColorFlags::empty(),
        Some("Rust HTTP source"),
    )
});

static RUNTIME: Lazy<runtime::Runtime> = Lazy::new(|| {
    runtime::Builder::new()
        .threaded_scheduler()
        .enable_all()
        .core_threads(1)
        .build()
        .unwrap()
});

impl ReqwestHttpSrc {
    fn set_location(
        &self,
        _element: &super::ReqwestHttpSrc,
        uri: Option<&str>,
    ) -> Result<(), glib::Error> {
        let state = self.state.lock().unwrap();
        if let State::Started { .. } = *state {
            return Err(glib::Error::new(
                gst::URIError::BadState,
                "Changing the `location` property on a started `reqwesthttpsrc` is not supported",
            ));
        }

        let mut settings = self.settings.lock().unwrap();

        if uri.is_none() {
            settings.location = DEFAULT_LOCATION;
            return Ok(());
        }

        let uri = uri.unwrap();
        let uri = Url::parse(uri).map_err(|err| {
            glib::Error::new(
                gst::URIError::BadUri,
                format!("Failed to parse URI '{}': {:?}", uri, err).as_str(),
            )
        })?;

        if uri.scheme() != "http" && uri.scheme() != "https" {
            return Err(glib::Error::new(
                gst::URIError::UnsupportedProtocol,
                format!("Unsupported URI scheme '{}'", uri.scheme()).as_str(),
            ));
        }

        settings.location = Some(uri);

        Ok(())
    }

    fn ensure_client(
        &self,
        src: &super::ReqwestHttpSrc,
    ) -> Result<ClientContext, gst::ErrorMessage> {
        let mut client_guard = self.client.lock().unwrap();
        if let Some(ref client) = *client_guard {
            gst_debug!(CAT, obj: src, "Using already configured client");
            return Ok(client.clone());
        }

        let srcpad = src.get_static_pad("src").unwrap();
        let mut q = gst::query::Context::new(REQWEST_CLIENT_CONTEXT);
        if srcpad.peer_query(&mut q) {
            if let Some(context) = q.get_context_owned() {
                src.set_context(&context);
            }
        } else {
            let _ = src.post_message(
                gst::message::NeedContext::builder(REQWEST_CLIENT_CONTEXT)
                    .src(src)
                    .build(),
            );
        }

        if let Some(client) = {
            // FIXME: Is there a simpler way to ensure the lock is not hold
            // after this block anymore?
            let external_client = self.external_client.lock().unwrap();
            let client = external_client.as_ref().cloned();
            drop(external_client);
            client
        } {
            gst_debug!(CAT, obj: src, "Using shared client");
            *client_guard = Some(client.clone());

            return Ok(client);
        }

        gst_debug!(CAT, obj: src, "Creating new client");
        let client = ClientContext(Arc::new(ClientContextInner {
            client: Client::builder()
                .cookie_store(true)
                .gzip(true)
                .build()
                .map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to create Client: {}", err]
                    )
                })?,
        }));

        gst_debug!(CAT, obj: src, "Sharing new client with other elements");
        let mut context = gst::Context::new(REQWEST_CLIENT_CONTEXT, true);
        {
            let context = context.get_mut().unwrap();
            let s = context.get_mut_structure();
            s.set("client", &client);
        }
        src.set_context(&context);
        let _ = src.post_message(gst::message::HaveContext::builder(context).src(src).build());

        *client_guard = Some(client.clone());

        Ok(client)
    }

    fn do_request(
        &self,
        src: &super::ReqwestHttpSrc,
        uri: Url,
        start: u64,
        stop: Option<u64>,
    ) -> Result<State, Option<gst::ErrorMessage>> {
        use hyperx::header::{
            qitem, AcceptEncoding, AcceptRanges, ByteRangeSpec, Connection, ContentLength,
            ContentRange, ContentRangeSpec, ContentType, Cookie, Encoding, Range, RangeUnit,
            TypedHeaders, UserAgent,
        };
        use reqwest::header::HeaderMap;

        gst_debug!(CAT, obj: src, "Creating new request for {}", uri);

        let req = {
            let client = self.ensure_client(src)?;
            client.0.client.get(uri.clone())
        };
        let settings = self.settings.lock().unwrap().clone();

        let mut headers = HeaderMap::new();

        if settings.keep_alive {
            headers.encode(&Connection::keep_alive());
        } else {
            headers.encode(&Connection::close());
        }

        match (start != 0, stop) {
            (false, None) => (),
            (true, None) => {
                headers.encode(&Range::Bytes(vec![ByteRangeSpec::AllFrom(start)]));
            }
            (_, Some(stop)) => {
                headers.encode(&Range::Bytes(vec![ByteRangeSpec::FromTo(start, stop - 1)]));
            }
        }

        headers.encode(&UserAgent::new(settings.user_agent));

        if !settings.compress {
            // Compression is the default
            headers.encode(&AcceptEncoding(vec![qitem(Encoding::Identity)]));
        };

        if let Some(ref extra_headers) = settings.extra_headers {
            use reqwest::header::{HeaderName, HeaderValue};
            use std::convert::TryFrom;

            for (field, value) in extra_headers.iter() {
                let field = match HeaderName::try_from(field) {
                    Ok(field) => field,
                    Err(err) => {
                        gst_warning!(
                            CAT,
                            obj: src,
                            "Failed to transform extra-header field name '{}' to header name: {}",
                            field,
                            err,
                        );

                        continue;
                    }
                };

                let mut append_header = |field: &HeaderName, value: &glib::Value| {
                    let value = match value.transform::<String>() {
                        Some(value) => value,
                        None => {
                            gst_warning!(
                                CAT,
                                obj: src,
                                "Failed to transform extra-header '{}' value to string",
                                field
                            );
                            return;
                        }
                    };

                    let value = value.get::<&str>().unwrap().unwrap_or("");

                    let value = match HeaderValue::from_str(value) {
                        Ok(value) => value,
                        Err(_) => {
                            gst_warning!(
                                CAT,
                                obj: src,
                                "Failed to transform extra-header '{}' value to header value",
                                field
                            );
                            return;
                        }
                    };

                    headers.append(field.clone(), value);
                };

                if let Ok(Some(values)) = value.get::<gst::Array>() {
                    for value in values.as_slice() {
                        append_header(&field, value);
                    }
                } else if let Ok(Some(values)) = value.get::<gst::List>() {
                    for value in values.as_slice() {
                        append_header(&field, value);
                    }
                } else {
                    append_header(&field, value);
                }
            }
        }

        if !settings.cookies.is_empty() {
            let mut cookies = Cookie::new();
            for cookie in settings.cookies {
                let mut split = cookie.splitn(2, '=');
                let key = split.next();
                let value = split.next();
                if let (Some(key), Some(value)) = (key, value) {
                    cookies.append(String::from(key), String::from(value));
                }
            }
            headers.encode(&cookies);
        }

        if settings.iradio_mode {
            headers.append("icy-metadata", "1".parse().unwrap());
        }

        // Add all headers for the request here
        let req = req.headers(headers);

        let req = if let Some(ref user_id) = settings.user_id {
            // HTTP auth available
            req.basic_auth(user_id, settings.user_pw)
        } else {
            req
        };

        gst_debug!(CAT, obj: src, "Sending new request: {:?}", req);

        let future = async {
            req.send().await.map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to fetch {}: {:?}", uri, err]
                )
            })
        };
        let res = self.wait(future);

        let res = match res {
            Ok(res) => res,
            Err(Some(err)) => {
                gst_debug!(CAT, obj: src, "Error {:?}", err);
                return Err(Some(err));
            }
            Err(None) => {
                gst_debug!(CAT, obj: src, "Flushing");
                return Err(None);
            }
        };

        gst_debug!(CAT, obj: src, "Received response: {:?}", res);

        if !res.status().is_success() {
            match res.status() {
                StatusCode::NOT_FOUND => {
                    gst_error!(CAT, obj: src, "Resource not found");
                    return Err(Some(gst::error_msg!(
                        gst::ResourceError::NotFound,
                        ["Resource '{}' not found", uri]
                    )));
                }
                StatusCode::UNAUTHORIZED
                | StatusCode::PAYMENT_REQUIRED
                | StatusCode::FORBIDDEN
                | StatusCode::PROXY_AUTHENTICATION_REQUIRED => {
                    gst_error!(CAT, obj: src, "Not authorized: {}", res.status());
                    return Err(Some(gst::error_msg!(
                        gst::ResourceError::NotAuthorized,
                        ["Not Authorized for resource '{}': {}", uri, res.status()]
                    )));
                }
                _ => {
                    gst_error!(CAT, obj: src, "Request failed: {}", res.status());
                    return Err(Some(gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Request for '{}' failed: {}", uri, res.status()]
                    )));
                }
            }
        }

        let headers = res.headers();
        let size = headers.decode().map(|ContentLength(cl)| cl + start).ok();

        let accept_byte_ranges = if let Ok(AcceptRanges(ref ranges)) = headers.decode() {
            ranges.iter().any(|u| *u == RangeUnit::Bytes)
        } else {
            false
        };
        let seekable = size.is_some() && accept_byte_ranges;

        let position = if let Ok(ContentRange(ContentRangeSpec::Bytes {
            range: Some((range_start, _)),
            ..
        })) = headers.decode()
        {
            range_start
        } else {
            0
        };

        if position != start {
            return Err(Some(gst::error_msg!(
                gst::ResourceError::Seek,
                ["Failed to seek to {}: Got {}", start, position]
            )));
        }

        let mut caps = headers
            .get("icy-metaint")
            .and_then(|s| s.to_str().ok())
            .and_then(|s| s.parse::<i32>().ok())
            .map(|icy_metaint| {
                gst::Caps::builder("application/x-icy")
                    .field("metadata-interval", &icy_metaint)
                    .build()
            });

        if let Ok(ContentType(ref content_type)) = headers.decode() {
            gst_debug!(CAT, obj: src, "Got content type {}", content_type);
            if let Some(ref mut caps) = caps {
                let caps = caps.get_mut().unwrap();
                let s = caps.get_mut_structure(0).unwrap();
                s.set("content-type", &content_type.as_ref());
            } else if content_type.type_() == "audio" && content_type.subtype() == "L16" {
                let channels = content_type
                    .get_param("channels")
                    .and_then(|s| s.as_ref().parse::<i32>().ok())
                    .unwrap_or(2);
                let rate = content_type
                    .get_param("rate")
                    .and_then(|s| s.as_ref().parse::<i32>().ok())
                    .unwrap_or(44_100);

                caps = Some(
                    gst::Caps::builder("audio/x-unaligned-raw")
                        .field("format", &"S16BE")
                        .field("layout", &"interleaved")
                        .field("channels", &channels)
                        .field("rate", &rate)
                        .build(),
                );
            }
        }

        let mut tags = gst::TagList::new();
        {
            let tags = tags.get_mut().unwrap();

            if let Some(ref icy_name) = headers.get("icy-name").and_then(|s| s.to_str().ok()) {
                tags.add::<gst::tags::Organization>(icy_name, gst::TagMergeMode::Replace);
            }

            if let Some(ref icy_genre) = headers.get("icy-genre").and_then(|s| s.to_str().ok()) {
                tags.add::<gst::tags::Genre>(icy_genre, gst::TagMergeMode::Replace);
            }

            if let Some(ref icy_url) = headers.get("icy-url").and_then(|s| s.to_str().ok()) {
                tags.add::<gst::tags::Location>(icy_url, gst::TagMergeMode::Replace);
            }
        }

        gst_debug!(CAT, obj: src, "Request successful");

        Ok(State::Started {
            uri,
            response: Some(res),
            seekable,
            position,
            size,
            start,
            stop,
            caps,
            tags: if tags.n_tags() > 0 { Some(tags) } else { None },
        })
    }

    fn wait<F, T>(&self, future: F) -> Result<T, Option<gst::ErrorMessage>>
    where
        F: Send + Future<Output = Result<T, gst::ErrorMessage>>,
        T: Send + 'static,
    {
        let timeout = self.settings.lock().unwrap().timeout;

        let mut canceller = self.canceller.lock().unwrap();
        let (abort_handle, abort_registration) = future::AbortHandle::new_pair();
        canceller.replace(abort_handle);
        drop(canceller);

        // Wrap in a timeout
        let future = async {
            if timeout == 0 {
                future.await
            } else {
                let res = tokio::time::timeout(Duration::from_secs(timeout.into()), future).await;

                match res {
                    Ok(res) => res,
                    Err(_) => Err(gst::error_msg!(
                        gst::ResourceError::Read,
                        ["Request timeout"]
                    )),
                }
            }
        };

        // And make abortable
        let future = async {
            match future::Abortable::new(future, abort_registration).await {
                Ok(res) => res.map_err(Some),
                Err(_) => Err(None),
            }
        };

        let res = RUNTIME.enter(|| futures::executor::block_on(future));

        /* Clear out the canceller */
        let _ = self.canceller.lock().unwrap().take();

        res
    }
}

impl ObjectImpl for ReqwestHttpSrc {
    fn set_property(&self, obj: &Self::Type, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];
        match *prop {
            subclass::Property("location", ..) => {
                let location = value.get::<&str>().expect("type checked upstream");
                if let Err(err) = self.set_location(obj, location) {
                    gst_error!(
                        CAT,
                        obj: obj,
                        "Failed to set property `location`: {:?}",
                        err
                    );
                }
            }
            subclass::Property("user-agent", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let user_agent = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| DEFAULT_USER_AGENT.into());
                settings.user_agent = user_agent;
            }
            subclass::Property("is-live", ..) => {
                let is_live = value.get_some().expect("type checked upstream");
                obj.set_live(is_live);
            }
            subclass::Property("user-id", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let user_id = value.get().expect("type checked upstream");
                settings.user_id = user_id;
            }
            subclass::Property("user-pw", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let user_pw = value.get().expect("type checked upstream");
                settings.user_pw = user_pw;
            }
            subclass::Property("timeout", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let timeout = value.get_some().expect("type checked upstream");
                settings.timeout = timeout;
            }
            subclass::Property("compress", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let compress = value.get_some().expect("type checked upstream");
                settings.compress = compress;
            }
            subclass::Property("extra-headers", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let extra_headers = value.get().expect("type checked upstream");
                settings.extra_headers = extra_headers;
            }
            subclass::Property("cookies", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let cookies = value.get().expect("type checked upstream");
                settings.cookies = cookies.unwrap_or_else(Vec::new);
            }
            subclass::Property("iradio-mode", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let iradio_mode = value.get_some().expect("type checked upstream");
                settings.iradio_mode = iradio_mode;
            }
            subclass::Property("keep-alive", ..) => {
                let mut settings = self.settings.lock().unwrap();
                let keep_alive = value.get_some().expect("type checked upstream");
                settings.keep_alive = keep_alive;
            }
            _ => unimplemented!(),
        };
    }

    fn get_property(&self, obj: &Self::Type, id: usize) -> glib::Value {
        let prop = &PROPERTIES[id];
        match *prop {
            subclass::Property("location", ..) => {
                let settings = self.settings.lock().unwrap();
                let location = settings.location.as_ref().map(Url::to_string);

                location.to_value()
            }
            subclass::Property("user-agent", ..) => {
                let settings = self.settings.lock().unwrap();
                settings.user_agent.to_value()
            }
            subclass::Property("is-live", ..) => obj.is_live().to_value(),
            subclass::Property("user-id", ..) => {
                let settings = self.settings.lock().unwrap();
                settings.user_id.to_value()
            }
            subclass::Property("user-pw", ..) => {
                let settings = self.settings.lock().unwrap();
                settings.user_pw.to_value()
            }
            subclass::Property("timeout", ..) => {
                let settings = self.settings.lock().unwrap();
                settings.timeout.to_value()
            }
            subclass::Property("compress", ..) => {
                let settings = self.settings.lock().unwrap();
                settings.compress.to_value()
            }
            subclass::Property("extra-headers", ..) => {
                let settings = self.settings.lock().unwrap();
                settings.extra_headers.to_value()
            }
            subclass::Property("cookies", ..) => {
                let settings = self.settings.lock().unwrap();
                settings.cookies.to_value()
            }
            subclass::Property("iradio-mode", ..) => {
                let settings = self.settings.lock().unwrap();
                settings.iradio_mode.to_value()
            }
            subclass::Property("keep-alive", ..) => {
                let settings = self.settings.lock().unwrap();
                settings.keep_alive.to_value()
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);
        obj.set_automatic_eos(false);
        obj.set_format(gst::Format::Bytes);
    }
}

impl ElementImpl for ReqwestHttpSrc {
    fn set_context(&self, element: &Self::Type, context: &gst::Context) {
        if context.get_context_type() == REQWEST_CLIENT_CONTEXT {
            let mut external_client = self.external_client.lock().unwrap();
            let s = context.get_structure();
            *external_client = s
                .get_some::<&ClientContext>("client")
                .map(|c| Some(c.clone()))
                .unwrap_or(None);
        }

        self.parent_set_context(element, context);
    }

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if let gst::StateChange::ReadyToNull = transition {
            *self.client.lock().unwrap() = None;
        }

        self.parent_change_state(element, transition)
    }
}

impl BaseSrcImpl for ReqwestHttpSrc {
    fn is_seekable(&self, _src: &Self::Type) -> bool {
        match *self.state.lock().unwrap() {
            State::Started { seekable, .. } => seekable,
            _ => false,
        }
    }

    fn get_size(&self, _src: &Self::Type) -> Option<u64> {
        match *self.state.lock().unwrap() {
            State::Started { size, .. } => size,
            _ => None,
        }
    }

    fn unlock(&self, _src: &Self::Type) -> Result<(), gst::ErrorMessage> {
        let canceller = self.canceller.lock().unwrap();
        if let Some(ref canceller) = *canceller {
            canceller.abort();
        }
        Ok(())
    }

    fn start(&self, src: &Self::Type) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();

        *state = State::Stopped;

        let uri = self
            .settings
            .lock()
            .unwrap()
            .location
            .as_ref()
            .ok_or_else(|| {
                gst::error_msg!(gst::CoreError::StateChange, ["Can't start without an URI"])
            })
            .map(|uri| uri.clone())?;

        gst_debug!(CAT, obj: src, "Starting for URI {}", uri);

        *state = self.do_request(src, uri, 0, None).map_err(|err| {
            err.unwrap_or_else(|| {
                gst::error_msg!(gst::LibraryError::Failed, ["Interrupted during start"])
            })
        })?;

        Ok(())
    }

    fn stop(&self, src: &Self::Type) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: src, "Stopping");
        *self.state.lock().unwrap() = State::Stopped;

        Ok(())
    }

    fn query(&self, element: &Self::Type, query: &mut gst::QueryRef) -> bool {
        use gst::QueryView;

        match query.view_mut() {
            QueryView::Scheduling(ref mut q) => {
                q.set(
                    gst::SchedulingFlags::SEQUENTIAL | gst::SchedulingFlags::BANDWIDTH_LIMITED,
                    1,
                    -1,
                    0,
                );
                q.add_scheduling_modes(&[gst::PadMode::Push]);
                true
            }
            _ => BaseSrcImplExt::parent_query(self, element, query),
        }
    }

    fn do_seek(&self, src: &Self::Type, segment: &mut gst::Segment) -> bool {
        let segment = segment.downcast_mut::<gst::format::Bytes>().unwrap();

        let mut state = self.state.lock().unwrap();

        let (position, old_stop, uri) = match *state {
            State::Started {
                position,
                stop,
                ref uri,
                ..
            } => (position, stop, uri.clone()),
            State::Stopped => {
                gst::element_error!(src, gst::LibraryError::Failed, ["Not started yet"]);

                return false;
            }
        };

        let start = segment.get_start().expect("No start position given");
        let stop = segment.get_stop();

        gst_debug!(CAT, obj: src, "Seeking to {}-{:?}", start, stop);

        if position == start && old_stop == stop.0 {
            gst_debug!(CAT, obj: src, "No change to current request");
            return true;
        }

        *state = State::Stopped;
        match self.do_request(src, uri, start, stop.0) {
            Ok(s) => {
                *state = s;
                true
            }
            Err(Some(err)) => {
                src.post_error_message(err);
                false
            }
            Err(None) => false,
        }
    }
}

impl PushSrcImpl for ReqwestHttpSrc {
    fn create(&self, src: &Self::Type) -> Result<gst::Buffer, gst::FlowError> {
        let mut state = self.state.lock().unwrap();

        let (response, position, caps, tags) = match *state {
            State::Started {
                ref mut response,
                ref mut position,
                ref mut tags,
                ref mut caps,
                ..
            } => (response, position, caps, tags),
            State::Stopped => {
                gst::element_error!(src, gst::LibraryError::Failed, ["Not started yet"]);

                return Err(gst::FlowError::Error);
            }
        };

        let offset = *position;

        let mut current_response = match response.take() {
            Some(response) => response,
            None => {
                gst_error!(CAT, obj: src, "Don't have a response");
                gst::element_error!(src, gst::ResourceError::Read, ["Don't have a response"]);

                return Err(gst::FlowError::Error);
            }
        };

        let tags = tags.take();
        let caps = caps.take();
        drop(state);

        if let Some(caps) = caps {
            gst_debug!(CAT, obj: src, "Setting caps {:?}", caps);
            src.set_caps(&caps)
                .map_err(|_| gst::FlowError::NotNegotiated)?;
        }

        if let Some(tags) = tags {
            gst_debug!(CAT, obj: src, "Sending iradio tags {:?}", tags);
            let pad = src.get_static_pad("src").unwrap();
            pad.push_event(gst::event::Tag::new(tags));
        }

        let future = async {
            current_response.chunk().await.map_err(move |err| {
                gst::error_msg!(
                    gst::ResourceError::Read,
                    ["Failed to read chunk at offset {}: {:?}", offset, err]
                )
            })
        };
        let res = self.wait(future);

        let res = match res {
            Ok(res) => res,
            Err(Some(err)) => {
                gst_debug!(CAT, obj: src, "Error {:?}", err);
                src.post_error_message(err);
                return Err(gst::FlowError::Error);
            }
            Err(None) => {
                gst_debug!(CAT, obj: src, "Flushing");
                return Err(gst::FlowError::Flushing);
            }
        };

        let mut state = self.state.lock().unwrap();
        let (response, position) = match *state {
            State::Started {
                ref mut response,
                ref mut position,
                ..
            } => (response, position),
            State::Stopped => {
                gst::element_error!(src, gst::LibraryError::Failed, ["Not started yet"]);

                return Err(gst::FlowError::Error);
            }
        };

        match res {
            Some(chunk) => {
                /* do something with the chunk and store the body again in the state */

                gst_trace!(
                    CAT,
                    obj: src,
                    "Chunk of {} bytes received at offset {}",
                    chunk.len(),
                    offset
                );
                let size = chunk.len();
                assert_ne!(chunk.len(), 0);

                *position += size as u64;

                let mut buffer = gst::Buffer::from_slice(chunk);

                *response = Some(current_response);

                {
                    let buffer = buffer.get_mut().unwrap();
                    buffer.set_offset(offset);
                    buffer.set_offset_end(offset + size as u64);
                }

                Ok(buffer)
            }
            None => {
                /* No further data, end of stream */
                gst_debug!(CAT, obj: src, "End of stream");
                *response = Some(current_response);
                Err(gst::FlowError::Eos)
            }
        }
    }
}

impl URIHandlerImpl for ReqwestHttpSrc {
    fn get_uri(&self, _element: &Self::Type) -> Option<String> {
        let settings = self.settings.lock().unwrap();

        settings.location.as_ref().map(Url::to_string)
    }

    fn set_uri(&self, element: &Self::Type, uri: &str) -> Result<(), glib::Error> {
        self.set_location(&element, Some(uri))
    }

    fn get_uri_type() -> gst::URIType {
        gst::URIType::Src
    }

    fn get_protocols() -> Vec<String> {
        vec!["http".to_string(), "https".to_string()]
    }
}

impl ObjectSubclass for ReqwestHttpSrc {
    const NAME: &'static str = "ReqwestHttpSrc";
    type Type = super::ReqwestHttpSrc;
    type ParentType = gst_base::PushSrc;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib::object_subclass!();

    fn new() -> Self {
        Self {
            client: Mutex::new(None),
            external_client: Mutex::new(None),
            settings: Mutex::new(Default::default()),
            state: Mutex::new(Default::default()),
            canceller: Mutex::new(None),
        }
    }

    fn type_init(type_: &mut subclass::InitializingType<Self>) {
        type_.add_interface::<gst::URIHandler>();
    }

    fn class_init(klass: &mut Self::Class) {
        klass.set_metadata(
            "HTTP Source",
            "Source/Network/HTTP",
            "Read stream from an HTTP/HTTPS location",
            "Sebastian Dröge <sebastian@centricular.com>",
        );

        let caps = gst::Caps::new_any();
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
}
