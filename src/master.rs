use std;
use std::io;
use std::rc::Rc;
use std::ffi::OsStr;
use std::time::Duration;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixListener as StdUnixListener;

use nix;
use libc;
use serde_json as json;
use byteorder::{BigEndian , ByteOrder};
use bytes::{BytesMut, BufMut};
use futures::{Async, unsync};
use tokio_core::reactor;
use tokio_core::reactor::Timeout;
use tokio_uds::{UnixStream, UnixListener};
use tokio_io::codec::{Encoder, Decoder};

use ctx::prelude::*;

use client;
use logging;
use config::Config;
use version::PKG_INFO;
use cmd::{self, CommandCenter, CommandError};
use service::{StartStatus, ReloadStatus, ServiceOperationError};
use master_types::{MasterRequest, MasterResponse};

pub struct Master {
    cfg: Rc<Config>,
    cmd: Address<CommandCenter>,
}

impl Service for Master {

    type Context = Context<Self>;
    type Message = Result<(UnixStream, std::os::unix::net::SocketAddr), io::Error>;
    type Result = Result<(), ()>;

    fn finished(&mut self, _: &mut Self::Context) -> Result<Async<()>, ()> {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, ctx: &mut Self::Context, msg: Self::Message)
            -> Result<Async<()>, ()>
    {
        match msg {
            Ok((stream, _)) => {
                let cmd = self.cmd.clone();
                let (r, w) = stream.ctx_framed(MasterTransportCodec, MasterTransportCodec);
                Builder::from_context(
                    ctx, r, move |ctx| MasterClient{cmd: cmd,
                                                    sink: ctx.add_sink(MasterClientSink, w)}
                ).run();
            }
            _ => (),
        }
        Ok(Async::NotReady)
    }
}

impl Master {

    pub fn start(cfg: Config, lst: StdUnixListener) -> bool {
        let cfg = Rc::new(cfg);

        // create core
        let mut core = reactor::Core::new().unwrap();
        let handle = core.handle();

        // create uds stream
        let lst = match UnixListener::from_listener(lst, &handle) {
            Ok(lst) => lst,
            Err(err) => {
                error!("Can not create unix socket listener {:?}", err);
                return false
            }
        };

        // command center
        let (stop_tx, stop_rx) = unsync::oneshot::channel();
        let cmd = CommandCenter::start(cfg.clone(), &handle, stop_tx);

        // start uds master server
        let master = Master {
            cfg: cfg,
            cmd: cmd,
        };
        Builder::build(master, lst.incoming(), &handle).run();

        // run loop
        match core.run(stop_rx) {
            Ok(success) => success,
            Err(_) => false,
        }
    }
}

impl Drop for Master {
    fn drop(&mut self) {
        self.cfg.master.remove_files();
    }
}

struct MasterClient {
    cmd: Address<CommandCenter>,
    sink: Sink<MasterClientSink>,
}

#[derive(Debug)]
enum MasterClientMessage {
    Request(MasterRequest),
}

impl MasterClient {

    fn hb(&self, ctx: &mut Context<Self>) {
        let fut = Timeout::new(Duration::new(1, 0), ctx.handle())
            .unwrap()
            .ctxfuture()
            .then(|_, srv: &mut MasterClient, ctx: &mut Context<Self>| {
                srv.sink.send_buffered(MasterResponse::Pong);
                srv.hb(ctx);
                fut::ok(())
            });
        ctx.spawn(fut);
    }

