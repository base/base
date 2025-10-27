use std::collections::HashMap;
use std::fs;

pub fn load_kafka_config_from_file(
    properties_file_path: &str,
) -> Result<HashMap<String, String>, std::io::Error> {
    let kafka_properties = fs::read_to_string(properties_file_path)?;

    let mut config = HashMap::new();

    for line in kafka_properties.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            config.insert(key.trim().to_string(), value.trim().to_string());
        }
    }

    Ok(config)
}
