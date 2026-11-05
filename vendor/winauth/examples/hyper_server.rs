//! A minimal hyper server example
use hyper::{Body, Request, Response, Server};
use hyper::service::{make_service_fn, service_fn_ok};
use hyper::rt::{self, Future};
use winauth::http::Authenticator;

use cfg_if::cfg_if;
cfg_if! {
    if #[cfg(windows)] {

fn main() {
    let addr = ([127, 0, 0, 1], 3000).into();

    let server = Server::bind(&addr)
        .http1_only(true) // winauth only works for HTTP1
        .serve(make_service_fn(move |_socket: &hyper::server::conn::AddrStream| {
            let mut auth = AuthContext::None;
            service_fn_ok(move |req: Request<Body>| {
                // Perform authentication (at most once per connection) for Windows
                if let AuthContext::None = auth {
                    let sspi = winauth::windows::NtlmSspiBuilder::new()
                        .inbound()
                        .build()
                        .unwrap();
                    auth = AuthContext::Pending(sspi);
                }
                if let AuthContext::Pending(ref mut sspi) = auth {
                    let fetch_header = |header_name| Ok(
                        req.headers().get(header_name).map(|x| x.to_str()).transpose()?
                    );
                    auth = match sspi.http_incoming_auth(fetch_header).unwrap() {
                        // This cannot happen for the server-side
                        winauth::http::AuthState::NotRequested => unreachable!(),
                        // Pass along a http-response to the client, if needed for auth.
                        // The client will retry the request and include additional auth data.
                        winauth::http::AuthState::Response(ref resp) => {
                            let mut builder = hyper::Response::builder();
                            for (k, v) in &resp.headers {
                                builder.header(*k, v);
                            }
                            builder.status(resp.status_code);
                            return builder.body("".into()).unwrap();
                        },
                        // We finally have performed enough requests and authenticated successfully
                        // Get the username into the client name
                        winauth::http::AuthState::Success => AuthContext::Authenticated(sspi.client_identity().unwrap()),
                    };
                }

                // Authentication was successful. Store the username in a dummy session store (as we would in a database)
                let username = auth.username();
                Response::builder()
                    .body(Body::from(format!("Hello {}", username)))
                    .unwrap()
            })
        }))
        .map_err(|e| eprintln!("server error: {}", e));

    println!("Listening on http://{}", addr);
    rt::run(server);
}

/// State of windows authentication for the currently served HTTP connection
/// Ensures auth is performed at most once per connection.
enum AuthContext {
    /// We have not yet needed to authenticate for this connection
    None,
    /// The authentication process for this connection is pending
    Pending(winauth::windows::NtlmSspi),
    /// We are successfully authenticated as (username).
    Authenticated(String),
}

impl AuthContext {
    /// Return the username associated with this context, or panic.
    fn username(&self) -> &str {
        match *self {
            AuthContext::None | AuthContext::Pending(_) => panic!("This should not be reachable"),
            AuthContext::Authenticated(ref user) => &user,
        }
    }
}
        
    } // WINDOWS
    else {
        fn main() {
            panic!("only windows supported");
        }
    }
}
