use std::{cell::RefCell, error::Error, sync::Arc, time::Duration};

use faststr::FastStr;
use http::{
    header::{self, HeaderMap, HeaderName, HeaderValue},
    uri::{Scheme, Uri},
    Method,
};
use metainfo::{MetaInfo, METAINFO};
use motore::{
    layer::{Identity, Layer, Stack},
    service::Service,
};
use paste::paste;
use volo::{
    client::MkClient,
    context::Context,
    loadbalance::MkLbLayer,
    net::{
        dial::{DefaultMakeTransport, MakeTransport},
        Address,
    },
};

use self::{
    loadbalance::{DefaultLB, LbConfig},
    meta::{MetaService, MetaServiceConfig},
    transport::{ClientConfig, ClientTransport, ClientTransportConfig},
    utils::TargetBuilder,
};
use crate::{
    context::{
        client::{CalleeName, CallerName, Config},
        ClientContext,
    },
    error::{
        client::{builder_error, no_address, ClientError},
        BoxError,
    },
    request::ClientRequest,
    response::ClientResponse,
};

pub mod loadbalance;
mod meta;
mod request_builder;
mod transport;
pub mod utils;

pub use request_builder::RequestBuilder;

#[doc(hidden)]
pub mod prelude {
    pub use super::{Client, ClientBuilder};
}

const PKG_NAME_WITH_VER: &str = concat!(env!("CARGO_PKG_NAME"), '/', env!("CARGO_PKG_VERSION"));

pub type ClientMetaService = MetaService<ClientTransport>;
pub type DefaultClient = Client<ClientMetaService>;

pub struct ClientBuilder<IL, OL, C, LB> {
    http_config: ClientConfig,
    builder_config: BuilderConfig,
    connector: DefaultMakeTransport,
    callee_name: FastStr,
    caller_name: FastStr,
    target: TargetBuilder,
    headers: HeaderMap,
    inner_layer: IL,
    outer_layer: OL,
    mk_client: C,
    mk_lb: LB,
    #[cfg(feature = "__tls")]
    tls_config: Option<volo::net::tls::TlsConnector>,
}

struct BuilderConfig {
    caller_name_mode: CallerName,
    callee_name_mode: CalleeName,
    timeout: Option<Duration>,
    stat_enable: bool,
    fail_on_error_status: bool,
    #[cfg(feature = "__tls")]
    disable_tls: bool,
}

impl Default for BuilderConfig {
    fn default() -> Self {
        Self {
            caller_name_mode: CallerName::default(),
            callee_name_mode: CalleeName::default(),
            timeout: None,
            stat_enable: true,
            fail_on_error_status: false,
            #[cfg(feature = "__tls")]
            disable_tls: false,
        }
    }
}

impl ClientBuilder<Identity, Identity, DefaultMkClient, DefaultLB> {
    /// Create a new client builder.
    pub fn new() -> Self {
        Self {
            http_config: Default::default(),
            builder_config: Default::default(),
            connector: Default::default(),
            callee_name: FastStr::empty(),
            caller_name: FastStr::empty(),
            target: Default::default(),
            headers: Default::default(),
            inner_layer: Identity::new(),
            outer_layer: Identity::new(),
            mk_client: DefaultMkClient,
            mk_lb: Default::default(),
            #[cfg(feature = "__tls")]
            tls_config: None,
        }
    }
}

impl Default for ClientBuilder<Identity, Identity, DefaultMkClient, DefaultLB> {
    fn default() -> Self {
        Self::new()
    }
}

impl<IL, OL, C, LB, DISC> ClientBuilder<IL, OL, C, LbConfig<LB, DISC>> {
    /// Set load balancer for the client.
    pub fn load_balance<NLB>(
        self,
        load_balance: NLB,
    ) -> ClientBuilder<IL, OL, C, LbConfig<NLB, DISC>> {
        ClientBuilder {
            http_config: self.http_config,
            builder_config: self.builder_config,
            connector: self.connector,
            callee_name: self.callee_name,
            caller_name: self.caller_name,
            target: self.target,
            headers: self.headers,
            inner_layer: self.inner_layer,
            outer_layer: self.outer_layer,
            mk_client: self.mk_client,
            mk_lb: self.mk_lb.load_balance(load_balance),
            #[cfg(feature = "__tls")]
            tls_config: self.tls_config,
        }
    }

