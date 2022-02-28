#![warn(missing_docs, unreachable_pub)]
#![cfg_attr(docsrs, feature(doc_cfg))]

//! Idempotency layer for `tower::Service`s

use std::{
    error, fmt,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::Mutex;
use tower::{Layer, Service};

/// Layer that adds idempotency to a `tower::Service`
pub struct IdempotencyLayer<'a, P, R> {
    provider: P,
    _phantom: PhantomData<&'a R>,
}

impl<'a, P, R> IdempotencyLayer<'a, P, R> {
    /// Create a new `IdempotencyLayer`
    pub fn new(provider: P) -> Self {
        Self {
            provider,
            _phantom: PhantomData,
        }
    }
}

impl<'a, P, R, S> Layer<S> for IdempotencyLayer<'a, P, R>
where
    P: Clone,
{
    type Service = IdempotencyService<'a, S, P>;

    fn layer(&self, inner: S) -> Self::Service {
        IdempotencyService {
            inner: Arc::new(Mutex::new(inner)),
            provider: self.provider.clone(),
            _phantom: PhantomData,
        }
    }
}

/// Underlying service that is wrapped by the idempotency layer
pub struct IdempotencyService<'a, S, P> {
    inner: Arc<Mutex<S>>,
    provider: P,
    _phantom: PhantomData<&'a ()>,
}

impl<'a, S, P, R> Service<R> for IdempotencyService<'a, S, P>
where
    S: Service<R> + Send + 'a,
    S::Response: Clone + Send + 'a,
    S::Error: Into<Box<dyn error::Error + Send + Sync>>,
    S::Future: Send + 'a,
    P: Service<R, Response = ProviderResponse> + Send + 'a,
    P::Response: Send + 'a,
    P::Error: Into<Box<dyn error::Error + Send + Sync>> + Send,
    P::Future: Send + 'a,
    R: Clone + Send + 'a,
{
    type Response = S::Response;
    type Error = Error;
    type Future = IdempotencyFuture<'a, R, S>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.provider
            .poll_ready(cx)
            .map_err(|e| Error::ServiceError(e.into()))
    }

    fn call(&mut self, request: R) -> Self::Future {
        let idem_fut = self.provider.call(request.clone());
        let inner = self.inner.clone();
        let fut = Box::pin(async move {
            let res = match idem_fut.await {
                Ok(ProviderResponse::Found) => Err(Error::AlreadyProcessed),
                Ok(ProviderResponse::NotFound) => inner
                    .lock()
                    .await
                    .call(request)
                    .await
                    .map_err(|e| Error::ServiceError(e.into())),
                Err(e) => Err(Error::ProviderError(e.into())),
            };

            res
        });

        fut
    }
}

/// Responses sent by the idempotency provider
#[derive(Debug)]
pub enum ProviderResponse {
    /// The idempotency provider found a similar request
    Found,
    /// The idempotency provider did not find a similar request
    NotFound,
}

/// Errors from the Idempotency layer
///
/// As errors can come from both the idempotency provider, the inner service, or
/// the layer itself, this uses a custom enum to propagate errors.
#[derive(Debug)]
pub enum Error {
    /// Error generated by the idempotency provider
    ProviderError(Box<dyn error::Error + Send + Sync>),
    /// Error generated by the inner service
    ServiceError(Box<dyn error::Error + Send + Sync>),
    /// Internal error
    InternalError,
    /// The item has already been processed
    AlreadyProcessed,
}

impl error::Error for Error {}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::ProviderError(e) => write!(f, "provider error: {}", e),
            Error::ServiceError(e) => write!(f, "service error: {}", e),
            Error::InternalError => write!(f, "internal error"),
            Error::AlreadyProcessed => write!(f, "item already processed"),
        }
    }
}

type IdempotencyFuture<'a, R, S> =
    Pin<Box<dyn Future<Output = Result<<S as Service<R>>::Response, Error>> + Send + 'a>>;
