// Copyright (C) 2019 Amazon.com, Inc. or its affiliates <mkolny@amazon.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;

use aws_sdk_s3::client::fluent_builders::{
    AbortMultipartUpload, CompleteMultipartUpload, CreateMultipartUpload, UploadPart,
};
use aws_sdk_s3::config;
use aws_sdk_s3::model::{CompletedMultipartUpload, CompletedPart};
use aws_sdk_s3::types::ByteStream;
use aws_sdk_s3::Endpoint;
use aws_sdk_s3::{Client, Credentials, Region, RetryConfig};
use http::Uri;

use futures::future;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::convert::From;
use std::sync::Mutex;
use std::time::Duration;

use crate::s3url::*;
use crate::s3utils::{self, duration_from_millis, duration_to_millis, WaitError};

use super::OnError;

const DEFAULT_RETRY_ATTEMPTS: u32 = 5;
const DEFAULT_BUFFER_SIZE: u64 = 5 * 1024 * 1024;
const DEFAULT_MULTIPART_UPLOAD_ON_ERROR: OnError = OnError::DoNothing;

// General setting for create / abort requests
const DEFAULT_REQUEST_TIMEOUT_MSEC: u64 = 15_000;
const DEFAULT_RETRY_DURATION_MSEC: u64 = 60_000;
// This needs to be independently configurable, as the part size can be upto 5GB
const DEFAULT_UPLOAD_PART_REQUEST_TIMEOUT_MSEC: u64 = 10_000;
const DEFAULT_UPLOAD_PART_RETRY_DURATION_MSEC: u64 = 60_000;
// CompletedMultipartUpload can take minutes to complete, so we need a longer value here
// https://docs.aws.amazon.com/AmazonS3/latest/API/API_CompleteMultipartUpload.html
const DEFAULT_COMPLETE_REQUEST_TIMEOUT_MSEC: u64 = 600_000; // 10 minutes
const DEFAULT_COMPLETE_RETRY_DURATION_MSEC: u64 = 3_600_000; // 60 minutes

struct Started {
    client: Client,
    buffer: Vec<u8>,
    upload_id: String,
    part_number: i64,
    completed_parts: Vec<CompletedPart>,
}

impl Started {
    pub fn new(client: Client, buffer: Vec<u8>, upload_id: String) -> Started {
        Started {
            client,
            buffer,
            upload_id,
            part_number: 0,
            completed_parts: Vec::new(),
        }
    }

    pub fn increment_part_number(&mut self) -> Result<i64, gst::ErrorMessage> {
        // https://docs.aws.amazon.com/AmazonS3/latest/dev/qfacts.html
        const MAX_MULTIPART_NUMBER: i64 = 10000;

        if self.part_number > MAX_MULTIPART_NUMBER {
            return Err(gst::error_msg!(
                gst::ResourceError::Failed,
                [
                    "Maximum number of parts ({}) reached.",
                    MAX_MULTIPART_NUMBER
                ]
            ));
        }

        self.part_number += 1;
        Ok(self.part_number)
    }
}

enum State {
    Stopped,
    Started(Started),
}

impl Default for State {
    fn default() -> State {
        State::Stopped
    }
}

struct Settings {
    region: Region,
    bucket: Option<String>,
    key: Option<String>,
    content_type: Option<String>,
    buffer_size: u64,
    access_key: Option<String>,
    secret_access_key: Option<String>,
    session_token: Option<String>,
    metadata: Option<gst::Structure>,
    retry_attempts: u32,
    multipart_upload_on_error: OnError,
    request_timeout: Duration,
    endpoint_uri: Option<String>,
}

impl Settings {
    fn to_uri(&self) -> String {
        format!(
            "s3://{}/{}/{}",
            self.region,
            self.bucket.as_ref().unwrap(),
            self.key.as_ref().unwrap()
        )
    }