    /// Set service discover for the client.
    pub fn discover<NDISC>(self, discover: NDISC) -> ClientBuilder<IL, OL, C, LbConfig<LB, NDISC>> {
        ClientBuilder {
            http_config: self.http_config,
            builder_config: self.builder_config,
            connector: self.connector,
            callee_name: self.callee_name,
            caller_name: self.caller_name,
            target: self.target,
            headers: self.headers,
            inner_layer: self.inner_layer,
            outer_layer: self.outer_layer,
            mk_client: self.mk_client,
            mk_lb: self.mk_lb.discover(discover),
            #[cfg(feature = "__tls")]
            tls_config: self.tls_config,
        }
    }
}

impl<IL, OL, C, LB> ClientBuilder<IL, OL, C, LB> {
    /// This is unstable now and may be changed in the future.
    #[doc(hidden)]
    pub fn client_maker<C2>(self, new_mk_client: C2) -> ClientBuilder<IL, OL, C2, LB> {
        ClientBuilder {
            http_config: self.http_config,
            builder_config: self.builder_config,
            connector: self.connector,
            callee_name: self.callee_name,
            caller_name: self.caller_name,
            target: self.target,
            headers: self.headers,
            inner_layer: self.inner_layer,
            outer_layer: self.outer_layer,
            mk_client: new_mk_client,
            mk_lb: self.mk_lb,
            #[cfg(feature = "__tls")]
            tls_config: self.tls_config,
        }
    }

    /// Add a new inner layer to the client.
    ///
    /// The layer's `Service` should be `Send + Sync + Clone + 'static`.
    ///
    /// # Order
    ///
    /// Assume we already have two layers: foo and bar. We want to add a new layer baz.
    ///
    /// The current order is: foo -> bar (the request will come to foo first, and then bar).
    ///
    /// After we call `.layer_inner(baz)`, we will get: foo -> bar -> baz.
    ///
    /// The overall order for layers is: outer -> LoadBalance -> [inner] -> transport.
    pub fn layer_inner<Inner>(self, layer: Inner) -> ClientBuilder<Stack<Inner, IL>, OL, C, LB> {
        ClientBuilder {
            http_config: self.http_config,
            builder_config: self.builder_config,
            connector: self.connector,
            callee_name: self.callee_name,
            caller_name: self.caller_name,
            target: self.target,
            headers: self.headers,
            inner_layer: Stack::new(layer, self.inner_layer),
            outer_layer: self.outer_layer,
            mk_client: self.mk_client,
            mk_lb: self.mk_lb,
            #[cfg(feature = "__tls")]
            tls_config: self.tls_config,
        }
    }

    /// Add a new inner layer to the client.
    ///
    /// The layer's `Service` should be `Send + Sync + Clone + 'static`.
    ///
    /// # Order
    ///
    /// Assume we already have two layers: foo and bar. We want to add a new layer baz.
    ///
    /// The current order is: foo -> bar (the request will come to foo first, and then bar).
    ///
    /// After we call `.layer_inner_front(baz)`, we will get: baz -> foo -> bar.
    ///
    /// The overall order for layers is: outer -> LoadBalance -> [inner] -> transport.
    pub fn layer_inner_front<Inner>(
        self,
        layer: Inner,
    ) -> ClientBuilder<Stack<IL, Inner>, OL, C, LB> {
        ClientBuilder {
            http_config: self.http_config,
            builder_config: self.builder_config,
            connector: self.connector,
            callee_name: self.callee_name,
            caller_name: self.caller_name,
            target: self.target,
            headers: self.headers,
            inner_layer: Stack::new(self.inner_layer, layer),
            outer_layer: self.outer_layer,
            mk_client: self.mk_client,
            mk_lb: self.mk_lb,
            #[cfg(feature = "__tls")]
            tls_config: self.tls_config,
        }
    }

