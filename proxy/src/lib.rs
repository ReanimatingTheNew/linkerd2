#![cfg_attr(feature = "cargo-clippy", allow(clone_on_ref_ptr))]
#![cfg_attr(feature = "cargo-clippy", allow(new_without_default_derive))]
// #![deny(warnings)]
#![recursion_limit="128"]

extern crate bytes;
extern crate conduit_proxy_controller_grpc;
extern crate convert;
extern crate env_logger;
extern crate deflate;
#[macro_use]
extern crate futures;
extern crate futures_mpsc_lossy;
extern crate futures_watch;
extern crate h2;
extern crate http;
extern crate httparse;
extern crate hyper;
extern crate ipnet;
#[cfg(target_os = "linux")]
extern crate libc;
#[macro_use]
extern crate log;
#[cfg_attr(test, macro_use)]
extern crate indexmap;
extern crate prost;
extern crate prost_types;
#[cfg(test)]
#[macro_use]
extern crate quickcheck;
extern crate rand;
extern crate regex;
extern crate tokio_connect;
extern crate tokio;
extern crate tokio_executor;
extern crate tokio_timer;
extern crate tokio_threadpool;
extern crate tokio_reactor;
extern crate tower_balance;
extern crate tower_buffer;
extern crate tower_discover;
extern crate tower_grpc;
extern crate tower_h2;
extern crate tower_reconnect;
extern crate tower_service;
extern crate conduit_proxy_router;
extern crate tower_util;
extern crate tower_in_flight_limit;
extern crate trust_dns_resolver;

use futures::*;
use futures::future::Executor;

use std::error::Error;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use indexmap::IndexSet;
use tokio::{
    executor::{
        current_thread::{self, CurrentThread},
        thread_pool::{self, ThreadPool, Sender},
    },
    reactor,
    runtime::{Runtime, TaskExecutor},
};
use tokio_timer::timer;
use tower_service::NewService;
use tower_fn::*;
use conduit_proxy_router::{Recognize, Router, Error as RouteError};

pub mod app;
mod bind;
pub mod config;
mod connection;
pub mod control;
pub mod ctx;
mod dns;
mod drain;
mod inbound;
mod logging;
mod map_err;
mod outbound;
pub mod telemetry;
mod transparency;
mod transport;
pub mod timeout;
mod tower_fn; // TODO: move to tower-fn

use bind::Bind;
use connection::BoundPort;
use inbound::Inbound;
use map_err::MapErr;
use transparency::{HttpBody, Server};
pub use transport::{GetOriginalDst, SoOriginalDst};
use outbound::Outbound;

/// Runs a sidecar proxy.
///
/// The proxy binds two listeners:
///
/// - a private socket (TCP or UNIX) for outbound requests to other instances;
/// - and a public socket (TCP and optionally TLS) for inbound requests from other
///   instances.
///
/// The public listener forwards requests to a local socket (TCP or UNIX).
///
/// The private listener routes requests to service-discovery-aware load-balancer.
///

pub struct Main<G> {
    config: config::Config,

    control_listener: BoundPort,
    inbound_listener: BoundPort,
    outbound_listener: BoundPort,
    metrics_listener: BoundPort,

    get_original_dst: G,

    // runtime: CurrentThread<tokio_timer::Timer<tokio_reactor::Reactor>>,
    // handle: reactor::Handle,
}