    fn handle_error(&mut self, err: CommandError) {
        match err {
            CommandError::NotReady =>
                self.sink.send_buffered(MasterResponse::ErrorNotReady),
            CommandError::UnknownService =>
                self.sink.send_buffered(MasterResponse::ErrorUnknownService),
            CommandError::ServiceStopped =>
                self.sink.send_buffered(MasterResponse::ErrorServiceStopped),
            CommandError::Service(err) => match err {
                ServiceOperationError::Starting =>
                    self.sink.send_buffered(MasterResponse::ErrorServiceStarting),
                ServiceOperationError::Reloading =>
                    self.sink.send_buffered(MasterResponse::ErrorServiceReloading),
                ServiceOperationError::Stopping =>
                    self.sink.send_buffered(MasterResponse::ErrorServiceStopping),
                ServiceOperationError::Running =>
                    self.sink.send_buffered(MasterResponse::ErrorServiceRunning),
                ServiceOperationError::Stopped =>
                    self.sink.send_buffered(MasterResponse::ErrorServiceStopped),
                ServiceOperationError::Failed =>
                    self.sink.send_buffered(MasterResponse::ErrorServiceFailed),
            }
        }
    }

    fn stop(&mut self, name: String, ctx: &mut Context<Self>) {
        info!("Client command: Stop service '{}'", name);

        cmd::StopService(name, true).send_to(&self.cmd).ctxfuture()
            .then(|res, srv: &mut MasterClient, _| {
                match res {
                    Err(_) => (),
                    Ok(Err(err)) => match err {
                        CommandError::ServiceStopped =>
                            srv.sink.send_buffered(MasterResponse::ServiceStarted),
                        _ => srv.handle_error(err),
                    }
                    Ok(Ok(_)) =>
                        srv.sink.send_buffered(MasterResponse::ServiceStopped),
                };
                fut::ok(())
            }).spawn(ctx);
    }

    fn reload(&mut self, name: String, ctx: &mut Context<Self>, graceful: bool)
    {
        info!("Client command: Reload service '{}'", name);

        cmd::ReloadService(name, graceful).send_to(&self.cmd).ctxfuture()
            .then(|res, srv: &mut MasterClient, _| {
                match res {
                    Err(_) => (),
                    Ok(Err(err)) => srv.handle_error(err),
                    Ok(Ok(res)) => match res {
                        ReloadStatus::Success =>
                            srv.sink.send_buffered(MasterResponse::ServiceStarted),
                        ReloadStatus::Failed =>
                            srv.sink.send_buffered(MasterResponse::ServiceFailed),
                        ReloadStatus::Stopping =>
                            srv.sink.send_buffered(MasterResponse::ErrorServiceStopping),
                    }
                }
                fut::ok(())
            }).spawn(ctx);
    }

    fn start_service(&mut self, name: String, ctx: &mut Context<Self>) {
        info!("Client command: Start service '{}'", name);

        cmd::StartService(name).send_to(&self.cmd).ctxfuture()
            .then(|res, srv: &mut MasterClient, _| {
                match res {
                    Err(_) => (),
                    Ok(Err(err)) => srv.handle_error(err),
                    Ok(Ok(res)) => match res {
                        StartStatus::Success =>
                            srv.sink.send_buffered(MasterResponse::ServiceStarted),
                        StartStatus::Failed =>
                            srv.sink.send_buffered(MasterResponse::ServiceFailed),
                        StartStatus::Stopping =>
                            srv.sink.send_buffered(MasterResponse::ErrorServiceStopping),
                    }
                }
                fut::ok(())
            }).spawn(ctx);
    }
}

struct MasterClientSink;

impl SinkService for MasterClientSink {

    type Service = MasterClient;
    type SinkMessage = Result<MasterResponse, io::Error>;
}

impl Service for MasterClient {

    type Context = Context<Self>;
    type Message = Result<MasterClientMessage, io::Error>;
    type Result = Result<(), ()>;

    fn start(&mut self, ctx: &mut Self::Context) {
        self.hb(ctx);
    }

