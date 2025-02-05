use redis::{Client, Commands, RedisResult};
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub struct Cache {
    client: Client,
}

impl Cache {
    pub fn new(redis_url: &str) -> RedisResult<Self> {
        let client = Client::open(redis_url)?;
        Ok(Self { client })
    }

    #[allow(dead_code)]
    pub fn set<T: Serialize>(
        &self,
        key: &str,
        value: &T,
        expiry_secs: Option<u64>,
    ) -> RedisResult<()> {
        let mut conn = self.client.get_connection()?;
        let serialized = serde_json::to_string(value).map_err(|e| {
            redis::RedisError::from((
                redis::ErrorKind::ParseError,
                "Serialization error",
                e.to_string(),
            ))
        })?;

        match expiry_secs {
            Some(secs) => conn.set_ex(key, serialized, secs)?,
            None => conn.set(key, serialized)?,
        }
        Ok(())
    }

    pub fn get<T: for<'de> Deserialize<'de>>(&self, key: &str) -> RedisResult<Option<T>> {
        let mut conn = self.client.get_connection()?;
        let value: Option<String> = conn.get(key)?;

        match value {
            Some(val) => {
                let deserialized = serde_json::from_str(&val).map_err(|e| {
                    redis::RedisError::from((
                        redis::ErrorKind::ParseError,
                        "Deserialization error",
                        e.to_string(),
                    ))
                })?;
                Ok(Some(deserialized))
            }
            None => Ok(None),
        }
    }
}