impl<G> Main<G>
where
    G: GetOriginalDst + Clone + Send + 'static,
{
    pub fn new(config: config::Config, get_original_dst: G) -> Self {

        let control_listener = BoundPort::new(config.control_listener.addr)
            .expect("controller listener bind");
        let inbound_listener = BoundPort::new(config.public_listener.addr)
            .expect("public listener bind");
        let outbound_listener = BoundPort::new(config.private_listener.addr)
            .expect("private listener bind");

        // let reactor = reactor::Reactor::new()
        //     .expect("reactor");
        // // The reactor itself will get consumed by timer,
        // // so we keep a handle to communicate with it.
        // let handle = reactor.handle();
        // let timer = timer::Timer::new(reactor);
        // let timer_handle = timer.handle();
        // let mut runtime = CurrentThread::new_with_park(timer);

        let metrics_listener = BoundPort::new(config.metrics_listener.addr)
            .expect("metrics listener bind");
        Main {
            config,
            control_listener,
            inbound_listener,
            outbound_listener,
            metrics_listener,
            get_original_dst,
            // runtime,
            // handle,
        }
    }


    pub fn control_addr(&self) -> SocketAddr {
        self.control_listener.local_addr()
    }

    pub fn inbound_addr(&self) -> SocketAddr {
        self.inbound_listener.local_addr()
    }

    pub fn outbound_addr(&self) -> SocketAddr {
        self.outbound_listener.local_addr()
    }

    // pub fn handle(&self) -> TaskExecutor {
    //     // self.runtime.executor()
    //     unimplemented!()
    // }

    pub fn metrics_addr(&self) -> SocketAddr {
        self.metrics_listener.local_addr()
    }

    pub fn run_until<F>(self, shutdown_signal: F)
    where
        F: Future<Item = (), Error = ()> + 'static,
        F: Send,
    {
        let process_ctx = ctx::Process::new(&self.config);

        let Main {
            config,
            control_listener,
            inbound_listener,
            outbound_listener,
            metrics_listener,
            get_original_dst,
            // runtime: mut core,
            // handle,
        } = self;



        // The reactor will (eventually) run on this thread. Our "pool" of
        // one worker thread will use this reactor.
        let pool_reactor = reactor::Reactor::new()
            .expect("initialize main reactor")
            .background()
            .expect("start main reactor");
        let reactor_handle = pool_reactor.handle().clone();

        // Since we're using a single worker thread, we only need to create
        // one timer.
        // We have to construct the timer in the closure passed to
        // `custom_park`, since we can't move it into the closure, and the closure
        // has to return the `Park` instance, so we'll use this to hold it
        // temporarily until it's created. This is basically the same as what
        // `tokio::runtime::Builder::build()` does:
        // https://github.com/tokio-rs/tokio/blob/363b207f2b6c25857c70d76b303356db87212f59/src/runtime/builder.rs#L90-L119
        // XXX: This is rather convoluted and I wish we didn't have to do it....
        let take_timer_handle: Arc<Mutex<Option<timer::Handle>>> =
            Arc::new(Mutex::new(None));
        let put_timer_handle = take_timer_handle.clone();

        // XXX: We would prefer to use the `CurrentThread` executor; however,
        // Hyper requires executors that are `Send`, so we have to use the
        // theadpool, as its' `Sender` implements `Send`, whicih
        // `CurrentThread::TaskExecutor` does not.
        let mut pool = thread_pool::Builder::new()
            .name_prefix("conduit-worker-")
            // TODO: eventually, we may want to make the size of the threadpool
            //       configurable to support use cases such as ingress.
            .pool_size(1)
            .around_worker(move |w, enter| {
                // Take the timer handle that should have already been created in
                // the `custom_park` closure.
                let timer_handle = take_timer_handle
                    .lock().expect("lock lazy timer handle in around_worker")
                    .take().expect("timer should already have been initialized");
                // Set our timer and reactor as the default timer and reactor
                // for the(single) worker thread in the "pool".
                // NOTE: if we wanted to run more than one worker thread in our
                //       threadpool (read: if we made the pool size
                //       configurable), we will probably want each worker to
                //       have its own timer instead.
                tokio_reactor::with_default(&reactor_handle, enter, |enter| {
                    timer::with_default(&timer_handle, enter, |_| {
                        w.run();
                    })
                });
            })
            .custom_park(move |_| {
                use tokio_threadpool::park::DefaultPark;
                let timer = timer::Timer::new(DefaultPark::new());
                // Put the timer handle in the mutex so it can be passed
                // to `around_worker`.
                let mut handle = put_timer_handle
                    .lock()
                    .expect("lock lazy timer handle in custom_park");
                *handle = Some(timer.handle());
                timer
            })
            .build();

        let control_host_and_port = config.control_host_and_port.clone();

        info!("using controller at {:?}", control_host_and_port);
        info!("routing on {:?}", outbound_listener.local_addr());
        info!(
            "proxying on {:?} to {:?}",
            inbound_listener.local_addr(),
            config.private_forward
        );
        info!(
            "serving Prometheus metrics on {:?}",
            metrics_listener.local_addr(),
        );
        info!(
            "protocol detection disabled for inbound ports {:?}",
            config.inbound_ports_disable_protocol_detection,
        );
        info!(
            "protocol detection disabled for outbound ports {:?}",
            config.outbound_ports_disable_protocol_detection,
        );

        let (sensors, telemetry) = telemetry::new(
            &process_ctx,
            config.event_buffer_capacity,
            config.metrics_retain_idle,
        );

        let dns_config = dns::Config::from_system_config()
            .unwrap_or_else(|e| {
                // TODO: Make DNS configuration infallible.
                panic!("invalid DNS configuration: {:?}", e);
            });

        let (control, control_bg) = control::new(dns_config.clone(), config.pod_namespace.clone());

        let mut executor = pool.sender().clone();
        let (drain_tx, drain_rx) = drain::channel();

        let bind = Bind::new(executor.clone()).with_sensors(sensors.clone());

        // Setup the public listener. This will listen on a publicly accessible
        // address and listen for inbound connections that should be forwarded
        // to the managed application (private destination).
        let inbound = {
            let ctx = ctx::Proxy::inbound(&process_ctx);

            let bind = bind.clone().with_ctx(ctx.clone());

            let default_addr = config.private_forward.map(|a| a.into());

            let fut = serve(
                inbound_listener,
                Inbound::new(default_addr, bind),
                config.inbound_router_capacity,
                config.private_connect_timeout,
                config.inbound_ports_disable_protocol_detection,
                ctx,
                sensors.clone(),
                get_original_dst.clone(),
                drain_rx.clone(),
                // &handle,
                &executor,
            );
            ::logging::context_future("inbound", fut)
        };

        // Setup the private listener. This will listen on a locally accessible
        // address and listen for outbound requests that should be routed
        // to a remote service (public destination).
        let outbound = {
            let ctx = ctx::Proxy::outbound(&process_ctx);
            let bind = bind.clone().with_ctx(ctx.clone());
            let outgoing = Outbound::new(bind, control, config.bind_timeout);
            let fut = serve(
                outbound_listener,
                outgoing,
                config.outbound_router_capacity,
                config.public_connect_timeout,
                config.outbound_ports_disable_protocol_detection,
                ctx,
                sensors,
                get_original_dst,
                drain_rx,
                // &handle,
                &executor,
            );
            ::logging::context_future("outbound", fut)
        };

        trace!("running");

        let (_tx, controller_shutdown_signal) = futures::sync::oneshot::channel::<()>();
        {
            thread::Builder::new()
                .name("controller-client".into())
                .spawn(move || {
                    use conduit_proxy_controller_grpc::tap::server::TapServer;
                    let mut enter = tokio_executor::enter()
                        .expect("multiple executors on control thread");
                    let reactor = reactor::Reactor::new()
                        .expect("initialize controller reactor");
                    let handle = reactor.handle();
                    let timer = timer::Timer::new(reactor);
                    let timer_handle = timer.handle();
                    // Use the `CurrentThread` executor to ensure that all the
                    // controller client's tasks stay on this thread.
                    let mut rt = CurrentThread::new_with_park(timer);

                    // Configure the default tokio runtime for the control thread.
                    tokio_reactor::with_default(&handle, &mut enter, |enter| {
                        timer::with_default(&timer_handle, enter, |enter| {
                            let mut default_executor = current_thread::TaskExecutor::current();
                            tokio_executor::with_default(&mut default_executor, enter, |enter| {
                                let (taps, observe) = control::Observe::new(100);
                                let new_service = TapServer::new(observe);

                                let server = serve_control(
                                    control_listener,
                                    new_service,
                                    &handle,
                                );

                                let telemetry = telemetry
                                    .make_control(&taps, &handle)
                                    .expect("bad news in telemetry town");

                                let metrics_server = telemetry
                                    .serve_metrics(metrics_listener);

                                let client = control_bg.bind(
                                    control_host_and_port,
                                    dns_config,
                                    &current_thread::TaskExecutor::current(),
                                );

                                let fut = client.join4(
                                    server.map_err(|_| {}),
                                    telemetry,
                                    metrics_server.map_err(|_| {}),
                                ).map(|_| {});
                                let fut = ::logging::context_future("controller-client", fut);
                                rt.spawn(Box::new(fut));
                                trace!("controller client: spawned everything except for shutdown");
                                let shutdown = controller_shutdown_signal.then(|_| {
                                    trace!("controller shutdown signal fired");
                                    Ok::<(), ()>(())
                                });
                                rt.enter(enter).block_on(shutdown).expect("controller api");
                                trace!("controller client over")
                            })
                        })
                    })
                })
                .expect("initialize controller api thread");
        }
        trace!("controller thread spawned");
        // let mut enter = tokio_executor::enter()
        //     .expect("multiple executors on main thread");

        // tokio_reactor::with_default(&handle, &mut enter, |enter| {
        //     timer::with_default(&timer_handle, enter, |enter| {
        //         let mut default_executor = pool.sender().clone();
        //         tokio_executor::with_default(&mut default_executor, enter, |enter| {

        let fut = inbound
            .join(outbound)
            .map(|_| ())
            .map_err(|err| error!("main error: {:?}", err));

        pool.spawn(Box::new(fut));
        trace!("main task spawned");
        //         })
        //     })
        // });
        let shutdown_signal = shutdown_signal.and_then(move |()| {
            debug!("shutdown signaled");
            drain_tx.drain().and_then(|()| pool.shutdown())
        });
        shutdown_signal.wait().unwrap();
        trace!("shutdown complete");
    }
}

