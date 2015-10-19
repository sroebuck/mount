use std::error::Error;
use std::path::{Path, Component};
use iron::{Handler, Response, Request, IronResult, IronError, Url, status};
use iron::typemap;
use sequence_trie::SequenceTrie;
use std::fmt;

/// Exposes the original, unmodified path to be stored in `Request::extensions`.
#[derive(Copy, Clone)]
pub struct OriginalUrl;
impl typemap::Key for OriginalUrl { type Value = Url; }

/// `Mount` is a simple mounting middleware.
///
/// Mounting allows you to install a handler on a route and have it receive requests as if they
/// are relative to that route. For example, a handler mounted on `/foo/` will receive
/// requests like `/foo/bar` as if they are just `/bar`. Iron's mounting middleware allows
/// you to specify multiple mountings using one middleware instance. Requests that pass through
/// the mounting middleware are passed along to the mounted handler that best matches the request's
/// path. `Request::url` is modified so that requests appear to be relative to the mounted handler's
/// route.
///
/// Mounted handlers may also access the *original* URL by requesting the `OriginalUrl` key
/// from `Request::extensions`.
pub struct Mount {
    inner: SequenceTrie<String, Match>
}

struct Match {
    handler: Box<Handler>,
    length: usize
}

/// The error returned by `Mount` when a request doesn't match any mounted handlers.
#[derive(Debug)]
pub struct NoMatch;

impl Error for NoMatch {
    fn description(&self) -> &'static str { "No Match" }
}

impl fmt::Display for NoMatch {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.description())
    }
}

impl Mount {
    /// Creates a new instance of `Mount`.
    pub fn new() -> Mount {
        Mount {
            inner: SequenceTrie::new()
        }
    }

    /// Mounts a given `Handler` onto a route.
    ///
    /// This method may be called multiple times with different routes.
    /// For a given request, the *most specific* handler will be selected.
    ///
    /// Existing handlers on the same route will be overwritten.
    pub fn mount<H: Handler>(&mut self, route: &str, handler: H) -> &mut Mount {
        // Parse the route into a list of strings. The unwrap is safe because strs are UTF-8.
        let key: Vec<String> = Path::new(route).components().flat_map(|c|
            match c {
                Component::RootDir => None,
                c => Some(c.as_os_str().to_str().unwrap().to_string())
            }.into_iter()
        ).collect();

        // Insert a match struct into the trie.
        self.inner.insert(key.as_ref(), Match {
            handler: Box::new(handler) as Box<Handler>,
            length: key.len()
        });
        self
    }
}

impl Handler for Mount {
    fn handle(&self, req: &mut Request) -> IronResult<Response> {
        // Find the matching handler.
        let matched = {
            // Extract the request path.
            let path = &*req.url.path;

            // If present, remove the trailing empty string (which represents a trailing slash).
            // If it isn't removed the path will never match anything, because
            // Path::str_components ignores trailing slashes and will never create routes
            // ending in "".
            let key = match path.last() {
                Some(s) if s.is_empty() => &path[..path.len() - 1],
                _ => path
            };

            // Search the Trie for the nearest most specific match.
            match self.inner.get_ancestor(key) {
                Some(matched) => matched,
                None => return Err(IronError::new(NoMatch, status::NotFound))
            }
        };

        // We have a match, so fire off the child.
        // If another mount middleware hasn't already, insert the unmodified url
        // into the extensions as the "original url".
        let is_outer_mount = !req.extensions.contains::<OriginalUrl>();
        if is_outer_mount {
            req.extensions.insert::<OriginalUrl>(req.url.clone());
        }

        // Remove the prefix from the request's path before passing it to the mounted handler.
        // If the prefix is entirely removed and no trailing slash was present, the new path
        // will be the empty list. For the purposes of redirection, conveying that the path
        // did not include a trailing slash is more important than providing a non-empty list.
        req.url.path = req.url.path[matched.length..].to_vec();

        let res = matched.handler.handle(req);

        // Reverse the URL munging, for future middleware.
        req.url = match req.extensions.get::<OriginalUrl>() {
            Some(original) => original.clone(),
            None => panic!("OriginalUrl unexpectedly removed from req.extensions.")
        };

        // If this mount middleware is the outermost mount middleware,
        // remove the original url from the extensions map to prevent leakage.
        if is_outer_mount {
            req.extensions.remove::<OriginalUrl>();
        }

        res
    }
}