    fn to_metadata(&self, element: &super::S3Sink) -> Option<HashMap<String, String>> {
        self.metadata.as_ref().map(|structure| {
            let mut hash = HashMap::new();

            for (key, value) in structure.iter() {
                if let Ok(Ok(value_str)) = value.transform::<String>().map(|v| v.get()) {
                    gst::log!(CAT, obj: element, "metadata '{}' -> '{}'", key, value_str);
                    hash.insert(key.to_string(), value_str);
                } else {
                    gst::warning!(
                        CAT,
                        obj: element,
                        "Failed to convert metadata '{}' to string ('{:?}')",
                        key,
                        value
                    );
                }
            }

            hash
        })
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            region: Region::new("us-west-2"),
            bucket: None,
            key: None,
            content_type: None,
            access_key: None,
            secret_access_key: None,
            session_token: None,
            metadata: None,
            buffer_size: DEFAULT_BUFFER_SIZE,
            retry_attempts: DEFAULT_RETRY_ATTEMPTS,
            multipart_upload_on_error: DEFAULT_MULTIPART_UPLOAD_ON_ERROR,
            request_timeout: Duration::from_millis(DEFAULT_REQUEST_TIMEOUT_MSEC),
            endpoint_uri: None,
        }
    }
}

#[derive(Default)]
pub struct S3Sink {
    url: Mutex<Option<GstS3Url>>,
    settings: Mutex<Settings>,
    state: Mutex<State>,
    canceller: Mutex<Option<future::AbortHandle>>,
    abort_multipart_canceller: Mutex<Option<future::AbortHandle>>,
}

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "aws3sink",
        gst::DebugColorFlags::empty(),
        Some("Amazon S3 Sink"),
    )
});

impl S3Sink {
    fn flush_current_buffer(
        &self,
        element: &super::S3Sink,
    ) -> Result<(), Option<gst::ErrorMessage>> {
        let upload_part_req: UploadPart = self.create_upload_part_request()?;

        let mut state = self.state.lock().unwrap();
        let state = match *state {
            State::Started(ref mut started_state) => started_state,
            State::Stopped => {
                unreachable!("Element should be started");
            }
        };

        let part_number = state.part_number;

        let upload_part_req_future = upload_part_req.send();
        let output =
            s3utils::wait(&self.canceller, upload_part_req_future).map_err(|err| match err {
                WaitError::FutureError(err) => {
                    let settings = self.settings.lock().unwrap();
                    match settings.multipart_upload_on_error {
                        OnError::Abort => {
                            gst::log!(
                                CAT,
                                obj: element,
                                "Aborting multipart upload request with id: {}",
                                state.upload_id
                            );
                            match self.abort_multipart_upload_request(state) {
                                Ok(()) => {
                                    gst::log!(
                                        CAT,
                                        obj: element,
                                        "Aborting multipart upload request succeeded."
                                    );
                                }
                                Err(err) => gst::error!(
                                    CAT,
                                    obj: element,
                                    "Aborting multipart upload failed: {}",
                                    err.to_string()
                                ),
                            }
                        }
                        OnError::Complete => {
                            gst::log!(
                                CAT,
                                obj: element,
                                "Completing multipart upload request with id: {}",
                                state.upload_id
                            );
                            match self.complete_multipart_upload_request(state) {
                                Ok(()) => {
                                    gst::log!(
                                        CAT,
                                        obj: element,
                                        "Complete multipart upload request succeeded."
                                    );
                                }
                                Err(err) => gst::error!(
                                    CAT,
                                    obj: element,
                                    "Completing multipart upload failed: {}",
                                    err.to_string()
                                ),
                            }
                        }
                        OnError::DoNothing => (),
                    }
                    Some(gst::error_msg!(
                        gst::ResourceError::OpenWrite,
                        ["Failed to upload part: {}", err]
                    ))
                }
                WaitError::Cancelled => None,
            })?;

        let completed_part = CompletedPart::builder()
            .set_e_tag(output.e_tag)
            .set_part_number(Some(part_number as i32))
            .build();
        state.completed_parts.push(completed_part);

        gst::info!(CAT, obj: element, "Uploaded part {}", part_number);

        Ok(())
    }

    fn create_upload_part_request(&self) -> Result<UploadPart, gst::ErrorMessage> {
        let url = self.url.lock().unwrap();
        let settings = self.settings.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        let state = match *state {
            State::Started(ref mut started_state) => started_state,
            State::Stopped => {
                unreachable!("Element should be started");
            }
        };

        let part_number = state.increment_part_number()?;
        let body = Some(ByteStream::from(std::mem::replace(
            &mut state.buffer,
            Vec::with_capacity(settings.buffer_size as usize),
        )));

        let bucket = Some(url.as_ref().unwrap().bucket.to_owned());
        let key = Some(url.as_ref().unwrap().object.to_owned());
        let upload_id = Some(state.upload_id.to_owned());

        let client = &state.client;
        let upload_part = client
            .upload_part()
            .set_body(body)
            .set_bucket(bucket)
            .set_key(key)
            .set_upload_id(upload_id)
            .set_part_number(Some(part_number as i32));

        Ok(upload_part)
    }

