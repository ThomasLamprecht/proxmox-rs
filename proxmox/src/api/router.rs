use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use failure::Error;
use http::request::Parts;
use http::{Method, Response};
use hyper::Body;
use serde_json::Value;

use crate::api::schema::{self, ObjectSchema, Schema};
use crate::api::RpcEnvironment;

use super::Permission;

/// A synchronous API handler gets a json Value as input and returns a json Value as output.
///
/// Most API handler are synchronous. Use this to define such handler:
/// ```
/// # use failure::*;
/// # use serde_json::{json, Value};
/// # use proxmox::api::{*, schema::*};
/// #
/// fn hello(
///    param: Value,
///    info: &ApiMethod,
///    rpcenv: &mut dyn RpcEnvironment,
/// ) -> Result<Value, Error> {
///    Ok(json!("Hello world!"))
/// }
///
/// const API_METHOD_HELLO: ApiMethod = ApiMethod::new(
///    &ApiHandler::Sync(&hello),
///    &ObjectSchema::new("Hello World Example", &[])
/// );
/// ```
pub type ApiHandlerFn = &'static (dyn Fn(Value, &ApiMethod, &mut dyn RpcEnvironment) -> Result<Value, Error>
              + Send
              + Sync
              + 'static);

/// Asynchronous API handlers
///
/// Returns a future Value.
/// ```
/// # use failure::*;
/// # use serde_json::{json, Value};
/// # use proxmox::api::{*, schema::*};
/// #
/// use futures::*;
///
/// fn hello_future<'a>(
///    param: Value,
///    info: &ApiMethod,
///    rpcenv: &'a mut dyn RpcEnvironment,
/// ) -> ApiFuture<'a> {
///    async move {
///        let data = json!("hello world!");
///        Ok(data)
///    }.boxed()
/// }
///
/// const API_METHOD_HELLO_FUTURE: ApiMethod = ApiMethod::new(
///    &ApiHandler::Async(&hello_future),
///    &ObjectSchema::new("Hello World Example (async)", &[])
/// );
/// ```
pub type ApiAsyncHandlerFn = &'static (dyn for<'a> Fn(Value, &'static ApiMethod, &'a mut dyn RpcEnvironment) -> ApiFuture<'a>
              + Send
              + Sync);

pub type ApiFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, failure::Error>> + Send + 'a>>;

/// Asynchronous HTTP API handlers
///
/// They get low level access to request and response data. Use this
/// to implement custom upload/download functions.
/// ```
/// # use failure::*;
/// # use serde_json::{json, Value};
/// # use proxmox::api::{*, schema::*};
/// #
/// use futures::*;
/// use hyper::{Body, Response, http::request::Parts};
///
/// fn low_level_hello(
///    parts: Parts,
///    req_body: Body,
///    param: Value,
///    info: &ApiMethod,
///    rpcenv: Box<dyn RpcEnvironment>,
/// ) -> ApiResponseFuture {
///    async move {
///        let response = http::Response::builder()
///            .status(200)
///            .body(Body::from("Hello world!"))?;
///        Ok(response)
///    }.boxed()
/// }
///
/// const API_METHOD_LOW_LEVEL_HELLO: ApiMethod = ApiMethod::new(
///    &ApiHandler::AsyncHttp(&low_level_hello),
///    &ObjectSchema::new("Hello World Example (low level)", &[])
/// );
/// ```
pub type ApiAsyncHttpHandlerFn = &'static (dyn Fn(
    Parts,
    Body,
    Value,
    &'static ApiMethod,
    Box<dyn RpcEnvironment>,
) -> ApiResponseFuture
              + Send
              + Sync
              + 'static);

/// The output of an asynchronous API handler is a future yielding a `Response`.
pub type ApiResponseFuture =
    Pin<Box<dyn Future<Output = Result<Response<Body>, failure::Error>> + Send>>;

/// Enum for different types of API handler functions.
pub enum ApiHandler {
    Sync(ApiHandlerFn),
    Async(ApiAsyncHandlerFn),
    AsyncHttp(ApiAsyncHttpHandlerFn),
}

#[cfg(feature = "test-harness")]
impl Eq for ApiHandler {}

#[cfg(feature = "test-harness")]
impl PartialEq for ApiHandler {
    fn eq(&self, rhs: &Self) -> bool {
        unsafe {
            match (self, rhs) {
                (ApiHandler::Sync(l), ApiHandler::Sync(r)) => {
                    core::mem::transmute::<_, usize>(l) == core::mem::transmute::<_, usize>(r)
                }
                (ApiHandler::Async(l), ApiHandler::Async(r)) => {
                    core::mem::transmute::<_, usize>(l) == core::mem::transmute::<_, usize>(r)
                }
                (ApiHandler::AsyncHttp(l), ApiHandler::AsyncHttp(r)) => {
                    core::mem::transmute::<_, usize>(l) == core::mem::transmute::<_, usize>(r)
                }
                _ => false,
            }
        }
    }
}