    /// Add a new outer layer to the client.
    ///
    /// The layer's `Service` should be `Send + Sync + Clone + 'static`.
    ///
    /// # Order
    ///
    /// Assume we already have two layers: foo and bar. We want to add a new layer baz.
    ///
    /// The current order is: foo -> bar (the request will come to foo first, and then bar).
    ///
    /// After we call `.layer_outer(baz)`, we will get: foo -> bar -> baz.
    ///
    /// The overall order for layers is: [outer] -> Timeout -> LoadBalance -> inner -> transport.
    pub fn layer_outer<Outer>(self, layer: Outer) -> ClientBuilder<IL, Stack<Outer, OL>, C, LB> {
        ClientBuilder {
            http_config: self.http_config,
            builder_config: self.builder_config,
            connector: self.connector,
            callee_name: self.callee_name,
            caller_name: self.caller_name,
            target: self.target,
            headers: self.headers,
            inner_layer: self.inner_layer,
            outer_layer: Stack::new(layer, self.outer_layer),
            mk_client: self.mk_client,
            mk_lb: self.mk_lb,
            #[cfg(feature = "__tls")]
            tls_config: self.tls_config,
        }
    }

    /// Add a new outer layer to the client.
    ///
    /// The layer's `Service` should be `Send + Sync + Clone + 'static`.
    ///
    /// # Order
    ///
    /// Assume we already have two layers: foo and bar. We want to add a new layer baz.
    ///
    /// The current order is: foo -> bar (the request will come to foo first, and then bar).
    ///
    /// After we call `.layer_outer_front(baz)`, we will get: baz -> foo -> bar.
    ///
    /// The overall order for layers is: outer -> LoadBalance -> [inner] -> transport.
    pub fn layer_outer_front<Outer>(
        self,
        layer: Outer,
    ) -> ClientBuilder<IL, Stack<OL, Outer>, C, LB> {
        ClientBuilder {
            http_config: self.http_config,
            builder_config: self.builder_config,
            connector: self.connector,
            callee_name: self.callee_name,
            caller_name: self.caller_name,
            target: self.target,
            headers: self.headers,
            inner_layer: self.inner_layer,
            outer_layer: Stack::new(self.outer_layer, layer),
            mk_client: self.mk_client,
            mk_lb: self.mk_lb,
            #[cfg(feature = "__tls")]
            tls_config: self.tls_config,
        }
    }

    /// Set a new load balance for the client.
    pub fn mk_load_balance<NLB>(self, mk_load_balance: NLB) -> ClientBuilder<IL, OL, C, NLB> {
        ClientBuilder {
            http_config: self.http_config,
            builder_config: self.builder_config,
            connector: self.connector,
            callee_name: self.callee_name,
            caller_name: self.caller_name,
            target: self.target,
            headers: self.headers,
            inner_layer: self.inner_layer,
            outer_layer: self.outer_layer,
            mk_client: self.mk_client,
            mk_lb: mk_load_balance,
            #[cfg(feature = "__tls")]
            tls_config: self.tls_config,
        }
    }

    /// Set the target server's name.
    pub fn callee_name<S>(&mut self, callee: S) -> &mut Self
    where
        S: Into<FastStr>,
    {
        self.callee_name = callee.into();
        self
    }

    /// Set the client's name sent to the server.
    pub fn caller_name<S>(&mut self, caller: S) -> &mut Self
    where
        S: Into<FastStr>,
    {
        self.caller_name = caller.into();
        self
    }

