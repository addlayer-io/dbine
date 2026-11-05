//! Components to perform HTTP-based winauth authentication with a generic http library

/// The response, that the user of the API is expected to transform into a HTTP response to the client
pub struct Response {
    pub headers: Vec<(&'static str, String)>,
    pub status_code: u16,
}

/// The current authentication state that incoming data transitioned into
pub enum AuthState {
    /// Authentication was performed.
    Success,
    /// An authentication HTTP response that should be sent to the other party (client/server)
    Response(Response),
    /// Client only. The server does not support or wish authentication.
    NotRequested,
}

/// An authenticator that can authenticate incoming requests for servers
pub trait Authenticator: crate::NextBytes {
    /// HTTP auth schemes, as defined in RFC7235
    fn auth_scheme(&self) -> &'static str;

    /// Performs authentication against a received request from the client. 
    /// If authentication is incomplete, the caller is instructed through AuthState::Response 
    /// to send the http response contained in AuthState::Response (401) to the client.
    /// After a full authentication attempt, do not call this method on the same Authenticator instance again.
    fn http_incoming_auth<'a, R>(&'a mut self, get_header: R) -> Result<AuthState, Box<dyn std::error::Error>>
    where
        R: Fn(&'static str) -> Result<Option<&'a str>, Box<dyn std::error::Error>> 
    {
        let auth_scheme = self.auth_scheme();

        let auth_bytes = match get_header("Authorization")? {
            None => {
                // Initially prompt the client that we require authentication
                return Ok(AuthState::Response(Response {
                    headers: vec![("WWW-Authenticate", auth_scheme.to_owned())],
                    status_code: 401,
                }))
            }
            Some(header) => {
                // Extract received challenge
                if !header.starts_with(auth_scheme) {
                    return Err(format!("unsupported auth scheme: {}", header))?;
                }
                let challenge = header.trim_start_matches(auth_scheme).trim_start();
                base64::decode(challenge).map_err(|err| format!("Malformed Base64 in Authorization header: {:?}", err))?
            }
        };
        // Get response, if we're not done yet
        if let Some(next_bytes) = self.next_bytes(Some(&auth_bytes))? {
            return Ok(AuthState::Response(Response {
                headers: vec![("WWW-Authenticate", format!("{} {}", auth_scheme, base64::encode(&next_bytes)))],
                status_code: 401,
            }));
        }

        Ok(AuthState::Success)
    }

    /// Provide an authentication state so the caller can retry an outgoing request until the server
    /// has all needed authentication information.
    /// If authentication is incomplete, the caller is instructed through AuthState::Response 
    /// to retry the http request to the server with the headers contained in AuthState::Response.
    /// After a full authentication attempt, do not call this method on the same Authenticator instance again.
    fn http_outgoing_auth<'a, F>(&'a mut self, get_header: F) -> Result<AuthState, Box<dyn std::error::Error>>
    where
        F: Fn(&'static str) -> Result<Vec<&'a str>, Box<dyn std::error::Error>>
    {
        let methods = get_header("WWW-Authenticate")?;
        let auth_scheme = self.auth_scheme();

        // Start a new authentication process
        if methods.contains(&auth_scheme) {
            if let Some(next_bytes) = self.next_bytes(None)? {
                return Ok(AuthState::Response(Response {
                    headers: vec![("Authorization", format!("{} {}", auth_scheme, base64::encode(&next_bytes)))],
                    status_code: 0,
                }));
            }
        }
        // Continue authentication, if already started
        else if methods.len() == 1 && methods[0].starts_with(auth_scheme) {
            let challenge = methods[0].trim_start_matches(auth_scheme).trim_start();
            let in_bytes = base64::decode(challenge).map_err(|err| format!("Malformed Base64 in WWW-Authenticate header: {:?}", err))?;
            if let Some(next_bytes) = self.next_bytes(Some(&in_bytes))? {
                return Ok(AuthState::Response(Response { 
                    headers: vec![("Authorization", format!("{} {}", auth_scheme, base64::encode(&next_bytes)))],
                    status_code: 0,
                }));
            }
            return Ok(AuthState::Success);
        }

        // No authentication is possible / required (requested by the server)
        Ok(AuthState::NotRequested)
    }
}
