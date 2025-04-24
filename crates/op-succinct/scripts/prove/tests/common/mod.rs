use reqwest::Client;
use serde_json::json;

/// Posts the provided message on the provided PR on Github.
pub async fn post_to_github_pr(
    owner: &str,
    repo: &str,
    pr_number: &str,
    token: &str,
    message: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new();
    let base_url = format!("https://api.github.com/repos/{owner}/{repo}");

    // Get all comments on the PR
    let comments_url = format!("{base_url}/issues/{pr_number}/comments");
    let comments_response = client
        .get(&comments_url)
        .header("Authorization", format!("token {token}"))
        .header("User-Agent", "sp1-perf-bot")
        .send()
        .await?;

    let comments: Vec<serde_json::Value> = comments_response.json().await?;

    // Look for an existing comment from our bot
    let bot_comment = comments.iter().find(|comment| {
        comment["user"]["login"]
            .as_str()
            .map(|login| login == "github-actions[bot]")
            .unwrap_or(false)
    });

    if let Some(existing_comment) = bot_comment {
        // Update the existing comment
        let comment_url = existing_comment["url"].as_str().unwrap();
        let response = client
            .patch(comment_url)
            .header("Authorization", format!("token {token}"))
            .header("User-Agent", "sp1-perf-bot")
            .json(&json!({
                "body": message
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(format!("Failed to update comment: {:?}", response.text().await?).into());
        }
    } else {
        // Create a new comment
        let response = client
            .post(&comments_url)
            .header("Authorization", format!("token {token}"))
            .header("User-Agent", "sp1-perf-bot")
            .json(&json!({
                "body": message
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(format!("Failed to post comment: {:?}", response.text().await?).into());
        }
    }

    Ok(())
}