    /// Set the target address of the client.
    ///
    /// If there is no target specified when building a request, client will use this address.
    pub fn address<A>(
        &mut self,
        address: A,
        #[cfg(feature = "__tls")]
        #[cfg_attr(docsrs, doc(cfg(any(feature = "rustls", feature = "native-tls"))))]
        use_tls: bool,
    ) -> &mut Self
    where
        A: Into<Address>,
    {
        self.target = TargetBuilder::Address {
            addr: address.into(),
            #[cfg(feature = "__tls")]
            use_tls,
        };
        self
    }

    /// Set the target host of the client.
    ///
    /// If there is no target specified when building a request, client will use this address.
    ///
    /// If tls is enabled, the scheme will be set to `https`, otherwise it will be set to `http`.
    ///
    /// To specify scheme or port, use `scheme_host_and_port` instead.
    pub fn host<H>(&mut self, host: H) -> &mut Self
    where
        H: Into<FastStr>,
    {
        self.target = TargetBuilder::Host {
            scheme: None,
            host: host.into(),
            port: None,
        };
        self
    }

    /// Set the target scheme, host and port of the client.
    ///
    /// If there is no target specified when building a request, client will use this address.
    pub fn scheme_host_and_port<H>(
        &mut self,
        scheme: Option<Scheme>,
        host: H,
        port: Option<u16>,
    ) -> &mut Self
    where
        H: Into<FastStr>,
    {
        self.target = TargetBuilder::Host {
            scheme,
            host: host.into(),
            port,
        };
        self
    }

    /// Insert a header to the request.
    pub fn header<K, V>(&mut self, key: K, value: V) -> Result<&mut Self, ClientError>
    where
        K: TryInto<HeaderName>,
        K::Error: Error + Send + Sync + 'static,
        V: TryInto<HeaderValue>,
        V::Error: Error + Send + Sync + 'static,
    {
        self.headers.insert(
            key.try_into().map_err(builder_error)?,
            value.try_into().map_err(builder_error)?,
        );
        Ok(self)
    }

    /// Set tls config for the client.
    #[cfg(feature = "__tls")]
    #[cfg_attr(docsrs, doc(cfg(any(feature = "rustls", feature = "native-tls"))))]
    pub fn set_tls_config<T>(&mut self, tls_config: T) -> &mut Self
    where
        T: Into<volo::net::tls::TlsConnector>,
    {
        self.tls_config = Some(Into::into(tls_config));
        self
    }

    /// Get a reference to the default headers of the client.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Get a mutable reference to the default headers of the client.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    /// Get a reference to the HTTP configuration of the client.
    pub fn http_config(&self) -> &ClientConfig {
        &self.http_config
    }

    /// Get a mutable reference to the HTTP configuration of the client.
    pub fn http_config_mut(&mut self) -> &mut ClientConfig {
        &mut self.http_config
    }

    /// Set mode for setting `Host` in request headers, and server name when using TLS.
    ///
    /// Default is callee name.
    pub fn set_callee_name_mode(&mut self, mode: CalleeName) -> &mut Self {
        self.builder_config.callee_name_mode = mode;
        self
    }

    /// Set mode for setting `User-Agent` in request headers.
    ///
    /// Default is the current crate name and version.
    pub fn set_caller_name_mode(&mut self, mode: CallerName) -> &mut Self {
        self.builder_config.caller_name_mode = mode;
        self
    }

    /// This is unstable now and may be changed in the future.
    #[doc(hidden)]
    pub fn stat_enable(&mut self, enable: bool) -> &mut Self {
        self.builder_config.stat_enable = enable;
        self
    }

    /// Return `Err` rather than full `ClientResponse` when the response status code is 4xx or 5xx.
    ///
    /// Default is false.
    pub fn fail_on_error_status(&mut self, fail_on_error_status: bool) -> &mut Self {
        self.builder_config.fail_on_error_status = fail_on_error_status;
        self
    }