fn serve<R, B, E, F, G>(
    bound_port: BoundPort,
    recognize: R,
    router_capacity: usize,
    tcp_connect_timeout: Duration,
    disable_protocol_detection_ports: IndexSet<u16>,
    proxy_ctx: Arc<ctx::Proxy>,
    sensors: telemetry::Sensors,
    get_orig_dst: G,
    drain_rx: drain::Watch,
    // handle: &reactor::Handle,
    executor: &Sender,
) -> Box<Future<Item = (), Error = io::Error> + Send + 'static>
where
    B: tower_h2::Body + Send + Default + 'static,
    B::Data: Send,
    <B::Data as ::bytes::IntoBuf>::Buf: Send,
    E: Error + Send + 'static,
    F: Error + Send + 'static,
    R: Recognize<
        Request = http::Request<HttpBody>,
        Response = http::Response<telemetry::sensor::http::ResponseBody<B>>,
        Error = E,
        RouteError = F,
    >
        + Send + 'static,
    R::Service: Send,
    R::Key: Send,
    <R::Service as tower_service::Service>::Future: Send,
    G: GetOriginalDst + Send + 'static,
{
    let router = Router::new(recognize, router_capacity);
    let stack = Arc::new(NewServiceFn::new(move || {
        // Clone the router handle
        let router = router.clone();

        // Map errors to appropriate response error codes.
        let map_err = MapErr::new(router, |e| {
            match e {
                RouteError::Route(r) => {
                    error!(" turning route error: {} into 500", r);
                    http::StatusCode::INTERNAL_SERVER_ERROR
                }
                RouteError::Inner(i) => {
                    error!("turning {} into 500", i);
                    http::StatusCode::INTERNAL_SERVER_ERROR
                }
                RouteError::NotRecognized => {
                    error!("turning route not recognized error into 500");
                    http::StatusCode::INTERNAL_SERVER_ERROR
                }
                RouteError::NoCapacity(capacity) => {
                    // TODO For H2 streams, we should probably signal a protocol-level
                    // capacity change.
                    error!("router at capacity ({}); returning a 503", capacity);
                    http::StatusCode::SERVICE_UNAVAILABLE
                }
            }
        });

        // Install the request open timestamp module at the very top
        // of the stack, in order to take the timestamp as close as
        // possible to the beginning of the request's lifetime.
        telemetry::sensor::http::TimestampRequestOpen::new(map_err)
    }));

    let listen_addr = bound_port.local_addr();
    let server = Server::new(
        listen_addr,
        proxy_ctx,
        sensors,
        get_orig_dst,
        stack,
        tcp_connect_timeout,
        disable_protocol_detection_ports,
        drain_rx.clone(),
        executor.clone(),
    );


    let accept = bound_port.listen_and_fold(
        &tokio::reactor::Handle::current(),
        (),
        move |(), (connection, remote_addr)| {
            server.serve(connection, remote_addr);
            Ok(())
        },
    );

    let accept_until = Cancelable {
        future: accept,
        canceled: false,
    };

    // As soon as we get a shutdown signal, the listener
    // is canceled immediately.
    Box::new(drain_rx.watch(accept_until, |accept| {
        accept.canceled = true;
    }))
}