    fn finished(&mut self, _: &mut Self::Context) -> Result<Async<()>, ()>
    {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, ctx: &mut Self::Context, msg: Self::Message) -> Result<Async<()>, ()>
    {
        match msg {
            Ok(MasterClientMessage::Request(req)) => {
                match req {
                    MasterRequest::Ping =>
                        self.sink.send_buffered(MasterResponse::Pong),
                    MasterRequest::Start(name) =>
                        self.start_service(name, ctx),
                    MasterRequest::Reload(name) =>
                        self.reload(name, ctx, true),
                    MasterRequest::Restart(name) =>
                        self.reload(name, ctx, false),
                    MasterRequest::Stop(name) =>
                        self.stop(name, ctx),
                    MasterRequest::Pause(name) => {
                        info!("Client command: Pause service '{}'", name);
                        cmd::PauseService(name).send_to(&self.cmd).ctxfuture()
                            .then(|res, srv: &mut MasterClient, _| {
                                match res {
                                    Err(_) => (),
                                    Ok(Err(err)) => srv.handle_error(err),
                                    Ok(Ok(_)) => srv.sink.send_buffered(MasterResponse::Done),
                                };
                                fut::ok(())
                            }).spawn(ctx);
                    }
                    MasterRequest::Resume(name) => {
                        info!("Client command: Resume service '{}'", name);
                        cmd::ResumeService(name).send_to(&self.cmd).ctxfuture()
                            .then(|res, srv: &mut MasterClient, _| {
                                match res {
                                    Err(_) => (),
                                    Ok(Err(err)) => srv.handle_error(err),
                                    Ok(Ok(_)) => srv.sink.send_buffered(MasterResponse::Done),
                                };
                                fut::ok(())
                            }).spawn(ctx);
                    }
                    MasterRequest::Status(name) => {
                        debug!("Client command: Service status '{}'", name);
                        cmd::StatusService(name).send_to(&self.cmd).ctxfuture()
                            .then(|res, srv: &mut MasterClient, _| {
                                match res {
                                    Err(_) => (),
                                    Ok(Err(err)) => srv.handle_error(err),
                                    Ok(Ok(status)) => srv.sink.send_buffered(
                                        MasterResponse::ServiceStatus(status)),
                                };
                                fut::ok(())
                            }).spawn(ctx);
                    }
                    MasterRequest::SPid(name) => {
                        debug!("Client command: Service status '{}'", name);
                        cmd::ServicePids(name).send_to(&self.cmd).ctxfuture()
                            .then(|res, srv: &mut MasterClient, _| {
                                match res {
                                    Err(_) => (),
                                    Ok(Err(err)) => srv.handle_error(err),
                                    Ok(Ok(pids)) => srv.sink.send_buffered(
                                        MasterResponse::ServiceWorkerPids(pids)),
                                };
                                fut::ok(())
                            }).spawn(ctx);
                    }
                    MasterRequest::Pid => {
                        self.sink.send_buffered(MasterResponse::Pid(
                            format!("{}", nix::unistd::getpid())));
                    },
                    MasterRequest::Version => {
                        self.sink.send_buffered(MasterResponse::Version(
                            format!("{} {}", PKG_INFO.name, PKG_INFO.version)));
                    },
                    MasterRequest::Quit => {
                        cmd::Stop.send_to(&self.cmd).ctxfuture()
                            .then(|_, srv: &mut MasterClient, _| {
                                srv.sink.send_buffered(MasterResponse::Done);
                                fut::ok(())
                            }).spawn(ctx);
                    }
                };
                Ok(Async::NotReady)
            },
            Err(_) => Err(()),
        }
    }
}

/// Codec for Master transport
struct MasterTransportCodec;

impl Decoder for MasterTransportCodec
{
    type Item = MasterClientMessage;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let size = {
            if src.len() < 2 {
                return Ok(None)
            }
            BigEndian::read_u16(src.as_ref()) as usize
        };

        if src.len() >= size + 2 {
            src.split_to(2);
            let buf = src.split_to(size);
            Ok(Some(MasterClientMessage::Request(json::from_slice::<MasterRequest>(&buf)?)))
        } else {
            Ok(None)
        }
    }
}

impl Encoder for MasterTransportCodec
{
    type Item = MasterResponse;
    type Error = io::Error;

    fn encode(&mut self, msg: MasterResponse, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let msg = json::to_string(&msg).unwrap();
        let msg_ref: &[u8] = msg.as_ref();

        dst.reserve(msg_ref.len() + 2);
        dst.put_u16::<BigEndian>(msg_ref.len() as u16);
        dst.put(msg_ref);

        Ok(())
    }
}