    /// Disable TLS for the client.
    ///
    /// Default is false, when TLS related feature is enabled, TLS is enabled by default.
    #[cfg(feature = "__tls")]
    #[cfg_attr(docsrs, doc(cfg(any(feature = "rustls", feature = "native-tls"))))]
    pub fn disable_tls(&mut self, disable: bool) -> &mut Self {
        self.builder_config.disable_tls = disable;
        self
    }

    /// Set whether HTTP/1 connections will write header names as title case at
    /// the socket level.
    ///
    /// Default is false.
    pub fn set_title_case_headers(&mut self, title_case_headers: bool) -> &mut Self {
        self.http_config.title_case_headers = title_case_headers;
        self
    }

    /// Set whether to support preserving original header cases.
    ///
    /// Currently, this will record the original cases received, and store them
    /// in a private extension on the `Response`. It will also look for and use
    /// such an extension in any provided `Request`.
    ///
    /// Since the relevant extension is still private, there is no way to
    /// interact with the original cases. The only effect this can have now is
    /// to forward the cases in a proxy-like fashion.
    ///
    /// Default is false.
    pub fn set_preserve_header_case(&mut self, preserve_header_case: bool) -> &mut Self {
        self.http_config.preserve_header_case = preserve_header_case;
        self
    }

    /// Set the maximum number of headers.
    ///
    /// When a response is received, the parser will reserve a buffer to store headers for optimal
    /// performance.
    ///
    /// If client receives more headers than the buffer size, the error "message header too large"
    /// is returned.
    ///
    /// Note that headers is allocated on the stack by default, which has higher performance. After
    /// setting this value, headers will be allocated in heap memory, that is, heap memory
    /// allocation will occur for each response, and there will be a performance drop of about 5%.
    ///
    /// Default is 100.
    pub fn set_max_headers(&mut self, max_headers: usize) -> &mut Self {
        self.http_config.max_headers = Some(max_headers);
        self
    }

    /// Set the maximum idle time for a connection.
    pub fn set_connect_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.connector.set_connect_timeout(Some(timeout));
        self
    }

    /// Set the maximum idle time for reading data from the connection.
    pub fn set_read_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.connector.set_read_timeout(Some(timeout));
        self
    }

    /// Set the maximum idle time for writing data to the connection.
    pub fn set_write_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.connector.set_write_timeout(Some(timeout));
        self
    }

    /// Set the maximin idle time for the request.
    ///
    /// The whole request includes connecting, writting, and reading the whole HTTP protocol
    /// headers (without reading response body).
    pub fn set_request_timeout(&mut self, timeout: Duration) -> &mut Self {
        self.builder_config.timeout = Some(timeout);
        self
    }

    /// Build the HTTP client.
    pub fn build(mut self) -> C::Target
    where
        IL: Layer<MetaService<ClientTransport>>,
        IL::Service: Send + Sync + 'static,
        LB: MkLbLayer,
        LB::Layer: Layer<IL::Service>,
        <LB::Layer as Layer<IL::Service>>::Service: Send + Sync,
        OL: Layer<<LB::Layer as Layer<IL::Service>>::Service>,
        OL::Service: Send + Sync + 'static,
        C: MkClient<Client<OL::Service>>,
    {
        let transport_config = ClientTransportConfig {
            stat_enable: self.builder_config.stat_enable,
            #[cfg(feature = "__tls")]
            disable_tls: self.builder_config.disable_tls,
        };
        let meta_config = MetaServiceConfig {
            default_timeout: self.builder_config.timeout,
            fail_on_error_status: self.builder_config.fail_on_error_status,
        };
        let transport = ClientTransport::new(
            self.http_config,
            transport_config,
            self.connector,
            #[cfg(feature = "__tls")]
            self.tls_config.unwrap_or_default(),
        );
        let meta_service = MetaService::new(transport, meta_config);
        let service = self.outer_layer.layer(
            self.mk_lb
                .make()
                .layer(self.inner_layer.layer(meta_service)),
        );

        let caller_name = match &self.builder_config.caller_name_mode {
            CallerName::PkgNameWithVersion => FastStr::from_static_str(PKG_NAME_WITH_VER),
            CallerName::OriginalCallerName => self.caller_name,
            CallerName::CallerNameWithVersion if !self.caller_name.is_empty() => {
                FastStr::from_string(format!(
                    "{}/{}",
                    self.caller_name,
                    env!("CARGO_PKG_VERSION")
                ))
            }
            CallerName::Specified(val) => val.clone(),
            _ => FastStr::empty(),
        };
        if !caller_name.is_empty() && self.headers.get(header::USER_AGENT).is_none() {
            self.headers.insert(
                header::USER_AGENT,
                HeaderValue::from_str(caller_name.as_str()).expect("Invalid caller name"),
            );
        }

        let client_inner = ClientInner {
            caller_name,
            callee_name_mode: self.builder_config.callee_name_mode,
            default_callee_name: self.callee_name,
            default_target: self.target,
            headers: self.headers,
        };
        let client = Client {
            service,
            inner: Arc::new(client_inner),
        };
        self.mk_client.mk_client(client)
    }
}

