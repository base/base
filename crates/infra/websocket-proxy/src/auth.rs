use std::collections::{HashMap, HashSet};

use crate::auth::AuthenticationParseError::{
    DuplicateAPIKeyArgument, DuplicateApplicationArgument, MissingAPIKeyArgument,
    MissingApplicationArgument, NoData, TooManyComponents,
};

/// Maps API keys to application names for request authentication.
#[derive(Clone, Debug)]
pub struct Authentication {
    key_to_application: HashMap<String, String>,
}

/// Errors that can occur when parsing authentication arguments.
#[derive(Debug, PartialEq, Eq)]
pub enum AuthenticationParseError {
    /// No API key arguments were provided.
    NoData(),
    /// The application name is missing from the argument.
    MissingApplicationArgument(String),
    /// The API key is missing from the argument.
    MissingAPIKeyArgument(String),
    /// The argument contains more than the expected `app:key` components.
    TooManyComponents(String),
    /// The same application name appears more than once.
    DuplicateApplicationArgument(String),
    /// The same API key appears more than once.
    DuplicateAPIKeyArgument(String),
}

impl std::fmt::Display for AuthenticationParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoData() => write!(f, "No API Keys Provided"),
            MissingApplicationArgument(arg) => write!(f, "Missing application argument: [{arg}]"),
            MissingAPIKeyArgument(app) => write!(f, "Missing API Key argument: [{app}]"),
            TooManyComponents(app) => write!(f, "Too many components: [{app}]"),
            DuplicateApplicationArgument(app) => {
                write!(f, "Duplicate application argument: [{app}]")
            }
            DuplicateAPIKeyArgument(app) => write!(f, "Duplicate API key: [{app}]"),
        }
    }
}

impl std::error::Error for AuthenticationParseError {}

impl TryFrom<Vec<String>> for Authentication {
    type Error = AuthenticationParseError;

    fn try_from(args: Vec<String>) -> Result<Self, Self::Error> {
        let mut applications = HashSet::new();
        let mut key_to_application: HashMap<String, String> = HashMap::new();

        if args.is_empty() {
            return Err(NoData());
        }

        for arg in args {
            let mut parts = arg.split(':');
            let app = parts.next().ok_or_else(|| MissingApplicationArgument(arg.clone()))?;
            if app.is_empty() {
                return Err(MissingApplicationArgument(arg.clone()));
            }

            let key = parts.next().ok_or_else(|| MissingAPIKeyArgument(app.to_string()))?;
            if key.is_empty() {
                return Err(MissingAPIKeyArgument(app.to_string()));
            }

            if parts.count() > 0 {
                return Err(TooManyComponents(app.to_string()));
            }

            if applications.contains(app) {
                return Err(DuplicateApplicationArgument(app.to_string()));
            }

            if key_to_application.contains_key(key) {
                return Err(DuplicateAPIKeyArgument(app.to_string()));
            }

            applications.insert(app.to_string());
            key_to_application.insert(key.to_string(), app.to_string());
        }

        Ok(Self { key_to_application })
    }
}

impl Authentication {
    /// Create an authentication instance with no registered keys.
    pub fn none() -> Self {
        Self { key_to_application: HashMap::new() }
    }

    /// Create a new authentication instance from a map of API keys to application names.
    pub const fn new(api_keys: HashMap<String, String>) -> Self {
        Self { key_to_application: api_keys }
    }

    /// Look up the application name associated with the given API key.
    pub fn get_application_for_key(&self, api_key: &str) -> Option<&String> {
        self.key_to_application.get(api_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parsing() {
        let auth = Authentication::try_from(vec![
            "app1:key1".to_string(),
            "app2:key2".to_string(),
            "app3:key3".to_string(),
        ])
        .unwrap();

        assert_eq!(auth.key_to_application.len(), 3);
        assert_eq!(auth.key_to_application["key1"], "app1");
        assert_eq!(auth.key_to_application["key2"], "app2");
        assert_eq!(auth.key_to_application["key3"], "app3");

        let auth = Authentication::try_from(vec![
            "app1:key1".to_string(),
            String::new(),
            "app3:key3".to_string(),
        ]);
        assert!(auth.is_err());
        assert_eq!(auth.unwrap_err(), MissingApplicationArgument(String::new()));

        let auth = Authentication::try_from(vec![
            "app1:key1".to_string(),
            "app2".to_string(),
            "app3:key3".to_string(),
        ]);
        assert!(auth.is_err());
        assert_eq!(auth.unwrap_err(), MissingAPIKeyArgument("app2".into()));

        let auth = Authentication::try_from(vec![
            "app1:key1".to_string(),
            ":".to_string(),
            "app3:key3".to_string(),
        ]);
        assert!(auth.is_err());
        assert_eq!(auth.unwrap_err(), MissingApplicationArgument(":".into()));

        let auth = Authentication::try_from(vec![
            "app1:key1".to_string(),
            "app2:".to_string(),
            "app3:key3".to_string(),
        ]);
        assert!(auth.is_err());
        assert_eq!(auth.unwrap_err(), MissingAPIKeyArgument("app2".into()));

        let auth = Authentication::try_from(vec![
            "app1:key1".to_string(),
            "app2:key2:unexpected2".to_string(),
            "app3:key3".to_string(),
        ]);
        assert!(auth.is_err());
        assert_eq!(auth.unwrap_err(), TooManyComponents("app2".into()));

        let auth = Authentication::try_from(vec![
            "app1:key1".to_string(),
            "app1:key3".to_string(),
            "app2:key2".to_string(),
        ]);
        assert!(auth.is_err());
        assert_eq!(auth.unwrap_err(), DuplicateApplicationArgument("app1".into()));

        let auth = Authentication::try_from(vec!["app1:key1".to_string(), "app2:key1".to_string()]);
        assert!(auth.is_err());
        assert_eq!(auth.unwrap_err(), DuplicateAPIKeyArgument("app2".into()));
    }
}
