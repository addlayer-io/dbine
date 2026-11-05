use winauth::http::Authenticator;

macro_rules! perform_ntlm_request {
    ($method:expr, $url:expr, $builder:ident, $bl:expr) => {{
        let client = reqwest::Client::new();
        let mut out_resp: Option<winauth::http::Response> = None;
        let mut sspi = winauth::windows::NtlmSspiBuilder::new()
            .outbound()
            // .target_spn("HTTP/localhost:3030")
            .build()
            .unwrap();

        loop
        {
            println!("Perform req...");
            let mut builder = client.request($method, $url);
            {
                let mut $builder = builder;
                $bl;
                if let Some(out_resp) = out_resp {
                    for (k, v) in out_resp.headers { 
                        $builder = $builder.header(k, v); 
                    }
                }
                builder = $builder;
            }
            let res = builder.send()?;
            
            let ret = sspi.http_outgoing_auth(|header| Ok(res.headers().get_all(header).into_iter().map(|x| x.to_str().unwrap()).collect()))?;
            match ret {
                winauth::http::AuthState::Response(resp) => {
                    out_resp = Some(resp);
                }
                // We treat both cases as successful. Depending on requirements
                // you might want to require authentication (Success)
                winauth::http::AuthState::Success | winauth::http::AuthState::NotRequested => break res,
            }
        }
    }}
}

use cfg_if::cfg_if;
cfg_if! {
    if #[cfg(windows)] {

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut res = perform_ntlm_request!(reqwest::Method::GET, "http://localhost:3000", builder, {
        builder = builder.header("foo", "bar");
    });
    
    println!("Response {:?}", res);
    println!("Body: {:?}", res.text().unwrap());

    Ok(())
}
     
    } // WINDOWS
    else {
        fn main() {
            panic!("only windows supported");
        }
    }
}