#[cfg(test)]
mod tests {    
    use super::Mount;
    use iron::{Request, Response, IronResult, Url};
    use iron::status;
    use hyper::method::Method;
    use hyper::buffer::BufReader;
    use hyper::net::NetworkStream;
    use std::io::Cursor;
    use iron::middleware::Handler;

     fn send_hello(_: &mut Request) -> IronResult<Response> {
        Ok(Response::with((status::Ok, "Hello!")))
    }

    #[test]
    fn it_mounts() {
        let mut mount = Mount::new();
        mount.mount("/testing", send_hello);
        let data = Cursor::new("Test".to_string().into_bytes());
        let mut stream = mock::MockStream::new(data);
        let mut reader = BufReader::new(&mut stream as &mut NetworkStream);
        let mut req = mock::request::new(Method::Get, Url::parse("http://localhost/test").unwrap(),
            &mut reader);
        let res = mount.handle(& mut req);
        println!("res = {:?}", res);
        // Should fail because mount point doesn't match request...
        assert!(res.is_err());
    }


    pub mod mock {
        use hyper::net::NetworkStream;
        use std::net::SocketAddr;
        use std::io::{Read, Write, Result};
        use std::any::Any;

        /// A mock network stream
        #[derive(Clone)]
        pub struct MockStream<T> {
            data: T
        }

        impl<T> MockStream<T> {
            /// Create a new mock stream that reads from the given data
            pub fn new(data: T) -> MockStream<T> {
                MockStream { data: data }
            }
        }

        impl<T: Send + Read + Write + Clone + Any> NetworkStream for MockStream<T> {
            fn peer_addr(&mut self) -> Result<SocketAddr> {
                Ok("127.0.0.1:3000".parse().unwrap())
            }
        }

        impl<T: Read> Read for MockStream<T> {
            fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
                self.data.read(buf)
            }
        }

        impl<T: Write> Write for MockStream<T> {
            fn write(&mut self, buf: &[u8]) -> Result<usize> {
                self.data.write(buf)
            }

            fn flush(&mut self) -> Result<()> {
                self.data.flush()
            }
        }

        /// Contains constructors for mocking Iron Requests.
        pub mod request {
            use iron::{Request, TypeMap, Headers, Url};
            use iron::request::Body;
            use iron::{method, headers};

            use hyper::http::h1::HttpReader;
            use hyper::buffer::BufReader;
            use hyper::net::NetworkStream;

            use std::net::SocketAddr;

            /// Create a new mock Request with the given method, url, and data.
            pub fn new<'a, 'b>(method: method::Method, path: Url,
                               reader: &'a mut BufReader<&'b mut NetworkStream>) -> Request<'a, 'b> {
                let reader = HttpReader::EofReader(reader);
                let addr: SocketAddr = "127.0.0.1:3000".parse().unwrap();

                let mut headers = Headers::new();
                let host = Url::parse("http://127.0.0.1:3000").unwrap()
                    .into_generic_url()
                    .serialize_host().unwrap();

                headers.set(headers::Host {
                    hostname: host,
                    port: Some(3000),
                });

                headers.set(headers::UserAgent("iron-test".to_string()));

                Request {
                    method: method,
                    url: path,
                    body: Body::new(reader),
                    local_addr: addr.clone(),
                    remote_addr: addr,
                    headers: headers,
                    extensions: TypeMap::new()
                }
            }
        }
    }
}


