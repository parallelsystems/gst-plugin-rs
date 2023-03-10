// Copyright (C) 2020 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::prelude::*;

mod imp;
mod transcribe;
mod translate;

use once_cell::sync::Lazy;

static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "awstranscribe",
        gst::DebugColorFlags::empty(),
        Some("AWS Transcribe element"),
    )
});

use aws_sdk_transcribestreaming::model::{PartialResultsStability, VocabularyFilterMethod};

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstAwsTranscriberResultStability")]
#[non_exhaustive]
pub enum AwsTranscriberResultStability {
    #[enum_value(name = "High: stabilize results as fast as possible", nick = "high")]
    High = 0,
    #[enum_value(
        name = "Medium: balance between stability and accuracy",
        nick = "medium"
    )]
    Medium = 1,
    #[enum_value(
        name = "Low: relatively less stable partial transcription results with higher accuracy",
        nick = "low"
    )]
    Low = 2,
}

impl From<AwsTranscriberResultStability> for PartialResultsStability {
    fn from(val: AwsTranscriberResultStability) -> Self {
        use AwsTranscriberResultStability::*;
        match val {
            High => PartialResultsStability::High,
            Medium => PartialResultsStability::Medium,
            Low => PartialResultsStability::Low,
        }
    }
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstAwsTranscriberVocabularyFilterMethod")]
#[non_exhaustive]
pub enum AwsTranscriberVocabularyFilterMethod {
    #[enum_value(name = "Mask: replace words with ***", nick = "mask")]
    Mask = 0,
    #[enum_value(name = "Remove: delete words", nick = "remove")]
    Remove = 1,
    #[enum_value(name = "Tag: flag words without changing them", nick = "tag")]
    Tag = 2,
}

impl From<AwsTranscriberVocabularyFilterMethod> for VocabularyFilterMethod {
    fn from(val: AwsTranscriberVocabularyFilterMethod) -> Self {
        use AwsTranscriberVocabularyFilterMethod::*;
        match val {
            Mask => VocabularyFilterMethod::Mask,
            Remove => VocabularyFilterMethod::Remove,
            Tag => VocabularyFilterMethod::Tag,
        }
    }
}

glib::wrapper! {
    pub struct Transcriber(ObjectSubclass<imp::Transcriber>) @extends gst::Element, gst::Object, @implements gst::ChildProxy;
}

glib::wrapper! {
    pub struct TranslationSrcPad(ObjectSubclass<imp::TranslationSrcPad>) @extends gst::Pad, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    #[cfg(feature = "doc")]
    {
        AwsTranscriberResultStability::static_type()
            .mark_as_plugin_api(gst::PluginAPIFlags::empty());
        AwsTranscriberVocabularyFilterMethod::static_type()
            .mark_as_plugin_api(gst::PluginAPIFlags::empty());
        TranslationSrcPad::static_type().mark_as_plugin_api(gst::PluginAPIFlags::empty());
    }
    gst::Element::register(
        Some(plugin),
        "awstranscriber",
        gst::Rank::None,
        Transcriber::static_type(),
    )
}
