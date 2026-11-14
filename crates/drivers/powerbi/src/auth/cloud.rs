//! The Power BI clouds (spec §3.6, Microsoft Learn's national clouds
//! table). The login host, the token's resource and the API base come only
//! from here, never from a form field: a token can't be sent elsewhere.
//! Owned by part B; written in phase 1 because [`crate::config`] reads it.

use dbine_driver::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cloud {
    Public,
    /// US Government Community Cloud (commercial Entra).
    Gcc,
    GccHigh,
    Dod,
    /// China, operated by 21Vianet.
    China,
}

/// The Fabric REST API (definition API, D2; rename and description, D4).
/// Offered in the public cloud only (sovereign endpoints not verified).
pub const FABRIC_API: &str = "https://api.fabric.microsoft.com/v1";
/// The Fabric API's host, the only one its token and its long-running
/// operations' `Location` may go to.
pub const FABRIC_HOST: &str = "api.fabric.microsoft.com";
/// The Fabric API's resource (its token's audience).
pub const FABRIC_RESOURCE: &str = "https://api.fabric.microsoft.com";
/// Power BI Service's application id, also a valid token audience (S10).
pub const POWER_BI_APP_ID: &str = "00000009-0000-0000-c000-000000000000";

impl Cloud {
    /// The form's `cloud` value.
    pub fn from_option(v: &str) -> Result<Cloud> {
        Ok(match v {
            "public" => Cloud::Public,
            "gcc" => Cloud::Gcc,
            "gcc_high" => Cloud::GccHigh,
            "dod" => Cloud::Dod,
            "china" => Cloud::China,
            _ => return Err(Error::Connect("nube desconocida".into())),
        })
    }

    /// The sign-in authority's base (`{login}/{tenant}/oauth2/v2.0/…`).
    pub fn login(self) -> &'static str {
        match self {
            Cloud::Public | Cloud::Gcc => "https://login.microsoftonline.com",
            Cloud::GccHigh | Cloud::Dod => "https://login.microsoftonline.us",
            Cloud::China => "https://login.chinacloudapi.cn",
        }
    }

    /// The token's resource (`{res}/Dataset.Read.All`, `{res}/.default`).
    pub fn resource(self) -> &'static str {
        match self {
            Cloud::Public => "https://analysis.windows.net/powerbi/api",
            Cloud::Gcc => "https://analysis.usgovcloudapi.net/powerbi/api",
            Cloud::GccHigh => "https://high.analysis.usgovcloudapi.net/powerbi/api",
            Cloud::Dod => "https://mil.analysis.usgovcloudapi.net/powerbi/api",
            Cloud::China => "https://analysis.chinacloudapi.cn/powerbi/api",
        }
    }

    /// The REST API's base; requests go to paths under it only.
    pub fn api_base(self) -> &'static str {
        match self {
            Cloud::Public => "https://api.powerbi.com/v1.0/myorg",
            Cloud::Gcc => "https://api.powerbigov.us/v1.0/myorg",
            Cloud::GccHigh => "https://api.high.powerbigov.us/v1.0/myorg",
            Cloud::Dod => "https://api.mil.powerbigov.us/v1.0/myorg",
            Cloud::China => "https://api.powerbi.cn/v1.0/myorg",
        }
    }

    /// The API base's host (what the HTTP layer compares, exactly).
    pub fn api_host(self) -> &'static str {
        let base = self.api_base();
        let rest = &base["https://".len()..];
        &rest[..rest.find('/').unwrap_or(rest.len())]
    }

    /// The XMLA endpoint's host (`powerbi://<host>/v1.0/myorg/<workspace>`).
    pub fn xmla_host(self) -> &'static str {
        self.api_host()
    }

    /// The Fabric API, where it's offered (public cloud only).
    pub fn fabric_api(self) -> Option<&'static str> {
        (self == Cloud::Public).then_some(FABRIC_API)
    }

    /// DBine's own app registration serves it (public and GCC, which uses
    /// the commercial Entra); elsewhere the user registers one.
    pub fn dbine_app(self) -> bool {
        matches!(self, Cloud::Public | Cloud::Gcc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table() {
        assert_eq!(Cloud::Public.api_host(), "api.powerbi.com");
        assert_eq!(Cloud::Gcc.api_host(), "api.powerbigov.us");
        assert_eq!(Cloud::GccHigh.api_host(), "api.high.powerbigov.us");
        assert_eq!(Cloud::Dod.api_host(), "api.mil.powerbigov.us");
        assert_eq!(Cloud::China.api_host(), "api.powerbi.cn");
        assert_eq!(Cloud::Gcc.login(), "https://login.microsoftonline.com");
        assert_eq!(Cloud::Dod.login(), "https://login.microsoftonline.us");
        assert!(Cloud::Public.fabric_api().is_some() && Cloud::Gcc.fabric_api().is_none());
        assert!(Cloud::Gcc.dbine_app() && !Cloud::China.dbine_app());
        assert!(Cloud::from_option("mars").is_err());
    }
}
