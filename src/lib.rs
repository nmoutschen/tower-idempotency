#![warn(missing_docs, unreachable_pub)]
#![cfg_attr(docsrs, feature(doc_cfg))]

//! # Cache layer for `tower::Service`s
//!
//! [`CacheLayer`] is a tower Layer that provides caches for `Service`s by using
//! another service to handle the cache. This allows the usage of asynchronous
//! and external caches.
//!
//! ## Usage
//!
//! Here is a sample implementation using an LRU cache as cache provider:
//!
//! ```rust
//! use std::convert::Infallible;
//! use tower::{ServiceBuilder, service_fn};
//! use tower_cache::{
//!     CacheLayer,
//!     lru::LruProvider,
//! };
//! async fn handler(req: String) -> Result<String, Infallible> {
//!     Ok(req.to_uppercase())
//! }
//!
//! // Initialize the cache provider service
//! let lru_provider = LruProvider::new::<String, String>(20);
//!
//! // Wrap the service with CacheLayer.
//! let my_service = ServiceBuilder::new()
//!     .layer(CacheLayer::new(lru_provider))
//!     .service(service_fn(handler));
//! ```
//!
//! ### With request transformer
//!
//! Certain cache providers might require specific trait bounds to be met in
//! order to cache that data. Or you might want to cache based on specific
//! fields in a struct, such as a request ID, URI path, etc.
//!
//! For this reason, you can specify a transformer function, that will
//! transform incoming requests before sending them to the cache provider.
//!
//! For example, to cache based on the URI path of `Request`s:
//!
//! ```rust
//! use http::{Request, Response, StatusCode};
//! use tower::{ServiceBuilder, service_fn};
//! use tower_cache::{
//!     CacheLayer,
//! lru::LruProvider,
//! };
//!
//! // Service handler function
//! async fn handler(_req: Request<()>) -> Response<()> {
//!     Response::builder()
//!         .status(StatusCode::OK)
//!         .body(())
//!         .unwrap()
//! }
//!
//! // Transform a Request into the path as a String
//! fn transform_req(req: Request<()>) -> String {
//!     req.uri().path().to_string()
//! }
//!
//! // Initialize the cache provider service
//! let lru_provider = LruProvider::new::<String, Response<()>>(20);
//!
//! // Create a cache layer with a transformer function
//! let cache_layer = CacheLayer::new(lru_provider)
//!     .with_transformer(transform_req);
//!
//! // Wrap the service with CacheLayer.
//! let my_service = ServiceBuilder::new()
//!     .layer(cache_layer)
//!     .service(service_fn(handler));
//! ```
//!
//! ## Creating cache providers
//!
//! A cache provider is a [`tower::Service`] that takes a [`ProviderRequest`]
//! as request and returns a [`ProviderResponse`].
//!

use std::{
    error, fmt,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};
use tower::{Layer, Service};

#[cfg(feature = "lru")]
#[cfg_attr(docsrs, doc(cfg(feature = "lru")))]
pub mod lru;

mod transform;
pub use transform::Transform;

/// Layer that adds cache to a [`tower::Service`]
///
/// This works by using a cache provider service that takes a [`ProviderRequest`]
/// and returns a [`ProviderResponse`].
pub struct CacheLayer<'a, P, T> {
    provider: P,
    transformer: T,
    _phantom: PhantomData<&'a ()>,
}

impl<'a> CacheLayer<'a, (), ()> {
    /// Create a new [`CacheLayer`]
    pub fn new<P>(provider: P) -> CacheLayer<'a, P, ()> {
        CacheLayer {
            provider,
            transformer: (),
            _phantom: PhantomData,
        }
    }
}

impl<'a, P, T> CacheLayer<'a, P, T> {
    /// Provide a function to transform requests before sending them to the
    /// cache provider.
    pub fn with_transformer<NT>(self, transformer: NT) -> CacheLayer<'a, P, NT> {
        CacheLayer {
            provider: self.provider,
            transformer,
            _phantom: PhantomData,
        }
    }
}

impl<'a, P, T, S> Layer<S> for CacheLayer<'a, P, T>
where
    P: Clone,
    T: Clone,
{
    type Service = CacheService<'a, S, P, T>;

    fn layer(&self, inner: S) -> Self::Service {
        CacheService {
            inner,
            provider: self.provider.clone(),
            transformer: self.transformer.clone(),
            _phantom: PhantomData,
        }
    }
}

/// Service generated by [`CacheLayer`].
pub struct CacheService<'a, S, P, T> {
    inner: S,
    provider: P,
    transformer: T,
    _phantom: PhantomData<&'a ()>,
}