    fn create_complete_multipart_upload_request(
        &self,
        started_state: &mut Started,
    ) -> CompleteMultipartUpload {
        started_state
            .completed_parts
            .sort_by(|a, b| a.part_number.cmp(&b.part_number));

        let parts = Some(std::mem::take(&mut started_state.completed_parts));

        let completed_upload = CompletedMultipartUpload::builder().set_parts(parts).build();

        let url = self.url.lock().unwrap();
        let client = &started_state.client;

        let bucket = Some(url.as_ref().unwrap().bucket.to_owned());
        let key = Some(url.as_ref().unwrap().object.to_owned());
        let upload_id = Some(started_state.upload_id.to_owned());
        let multipart_upload = Some(completed_upload);

        client
            .complete_multipart_upload()
            .set_bucket(bucket)
            .set_key(key)
            .set_upload_id(upload_id)
            .set_multipart_upload(multipart_upload)
    }

    fn create_create_multipart_upload_request(
        &self,
        client: &Client,
        url: &GstS3Url,
        settings: &Settings,
    ) -> CreateMultipartUpload {
        let bucket = Some(url.bucket.clone());
        let key = Some(url.object.clone());
        let content_type = settings.content_type.clone();
        let metadata = settings.to_metadata(&self.instance());

        client
            .create_multipart_upload()
            .set_bucket(bucket)
            .set_key(key)
            .set_content_type(content_type)
            .set_metadata(metadata)
    }

    fn create_abort_multipart_upload_request(
        &self,
        client: &Client,
        url: &GstS3Url,
        started_state: &Started,
    ) -> AbortMultipartUpload {
        let bucket = Some(url.bucket.clone());
        let key = Some(url.object.clone());

        client
            .abort_multipart_upload()
            .set_bucket(bucket)
            .set_expected_bucket_owner(None)
            .set_key(key)
            .set_request_payer(None)
            .set_upload_id(Some(started_state.upload_id.to_owned()))
    }

    fn abort_multipart_upload_request(
        &self,
        started_state: &Started,
    ) -> Result<(), gst::ErrorMessage> {
        let s3url = {
            let url = self.url.lock().unwrap();
            match *url {
                Some(ref url) => url.clone(),
                None => unreachable!("Element should be started"),
            }
        };

        let client = &started_state.client;
        let abort_req = self.create_abort_multipart_upload_request(client, &s3url, started_state);
        let abort_req_future = abort_req.send();

        s3utils::wait(&self.abort_multipart_canceller, abort_req_future)
            .map(|_| ())
            .map_err(|err| match err {
                WaitError::FutureError(err) => {
                    gst::error_msg!(
                        gst::ResourceError::Write,
                        ["Failed to abort multipart upload: {}.", err.to_string()]
                    )
                }
                WaitError::Cancelled => {
                    gst::error_msg!(
                        gst::ResourceError::Write,
                        ["Abort multipart upload request interrupted."]
                    )
                }
            })
    }

    fn complete_multipart_upload_request(
        &self,
        started_state: &mut Started,
    ) -> Result<(), gst::ErrorMessage> {
        let complete_req = self.create_complete_multipart_upload_request(started_state);
        let complete_req_future = complete_req.send();

        s3utils::wait(&self.canceller, complete_req_future)
            .map(|_| ())
            .map_err(|err| match err {
                WaitError::FutureError(err) => gst::error_msg!(
                    gst::ResourceError::Write,
                    ["Failed to complete multipart upload: {}.", err.to_string()]
                ),
                WaitError::Cancelled => {
                    gst::error_msg!(
                        gst::LibraryError::Failed,
                        ["Complete multipart upload request interrupted"]
                    )
                }
            })
    }

