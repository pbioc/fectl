use std;
use std::io;
use std::rc::Rc;
use std::ffi::OsStr;
use std::time::Duration;
use std::thread;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixListener as StdUnixListener;

use nix;
use libc;
use serde_json as json;
use byteorder::{BigEndian , ByteOrder};
use bytes::{BytesMut, BufMut};
use futures::Stream;
use tokio_core::reactor::Timeout;
use tokio_uds::{UnixStream, UnixListener};
use tokio_io::codec::{Encoder, Decoder};

use actix::prelude::*;

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

impl Actor for Master {
    type Context = Context<Self>;
}

struct NetStream(UnixStream, std::os::unix::net::SocketAddr);

impl ResponseType for NetStream {
    type Item = ();
    type Error = ();
}

impl StreamHandler<NetStream, io::Error> for Master {}

impl Handler<NetStream, io::Error> for Master {

    fn handle(&mut self, msg: NetStream, _: &mut Context<Self>) -> Response<Self, NetStream>
    {
        let cmd = self.cmd.clone();
        let _: () = MasterClient{cmd: cmd}.framed(msg.0, MasterTransportCodec);
        Self::empty()
    }
}

impl Drop for Master {
    fn drop(&mut self) {
        self.cfg.master.remove_files();
    }
}

struct MasterClient {
    cmd: Address<CommandCenter>,
}

impl Actor for MasterClient {
    type Context = FramedContext<Self>;
}

impl FramedActor for MasterClient {
    type Io = UnixStream;
    type Codec= MasterTransportCodec;
}

impl MasterClient {

    fn hb(&self, ctx: &mut FramedContext<Self>) {
        let fut = Timeout::new(Duration::new(1, 0), Arbiter::handle())
            .unwrap()
            .actfuture()
            .then(|_, act: &mut MasterClient, ctx: &mut FramedContext<Self>| {
                if let Ok(_) = ctx.send(MasterResponse::Pong) {
                    act.hb(ctx);
                }
                fut::ok(())
            });
        ctx.spawn(fut);
    }

    fn handle_error(&mut self, err: CommandError, ctx: &mut FramedContext<Self>) {
        let _ = match err {
            CommandError::NotReady =>
                ctx.send(MasterResponse::ErrorNotReady),
            CommandError::UnknownService =>
                ctx.send(MasterResponse::ErrorUnknownService),
            CommandError::ServiceStopped =>
                ctx.send(MasterResponse::ErrorServiceStopped),
            CommandError::Service(err) => match err {
                ServiceOperationError::Starting =>
                    ctx.send(MasterResponse::ErrorServiceStarting),
                ServiceOperationError::Reloading =>
                    ctx.send(MasterResponse::ErrorServiceReloading),
                ServiceOperationError::Stopping =>
                    ctx.send(MasterResponse::ErrorServiceStopping),
                ServiceOperationError::Running =>
                    ctx.send(MasterResponse::ErrorServiceRunning),
                ServiceOperationError::Stopped =>
                    ctx.send(MasterResponse::ErrorServiceStopped),
                ServiceOperationError::Failed =>
                    ctx.send(MasterResponse::ErrorServiceFailed),
            }
        };
    }

    fn stop(&mut self, name: String, ctx: &mut FramedContext<Self>) {
        info!("Client command: Stop service '{}'", name);

        self.cmd.call(self, cmd::StopService(name, true)).then(|res, srv, ctx| {
            match res {
                Err(_) => (),
                Ok(Err(err)) => match err {
                    CommandError::ServiceStopped => {
                        let _ = ctx.send(MasterResponse::ServiceStarted);
                    },
                    _ => srv.handle_error(err, ctx),
                }
                Ok(Ok(_)) => {
                    let _ = ctx.send(MasterResponse::ServiceStopped);
                }
            };
            fut::ok(())
        }).spawn(ctx);
    }

    fn reload(&mut self, name: String, ctx: &mut FramedContext<Self>, graceful: bool)
    {
        info!("Client command: Reload service '{}'", name);

        self.cmd.call(self, cmd::ReloadService(name, graceful)).then(|res, srv, ctx| {
            match res {
                Err(_) => (),
                Ok(Err(err)) => srv.handle_error(err, ctx),
                Ok(Ok(res)) => {
                    let _ = match res {
                        ReloadStatus::Success =>
                            ctx.send(MasterResponse::ServiceStarted),
                        ReloadStatus::Failed =>
                            ctx.send(MasterResponse::ServiceFailed),
                        ReloadStatus::Stopping =>
                            ctx.send(MasterResponse::ErrorServiceStopping),
                    };
                }
            }
            fut::ok(())
        }).spawn(ctx);
    }

