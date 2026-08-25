//! Using the result of a remote call before it has arrived.
//!
//! A method that returns a [client](super::Client) of another remotable trait hands out
//! access to a second object. Ordinarily the caller must wait for that client to arrive
//! before it can call anything on it, so reaching the second object costs one round trip
//! before its first call is even sent.
//!
//! Pipelining removes that wait. The caller creates the client and its
//! [request receiver](super::ReqReceiver) itself and passes the request receiver *into*
//! the call. It can then use its client immediately: the calls travel behind the request that
//! is still on its way. The server attaches the returned object
//! to the handed over request receiver, and the queued calls are served without ever
//! having waited for a reply.
//!
//! A pipelined call may itself hand out a further object, so a chain of objects can be
//! reached within a single round trip.
//!
//! # Enabling pipelining
//!
//! Mark a method returning a client with `#[pipelinable]` in the
//! [`remote`](super::remote) attribute's trait:
//!
//! ```ignore
//! #[rtc::remote]
//! pub trait Directory {
//!     #[pipelinable]
//!     async fn open_counter(&self, name: String) -> Result<CounterClient, OpenError>;
//! }
//! ```
//!
//! This generates the following twin, on the trait and on its client:
//!
//! ```ignore
//! async fn open_counter_pipelined(
//!     &self, name: String, __req_rx: CounterReqReceiver,
//! ) -> rtc::Call<Result<(), OpenError>>;
//! ```
//!
//! The request receiver takes the place of the returned client, and the value of the
//! result becomes `()`, since the caller already holds the client it would have received.
//! Awaiting the returned [`Call`](super::Call) yields once the object has been attached
//! to the request receiver, or the error of the method if it refused to hand one out.
//!
//! The twin is a provided method, so the object implementing the trait implements only
//! `open_counter`; attaching the client it returns to the handed over request receiver is
//! done for it.
//!
//! The ordinary `open_counter` keeps working and is unaffected, as is the request format,
//! so a method can gain `#[pipelinable]` without breaking existing clients.
//!
//! # Starting a call without awaiting it
//!
//! Every method also gets a `<name>_call` twin, whether or not it is pipelinable:
//!
//! ```ignore
//! #[rtc::remote]
//! pub trait Counter {
//!     async fn increase(&mut self, by: u32) -> Result<(), CallError>;
//!
//!     // generated:
//!     async fn increase_call(&mut self, by: u32) -> rtc::Call<Result<(), CallError>>;
//! }
//! ```
//!
//! Awaiting the twin only sends the request; awaiting the returned
//! [`Call`](super::Call) yields the result of the method. Starting several calls before
//! awaiting any of them is what keeps a series of calls to a single round trip.
//!
//! # Example
//!
//! A `Directory` hands out `Counter` objects. Opening the counter and the three calls
//! made on it are all sent before any reply is awaited, so the whole exchange takes a
//! single round trip instead of four.
//!
//! Note that awaiting each call individually, as in `counter.increase(20).await?`, still
//! works but waits for that call's reply before sending the next one. Pipelining then
//! only saves the round trip for obtaining the counter. Use the `<name>_call` twins,
//! here through [`calls!`](super::calls), to send the whole series at once.
//!
//! ```
//! use std::sync::Arc;
//! use tokio::sync::RwLock;
//! use remoc::prelude::*;
//! use remoc::rtc::CallError;
//!
//! #[derive(Debug, serde::Serialize, serde::Deserialize)]
//! pub enum OpenError {
//!     Denied,
//!     Call(CallError),
//! }
//!
//! impl From<CallError> for OpenError {
//!     fn from(err: CallError) -> Self {
//!         Self::Call(err)
//!     }
//! }
//!
//! #[rtc::remote]
//! pub trait Counter {
//!     async fn value(&self) -> Result<u32, CallError>;
//!     async fn increase(&mut self, by: u32) -> Result<(), CallError>;
//! }
//!
//! pub struct CounterObj {
//!     value: u32,
//! }
//!
//! impl Counter for CounterObj {
//!     async fn value(&self) -> Result<u32, CallError> {
//!         Ok(self.value)
//!     }
//!
//!     async fn increase(&mut self, by: u32) -> Result<(), CallError> {
//!         self.value += by;
//!         Ok(())
//!     }
//! }
//!
//! #[rtc::remote]
//! pub trait Directory {
//!     /// Opens the counter of the specified name.
//!     #[pipelinable]
//!     async fn open_counter(&self, name: String) -> Result<CounterClient, OpenError>;
//! }
//!
//! pub struct DirectoryObj;
//!
//! impl Directory for DirectoryObj {
//!     async fn open_counter(&self, name: String) -> Result<CounterClient, OpenError> {
//!         if name != "allowed" {
//!             return Err(OpenError::Denied);
//!         }
//!
//!         let obj = Arc::new(RwLock::new(CounterObj { value: 0 }));
//!         let (server, client) = CounterServerSharedMut::new(obj);
//!         tokio::spawn(server.serve());
//!
//!         Ok(client)
//!     }
//! }
//!
//! // Opens the counter and uses it, all within a single round trip.
//! async fn open_and_count(dir: &DirectoryClient) -> Result<u32, OpenError> {
//!     // Create the counter client and its request receiver up front.
//!     let (mut counter, counter_rx) = CounterClient::new();
//!
//!     // Every call is started before the result of any of them is awaited.
//!     // The 4 calls together thus take only a single roundtrip time.
//!     let value = rtc::calls!(
//!         dir.open_counter_pipelined("allowed".to_string(), counter_rx);
//!         counter.increase_call(20);
//!         counter.increase_call(45);
//!         counter.value_call()
//!     );
//!
//!     Ok(value)
//! }
//!
//! // This would be run on the client.
//! async fn client(mut rx: rch::base::Receiver<DirectoryClient>) {
//!     let dir = rx.recv().await.unwrap().unwrap();
//!     assert_eq!(open_and_count(&dir).await.unwrap(), 65);
//! }
//!
//! // This would be run on the server.
//! async fn server(mut tx: rch::base::Sender<DirectoryClient>) {
//!     let (server, client) = DirectoryServerShared::new(Arc::new(DirectoryObj));
//!     tokio::spawn(server.serve());
//!     tx.send(client).await.unwrap();
//! }
//! # tokio_test::block_on(remoc::doctest::client_server(server, client));
//! ```
//!
//! ### Without the `calls!` macro
//!
//! [`calls!`](super::calls) is a convenience for a straight series of calls.
//! Written by hand, the function above is:
//!
//! ```ignore
//! async fn open_and_count(dir: &DirectoryClient) -> Result<u32, OpenError> {
//!     let (mut counter, counter_rx) = CounterClient::new();
//!
//!     // Awaiting a `<name>_call` twin only sends the request and yields a `Call`;
//!     // it does not wait for the result. Thus all four requests are sent here.
//!     let session = dir.open_counter_pipelined("allowed".to_string(), counter_rx).await;
//!     let increased_by_20 = counter.increase_call(20).await;
//!     let increased_by_45 = counter.increase_call(45).await;
//!     let value = counter.value_call().await;
//!
//!     // Only now is anything awaited, once the replies come in.
//!     session.await?;
//!     increased_by_20.await?;
//!     increased_by_45.await?;
//!     Ok(value.await?)
//! }
//! ```
//!
//! Doing it by hand is necessary as soon as the series is not straight, for example when
//! a call is made conditionally, when the calls are collected in a loop, or when a result
//! must be inspected before the remaining results are awaited.
//!
//! To avoid roundtrips start every call before awaiting any of them.
//! Awaiting a call before starting the next one would turn the series back into one round
//! trip per call.
//!
//! A [`Call`](super::Call) that is dropped without being awaited cancels its call, so
//! the started calls must be kept until their results are collected.
//! Alternatively, use [`Call::spawn`](super::Call::spawn) to let it run unattended.
//!
//!
//! ### Differing error types
//!
//! [`calls!`](super::calls) propagates every result with `?`, so each call's error type
//! must convert into the error type of the enclosing function. Where it does not,
//! [`map_err`](super::CallFutureExt::map_err) brings them to a common one. It applies to
//! the future returned by a `<name>_call` twin, i.e. before the call is awaited:
//!
//! ```ignore
//! let value = rtc::calls!(
//!     dir.open_counter_pipelined("allowed".to_string(), counter_rx).map_err(MyError::from);
//!     counter.increase_call(20).map_err(MyError::from);
//!     counter.value_call().map_err(MyError::from)
//! );
//! ```
//!
//! Obtaining the result as a value rather than propagating it needs an async block that
//! states the error type:
//!
//! ```ignore
//! let value = async { Ok::<_, MyError>(rtc::calls!(/* ... */)) }.await;
//! ```
//!
//! # Order of execution
//!
//! The requests are transferred in the order the calls were started, but whether the
//! server processes them in that order depends on how it serves:
//!
//!  * [`Server`](super::Server), [`ServerRef`](super::ServerRef) and
//!    [`ServerRefMut`](super::ServerRefMut) process one call at a time, in order.
//!  * [`ServerShared`](super::ServerShared) and [`ServerSharedMut`](super::ServerSharedMut)
//!    dispatch calls concurrently, thus calls to methods taking `&self` may take effect out
//!    of order. A call to a method taking `&mut self` is always ordered against every other call.
//!
//! Use [`Client::set_sequential`](super::Client::set_sequential) on the client whose calls
//! must take effect in the order they were made. The server then processes them one after
//! another, whichever variant it is.
//!
//! A call that fails does not keep the calls behind it from being processed. Use
//! [`Client::set_stop_on_error`](super::Client::set_stop_on_error) to have the server stop
//! serving once one of this client's calls returns an error, so that the calls queued
//! behind it fail as well.