    fn finalize_upload(&self, element: &super::S3Sink) -> Result<(), gst::ErrorMessage> {
        if self.flush_current_buffer(element).is_err() {
            return Err(gst::error_msg!(
                gst::ResourceError::Settings,
                ["Failed to flush internal buffer."]
            ));
        }

        let mut state = self.state.lock().unwrap();
        let started_state = match *state {
            State::Started(ref mut started_state) => started_state,
            State::Stopped => {
                unreachable!("Element should be started");
            }
        };

        self.complete_multipart_upload_request(started_state)
    }

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        let settings = self.settings.lock().unwrap();

        if let State::Started { .. } = *state {
            unreachable!("Element should be started");
        }

        let s3url = {
            let url = self.url.lock().unwrap();
            match *url {
                Some(ref url) => url.clone(),
                None => {
                    return Err(gst::error_msg!(
                        gst::ResourceError::Settings,
                        ["Cannot start without a URL being set"]
                    ));
                }
            }
        };

        let timeout_config = s3utils::timeout_config(settings.request_timeout);

        let cred = match (
            settings.access_key.as_ref(),
            settings.secret_access_key.as_ref(),
        ) {
            (Some(access_key), Some(secret_access_key)) => Some(Credentials::new(
                access_key.clone(),
                secret_access_key.clone(),
                settings.session_token.clone(),
                None,
                "aws-s3-sink",
            )),
            _ => None,
        };

        let sdk_config =
            s3utils::wait_config(&self.canceller, s3url.region.clone(), timeout_config, cred)
                .map_err(|err| match err {
                    WaitError::FutureError(err) => gst::error_msg!(
                        gst::ResourceError::OpenWrite,
                        ["Failed to create SDK config: {}", err]
                    ),
                    WaitError::Cancelled => {
                        gst::error_msg!(
                            gst::LibraryError::Failed,
                            ["SDK config request interrupted during start"]
                        )
                    }
                })?;

        let endpoint_uri = match &settings.endpoint_uri {
            Some(endpoint) => match endpoint.parse::<Uri>() {
                Ok(uri) => Some(uri),
                Err(e) => {
                    return Err(gst::error_msg!(
                        gst::ResourceError::Settings,
                        ["Invalid S3 endpoint uri. Error: {}", e]
                    ));
                }
            },
            None => None,
        };

        let config_builder = config::Builder::from(&sdk_config)
            .retry_config(RetryConfig::new().with_max_attempts(settings.retry_attempts));

        let config = if let Some(uri) = endpoint_uri {
            config_builder
                .endpoint_resolver(Endpoint::mutable(uri))
                .build()
        } else {
            config_builder.build()
        };

        let client = Client::from_conf(config);

        let create_multipart_req =
            self.create_create_multipart_upload_request(&client, &s3url, &settings);
        let create_multipart_req_future = create_multipart_req.send();

        let response = s3utils::wait(&self.canceller, create_multipart_req_future).map_err(
            |err| match err {
                WaitError::FutureError(err) => gst::error_msg!(
                    gst::ResourceError::OpenWrite,
                    ["Failed to create multipart upload: {}", err]
                ),
                WaitError::Cancelled => {
                    gst::error_msg!(
                        gst::LibraryError::Failed,
                        ["Create multipart request interrupted during start"]
                    )
                }
            },
        )?;

        let upload_id = response.upload_id.ok_or_else(|| {
            gst::error_msg!(
                gst::ResourceError::Failed,
                ["Failed to get multipart upload ID"]
            )
        })?;

        *state = State::Started(Started::new(
            client,
            Vec::with_capacity(settings.buffer_size as usize),
            upload_id,
        ));

