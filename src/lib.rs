// Copyright (c) 2013-2015 Sandstorm Development Group, Inc. and contributors
// Licensed under the MIT License:
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.

//! An implementation of the [Cap'n Proto remote procedure call](https://capnproto.org/rpc.html)
//! protocol. Includes all [Level 1](https://capnproto.org/rpc.html#protocol-features) features.
//!
//! # Example
//!
//! ```capnp
//! # Cap'n Proto schema
//! interface Foo {
//!     identity @0 (x: UInt32) -> (y: UInt32);
//! }
//! ```
//!
//! ```ignore
//! // Rust server defining an implementation of Foo.
//! struct FooImpl;
//! impl foo::Server for FooImpl {
//!     fn identity(&mut self,
//!                 params: foo::IdentityParams,
//!                 mut results: foo::IdentityResults)
//!                 -> Promise<(), ::capnp::Error>
//!     {
//!         let x = pry!(params.get()).get_x();
//!         results.get().set_y(x);
//!         Promise::ok(())
//!     }
//! }
//! ```
//!
//! ```ignore
//! // Rust client calling a remote implementation of Foo.
//! let mut request = foo_client.identity_request();
//! request.get().set_x(123);
//! let promise = request.send().promise.map(|response| {
//!     println!("results = {}", try!(response.get()).get_y());
//!     Ok(())
//! });
//! ```
//!
//! For a more complete example, see https://github.com/dwrensha/capnp-rpc-rust/tree/master/examples/calculator


extern crate capnp;
#[macro_use] extern crate futures;
extern crate tokio_core;
extern crate capnp_futures;

use futures::{Future};
use futures::sync::oneshot;
use capnp::Error;
use capnp::capability::Promise;
use capnp::private::capability::{ClientHook, ServerHook};
use std::cell::RefCell;
use std::rc::Rc;

use task_set::TaskSet;

/// Code generated from [rpc.capnp]
/// (https://github.com/sandstorm-io/capnproto/blob/master/c%2B%2B/src/capnp/rpc.capnp).
pub mod rpc_capnp {
  include!(concat!(env!("OUT_DIR"), "/rpc_capnp.rs"));
}

/// Code generated from [rpc-twoparty.capnp]
/// (https://github.com/sandstorm-io/capnproto/blob/master/c%2B%2B/src/capnp/rpc-twoparty.capnp).
pub mod rpc_twoparty_capnp {
  include!(concat!(env!("OUT_DIR"), "/rpc_twoparty_capnp.rs"));
}

#[macro_export]
macro_rules! pry {
    ($expr:expr) => (
        match $expr {
            ::std::result::Result::Ok(val) => val,
            ::std::result::Result::Err(err) => {
                return ::capnp::capability::Promise::err(::std::convert::From::from(err))
            }
        })
}

mod broken;
mod local;
mod queued;
mod rpc;
mod stack;
mod task_set;
pub mod twoparty;

pub trait OutgoingMessage {
    fn get_body<'a>(&'a mut self) -> ::capnp::Result<::capnp::any_pointer::Builder<'a>>;
    fn get_body_as_reader<'a>(&'a self) -> ::capnp::Result<::capnp::any_pointer::Reader<'a>>;

    /// Sends the message. Returns a promise for the message that resolves once the send has completed.
    /// Dropping the returned promise does *not* cancel the send.
    fn send(self: Box<Self>)
            -> Promise<::capnp::message::Builder<::capnp::message::HeapAllocator>, ::capnp::Error>;

    fn take(self: Box<Self>) -> ::capnp::message::Builder<::capnp::message::HeapAllocator>;
}

pub trait IncomingMessage {
    fn get_body<'a>(&'a self) -> ::capnp::Result<::capnp::any_pointer::Reader<'a>>;
}

pub trait Connection<VatId> {
    fn get_peer_vat_id(&self) -> VatId;
    fn new_outgoing_message(&mut self, first_segment_word_size: u32) -> Box<OutgoingMessage>;

    /// Waits for a message to be received and returns it.  If the read stream cleanly terminates,
    /// returns None. If any other problem occurs, returns an Error.
    fn receive_incoming_message(&mut self) -> Promise<Option<Box<IncomingMessage>>, Error>;

    // Waits until all outgoing messages have been sent, then shuts down the outgoing stream. The
    // returned promise resolves after shutdown is complete.
    fn shutdown(&mut self) -> Promise<(), Error>;
}

pub trait VatNetwork<VatId> {
    /// Returns None if `hostId` refers to the local vat.
    fn connect(&mut self, host_id: VatId) -> Option<Box<Connection<VatId>>>;

    /// Waits for the next incoming connection and return it.
    fn accept(&mut self) -> Promise<Box<Connection<VatId>>, ::capnp::Error>;
}

