// Copyright (C) 2020 Natanael Mojica <neithanmo@gmail.com>
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

use glib::prelude::*;

mod imp;

glib::wrapper! {
    pub struct CsoundFilter(ObjectSubclass<imp::CsoundFilter>) @extends gst_base::BaseTransform, gst::Element, gst::Object;
}

unsafe impl Send for CsoundFilter {}
unsafe impl Sync for CsoundFilter {}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "csoundfilter",
        gst::Rank::None,
        CsoundFilter::static_type(),
    )
}