/// Can cancel a future by setting a flag.
///
/// Used to 'watch' the accept futures, and close the listeners
/// as soon as the shutdown signal starts.
struct Cancelable<F> {
    future: F,
    canceled: bool,
}

impl<F> Future for Cancelable<F>
where
    F: Future<Item=()>,
{
    type Item = ();
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if self.canceled {
            Ok(().into())
        } else {
            self.future.poll()
        }
    }
}

fn serve_control<N, B>(
    bound_port: BoundPort,
    new_service: N,
    handle: &reactor::Handle,
) -> Box<Future<Item = (), Error = io::Error> + 'static>
where
    B: tower_h2::Body + 'static,
    N: NewService<Request = http::Request<tower_h2::RecvBody>, Response = http::Response<B>> + 'static,
{
    let executor = current_thread::TaskExecutor::current();
    let h2_builder = h2::server::Builder::default();
    let server = tower_h2::Server::new(new_service, h2_builder, executor.clone());
    bound_port.listen_and_fold_local(
        handle,
        (server, executor.clone()),
        move |(server, executor), (session, _)| {
            let s = server.serve(session).map_err(|_| ());
            let s = ::logging::context_future("serve_control", s);

            executor.execute(Box::new(s));


            future::ok((server, executor))
        },
    )
}