pub(super) struct ClientInner {
    caller_name: FastStr,
    callee_name_mode: CalleeName,
    default_callee_name: FastStr,
    default_target: TargetBuilder,
    headers: HeaderMap,
}

#[derive(Clone)]
pub struct Client<S> {
    service: S,
    inner: Arc<ClientInner>,
}

macro_rules! method_requests {
    ($method:ident) => {
        paste! {
            pub fn [<$method:lower>]<U>(&self, uri: U) -> Result<RequestBuilder<S>, ClientError>
            where
                U: TryInto<Uri>,
                U::Error: Into<BoxError>,
            {
                self.request(Method::[<$method:upper>], uri)
            }
        }
    };
}

impl Client<()> {
    /// Create a new client builder.
    pub fn builder() -> ClientBuilder<Identity, Identity, DefaultMkClient, DefaultLB> {
        ClientBuilder::new()
    }
}

impl<S> Client<S> {
    /// Create a builder for building a request.
    pub fn request_builder(&self) -> RequestBuilder<S> {
        RequestBuilder::new(self)
    }

    /// Create a builder for building a request with the specified method and URI.
    pub fn request<U>(&self, method: Method, uri: U) -> Result<RequestBuilder<S>, ClientError>
    where
        U: TryInto<Uri>,
        U::Error: Into<BoxError>,
    {
        RequestBuilder::new_with_method_and_uri(
            self,
            method,
            uri.try_into().map_err(builder_error)?,
        )
    }

    method_requests!(options);
    method_requests!(get);
    method_requests!(post);
    method_requests!(put);
    method_requests!(delete);
    method_requests!(head);
    method_requests!(trace);
    method_requests!(connect);
    method_requests!(patch);

    /// Get the default target address of the client.
    pub fn default_target(&self) -> &TargetBuilder {
        &self.inner.default_target
    }

