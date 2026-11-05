//! A hyper server that authenticates an user initially and then keeps the auth credentials
//! as part of a "session" through a cookie and in-memory store
//! can be adopted to database-based session management

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use hyper::{Body, Request, Response, Server};
use hyper::service::{make_service_fn, service_fn_ok};
use hyper::rt::{self, Future};
use winauth::http::Authenticator;

use cfg_if::cfg_if;
cfg_if! {
    if #[cfg(windows)] {

fn main() {
    let addr = ([127, 0, 0, 1], 3000).into();

    // Stores a cookie <-> username mapping
    // FIXME Could be a Database or if there shouldnt be storage used for sessions
    // JWTs, which do not require a store but only a cookie that is signed.
    let dummy_session_store: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));

    let server = Server::bind(&addr)
        .http1_only(true) // winauth only works for HTTP1
        .serve(make_service_fn(move |_socket: &hyper::server::conn::AddrStream| {
            let dummy_session_store = dummy_session_store.clone(); 
            let mut auth = AuthContext::None;
            service_fn_ok(move |req: Request<Body>| { 
                // Handle requests that work with sessions or might not need auth first
                if !req.uri().path().starts_with("/auth/windows") {
                    // FIXME The cookie handling here is very minimal for demo purposes. This wont work in production!
                    let content = match req.headers().get("cookie").map(|x| x.to_str().unwrap()) {
                        Some(ref val) => {
                            let store = dummy_session_store.lock().unwrap();
                            let user = store.get(val.split("=").nth(1).unwrap()).unwrap();
                            format!("Welcome Back, {}", user)
                        }
                        None => "Not logged in. Go to /auth/windows".to_owned(),
                    };

                    let mut builder = hyper::Response::builder();
                    return builder.body(content.into()).unwrap();
                }

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
                let cookie_name = generate_INSECURE_random_string();
                println!("Got auth request from user {}. Setting cookie {}", username, cookie_name);
                dummy_session_store.lock().unwrap().insert(cookie_name.clone(), username.to_owned());
                Response::builder()
                    .header("Set-Cookie", format!("session={};path=/", cookie_name)) // FIXME Use the cookie crate for this in production!
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

// FIXME This is just for demonstration purposes.
// Use a cryptographically secure PRNG (e.g. rand's StdRng)!
fn generate_INSECURE_random_string() -> String {
    println!("WARNING: DO NOT USE THIS DUMMY COOKIE GENERATOR IN PRODUCTION");
    "abcdefghHIGHTLYsecureCOOKIE".to_owned()
}

        
    } // WINDOWS
    else {
        fn main() {
            panic!("only windows supported");
        }
    }
}
