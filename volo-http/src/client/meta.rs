use std::{error::Error, time::Duration};

use http::header;
use http_body::Body;
use motore::service::Service;
use volo::context::Context;

use crate::{
    context::ClientContext,
    error::client::{status_error, timeout, ClientError},
    request::ClientRequest,
    response::ClientResponse,
};

#[derive(Clone)]
pub struct MetaService<S> {
    inner: S,
    config: MetaServiceConfig,
}

#[derive(Clone)]
pub(super) struct MetaServiceConfig {
    pub default_timeout: Option<Duration>,
    pub fail_on_error_status: bool,
}

impl<S> MetaService<S> {
    pub(super) fn new(inner: S, config: MetaServiceConfig) -> Self {
        Self { inner, config }
    }
}

impl<S, B> Service<ClientContext, ClientRequest<B>> for MetaService<S>
where
    S: Service<ClientContext, ClientRequest<B>, Response = ClientResponse, Error = ClientError>
        + Send
        + Sync
        + 'static,
    B: Body + Send + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn Error + Send + Sync>> + 'static,
{
    type Response = S::Response;
    type Error = S::Error;

    async fn call(
        &self,
        cx: &mut ClientContext,
        mut req: ClientRequest<B>,
    ) -> Result<Self::Response, Self::Error> {
        // `Content-Length` must be set here because the body may be changed in previous layer(s).
        let exact_len = req.body().size_hint().exact();
        if let Some(len) = exact_len {
            if len > 0 && req.headers().get(header::CONTENT_LENGTH).is_none() {
                req.headers_mut().insert(header::CONTENT_LENGTH, len.into());
            }
        }

        tracing::trace!("sending request: {} {}", req.method(), req.uri());
        tracing::trace!("headers: {:?}", req.headers());

        let request_timeout = cx
            .rpc_info()
            .config()
            .timeout
            .or(self.config.default_timeout);
        let fut = self.inner.call(cx, req);
        let res = match request_timeout {
            Some(duration) => {
                let sleep = tokio::time::sleep(duration);
                tokio::select! {
                    res = fut => res,
                    _ = sleep => {
                        return Err(timeout());
                    }
                }
            }
            None => fut.await,
        };

        if !self.config.fail_on_error_status {
            return res;
        }

        let resp = res?;

        let status = resp.status();
        if status.is_client_error() || status.is_server_error() {
            Err(status_error(status))
        } else {
            Ok(resp)
        }
    }
}