    /// Send a request to the target address.
    ///
    /// This is a low-level method and you should build the `uri` and `request`, and get the
    /// address by yourself.
    ///
    /// For simple usage, you can use the `get`, `post` and other methods directly.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use std::net::SocketAddr;
    ///
    /// use http::{Method, Uri};
    /// use volo::net::Address;
    /// use volo_http::{body::Body, client::Client, request::ClientRequest};
    /// use volo_http::client::utils::TargetBuilder;
    ///
    /// let client = Client::builder().build();
    /// let addr: SocketAddr = "[::]:8080".parse().unwrap();
    /// let addr = Address::from(addr);
    /// let resp = client
    ///     .send_request(
    ///         TargetBuilder::Address { addr },
    ///         ClientRequest::builder()
    ///             .method(Method::GET)
    ///             .uri("/")
    ///             .body(Body::empty())
    ///             .expect("build request failed"),
    ///     )
    ///     .await
    ///     .expect("request failed")
    ///     .into_string()
    ///     .await
    ///     .expect("response failed to convert to string");
    /// println!("{resp:?}");
    /// ```
    pub async fn send_request<B>(
        &self,
        target: TargetBuilder,
        mut request: ClientRequest<B>,
        timeout: Option<Duration>,
    ) -> Result<S::Response, S::Error>
    where
        S: Service<ClientContext, ClientRequest<B>, Response = ClientResponse, Error = ClientError>
            + Send
            + Sync
            + 'static,
        B: Send + 'static,
    {
        let caller_name = self.inner.caller_name.clone();
        let target = match (&target, &self.inner.default_target) {
            (TargetBuilder::None, TargetBuilder::None) => {
                return Err(no_address());
            }
            (TargetBuilder::None, default_target) => default_target,
            (request_target, _) => request_target,
        };
        let target = target
            .to_target(&self.inner)
            .await?
            .ok_or_else(no_address)?;

        let callee_name = target.callee_name;
        let mut cx = ClientContext::new(
            #[cfg(feature = "__tls")]
            target.use_tls,
        );
        cx.rpc_info_mut().callee_mut().set_address(target.addr);
        if let Ok(host) = HeaderValue::from_maybe_shared(callee_name.clone()) {
            request.headers_mut().insert(header::HOST, host);
        }

        tracing::trace!(
            "create a request with caller_name: {caller_name}, callee_name: {callee_name}"
        );
        cx.rpc_info_mut().caller_mut().set_service_name(caller_name);
        cx.rpc_info_mut().callee_mut().set_service_name(callee_name);

        let config = Config { timeout };
        cx.rpc_info_mut().set_config(config);

        self.call(&mut cx, request).await
    }
}

impl<S, B> Service<ClientContext, ClientRequest<B>> for Client<S>
where
    S: Service<ClientContext, ClientRequest<B>, Response = ClientResponse, Error = ClientError>
        + Send
        + Sync
        + 'static,
    B: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;

    async fn call(
        &self,
        cx: &mut ClientContext,
        mut req: ClientRequest<B>,
    ) -> Result<Self::Response, Self::Error> {
        req.headers_mut().extend(self.inner.headers.clone());

        let has_metainfo = METAINFO.try_with(|_| {}).is_ok();

        let fut = self.service.call(cx, req);

        if has_metainfo {
            fut.await
        } else {
            METAINFO.scope(RefCell::new(MetaInfo::default()), fut).await
        }
    }
}

pub struct DefaultMkClient;

impl<S> MkClient<Client<S>> for DefaultMkClient {
    type Target = Client<S>;

    fn mk_client(&self, service: Client<S>) -> Self::Target {
        service
    }
}

/// Create a GET request to the specified URI.
pub async fn get<U>(uri: U) -> Result<ClientResponse, ClientError>
where
    U: TryInto<Uri>,
    U::Error: Into<BoxError>,
{
    ClientBuilder::new().build().get(uri)?.send().await
}

#[cfg(test)]
mod client_tests {
    use std::collections::HashMap;

    use http::{header, uri::Scheme, StatusCode};
    use serde::Deserialize;

    use super::{get, utils::resolve, Client};
    use crate::{body::BodyConversion, context::client::CalleeName};

    #[allow(dead_code)]
    #[derive(Deserialize)]
    struct HttpBinResponse {
        args: HashMap<String, String>,
        headers: HashMap<String, String>,
        origin: String,
        url: String,
    }

    const HTTPBIN_GET: &str = "http://httpbin.org/get";
    const HTTPBIN_GET_HTTPS: &str = "https://httpbin.org/get";
    const USER_AGENT_KEY: &str = "User-Agent";
    const USER_AGENT_VAL: &str = "volo-http-unit-test";

    #[tokio::test]
    async fn simple_get() {
        let resp = get(HTTPBIN_GET)
            .await
            .unwrap()
            .into_json::<HttpBinResponse>()
            .await
            .unwrap();
        assert!(resp.args.is_empty());
        assert_eq!(resp.url, HTTPBIN_GET);
    }