    fn start_service(&mut self, name: String, ctx: &mut FramedContext<Self>) {
        info!("Client command: Start service '{}'", name);

        self.cmd.call(self, cmd::StartService(name)).then(|res, srv, ctx| {
            match res {
                Err(_) => (),
                Ok(Err(err)) => srv.handle_error(err, ctx),
                Ok(Ok(res)) => {
                    let _ = match res {
                        StartStatus::Success =>
                            ctx.send(MasterResponse::ServiceStarted),
                        StartStatus::Failed =>
                            ctx.send(MasterResponse::ServiceFailed),
                        StartStatus::Stopping =>
                            ctx.send(MasterResponse::ErrorServiceStopping),
                    };
                }
            }
            fut::ok(())
        }).spawn(ctx);
    }
}

impl ResponseType for MasterRequest {
    type Item = ();
    type Error = ();
}

impl StreamHandler<MasterRequest, io::Error> for MasterClient {

    fn started(&mut self, ctx: &mut FramedContext<Self>) {
        self.hb(ctx);
    }

    fn finished(&mut self, ctx: &mut FramedContext<Self>) {
        ctx.stop()
    }
}

impl Handler<MasterRequest, io::Error> for MasterClient {

    fn error(&mut self, _: io::Error, ctx: &mut FramedContext<Self>) {
        ctx.stop()
    }

    fn handle(&mut self, msg: MasterRequest, ctx: &mut FramedContext<Self>)
              -> Response<Self, MasterRequest>
    {
        match msg {
            MasterRequest::Ping => {
                let _ = ctx.send(MasterResponse::Pong);
            },
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
                self.cmd.call(self, cmd::PauseService(name)).then(|res, srv, ctx| {
                    match res {
                        Err(_) => (),
                        Ok(Err(err)) => srv.handle_error(err, ctx),
                        Ok(Ok(_)) => {
                            let _ = ctx.send(MasterResponse::Done);
                        },
                    };
                    fut::ok(())
                }).spawn(ctx);
            }
            MasterRequest::Resume(name) => {
                info!("Client command: Resume service '{}'", name);
                self.cmd.call(self, cmd::ResumeService(name)).then(|res, srv, ctx| {
                    match res {
                        Err(_) => (),
                        Ok(Err(err)) => srv.handle_error(err, ctx),
                        Ok(Ok(_)) => {
                            let _ = ctx.send(MasterResponse::Done);
                        },
                    };
                    fut::ok(())
                }).spawn(ctx);
            }
            MasterRequest::Status(name) => {
                debug!("Client command: Service status '{}'", name);
                self.cmd.call(self, cmd::StatusService(name)).then(|res, srv, ctx| {
                    match res {
                        Err(_) => (),
                        Ok(Err(err)) => srv.handle_error(err, ctx),
                        Ok(Ok(status)) => {
                            let _ = ctx.send(
                                MasterResponse::ServiceStatus(status));
                        },
                    };
                    fut::ok(())
                }).spawn(ctx);
            }
            MasterRequest::SPid(name) => {
                debug!("Client command: Service status '{}'", name);
                self.cmd.call(self, cmd::ServicePids(name)).then(|res, srv, ctx| {
                    match res {
                        Err(_) => (),
                        Ok(Err(err)) => srv.handle_error(err, ctx),
                        Ok(Ok(pids)) => {
                            let _ = ctx.send(
                                MasterResponse::ServiceWorkerPids(pids));
                        },
                    };
                    fut::ok(())
                }).spawn(ctx);
            }
            MasterRequest::Pid => {
                let _ = ctx.send(MasterResponse::Pid(
                    format!("{}", nix::unistd::getpid())));
            },
            MasterRequest::Version => {
                let _ = ctx.send(MasterResponse::Version(
                    format!("{} {}", PKG_INFO.name, PKG_INFO.version)));
            },
            MasterRequest::Quit => {
                self.cmd.call(self, cmd::Stop).then(|_, _, ctx| {
                    let _ = ctx.send(MasterResponse::Done);
                    fut::ok(())
                }).spawn(ctx);
            }
        };
        Self::empty()
    }
}


/// Codec for Master transport
struct MasterTransportCodec;

impl Decoder for MasterTransportCodec
{
    type Item = MasterRequest;
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
            Ok(Some(json::from_slice::<MasterRequest>(&buf)?))
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

    // check if other app is running
    for idx in 0..10 {
        match std::net::TcpListener::bind(HOST) {
            Ok(listener) => {
                std::mem::forget(listener);
                break
            }
            Err(_) => {
                if idx == 8 {
                    error!("Can not start: Another process is running.");
                    return false
                }
                info!("Trying to bind address, sleep for 5 seconds");
                thread::sleep(Duration::new(5, 0));
            }
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

    let cfg = Rc::new(cfg);

    // create uds stream
    let lst = match UnixListener::from_listener(lst, Arbiter::handle()) {
        Ok(lst) => lst,
        Err(err) => {
            error!("Can not create unix socket listener {:?}", err);
            return false
        }
    };

    // command center
    let cmd = CommandCenter::start(cfg.clone());

    // start uds master server
    let _: () = Master::create(|ctx| {
        ctx.add_stream(lst.incoming().map(|(s, a)| NetStream(s, a)));
        Master{cfg: cfg, cmd: cmd}}
    );

    if !daemon {
        println!("");
    }
    true
}
