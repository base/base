//! Named backend definitions for Roxy.

use url::Url;

/// A named backend: one or more RPC target URLs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backend {
    /// Backend name (unique among configured backends).
    pub name: String,
    /// Target URLs for this backend (non-empty).
    pub urls: Vec<Url>,
}

impl Backend {
    /// Parses `name=url[,url...]`.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let (name, urls_part) = raw.split_once('=').ok_or_else(|| {
            format!("invalid --backend value '{raw}': expected name=url[,url...]")
        })?;

        let name = name.trim();
        if name.is_empty() {
            return Err(format!("invalid --backend value '{raw}': backend name is empty"));
        }

        let mut urls = Vec::new();
        for part in urls_part.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let url = Url::parse(part)
                .map_err(|error| format!("invalid --backend URL '{part}' in '{raw}': {error}"))?;
            urls.push(url);
        }

        if urls.is_empty() {
            return Err(format!("invalid --backend value '{raw}': at least one URL is required"));
        }

        Ok(Self { name: name.to_owned(), urls })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_url() {
        let backend = Backend::parse("rpcs=http://127.0.0.1:8545").expect("parse");
        assert_eq!(backend.name, "rpcs");
        assert_eq!(backend.urls.len(), 1, "exactly one URL");
        assert_eq!(backend.urls[0].as_str(), "http://127.0.0.1:8545/");
    }

    #[test]
    fn parse_multiple_urls() {
        let backend =
            Backend::parse("rpcs=http://127.0.0.1:8545,http://127.0.0.1:8546").expect("parse");
        assert_eq!(backend.name, "rpcs");
        assert_eq!(backend.urls.len(), 2, "exactly two URLs");
        assert_eq!(backend.urls[0].as_str(), "http://127.0.0.1:8545/");
        assert_eq!(backend.urls[1].as_str(), "http://127.0.0.1:8546/");
    }

    #[test]
    fn parse_rejects_missing_equals() {
        let error = Backend::parse("rpcs").expect_err("missing '='");
        assert!(error.contains("expected name=url"), "error={error}");
    }

    #[test]
    fn parse_rejects_empty_urls() {
        let error = Backend::parse("rpcs=").expect_err("empty urls");
        assert!(error.contains("at least one URL"), "error={error}");
    }
}