/// A portal to objects available on the network.
///
/// The RPC implemententation sits on top of an implementation of `VatNetwork`, which
/// determines how to form connections between vats. The RPC implementation determines
/// how to use such connections to manage object references and make method calls.
///
/// At the moment, this is all rather more general than it needs to be, because the only
/// implementation of `VatNetwork` is `twoparty::VatNetwork`. However, eventually we
/// will need to have more sophistocated `VatNetwork` implementations, in order to support
/// [level 3](https://capnproto.org/rpc.html#protocol-features) features.
pub struct RpcSystem<VatId> where VatId: 'static {
    network: Box<::VatNetwork<VatId>>,

    bootstrap_cap: Box<ClientHook>,

    // XXX To handle three or more party networks, this should be a map from connection pointers
    // to connection states.
    connection_state: Rc<RefCell<Option<Rc<rpc::ConnectionState<VatId>>>>>,

    spawner: tokio_core::reactor::Handle,
    _spawn_canceller: oneshot::Sender<()>,
//    tasks: TaskSet<(), Error>,
    handle: ::task_set::TaskSetHandle<(), Error>
}

impl <VatId> RpcSystem <VatId> {
    /// Constructs a new `RpcSystem` with the given network and bootstrap capability.
    pub fn new(
        network: Box<::VatNetwork<VatId>>,
        bootstrap: Option<::capnp::capability::Client>,
        spawner: tokio_core::reactor::Handle) -> RpcSystem<VatId>
    {
        let bootstrap_cap = match bootstrap {
            Some(cap) => cap.hook,
            None => broken::new_cap(Error::failed("no bootstrap capabiity".to_string())),
        };
        let (mut handle, tasks) = TaskSet::new(Box::new(SystemTaskReaper));

        let (sender, receiver) = oneshot::channel();
        let receiver = receiver.map_err(|e| e.into());

        let mut result = RpcSystem {
            network: network,
            bootstrap_cap: bootstrap_cap,
            connection_state: Rc::new(RefCell::new(None)),
            spawner: spawner.clone(),
            _spawn_canceller: sender,

//            tasks: tasks,
            handle: handle.clone(),
        };

        spawner.spawn(tasks.join(receiver).map_err(|e| { println!("{}", e); ()}).map(|_| ()));
        let accept_loop = result.accept_loop();
        handle.add(accept_loop);
        result
    }

    /// Connects to the given vat and returns its bootstrap interface.
    pub fn bootstrap<T>(&mut self, vat_id: VatId) -> T
        where T: ::capnp::capability::FromClientHook
    {
        let connection = match self.network.connect(vat_id) {
            Some(connection) => connection,
            None => {
                return T::new(self.bootstrap_cap.clone());
            }
        };
        let connection_state =
            RpcSystem::get_connection_state(self.connection_state.clone(),
                                            self.bootstrap_cap.clone(),
                                            connection, self.handle.clone(), self.spawner.clone());

        let hook = rpc::ConnectionState::bootstrap(connection_state.clone());
        T::new(hook)
    }

    // not really a loop, because it doesn't need to be for the two party case
    fn accept_loop(&mut self) -> Promise<(), Error> {
        let connection_state_ref = self.connection_state.clone();
        let bootstrap_cap = self.bootstrap_cap.clone();
        let handle = self.handle.clone();
        let spawner = self.spawner.clone();
        Promise::from_future(self.network.accept().map(move |connection| {
            RpcSystem::get_connection_state(connection_state_ref,
                                            bootstrap_cap,
                                            connection,
                                            handle,
                                            spawner);
        }))
    }

    fn get_connection_state(connection_state_ref: Rc<RefCell<Option<Rc<rpc::ConnectionState<VatId>>>>>,
                            bootstrap_cap: Box<ClientHook>,
                            connection: Box<::Connection<VatId>>,
                            mut handle: ::task_set::TaskSetHandle<(), Error>,
                            spawner: tokio_core::reactor::Handle)
                            -> Rc<rpc::ConnectionState<VatId>>
    {

        // TODO this needs to be updated once we allow more general VatNetworks.
        let result = match &*connection_state_ref.borrow() {
            &Some(ref connection_state) => {
                // return early.
                return connection_state.clone()
            }
            &None => {
                let (on_disconnect_fulfiller, on_disconnect_promise) =
                    oneshot::channel::<Promise<(), Error>>();
                let connection_state_ref1 = connection_state_ref.clone();
                handle.add(on_disconnect_promise.then(move |shutdown_promise| {
                    *connection_state_ref1.borrow_mut() = None;
                    match shutdown_promise {
                        Ok(s) => s,
                        Err(e) => Promise::err(Error::failed(format!("{}", e))),
                    }
                }));
                rpc::ConnectionState::new(bootstrap_cap, connection, on_disconnect_fulfiller, spawner)
            }
        };
        *connection_state_ref.borrow_mut() = Some(result.clone());
        result
    }
}