/// Lookup table to child `Router`s
///
/// Stores a sorted list of `(name, router)` tuples:
///
/// - `name`: The name of the subdir
/// - `router`: The router for this subdir
///
/// **Note:** The list has to be sorted by name, because we use a binary
/// search to find items.
///
/// This is a workaround unless RUST can const_fn `Hash::new()`
pub type SubdirMap = &'static [(&'static str, &'static Router)];

/// Classify different types of routers
pub enum SubRoute {
    //Hash(HashMap<String, Router>),
    /// Router with static lookup map.
    ///
    /// The first path element is used to lookup a new
    /// router with `SubdirMap`. If found, the remaining path is
    /// passed to that router.
    Map(SubdirMap),
    /// Router that always match the first path element
    ///
    /// The matched path element is stored as parameter
    /// `param_name`. The remaining path is matched using the `router`.
    MatchAll {
        router: &'static Router,
        param_name: &'static str,
    },
}

/// Macro to create an ApiMethod to list entries from SubdirMap
#[macro_export]
macro_rules! list_subdirs_api_method {
    ($map:expr) => {
        $crate::api::ApiMethod::new(
            &$crate::api::ApiHandler::Sync( & |_, _, _| {
                let index = ::serde_json::json!(
                    $map.iter().map(|s| ::serde_json::json!({ "subdir": s.0}))
                        .collect::<Vec<::serde_json::Value>>()
                );
                Ok(index)
            }),
            &$crate::api::schema::ObjectSchema::new("Directory index.", &[])
                .additional_properties(true)
        )
    }
}

/// Define APIs with routing information
///
/// REST APIs use hierarchical paths to identify resources. A path
/// consists of zero or more components, separated by `/`. A `Router`
/// is a simple data structure to define such APIs. Each `Router` is
/// responsible for a specific path, and may define `ApiMethod`s for
/// different HTTP requests (GET, PUT, POST, DELETE). If the path
/// contains more elements, `subroute` is used to find the correct
/// endpoint.
///
/// Routers are meant to be build a compile time, and you can use
/// all `const fn(mut self, ..)` methods to configure them.
///
///```
/// # use failure::*;
/// # use serde_json::{json, Value};
/// # use proxmox::api::{*, schema::*};
/// #
/// const API_METHOD_HELLO: ApiMethod = ApiMethod::new(
///    &ApiHandler::Sync(&|_, _, _| {
///         Ok(json!("Hello world!"))
///    }),
///    &ObjectSchema::new("Hello World Example", &[])
/// );
/// const ROUTER: Router = Router::new()
///    .get(&API_METHOD_HELLO);
///```
pub struct Router {
    /// GET requests
    pub get: Option<&'static ApiMethod>,
    /// PUT requests
    pub put: Option<&'static ApiMethod>,
    /// POST requests
    pub post: Option<&'static ApiMethod>,
    /// DELETE requests
    pub delete: Option<&'static ApiMethod>,
    /// Used to find the correct API endpoint.
    pub subroute: Option<SubRoute>,
}

impl Router {
    /// Create a new Router.
    pub const fn new() -> Self {
        Self {
            get: None,
            put: None,
            post: None,
            delete: None,
            subroute: None,
        }
    }

    /// Configure a static map as `subroute`.
    pub const fn subdirs(mut self, map: SubdirMap) -> Self {
        self.subroute = Some(SubRoute::Map(map));
        self
    }

    /// Configure a `SubRoute::MatchAll` as `subroute`.
    pub const fn match_all(mut self, param_name: &'static str, router: &'static Router) -> Self {
        self.subroute = Some(SubRoute::MatchAll { router, param_name });
        self
    }

    /// Configure the GET method.
    pub const fn get(mut self, m: &'static ApiMethod) -> Self {
        self.get = Some(m);
        self
    }

    /// Configure the PUT method.
    pub const fn put(mut self, m: &'static ApiMethod) -> Self {
        self.put = Some(m);
        self
    }

    /// Configure the POST method.
    pub const fn post(mut self, m: &'static ApiMethod) -> Self {
        self.post = Some(m);
        self
    }

    /// Same as `post`, but expects an `AsyncHttp` handler.
    pub const fn upload(mut self, m: &'static ApiMethod) -> Self {
        // fixme: expect AsyncHttp
        self.post = Some(m);
        self
    }

    /// Same as `get`, but expects an `AsyncHttp` handler.
    pub const fn download(mut self, m: &'static ApiMethod) -> Self {
        // fixme: expect AsyncHttp
        self.get = Some(m);
        self
    }

    /// Same as `get`, but expects an `AsyncHttp` handler.
    pub const fn upgrade(mut self, m: &'static ApiMethod) -> Self {
        // fixme: expect AsyncHttp
        self.get = Some(m);
        self
    }