impl<'a, S, P, T, R> Service<R> for CacheService<'a, S, P, T>
where
    S: Service<R> + Clone + Send + 'a,
    S::Response: Clone + Send + 'a,
    S::Error: Into<Box<dyn error::Error + Send + Sync>>,
    S::Future: Send + 'a,

    P: Service<ProviderRequest<T::Output, S::Response>, Response = ProviderResponse<S::Response>>
        + Clone
        + Send
        + 'a,
    P::Response: Send + 'a,
    P::Error: Into<Box<dyn error::Error + Send + Sync>> + Send,
    P::Future: Send + 'a,

    T: Transform<R>,
    T::Output: Clone + Send + 'a,
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
        let cache_request = self.transformer.transform(request.clone());
        let idem_fut = self
            .provider
            .call(ProviderRequest::Get(cache_request.clone()));

        Box::pin(async move {
            let res = match idem_fut.await {
                // If we have a response in the cache, we can immediately return without
                // calling the inner service.
                Ok(ProviderResponse::Found(res)) => Ok(res),
                // Response not found - we need to call the inner service and update the
                // cache.
                Ok(ProviderResponse::NotFound) => {
                    // Fetch the response from the inner service.
                    let response = inner
                        .call(request)
                        .await
                        .map_err(|e| Error::ServiceError(e.into()));
                    match response {
                        Ok(res) => {
                            // Store the response in the cache provider.
                            let new_res = res.clone();
                            match provider
                                .call(ProviderRequest::Insert(cache_request, new_res))
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
        })
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

/// Error returned by the [`CacheLayer`]
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
    use tower::{service_fn, Service, ServiceBuilder};

    #[derive(Clone, Default, Debug)]
    struct SimpleCache<R>
    where
        R: Eq + std::hash::Hash,
    {
        cache: Arc<Mutex<HashMap<R, R>>>,
    }

    impl<R> Service<ProviderRequest<R, R>> for SimpleCache<R>
    where
        R: Eq + std::hash::Hash + Clone + Send + 'static,
    {
        type Response = ProviderResponse<R>;
        type Error = Error;
        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, request: ProviderRequest<R, R>) -> Self::Future {
            Box::pin(ready(match request {
                ProviderRequest::Get(req) => match self.cache.lock().unwrap().get(&req) {
                    Some(res) => Ok(ProviderResponse::Found(res.clone())),
                    None => Ok(ProviderResponse::NotFound),
                },
                ProviderRequest::Insert(req, res) => {
                    self.cache.lock().unwrap().insert(req, res.clone());
                    Ok(ProviderResponse::Found(res))
                }
            }))
        }
    }

    async fn service(req: String) -> Result<String, Error> {
        Ok(req.to_uppercase())
    }

    async fn service_num(req: String) -> Result<usize, Error> {
        Ok(req.len() * 2)
    }

    #[tokio::test]
    async fn test_insert() -> Result<(), Error> {
        let cache = SimpleCache::default();
        let cache_layer = CacheLayer::new(cache.clone());

        let mut service = ServiceBuilder::new()
            .layer(cache_layer)
            .service(service_fn(service));

        assert_eq!(cache.cache.lock().unwrap().len(), 0);
        let res = service.call(String::from("Hello")).await?;

        assert_eq!(res, String::from("HELLO"));
        {
            let inner_cache = cache.cache.lock().unwrap();
            assert_eq!(inner_cache.len(), 1);
            assert_eq!(
                inner_cache.get(&String::from("Hello")),
                Some(&String::from("HELLO"))
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_get() -> Result<(), Error> {
        let cache = SimpleCache::default();
        {
            let mut inner_cache = cache.cache.lock().unwrap();
            inner_cache.insert(String::from("Hello"), String::from("hello"));
        }
        let cache_layer = CacheLayer::new(cache.clone());

        let mut service = ServiceBuilder::new()
            .layer(cache_layer)
            .service(service_fn(service));

        let res = service.call(String::from("Hello")).await?;
        assert_eq!(res, String::from("hello"));

        Ok(())
    }

    #[tokio::test]
    async fn test_insert_transformer() -> Result<(), Error> {
        fn transform(req: String) -> usize {
            req.len()
        }

        let cache = SimpleCache::default();
        let cache_layer = CacheLayer::new(cache.clone())
            .with_transformer(transform);

        let mut service = ServiceBuilder::new()
            .layer(cache_layer)
            .service(service_fn(service_num));

        assert_eq!(cache.cache.lock().unwrap().len(), 0);
        let res = service.call(String::from("Hello")).await?;

        assert_eq!(res, 10);
        {
            let inner_cache = cache.cache.lock().unwrap();
            assert_eq!(inner_cache.len(), 1);
            assert_eq!(
                inner_cache.get(&5),
                Some(&10)
            );
        }

        Ok(())
    }
}