/// Hook that allows local implementations of interfaces to be passed to the RPC system
/// so that they can be called remotely.
///
/// To use this, you need to do the following dance:
///
/// ```ignore
/// let client = foo::ToClient::new(FooImpl).from_server::<::capnp_rpc::Server>());
/// ```
pub struct Server;

impl ServerHook for Server {
    fn new_client(server: Box<::capnp::capability::Server>) -> ::capnp::capability::Client {
        ::capnp::capability::Client::new(Box::new(local::Client::new(server)))
    }
}


/// Converts a promise for a client into a client that queues up any calls that arrive
/// before the promise resolves.
// TODO: figure out a better way to allow construction of promise clients.
pub fn new_promise_client<T, F>(client_future: F) -> T
    where T: ::capnp::capability::FromClientHook,
          F: Future<Item=::capnp::capability::Client, Error=Error> + 'static,
{
    T::new(Box::new(queued::Client::new(Promise::from_future(client_future.map(|c| c.hook)))))
}


struct ForkedPromiseInner<F> where F: Future {
    original_future: F,
    state: ForkedPromiseState<F::Item, F::Error>,
}

enum ForkedPromiseState<T, E> {
    Waiting(Vec<::futures::task::Task>),
    Done(Result<T, E>),
}

struct ForkedPromise<F> where F: Future {
    inner: Rc<RefCell<ForkedPromiseInner<F>>>,
}

impl <F> Clone for ForkedPromise<F> where F: Future {
    fn clone(&self) -> ForkedPromise<F> {
        ForkedPromise {
            inner: self.inner.clone()
        }
    }
}

impl <F> ForkedPromise<F> where F: Future {
    fn new(f: F) -> ForkedPromise<F> {
        ForkedPromise {
            inner: Rc::new(RefCell::new(ForkedPromiseInner {
                original_future: f,
                state: ForkedPromiseState::Waiting(Vec::new()),
            }))
        }
    }
}

impl<F> Drop for ForkedPromise<F> where F: Future {
    fn drop(&mut self) {
        let ForkedPromiseInner { ref mut original_future, ref mut state } = *self.inner.borrow_mut();
        match *state {
            ForkedPromiseState::Waiting(ref mut waiters) => {
                for waiter in waiters {
                    waiter.unpark();
                }
            }
            _ => (),
        };
    }
}

impl <F> Future for ForkedPromise<F>
    where F: Future, F::Item: Clone, F::Error: Clone,
{
    type Item = F::Item;
    type Error = F::Error;

    fn poll(&mut self) -> ::futures::Poll<Self::Item, Self::Error> {
        let ForkedPromiseInner { ref mut original_future, ref mut state } = *self.inner.borrow_mut();
        let done_val = match *state {
            ForkedPromiseState::Waiting(ref mut waiters) => {
                let done_val = match original_future.poll() {
                    Ok(::futures::Async::NotReady) => {
                        waiters.push(::futures::task::park());
                        return Ok(::futures::Async::NotReady)
                    }
                    Ok(::futures::Async::Ready(v)) => {
                        Ok(v)
                    }
                    Err(e) => {
                        Err(e)
                    }
                };
                for task in waiters {
                    task.unpark();
                }
                done_val
            }
            ForkedPromiseState::Done(Ok(ref v)) => return Ok(::futures::Async::Ready(v.clone())),
            ForkedPromiseState::Done(Err(ref e)) => return Err(e.clone()),
        };
        *state = ForkedPromiseState::Done(done_val.clone());
        match done_val {
            Ok(v) => Ok(::futures::Async::Ready(v)),
            Err(e) => Err(e),
        }
    }
}

struct SystemTaskReaper;
impl ::task_set::TaskReaper<(), Error> for SystemTaskReaper {
    fn task_failed(&mut self, error: Error) {
        println!("ERROR: {}", error);
    }
}

struct AttachFuture<F, T> where F: Future {
    original_future: F,
    value: Option<T>,
}

impl <F, T> Future for AttachFuture<F, T>
    where F: Future,
{
    type Item = F::Item;
    type Error = F::Error;

    fn poll(&mut self) -> ::futures::Poll<Self::Item, Self::Error> {
        let result = self.original_future.poll();
        if let Ok(::futures::Async::Ready(_)) = result {
            self.value.take();
        }
        result
    }
}

trait Attach : Future {
    fn attach<T>(self, value: T) -> AttachFuture<Self, T>
        where Self: Sized
    {
        AttachFuture {
            original_future: self,
            value: Some(value),
        }
    }
}

impl <F> Attach for F where F: Future {}