    /// Configure the DELETE method
    pub const fn delete(mut self, m: &'static ApiMethod) -> Self {
        self.delete = Some(m);
        self
    }

    /// Find the router for a specific path.
    ///
    /// - `components`: Path, split into individual components.
    /// - `uri_param`: Mutable hash map to store parameter from `MatchAll` router.
    pub fn find_route(
        &self,
        components: &[&str],
        uri_param: &mut HashMap<String, String>,
    ) -> Option<&Router> {
        if components.is_empty() {
            return Some(self);
        };

        let (dir, remaining) = (components[0], &components[1..]);

        match self.subroute {
            None => {}
            Some(SubRoute::Map(dirmap)) => {
                if let Ok(ind) = dirmap.binary_search_by_key(&dir, |(name, _)| name) {
                    let (_name, router) = dirmap[ind];
                    //println!("FOUND SUBDIR {}", dir);
                    return router.find_route(remaining, uri_param);
                }
            }
            Some(SubRoute::MatchAll { router, param_name }) => {
                //println!("URI PARAM {} = {}", param_name, dir); // fixme: store somewhere
                uri_param.insert(param_name.to_owned(), dir.into());
                return router.find_route(remaining, uri_param);
            }
        }

        None
    }

    /// Lookup the API method for a specific path.
    /// - `components`: Path, split into individual components.
    /// - `method`: The HTTP method.
    /// - `uri_param`: Mutable hash map to store parameter from `MatchAll` router.
    pub fn find_method(
        &self,
        components: &[&str],
        method: Method,
        uri_param: &mut HashMap<String, String>,
    ) -> Option<&ApiMethod> {
        if let Some(info) = self.find_route(components, uri_param) {
            return match method {
                Method::GET => info.get,
                Method::PUT => info.put,
                Method::POST => info.post,
                Method::DELETE => info.delete,
                _ => None,
            };
        }
        None
    }
}

impl Default for Router {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

const NULL_SCHEMA: Schema = Schema::Null;

fn dummy_handler_fn(
    _arg: Value,
    _method: &ApiMethod,
    _env: &mut dyn RpcEnvironment,
) -> Result<Value, Error> {
    // do nothing
    Ok(Value::Null)
}

const DUMMY_HANDLER: ApiHandler = ApiHandler::Sync(&dummy_handler_fn);

/// Access permission with description
pub struct ApiAccessPermissions {
    pub description: &'static str,
    pub permission: &'static Permission,
}

/// This struct defines a synchronous API call which returns the result as json `Value`
#[cfg_attr(feature = "test-harness", derive(Eq, PartialEq))]
pub struct ApiMethod {
    /// The protected flag indicates that the provides function should be forwarded
    /// to the daemon running in privileged mode.
    pub protected: bool,
    /// This flag indicates that the provided method may change the local timezone, so the server
    /// should do a tzset afterwards
    pub reload_timezone: bool,
    /// Parameter type Schema
    pub parameters: &'static schema::ObjectSchema,
    /// Return type Schema
    pub returns: &'static schema::Schema,
    /// Handler function
    pub handler: &'static ApiHandler,
    /// Access Permissions
    pub access: ApiAccessPermissions,
}

impl std::fmt::Debug for ApiMethod {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "ApiMethod {{ ")?;
        write!(f, "  parameters: {:?}", self.parameters)?;
        write!(f, "  returns: {:?}", self.returns)?;
        write!(f, "  handler: {:p}", &self.handler)?;
        write!(f, "  permissions: {:?}", &self.access.permission)?;
        write!(f, "}}")
    }
}

impl ApiMethod {
    pub const fn new(handler: &'static ApiHandler, parameters: &'static ObjectSchema) -> Self {
        Self {
            parameters,
            handler,
            returns: &NULL_SCHEMA,
            protected: false,
            reload_timezone: false,
            access: ApiAccessPermissions {
                description: "Default access permissions (superuser only).",
                permission: &Permission::Superuser
            },
        }
    }

    pub const fn new_dummy(parameters: &'static ObjectSchema) -> Self {
        Self {
            parameters,
            handler: &DUMMY_HANDLER,
            returns: &NULL_SCHEMA,
            protected: false,
            reload_timezone: false,
            access: ApiAccessPermissions {
                description: "Default access permissions (superuser only).",
                permission: &Permission::Superuser
            },
        }
    }

    pub const fn returns(mut self, schema: &'static Schema) -> Self {
        self.returns = schema;

        self
    }

    pub const fn protected(mut self, protected: bool) -> Self {
        self.protected = protected;

        self
    }

    pub const fn reload_timezone(mut self, reload_timezone: bool) -> Self {
        self.reload_timezone = reload_timezone;

        self
    }

    pub const fn permissions(mut self, description: &'static str, permission: &'static Permission) -> Self {
        self.access = ApiAccessPermissions { description, permission };

        self
    }
}
