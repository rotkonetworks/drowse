//! "Your server as a function" (Eriksen, 2013) on the standard tower traits.
//!
//! Verbs are plain functions lifted into [`tower_service::Service`]; filters
//! are [`tower_layer::Layer`]s; the daemon is a composed stack. Every future
//! produced here is [`Ready`] at construction, so the transport drives a call
//! with one no-op-waker poll — the tower contract without an async runtime.

use drowse_api::{RpcRequest, RpcResponse};
use std::convert::Infallible;
use std::future::{Future, Ready, ready};
use std::task::{Context, Poll, Waker};

pub use tower_layer::Layer;
pub use tower_service::Service;

/// The uniform future type of every drowse service and layer.
pub type RpcFuture = Ready<Result<RpcResponse, Infallible>>;

/// Object-safe alias for a boxed verb or composed stack.
pub type BoxRpc = Box<
    dyn Service<RpcRequest, Response = RpcResponse, Error = Infallible, Future = RpcFuture>
        + Send,
>;

/// Lift a plain function into a Service. The function IS the server.
pub struct Handler<F>(F);

pub fn handler<F>(f: F) -> Handler<F>
where
    F: FnMut(RpcRequest) -> RpcResponse,
{
    Handler(f)
}

impl<F> Service<RpcRequest> for Handler<F>
where
    F: FnMut(RpcRequest) -> RpcResponse,
{
    type Response = RpcResponse;
    type Error = Infallible;
    type Future = RpcFuture;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RpcRequest) -> RpcFuture {
        ready(Ok((self.0)(req)))
    }
}

/// Method-name router; itself a Service, so layers wrap it like any other.
#[derive(Default)]
pub struct Router {
    routes: Vec<(&'static str, BoxRpc)>,
}

impl Router {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn route(
        mut self,
        method: &'static str,
        svc: impl Service<
            RpcRequest,
            Response = RpcResponse,
            Error = Infallible,
            Future = RpcFuture,
        > + Send
        + 'static,
    ) -> Self {
        self.routes.push((method, Box::new(svc)));
        self
    }
}

impl Service<RpcRequest> for Router {
    type Response = RpcResponse;
    type Error = Infallible;
    type Future = RpcFuture;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RpcRequest) -> RpcFuture {
        for (m, svc) in &mut self.routes {
            if *m == req.method {
                return svc.call(req);
            }
        }
        let id = req.id.clone();
        ready(Ok(RpcResponse::err(
            id,
            -32601,
            format!("method not found: {}", req.method),
            None,
        )))
    }
}

/// Drive one call to completion. Our futures are Ready by construction; a
/// Pending here is a bug in a service implementation, not a runtime concern.
pub fn drive<S>(svc: &mut S, req: RpcRequest) -> RpcResponse
where
    S: Service<RpcRequest, Response = RpcResponse, Error = Infallible>,
{
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    match svc.poll_ready(&mut cx) {
        Poll::Ready(Ok(())) => {}
        Poll::Ready(Err(e)) => match e {},
        Poll::Pending => unreachable!("drowse services are always ready"),
    }
    let mut fut = std::pin::pin!(svc.call(req));
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(Ok(resp)) => resp,
        Poll::Ready(Err(e)) => match e {},
        Poll::Pending => unreachable!("drowse futures are Ready by construction"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str) -> RpcRequest {
        RpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(serde_json::json!(1)),
            method: method.into(),
            params: None,
        }
    }

    #[test]
    fn router_dispatches_and_404s() {
        let mut r = Router::new().route(
            "ping",
            handler(|q: RpcRequest| RpcResponse::ok(q.id, serde_json::json!("pong"))),
        );
        let ok = drive(&mut r, req("ping"));
        assert_eq!(ok.result, Some(serde_json::json!("pong")));
        let missing = drive(&mut r, req("nope"));
        assert_eq!(missing.error.unwrap().code, -32601);
    }

    /// A filter in the paper's sense: wraps any inner service.
    struct Tag<S>(S);
    impl<S> Service<RpcRequest> for Tag<S>
    where
        S: Service<RpcRequest, Response = RpcResponse, Error = Infallible, Future = RpcFuture>,
    {
        type Response = RpcResponse;
        type Error = Infallible;
        type Future = RpcFuture;
        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            self.0.poll_ready(cx)
        }
        fn call(&mut self, req: RpcRequest) -> RpcFuture {
            let mut resp = drive(&mut self.0, req);
            if let Some(v) = resp.result.as_mut().and_then(|v| v.as_object_mut()) {
                v.insert("tagged".into(), serde_json::json!(true));
            }
            ready(Ok(resp))
        }
    }

    #[test]
    fn filters_compose() {
        let inner = handler(|q: RpcRequest| RpcResponse::ok(q.id, serde_json::json!({"x": 1})));
        let mut stack = Tag(inner);
        let resp = drive(&mut stack, req("any"));
        assert_eq!(resp.result.unwrap()["tagged"], serde_json::json!(true));
    }
}