    #[tokio::test]
    async fn client_builder_with_header() {
        let mut builder = Client::builder();
        builder.header(header::USER_AGENT, USER_AGENT_VAL).unwrap();
        let client = builder.build();

        let resp = client
            .get(HTTPBIN_GET)
            .unwrap()
            .send()
            .await
            .unwrap()
            .into_json::<HttpBinResponse>()
            .await
            .unwrap();
        assert!(resp.args.is_empty());
        assert_eq!(resp.headers.get(USER_AGENT_KEY).unwrap(), USER_AGENT_VAL);
        assert_eq!(resp.url, HTTPBIN_GET);
    }

    #[tokio::test]
    async fn client_builder_with_host() {
        let mut builder = Client::builder();
        builder.host("httpbin.org");
        let client = builder.build();

        let resp = client
            .get("/get")
            .unwrap()
            .send()
            .await
            .unwrap()
            .into_json::<HttpBinResponse>()
            .await
            .unwrap();
        assert!(resp.args.is_empty());
        assert_eq!(resp.url, HTTPBIN_GET);
    }

    #[tokio::test]
    async fn client_builder_with_address() {
        let addr = resolve(None, "httpbin.org", None).await.unwrap();
        let mut builder = Client::builder();
        builder
            .address(addr, false)
            .callee_name("httpbin.org")
            .set_callee_name_mode(CalleeName::OriginalCalleeName);
        let client = builder.build();

        let resp = client
            .get("/get")
            .unwrap()
            .send()
            .await
            .unwrap()
            .into_json::<HttpBinResponse>()
            .await
            .unwrap();
        assert!(resp.args.is_empty());
        assert_eq!(resp.url, HTTPBIN_GET);
    }

    #[tokio::test]
    async fn client_builder_with_https() {
        let mut builder = Client::builder();
        builder.scheme_host_and_port(Some(Scheme::HTTPS), "httpbin.org", None);
        let client = builder.build();

        let resp = client
            .get("/get")
            .unwrap()
            .send()
            .await
            .unwrap()
            .into_json::<HttpBinResponse>()
            .await
            .unwrap();
        assert!(resp.args.is_empty());
        assert_eq!(resp.url, HTTPBIN_GET_HTTPS);
    }

    #[tokio::test]
    async fn client_builder_with_address_and_https() {
        let addr = resolve(Some(&Scheme::HTTPS), "httpbin.org", None)
            .await
            .unwrap();
        let mut builder = Client::builder();
        builder
            .address(addr, true)
            .callee_name("httpbin.org")
            .set_callee_name_mode(CalleeName::OriginalCalleeName);
        let client = builder.build();

        let resp = client
            .get("/get")
            .unwrap()
            .send()
            .await
            .unwrap()
            .into_json::<HttpBinResponse>()
            .await
            .unwrap();
        assert!(resp.args.is_empty());
        assert_eq!(resp.url, HTTPBIN_GET_HTTPS);
    }

    #[tokio::test]
    async fn client_builder_with_port() {
        let mut builder = Client::builder();
        builder.scheme_host_and_port(None, "httpbin.org", Some(443));
        let client = builder.build();

        let resp = client.get("/get").unwrap().send().await.unwrap();
        // Send HTTP request to the HTTPS port (443), `httpbin.org` will response `400 Bad
        // Request`.
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn fail_on_status() {
        let mut builder = Client::builder();
        builder.host("httpbin.org").fail_on_error_status(true);
        let client = builder.build();
        client
            .get("/post")
            .unwrap()
            .send()
            .await
            .expect_err("Request `/post` with GET should fail!");
    }

    #[tokio::test]
    async fn client_disable_tls() {
        let mut builder = Client::builder();
        builder.disable_tls(true);
        let client = builder.build();
        client
            .get("https://httpbin.org/post")
            .unwrap()
            .send()
            .await
            .expect_err("HTTPS request should fail when TLS is disabled!");
    }
}
