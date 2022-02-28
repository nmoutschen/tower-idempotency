#![warn(missing_docs, unreachable_pub)]
#![cfg_attr(docsrs, feature(doc_cfg))]

//! Cache layer for `tower::Service`s

use std::{
    error, fmt,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tower::{Layer, Service};

/// Layer that adds cache to a `tower::Service`
pub struct CacheLayer<'a, P, R> {
    provider: P,
    _phantom: PhantomData<&'a R>,
}

impl<'a, P, R> CacheLayer<'a, P, R> {
    /// Create a new `CacheLayer`
    pub fn new(provider: P) -> Self {
        Self {
            provider,
            _phantom: PhantomData,
        }
    }
}

impl<'a, P, R, S> Layer<S> for CacheLayer<'a, P, R>
where
    P: Clone,
{
    type Service = CacheService<'a, S, P>;

    fn layer(&self, inner: S) -> Self::Service {
        CacheService {
            inner,
            provider: self.provider.clone(),
            _phantom: PhantomData,
        }
    }
}

/// Service generated by [`CacheLayer`].
pub struct CacheService<'a, S, P> {
    inner: S,
    provider: P,
    _phantom: PhantomData<&'a ()>,
}

impl<'a, S, P, R> Service<R> for CacheService<'a, S, P>
where
    S: Service<R> + Clone + Send + 'a,
    S::Response: Clone + Send + 'a,
    S::Error: Into<Box<dyn error::Error + Send + Sync>>,
    S::Future: Send + 'a,
    P: Service<ProviderRequest<R, S::Response>, Response = ProviderResponse<S::Response>> + Clone + Send + 'a,
    P::Response: Send + 'a,
    P::Error: Into<Box<dyn error::Error + Send + Sync>> + Send,
    P::Future: Send + 'a,
    R: Clone + Send + Sync + 'a,
{
    type Response = S::Response;
    type Error = Error;
    type Future = CacheFuture<'a, R, S>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.provider
            .poll_ready(cx)
            .map_err(|e| Error::ServiceError(e.into()))
    }

    fn call(&mut self, request: R) -> Self::Future {
        let mut provider = self.provider.clone();
        let mut inner = self.inner.clone();

        let idem_fut = self.provider.call(ProviderRequest::Get(request.clone()));
        let fut = Box::pin(async move {
            let res = match idem_fut.await {
                Ok(ProviderResponse::Found(res)) => Ok(res),
                Ok(ProviderResponse::NotFound) => {
                    let response = inner
                        .call(request.clone())
                        .await
                        .map_err(|e| Error::ServiceError(e.into()));
                    match response {
                        Ok(res) => {
                            let new_res = res.clone();
                            match provider
                                .call(ProviderRequest::Insert(request, new_res))
                                .await
                            {
                                Ok(_) => Ok(res),
                                Err(e) => Err(Error::ProviderError(e.into())),
                            }
                        }
                        res => res,
                    }
                }
                Err(e) => Err(Error::ProviderError(e.into())),
            };

            res
        });

        fut
    }
}

/// Requests sent to the cache provider
#[derive(Clone, Debug)]
pub enum ProviderRequest<Req, Res> {
    /// Check if the provider has a similar request
    Get(Req),
    /// Insert a response into the provider
    Insert(Req, Res),
}

/// Responses sent by the cache provider
#[derive(Debug)]
pub enum ProviderResponse<Res> {
    /// The cache provider found a similar request
    Found(Res),
    /// The cache provider did not find a similar request
    NotFound,
}

/// Errors from the Cache layer
///
/// As errors can come from both the cache provider, the inner service, or
/// the layer itself, this uses a custom enum to propagate errors.
#[derive(Debug)]
pub enum Error {
    /// Error generated by the cache provider
    ProviderError(Box<dyn error::Error + Send + Sync>),
    /// Error generated by the inner service
    ServiceError(Box<dyn error::Error + Send + Sync>),
    /// Internal error
    InternalError,
}

impl error::Error for Error {}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::ProviderError(e) => write!(f, "provider error: {}", e),
            Error::ServiceError(e) => write!(f, "service error: {}", e),
            Error::InternalError => write!(f, "internal error"),
        }
    }
}

type CacheFuture<'a, R, S> =
    Pin<Box<dyn Future<Output = Result<<S as Service<R>>::Response, Error>> + Send + 'a>>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::HashMap,
        future::ready,
        sync::{Arc, Mutex},
    };
    use tower::{service_fn, ServiceBuilder, Service};

    #[derive(Clone, Default, Debug)]
    struct SimpleCache {
        cache: Arc<Mutex<HashMap<String, String>>>,
    }

    impl Service<ProviderRequest<String, String>> for SimpleCache {
        type Response = ProviderResponse<String>;
        type Error = Error;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: ProviderRequest<String, String>) -> Self::Future {
            Box::pin(ready(match request {
                ProviderRequest::Get(req) => {
                    match self.cache.lock().unwrap().get(&req) {
                        Some(res) => Ok(ProviderResponse::Found(res.clone())),
                        None => Ok(ProviderResponse::NotFound),
                    }
                },
                ProviderRequest::Insert(req, res) => {
                    self.cache.lock().unwrap().insert(req, res.clone());
                    Ok(ProviderResponse::Found(res))
                }
            }))
        }
    }

    #[tokio::test]
    async fn test_insert() -> Result<(), Error> {
        let cache = SimpleCache::default();
        let cache_layer = CacheLayer::<_, String>::new(cache.clone());

        async fn service(req: String) -> Result<String, Error> {
            Ok(req.to_uppercase())
        }

        let mut service = ServiceBuilder::new()
            .layer(cache_layer)
            .service(service_fn(service));

        assert_eq!(cache.cache.lock().unwrap().len(), 0);
        let res = service.call(String::from("Hello")).await?;

        assert_eq!(res, String::from("HELLO"));
        {
            let inner_cache = cache.cache.lock().unwrap();
            assert_eq!(inner_cache.len(), 1);
            assert_eq!(inner_cache.get(&String::from("Hello")), Some(&String::from("HELLO")));
        }

        Ok(())
    }
}