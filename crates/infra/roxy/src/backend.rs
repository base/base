//! Named backend configuration.

use url::Url;

/// A named backend with one or more RPC URLs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backend {
    /// Backend name.
    pub name: String,
    /// RPC URLs assigned to the backend.
    pub urls: Vec<Url>,
}

impl Backend {
    /// Parses a backend in `name=url[,url...]` format.
    pub fn parse(raw: &str) -> Result<Self, String> {
        if raw.trim().is_empty() {
            return Err("invalid backend: entry is empty".to_owned());
        }

        let (name, raw_urls) = raw
            .split_once('=')
            .ok_or_else(|| format!("invalid backend '{raw}': expected name=url[,url...]"))?;
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("invalid backend '{raw}': backend name is empty"));
        }
        if raw_urls.trim().is_empty() {
            return Err(format!("invalid backend '{raw}': at least one URL is required"));
        }

        let urls = raw_urls
            .split(',')
            .map(|raw_url| {
                let raw_url = raw_url.trim();
                let url = Url::parse(raw_url)
                    .map_err(|error| format!("invalid backend URL '{raw_url}': {error}"))?;
                match url.scheme() {
                    "http" | "https" => Ok(url),
                    scheme => Err(format!(
                        "invalid backend URL '{raw_url}': unsupported scheme '{scheme}'"
                    )),
                }
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self { name: name.to_owned(), urls })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_named_backend_urls() {
        let backend = Backend::parse("rpcs=http://127.0.0.1:8545,https://rpc.example.com")
            .expect("valid backend");

        assert_eq!(backend.name, "rpcs", "backend name must be preserved");
        assert_eq!(backend.urls.len(), 2, "both URLs must be parsed");
        assert_eq!(
            backend.urls[0].as_str(),
            "http://127.0.0.1:8545/",
            "first URL must be preserved"
        );
        assert_eq!(
            backend.urls[1].as_str(),
            "https://rpc.example.com/",
            "second URL must be preserved"
        );
    }

    #[test]
    fn rejects_invalid_backend_values() {
        for (raw, expected) in [
            ("", "entry is empty"),
            ("rpcs", "expected name=url"),
            ("=http://127.0.0.1:8545", "backend name is empty"),
            ("rpcs=", "at least one URL is required"),
            ("rpcs= ", "at least one URL is required"),
            ("rpcs=file:///tmp/rpc", "unsupported scheme 'file'"),
        ] {
            let error = Backend::parse(raw).expect_err("invalid backend");
            assert!(
                error.contains(expected),
                "error '{error}' must contain '{expected}' for '{raw}'"
            );
        }
    }
}