const HOST: &str = "127.0.0.1:57897";

/// Start master process
pub fn start(cfg: Config) -> bool {
    // init logging
    logging::init_logging(&cfg.logging);

    info!("Starting fectl process");

    // change working dir
    if let Err(err) = nix::unistd::chdir::<OsStr>(cfg.master.directory.as_ref()) {
        error!("Can not change directory {:?} err: {}", cfg.master.directory, err);
        return false
    }

    // sem
    match std::net::TcpListener::bind(HOST) {
        Ok(listener) => {
            std::mem::forget(listener);
        }
        Err(_) => {
            error!("Can not start: Another process is running.");
            return false
        }
    }

    // create commands listener and also check if service process is running
    let lst = match StdUnixListener::bind(&cfg.master.sock) {
        Ok(lst) => lst,
        Err(err) => match err.kind() {
            io::ErrorKind::PermissionDenied => {
                error!("Can not create socket file {:?} err: Permission denied.",
                       cfg.master.sock);
                return false
            },
            io::ErrorKind::AddrInUse => {
                match client::is_alive(&cfg.master) {
                    client::AliveStatus::Alive => {
                        error!("Can not start: Another process is running.");
                        return false
                    },
                    client::AliveStatus::NotResponding => {
                        error!("Master process is not responding.");
                        if let Some(pid) = cfg.master.load_pid() {
                            error!("Master process: (pid:{})", pid);
                        } else {
                            error!("Can not load pid of the master process.");
                        }
                        return false
                    },
                    client::AliveStatus::NotAlive => {
                        // remove socket and try again
                        let _ = std::fs::remove_file(&cfg.master.sock);
                        match StdUnixListener::bind(&cfg.master.sock) {
                            Ok(lst) => lst,
                            Err(err) => {
                                error!("Can not create listener socket: {}", err);
                                return false
                            }
                        }
                    }
                }
            }
            _ => {
                error!("Can not create listener socket: {}", err);
                return false
            }
        }
    };

    // try to save pid
    if let Err(err) = cfg.master.save_pid() {
        error!("Can not write pid file {:?} err: {}", cfg.master.pid, err);
        return false
    }

    // set uid
    if let Some(uid) = cfg.master.uid {
        if let Err(err) = nix::unistd::setuid(uid) {
            error!("Can not set process uid, err: {}", err);
            return false
        }
    }

    // set gid
    if let Some(gid) = cfg.master.gid {
        if let Err(err) = nix::unistd::setgid(gid) {
            error!("Can not set process gid, err: {}", err);
            return false
        }
    }

    let daemon = cfg.master.daemon;
    if daemon {
        if let Err(err) = nix::unistd::daemon(true, false) {
            error!("Can not daemonize process: {}", err);
            return false
        }

        // close stdin
        let _ = nix::unistd::close(libc::STDIN_FILENO);

        // redirect stdout and stderr
        if let Some(ref stdout) = cfg.master.stdout {
            match std::fs::OpenOptions::new().append(true).create(true).open(stdout)
            {
                Ok(f) => {
                    let _ = nix::unistd::dup2(f.as_raw_fd(), libc::STDOUT_FILENO);
                }
                Err(err) =>
                    error!("Can open stdout file {}: {}", stdout, err),
            }
        }
        if let Some(ref stderr) = cfg.master.stderr {
            match std::fs::OpenOptions::new().append(true).create(true).open(stderr)
            {
                Ok(f) => {
                    let _ = nix::unistd::dup2(f.as_raw_fd(), libc::STDERR_FILENO);

                },
                Err(err) => error!("Can open stderr file {}: {}", stderr, err)
            }
        }

        // continue start process
        nix::sys::stat::umask(nix::sys::stat::Mode::from_bits(0o22).unwrap());
    }

    // start
    let result = Master::start(cfg, lst);

    if !daemon {
        println!("");
    }

    result
}
