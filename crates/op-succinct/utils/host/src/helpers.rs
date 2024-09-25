use alloy_primitives::hex;
use op_succinct_client_utils::BytesHasherBuilder;
use std::{collections::HashMap, fs, io::Read, path::PathBuf};

pub fn load_kv_store(data_dir: &PathBuf) -> HashMap<[u8; 32], Vec<u8>, BytesHasherBuilder> {
    let capacity = get_file_count(data_dir);
    let mut cache: HashMap<[u8; 32], Vec<u8>, BytesHasherBuilder> =
        HashMap::with_capacity_and_hasher(capacity, BytesHasherBuilder);

    // Iterate over the files in the 'data' directory
    for entry in fs::read_dir(data_dir)
        .expect("Failed to read data directory")
        .flatten()
    {
        let path = entry.path();
        if path.is_file() {
            // Extract the file name
            let file_name = path.file_stem().unwrap().to_str().unwrap();
            // Convert the file name to PreimageKey
            if let Ok(key) = hex::decode(file_name) {
                // Read the file contents
                let mut file = fs::File::open(path).expect("Failed to open file");
                let mut contents = Vec::new();
                file.read_to_end(&mut contents)
                    .expect("Failed to read file");

                // Insert the key-value pair into the cache
                cache.insert(key.try_into().unwrap(), contents);
            }
        }
    }

    cache
}

fn get_file_count(data_dir: &PathBuf) -> usize {
    let mut file_count = 0;
    for entry in fs::read_dir(data_dir).expect("failed to read data dir") {
        let entry = entry.unwrap();
        if entry.metadata().unwrap().is_file() {
            file_count += 1;
        }
    }
    file_count
}