        Ok(())
    }

    fn update_buffer(
        &self,
        src: &[u8],
        element: &super::S3Sink,
    ) -> Result<(), Option<gst::ErrorMessage>> {
        let mut state = self.state.lock().unwrap();
        let started_state = match *state {
            State::Started(ref mut started_state) => started_state,
            State::Stopped => {
                unreachable!("Element should be started already");
            }
        };

        let to_copy = std::cmp::min(
            started_state.buffer.capacity() - started_state.buffer.len(),
            src.len(),
        );

        let (head, tail) = src.split_at(to_copy);
        started_state.buffer.extend_from_slice(head);
        let do_flush = started_state.buffer.capacity() == started_state.buffer.len();
        drop(state);

        if do_flush {
            self.flush_current_buffer(element)?;
        }

        if to_copy < src.len() {
            self.update_buffer(tail, element)?;
        }

        Ok(())
    }

    fn cancel(&self) {
        let mut canceller = self.canceller.lock().unwrap();
        let mut abort_canceller = self.abort_multipart_canceller.lock().unwrap();

        if let Some(c) = abort_canceller.take() {
            c.abort()
        };

        if let Some(c) = canceller.take() {
            c.abort()
        };
    }

    fn set_uri(
        self: &S3Sink,
        object: &super::S3Sink,
        url_str: Option<&str>,
    ) -> Result<(), glib::Error> {
        let state = self.state.lock().unwrap();

        if let State::Started { .. } = *state {
            return Err(glib::Error::new(
                gst::URIError::BadState,
                "Cannot set URI on a started s3sink",
            ));
        }

        let mut url = self.url.lock().unwrap();

        if url_str.is_none() {
            *url = None;
            return Ok(());
        }

        gst::debug!(CAT, obj: object, "Setting uri to {:?}", url_str);

        let url_str = url_str.unwrap();
        match parse_s3_url(url_str) {
            Ok(s3url) => {
                *url = Some(s3url);
                Ok(())
            }
            Err(_) => Err(glib::Error::new(
                gst::URIError::BadUri,
                "Could not parse URI",
            )),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for S3Sink {
    const NAME: &'static str = "AwsS3Sink";
    type Type = super::S3Sink;
    type ParentType = gst_base::BaseSink;
    type Interfaces = (gst::URIHandler,);
}

impl ObjectImpl for S3Sink {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpecString::new(
                    "bucket",
                    "S3 Bucket",
                    "The bucket of the file to write",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecString::new(
                    "key",
                    "S3 Key",
                    "The key of the file to write",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecString::new(
                    "region",
                    "AWS Region",
                    "An AWS region (e.g. eu-west-2).",
                    Some("us-west-2"),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecUInt64::new(
                    "part-size",
                    "Part size",
                    "A size (in bytes) of an individual part used for multipart upload.",
                    5 * 1024 * 1024,        // 5 MB
                    5 * 1024 * 1024 * 1024, // 5 GB
                    DEFAULT_BUFFER_SIZE,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecString::new(
                    "uri",
                    "URI",
                    "The S3 object URI",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecString::new(
                    "access-key",
                    "Access Key",
                    "AWS Access Key",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecString::new(
                    "secret-access-key",
                    "Secret Access Key",
                    "AWS Secret Access Key",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecString::new(
                    "session-token",
                    "Session Token",
                    "AWS temporary Session Token from STS",
                    None,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecBoxed::new(
                    "metadata",
                    "Metadata",
                    "A map of metadata to store with the object in S3; field values need to be convertible to strings.",
                    gst::Structure::static_type(),
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecEnum::new(
                    "on-error",
                    "Whether to upload or complete the multipart upload on error",
                    "Do nothing, abort or complete a multipart upload request on error",
                    OnError::static_type(),
                    DEFAULT_MULTIPART_UPLOAD_ON_ERROR as i32,
                    glib::ParamFlags::READWRITE | gst::PARAM_FLAG_MUTABLE_READY,
                ),
                glib::ParamSpecUInt::new(
                    "retry-attempts",
                    "Retry attempts",
                    "Number of times AWS SDK attempts a request before abandoning the request",
                    1,
                    10,
                    DEFAULT_RETRY_ATTEMPTS,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecInt64::new(
                    "request-timeout",
                    "Request timeout",
                    "Timeout for general S3 requests (in ms, set to -1 for infinity)",
                    -1,
                    std::i64::MAX,
                    DEFAULT_REQUEST_TIMEOUT_MSEC as i64,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecInt64::new(
                    "upload-part-request-timeout",
                    "Upload part request timeout",
                    "Timeout for a single upload part request (in ms, set to -1 for infinity) (Deprecated. Use request-timeout.)",
                    -1,
                    std::i64::MAX,
                    DEFAULT_UPLOAD_PART_REQUEST_TIMEOUT_MSEC as i64,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecInt64::new(
                    "complete-upload-request-timeout",
                    "Complete upload request timeout",
                    "Timeout for the complete multipart upload request (in ms, set to -1 for infinity) (Deprecated. Use request-timeout.)",
                    -1,
                    std::i64::MAX,
                    DEFAULT_COMPLETE_REQUEST_TIMEOUT_MSEC as i64,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecInt64::new(
                    "retry-duration",
                    "Retry duration",
                    "How long we should retry general S3 requests before giving up (in ms, set to -1 for infinity) (Deprecated. Use retry-attempts.)",
                    -1,
                    std::i64::MAX,
                    DEFAULT_RETRY_DURATION_MSEC as i64,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecInt64::new(
                    "upload-part-retry-duration",
                    "Upload part retry duration",
                    "How long we should retry upload part requests before giving up (in ms, set to -1 for infinity) (Deprecated. Use retry-attempts.)",
                    -1,
                    std::i64::MAX,
                    DEFAULT_UPLOAD_PART_RETRY_DURATION_MSEC as i64,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecInt64::new(
                    "complete-upload-retry-duration",
                    "Complete upload retry duration",
                    "How long we should retry complete multipart upload requests before giving up (in ms, set to -1 for infinity) (Deprecated. Use retry-attempts.)",
                    -1,
                    std::i64::MAX,
                    DEFAULT_COMPLETE_RETRY_DURATION_MSEC as i64,
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpecString::new(
                    "endpoint-uri",
                    "S3 endpoint URI",
                    "The S3 endpoint URI to use",
                    None,
                    glib::ParamFlags::READWRITE,
                ),
            ]
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
        let mut settings = self.settings.lock().unwrap();

        gst::debug!(
            CAT,
            obj: obj,
            "Setting property '{}' to '{:?}'",
            pspec.name(),
            value
        );

        match pspec.name() {
            "bucket" => {
                settings.bucket = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
                if settings.key.is_some() {
                    let _ = self.set_uri(obj, Some(&settings.to_uri()));
                }
            }
            "key" => {
                settings.key = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
                if settings.bucket.is_some() {
                    let _ = self.set_uri(obj, Some(&settings.to_uri()));
                }
            }
            "region" => {
                let region = value.get::<String>().expect("type checked upstream");
                settings.region = Region::new(region);
                if settings.key.is_some() && settings.bucket.is_some() {
                    let _ = self.set_uri(obj, Some(&settings.to_uri()));
                }
            }
            "part-size" => {
                settings.buffer_size = value.get::<u64>().expect("type checked upstream");
            }
            "uri" => {
                let _ = self.set_uri(obj, value.get().expect("type checked upstream"));
            }
            "access-key" => {
                settings.access_key = value.get().expect("type checked upstream");
            }
            "secret-access-key" => {
                settings.secret_access_key = value.get().expect("type checked upstream");
            }
            "session-token" => {
                settings.session_token = value.get().expect("type checked upstream");
            }
            "metadata" => {
                settings.metadata = value.get().expect("type checked upstream");
            }
            "on-error" => {
                settings.multipart_upload_on_error =
                    value.get::<OnError>().expect("type checked upstream");
            }
            "retry-attempts" => {
                settings.retry_attempts = value.get::<u32>().expect("type checked upstream");
            }
            "request-timeout" => {
                settings.request_timeout =
                    duration_from_millis(value.get::<i64>().expect("type checked upstream"));
            }
            "upload-part-request-timeout" => {
                settings.request_timeout =
                    duration_from_millis(value.get::<i64>().expect("type checked upstream"));
            }
            "complete-upload-request-timeout" => {
                settings.request_timeout =
                    duration_from_millis(value.get::<i64>().expect("type checked upstream"));
            }
            "retry-duration" => {
                /*
                 * To maintain backwards compatibility calculate retry attempts
                 * by dividing the provided duration from request timeout.
                 */
                let value = value.get::<i64>().expect("type checked upstream");
                let request_timeout = duration_to_millis(Some(settings.request_timeout));
                let retry_attempts = if value > request_timeout {
                    value / request_timeout
                } else {
                    request_timeout / value
                };
                settings.retry_attempts = retry_attempts as u32;
            }
            "upload-part-retry-duration" | "complete-upload-retry-duration" => {
                gst::warning!(CAT, "Use retry-attempts. retry/upload-part/complete-upload-retry duration are deprecated.");
            }
            "endpoint-uri" => {
                settings.endpoint_uri = value
                    .get::<Option<String>>()
                    .expect("type checked upstream");
                if settings.key.is_some() && settings.bucket.is_some() {
                    let _ = self.set_uri(obj, Some(&settings.to_uri()));
                }
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();

        match pspec.name() {
            "key" => settings.key.to_value(),
            "bucket" => settings.bucket.to_value(),
            "region" => settings.region.to_string().to_value(),
            "part-size" => settings.buffer_size.to_value(),
            "uri" => {
                let url = self.url.lock().unwrap();
                let url = match *url {
                    Some(ref url) => url.to_string(),
                    None => "".to_string(),
                };

                url.to_value()
            }
            "access-key" => settings.access_key.to_value(),
            "secret-access-key" => settings.secret_access_key.to_value(),
            "session-token" => settings.session_token.to_value(),
            "metadata" => settings.metadata.to_value(),
            "on-error" => settings.multipart_upload_on_error.to_value(),
            "retry-attempts" => settings.retry_attempts.to_value(),
            "request-timeout" => duration_to_millis(Some(settings.request_timeout)).to_value(),
            "upload-part-request-timeout" => {
                duration_to_millis(Some(settings.request_timeout)).to_value()
            }
            "complete-upload-request-timeout" => {
                duration_to_millis(Some(settings.request_timeout)).to_value()
            }
            "retry-duration" | "upload-part-retry-duration" | "complete-upload-retry-duration" => {
                let request_timeout = duration_to_millis(Some(settings.request_timeout));
                (settings.retry_attempts as i64 * request_timeout).to_value()
            }
            "endpoint-uri" => settings.endpoint_uri.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for S3Sink {}

impl ElementImpl for S3Sink {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Amazon S3 sink",
                "Source/Network",
                "Writes an object to Amazon S3",
                "Marcin Kolny <mkolny@amazon.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let caps = gst::Caps::new_any();
            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

impl URIHandlerImpl for S3Sink {
    const URI_TYPE: gst::URIType = gst::URIType::Sink;

    fn protocols() -> &'static [&'static str] {
        &["s3"]
    }

    fn uri(&self, _: &Self::Type) -> Option<String> {
        self.url.lock().unwrap().as_ref().map(|s| s.to_string())
    }

    fn set_uri(&self, element: &Self::Type, uri: &str) -> Result<(), glib::Error> {
        self.set_uri(element, Some(uri))
    }
}

impl BaseSinkImpl for S3Sink {
    fn start(&self, _element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        self.start()
    }

    fn stop(&self, element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        let mut state = self.state.lock().unwrap();
        *state = State::Stopped;
        gst::info!(CAT, obj: element, "Stopped");

        Ok(())
    }

    fn render(
        &self,
        element: &Self::Type,
        buffer: &gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        if let State::Stopped = *self.state.lock().unwrap() {
            gst::element_error!(element, gst::CoreError::Failed, ["Not started yet"]);
            return Err(gst::FlowError::Error);
        }

        gst::trace!(CAT, obj: element, "Rendering {:?}", buffer);
        let map = buffer.map_readable().map_err(|_| {
            gst::element_error!(element, gst::CoreError::Failed, ["Failed to map buffer"]);
            gst::FlowError::Error
        })?;

        match self.update_buffer(&map, element) {
            Ok(_) => Ok(gst::FlowSuccess::Ok),
            Err(err) => match err {
                Some(error_message) => {
                    gst::error!(
                        CAT,
                        obj: element,
                        "Multipart upload failed: {}",
                        error_message
                    );
                    element.post_error_message(error_message);
                    Err(gst::FlowError::Error)
                }
                _ => {
                    gst::info!(CAT, obj: element, "Upload interrupted. Flushing...");
                    Err(gst::FlowError::Flushing)
                }
            },
        }
    }

    fn unlock(&self, _element: &Self::Type) -> Result<(), gst::ErrorMessage> {
        self.cancel();

        Ok(())
    }

    fn event(&self, element: &Self::Type, event: gst::Event) -> bool {
        if let gst::EventView::Eos(_) = event.view() {
            if let Err(error_message) = self.finalize_upload(element) {
                gst::error!(
                    CAT,
                    obj: element,
                    "Failed to finalize the upload: {}",
                    error_message
                );
                return false;
            }
        }

        BaseSinkImplExt::parent_event(self, element, event)
    }
}