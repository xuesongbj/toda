// Copyright 2020 Chaos Mesh Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

#![feature(box_syntax)]
#![feature(async_closure)]
#![feature(vec_into_raw_parts)]
#![feature(atomic_mut_ptr)]
#![feature(drain_filter)]
#![allow(clippy::or_fun_call)]
#![allow(clippy::too_many_arguments)]

extern crate derive_more;

mod fuse_device;
mod hookfs;
mod injector;
mod jsonrpc;
mod mount;
mod mount_injector;
mod ptrace;
mod replacer;
mod stop;
mod utils;

use std::convert::TryFrom;
use std::os::unix::io::RawFd;
use std::path::PathBuf;
use std::sync::{mpsc, Mutex};
use std::{io, thread};

use anyhow::Result;
use injector::InjectorConfig;
use jsonrpc::start_server;
use mount_injector::{MountInjectionGuard, MountInjector};
use nix::sys::signal::{signal, SigHandler, Signal};
use nix::unistd::{pipe, read, write};
use nix::mount::{mount, MsFlags};
use replacer::{Replacer, UnionReplacer};
use structopt::StructOpt;
use tokio::runtime::Runtime;
use tracing::{info, instrument};
use tracing_subscriber::EnvFilter;
use utils::encode_path;

#[derive(StructOpt, Debug, Clone)]
#[structopt(name = "basic")]
struct Options {
    #[structopt(long)]
    path: PathBuf,

    #[structopt(long = "mount-only")]
    mount_only: bool,

    #[structopt(short = "v", long = "verbose", default_value = "trace")]
    verbose: String,
}

#[instrument(skip(option))]
fn inject(option: Options, injector_config: Vec<InjectorConfig>) -> Result<MountInjectionGuard> {
    info!("inject with config {:?}", injector_config);

    let path = option.path.clone();

    info!("canonicalizing path {}", path.display());
    let path = path.canonicalize()?;

    // 1. Set mount properties.
    // 2. Mirror mount.
    const NONE: Option<&'static [u8]> = None;
    mount(NONE, path.as_path(), NONE, MsFlags::MS_PRIVATE, NONE).unwrap_or_else(|e| panic!("make-private failed: {}", e));
    mount(Some(path.as_path()), path.as_path(), NONE, MsFlags::MS_BIND, NONE).unwrap_or_else(|e| panic!("mount bind failed: {}", e));

    let replacer = if !option.mount_only {
        let mut replacer = UnionReplacer::new();
        replacer.prepare(&path, &path)?;

        Some(replacer)
    } else {
        None
    };

    if let Err(err) = fuse_device::mkfuse_node() {
        info!("fail to make /dev/fuse node: {}", err)
    }

    let mut injection = MountInjector::create_injection(&option.path, injector_config)?;
    let mount_guard = injection.mount()?;
    info!("mount successfully");

    if let Some(mut replacer) = replacer {
        // At this time, `mount --move` has already been executed.
        // Our FUSE are mounted on the "path", so we
        replacer.run()?;
        drop(replacer);
        info!("replacer detached");
    }

    info!("enable injection");
    mount_guard.enable_injection();

    Ok(mount_guard)
}

#[instrument(skip(option, mount_guard))]
fn resume(option: Options, mount_guard: MountInjectionGuard) -> Result<()> {
    info!("disable injection");
    mount_guard.disable_injection();

    let path = option.path.clone();

    info!("canonicalizing path {}", path.display());
    let path = path.canonicalize()?;
    let (_, new_path) = encode_path(&path)?;

    let replacer = if !option.mount_only {
        let mut replacer = UnionReplacer::new();
        replacer.prepare(&path, &new_path)?;
        info!("running replacer");
        let result = replacer.run();
        info!("replace result: {:?}", result);

        Some(replacer)
    } else {
        None
    };

    info!("recovering mount");
    mount_guard.recover_mount()?;

    info!("replacers detached");
    info!("recover successfully");

    drop(replacer);
    Ok(())
}

static mut SIGNAL_PIPE_WRITER: RawFd = 0;

const SIGNAL_MSG: [u8; 6] = *b"SIGNAL";

extern "C" fn signal_handler(_: libc::c_int) {
    unsafe {
        write(SIGNAL_PIPE_WRITER, &SIGNAL_MSG).unwrap();
    }
}

fn wait_for_signal(chan: RawFd) -> Result<()> {
    let mut buf = vec![0u8; 6];
    read(chan, buf.as_mut_slice())?;
    Ok(())
}

fn main() -> Result<()> {
    let (reader, writer) = pipe()?;
    unsafe {
        SIGNAL_PIPE_WRITER = writer;
    }

    unsafe { signal(Signal::SIGINT, SigHandler::Handler(signal_handler))? };
    unsafe { signal(Signal::SIGTERM, SigHandler::Handler(signal_handler))? };

    let option = Options::from_args();
    let env_filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_from(&option.verbose))
        .or_else(|_| EnvFilter::try_new("trace"))
        .unwrap();
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(env_filter)
        .init();
    info!("start with option: {:?}", option);
    let mount_injector = inject(option.clone(), vec![]);

    let status = match &mount_injector {
        Ok(_) => Ok(()),
        Err(e) => Err(anyhow::Error::msg(e.to_string())),
    };

    let (tx, _rx) = mpsc::channel();
    {
        let hookfs = match &mount_injector {
            Ok(e) => Some(e.hookfs.clone().into()),
            Err(_) => None,
        };
        thread::spawn(|| {
            Runtime::new()
                .expect("Failed to create Tokio runtime")
                .block_on(start_server(jsonrpc::RpcImpl::new(
                    Mutex::new(status),
                    Mutex::new(tx),
                    hookfs,
                )));
        });
    }
    info!("waiting for signal to exit");
    wait_for_signal(reader)?;
    info!("start to recover and exit");
    if let Ok(v) = mount_injector {
        resume(option, v)?;
    }
    Ok(())
}
