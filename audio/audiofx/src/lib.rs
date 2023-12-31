// Copyright (C) 2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

mod audioecho;
mod audioloudnorm;
mod audiornnoise;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    audioecho::register(plugin)?;
    audioloudnorm::register(plugin)?;
    audiornnoise::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    rsaudiofx,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MIT/X11",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
