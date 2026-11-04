use std::sync::Arc;

use axum::{extract::State, Json};
use serde::Serialize;
use tracing::warn;

#[derive(Serialize)]
pub struct SourceBranch {
    pub name: String,
    pub commit_sha: String,
    pub updated_at: String,
}

#[derive(Serialize)]
pub struct SourceRepo {
    pub id: String,
    pub title: String,
    pub description: String,
    pub default_branch: String,
    pub size_kb: u64,
    pub updated_at: String,
    pub branches: Vec<SourceBranch>,
    pub download_url_template: String,
}

#[derive(Serialize)]
pub struct SourcesResponse {
    pub repo: SourceRepo,
    pub branches: Vec<SourceBranch>,
}

pub async fn api_list_sources(State(state): State<Arc<crate::state::AppState>>) -> Json<SourcesResponse> {
    let repo_slug = std::env::var("GITHUB_REPO").unwrap_or_else(|_| "sandhikagalih/project-kalian".into());
    let api_base = format!("https://api.github.com/repos/{}", repo_slug);
    let archive_template = format!("https://github.com/{}/archive/refs/heads/{{branch}}.zip", repo_slug);

    let client = match reqwest::Client::builder()
        .user_agent("nuvcode")
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            warn!("sources: cannot build HTTP client");
            return Json(SourcesResponse { repo: SourceRepo {
                id: repo_slug.clone(), title: repo_slug.clone(), description: "".into(),
                default_branch: "main".into(), size_kb: 0, updated_at: "".into(),
                branches: vec![], download_url_template: archive_template,
            }, branches: vec![] });
        }
    };

    let repo_meta = client.get(format!("{}/", api_base)).send().await;
    let mut repo_info = SourceRepo {
        id: repo_slug.clone(),
        title: repo_slug.clone(),
        description: "".into(),
        default_branch: "main".into(),
        size_kb: 0,
        updated_at: "".into(),
        branches: vec![],
        download_url_template: archive_template,
    };

    if let Ok(r) = repo_meta {
        if let Ok(json) = r.json::<serde_json::Value>().await {
            repo_info.title = json.get("name").and_then(|v| v.as_str()).unwrap_or(&repo_slug).to_string();
            repo_info.description = json.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
            repo_info.default_branch = json.get("default_branch").and_then(|v| v.as_str()).unwrap_or("main").to_string();
            repo_info.size_kb = json.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
            repo_info.updated_at = json.get("updated_at").and_then(|v| v.as_str()).unwrap_or("").to_string();
        }
    }

    let branches_resp = client.get(format!("{}/branches", api_base)).send().await;
    let mut branches = Vec::new();
    if let Ok(r) = branches_resp {
        if let Ok(json) = r.json::<serde_json::Value>().await {
            if let Some(arr) = json.as_array() {
                for b in arr {
                    let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let sha = b.get("commit").and_then(|c| c.get("sha")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let updated = b.get("commit").and_then(|c| c.get("commit")).and_then(|c| c.get("committer")).and_then(|c| c.get("date")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                    branches.push(SourceBranch { name, commit_sha: sha, updated_at: updated });
                }
            }
        }
    }

    Json(SourcesResponse { repo: repo_info, branches })
}
