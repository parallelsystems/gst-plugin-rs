extern crate cc;
extern crate gst_plugin_version_helper;
extern crate pkg_config;

use pkg_config::{Config, Error};
use std::env;
use std::io;
use std::io::prelude::*;
use std::process;

fn main() {
    gst_plugin_version_helper::get_info();

    let gstreamer = pkg_config::probe_library("gstreamer-1.0").unwrap();
    let gstrtp = pkg_config::probe_library("gstreamer-rtp-1.0").unwrap();
    let includes = [gstreamer.include_paths, gstrtp.include_paths];

    let mut build = cc::Build::new();

    for p in includes.iter().flat_map(|i| i) {
        build.include(p);
    }

    build.file("src/jitterbuffer/rtpjitterbuffer.c");
    build.file("src/jitterbuffer/rtpstats.c");

    build.compile("libthreadshare-c.a");

    println!("cargo:rustc-link-lib=dylib=gstrtp-1.0");

    for path in gstrtp.link_paths.iter() {
        println!(
            "cargo:rustc-link-search=native={}",
            path.to_str().expect("library path doesn't exist")
        );
    }

    if let Err(s) = find() {
        let _ = writeln!(io::stderr(), "{}", s);
        process::exit(1);
    }
}

fn find() -> Result<(), Error> {
    let package_name = "spandsp";
    let shared_libs = ["spandsp"];

    let target = env::var("TARGET").expect("TARGET environment variable doesn't exist");
    let hardcode_shared_libs = target.contains("windows");

    let mut config = Config::new();
    config.print_system_libs(false);
    if hardcode_shared_libs {
        config.cargo_metadata(false);
    }
    match config.probe(package_name) {
        Ok(library) => {
            if hardcode_shared_libs {
                for lib_ in shared_libs.iter() {
                    println!("cargo:rustc-link-lib=dylib={}", lib_);
                }
                for path in library.link_paths.iter() {
                    println!(
                        "cargo:rustc-link-search=native={}",
                        path.to_str().expect("library path doesn't exist")
                    );
                }
            }
            Ok(())
        }
        Err(Error::EnvNoPkgConfig(_)) | Err(Error::Command { .. }) => {
            for lib_ in shared_libs.iter() {
                println!("cargo:rustc-link-lib=dylib={}", lib_);
            }
            Ok(())
        }
        Err(err) => Err(err),
    }
}
