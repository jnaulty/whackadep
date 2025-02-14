//! This module abstracts the communication with GitHub API for a given crate

use anyhow::{anyhow, Result};
use chrono::{DateTime, Duration, FixedOffset, Utc};
use guppy::graph::PackageMetadata;
use reqwest::blocking::Response;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, USER_AGENT};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::str::FromStr;
use std::thread::sleep;
use thiserror::Error;
use url::Url;

#[derive(Serialize, Deserialize, Debug)]
pub struct CommitInfo {
    pub sha: String,
    pub commit: Commit,
    pub author: Option<GitHubUser>,
    pub committer: Option<GitHubUser>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Commit {
    pub author: User,
    pub committer: User,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct User {
    pub name: String,
    pub email: String,
    pub date: DateTime<FixedOffset>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct GitHubUser {
    // can be null if the user is not registered on GitHub
    pub login: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Issue {
    pub created_at: DateTime<FixedOffset>,
}

#[derive(Serialize, Deserialize)]
pub struct GitHubReport {
    pub name: String,               // name of the crate
    pub repository: Option<String>, // repository url
    pub is_github_repo: bool,
    pub repo_stats: RepoStats,
    pub activity_metrics: ActivityMetrics,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct RepoStats {
    pub full_name: Option<String>,
    pub default_branch: Option<String>,
    pub stargazers_count: u64,
    pub subscribers_count: u64,
    pub forks: u64,
    pub open_issues: u64, // issues + PR
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct ActivityMetrics {
    pub days_since_last_commit: u64, // on default branch
    pub days_since_last_open_issue: Option<u64>,
    pub open_issues_labeled_bug: u64,
    pub open_issues_labeled_security: u64,
    pub recent_activity: RecentActivity,
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct RecentActivity {
    // On all branches
    pub past_days: u64,
    pub commits: u64,
    pub committers: u64,
}

impl GitHubReport {
    fn new(name: String, repository: Option<String>) -> Self {
        //Returns a default GitHubReport with is_github_repo set as false
        GitHubReport {
            name,
            repository,
            is_github_repo: false,
            repo_stats: RepoStats {
                full_name: None,
                default_branch: None,
                ..Default::default()
            },
            activity_metrics: ActivityMetrics {
                ..Default::default()
            },
        }
    }
}

/// A GitHub client to fetch various metrics
pub struct GitHubAnalyzer {
    client: reqwest::blocking::Client,
}

#[derive(Debug, Error)]
pub enum GitHubRepoError {
    #[error("{} is not a valid github url", url)]
    InvalidUrl { url: Url },
    #[error("repository not found: {}", url)]
    RepoNotFound { url: Url },
    #[error("http request to GitHub failed, {:?}", error)]
    Unknown { error: anyhow::Error },
}

impl GitHubAnalyzer {
    fn construct_headers() -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("diem/whackadep"));

        let pat = std::env::var("GITHUB_TOKEN")?;
        let pat = format!("token {}", pat);
        let mut auth_value = HeaderValue::from_str(&pat)?;
        auth_value.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth_value);

        Ok(headers)
    }

    pub fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::blocking::Client::builder()
                .default_headers(Self::construct_headers()?)
                .build()?,
        })
    }

    fn make_github_rest_api_call(&self, api_endpoint: &str) -> Result<Response> {
        let mut response = self.client.get(api_endpoint).send()?;
        while response.status() == StatusCode::from_u16(429)?
            || response.status() == StatusCode::from_u16(403)?
        {
            // If api rate limit exceeded, sleep for a minute
            println!("GitHub API rate limit exceeded. Sleeping for a minute");
            sleep(std::time::Duration::from_secs(60));
            response = self.client.get(api_endpoint).send()?;
        }
        Ok(response)
    }

    /// Get overall usage and activity metrics
    pub fn analyze_github(self, package: &PackageMetadata) -> Result<GitHubReport> {
        let name = package.name();
        let repository = match package.repository().and_then(|r| Url::from_str(r).ok()) {
            Some(repository) => repository,
            None => return Ok(GitHubReport::new(name.to_string(), None)),
        };

        let repo_fullname = match self.get_github_repo_fullname(&repository) {
            Ok(name) => name,
            Err(error) => match error {
                GitHubRepoError::Unknown { .. } => return Err(error.into()),
                _ => {
                    return Ok(GitHubReport::new(
                        name.to_string(),
                        Some(repository.to_string()),
                    ))
                }
            },
        };

        // Get Overall stats for a given repo
        let repo_stats = self.get_github_repo_stats(&repo_fullname)?;

        // Get the default branch
        let default_branch = repo_stats.default_branch.clone();
        let default_branch = default_branch
            .ok_or_else(|| anyhow!("No default branch found for repository for {}", name))?;

        // Get recent activity metrics
        let activity_metrics = self.get_activity_metrics(&repo_fullname, &default_branch)?;

        Ok(GitHubReport {
            name: name.to_string(),
            repository: Some(repository.to_string()),
            is_github_repo: true,
            repo_stats,
            activity_metrics,
        })
    }

    fn is_github_url(url: &Url) -> bool {
        url.host_str()
            .map(|host| host == "github.com")
            .unwrap_or(false)
    }

    fn get_github_repo_fullname(&self, repo_url: &Url) -> Result<String, GitHubRepoError> {
        // Return the repository full name if a valid GitHub url
        if !Self::is_github_url(repo_url) {
            return Err(GitHubRepoError::InvalidUrl {
                url: repo_url.clone(),
            });
        }

        let mut segments = repo_url
            .path_segments()
            .ok_or_else(|| GitHubRepoError::InvalidUrl {
                url: repo_url.clone(),
            })?;
        let owner = segments.next().ok_or_else(|| GitHubRepoError::InvalidUrl {
            url: repo_url.clone(),
        })?;
        let repo = segments
            .next()
            .map(|repo| repo.trim_end_matches(".git"))
            .ok_or_else(|| GitHubRepoError::InvalidUrl {
                url: repo_url.clone(),
            })?;

        let repo_fullname = format!("{}/{}", owner, repo);
        match self.is_existing_github_repo(&repo_fullname) {
            Ok(flag) => match flag {
                true => Ok(repo_fullname),
                false => Err(GitHubRepoError::RepoNotFound {
                    url: repo_url.clone(),
                }),
            },
            Err(error) => Err(GitHubRepoError::Unknown { error }),
        }
    }

    pub fn is_existing_github_repo(&self, repo_fullname: &str) -> Result<bool> {
        let api_endpoint = format!("https://api.github.com/repos/{}", repo_fullname);
        let response = self.make_github_rest_api_call(&api_endpoint)?;

        if response.status().is_success() {
            Ok(true)
        } else if response.status() == reqwest::StatusCode::NOT_FOUND {
            Ok(false)
        } else {
            Err(anyhow!("http request to GitHub failed, {:?}", response))
        }
    }

    pub fn get_github_repo_stats(&self, repo_fullname: &str) -> Result<RepoStats> {
        let api_endpoint = format!("https://api.github.com/repos/{}", repo_fullname);
        let response = self.make_github_rest_api_call(&api_endpoint)?;

        if !response.status().is_success() {
            return Err(anyhow!("http request to GitHub failed, {:?}", response));
        }

        Ok(response.json()?)
    }

    pub fn get_activity_metrics(
        self,
        repo_fullname: &str,
        default_branch: &str,
    ) -> Result<ActivityMetrics> {
        let days_since_last_commit = self
            .get_time_since_last_commit(repo_fullname, default_branch)?
            .num_days() as u64;

        let days_since_last_open_issue = self
            .get_time_since_last_open_issue(repo_fullname)?
            .map(|duration| duration.num_days() as u64);

        let open_issues_labeled_bug =
            self.get_total_open_issue_count_for_label(repo_fullname, "bug")?;
        let open_issues_labeled_security =
            self.get_total_open_issue_count_for_label(repo_fullname, "security")?;

        let past_days = 6 * 30;
        let recent_activity = self.get_stats_on_recent_activity(repo_fullname, past_days)?;

        Ok(ActivityMetrics {
            days_since_last_commit,
            days_since_last_open_issue,
            open_issues_labeled_bug,
            open_issues_labeled_security,
            recent_activity,
        })
    }

    pub fn get_time_since_last_commit(
        &self,
        repo_fullname: &str,
        default_branch: &str,
    ) -> Result<Duration> {
        let api_endpoint = format!(
            "https://api.github.com/repos/{}/commits?sha={}&per_page=1",
            repo_fullname, default_branch
        );
        let response = self.make_github_rest_api_call(&api_endpoint)?;

        if !response.status().is_success() {
            return Err(anyhow!("http request to GitHub failed, {:?}", response));
        }
        let response: Vec<CommitInfo> = response.json()?;
        if response.is_empty() {
            // At lease one commit should be there
            return Err(anyhow!(
                "No commit found for {}, {:?}",
                repo_fullname,
                response
            ));
        }

        let last_commit = &response[0];
        let last_commit_date = last_commit.commit.committer.date;

        let utc_now: DateTime<Utc> = Utc::now();
        let duration = utc_now.signed_duration_since(last_commit_date);
        if duration.num_days() < 0 {
            return Err(anyhow!(
                "time not synched between last commit, {}, and current time, {}",
                last_commit_date,
                utc_now
            ));
        }
        Ok(duration)
    }

    pub fn get_time_since_last_open_issue(&self, repo_fullname: &str) -> Result<Option<Duration>> {
        let api_endpoint = format!(
            "https://api.github.com/repos/{}/issues?state=open&per_page=1",
            repo_fullname
        );
        let response = self.make_github_rest_api_call(&api_endpoint)?;

        if !response.status().is_success() {
            return Err(anyhow!("http request to GitHub failed, {:?}", response));
        }

        let response: Vec<Issue> = response.json()?;

        if response.is_empty() {
            Ok(None)
        } else {
            let last_open_issue = &response[0];
            let last_open_issue_date = last_open_issue.created_at;

            let utc_now: DateTime<Utc> = Utc::now();
            let duration = utc_now.signed_duration_since(last_open_issue_date);
            if duration.num_days() < 0 {
                return Err(anyhow!(
                    "time not synched between last commit, {}, and current time, {}",
                    last_open_issue_date,
                    utc_now
                ));
            };
            Ok(Some(duration))
        }
    }

    pub fn get_total_open_issue_count_for_label(
        &self,
        repo_fullname: &str,
        label: &str,
    ) -> Result<u64> {
        let mut total = 0;
        let mut page = 1;

        loop {
            let api_endpoint = format!(
                "https://api.github.com/repos/{}/issues?state=open&per_page=100&page={}&labels={}",
                repo_fullname, page, label
            );
            let response = self.make_github_rest_api_call(&api_endpoint)?;
            let response: Vec<Issue> = response.json()?;

            if response.is_empty() {
                break;
            } else {
                total += response.len() as u64;
                page += 1;
            }
        }
        Ok(total)
    }

    pub fn get_stats_on_recent_activity(
        &self,
        repo_fullname: &str,
        past_days: u64,
    ) -> Result<RecentActivity> {
        let since_query_string = chrono::Utc::now()
            .checked_sub_signed(chrono::Duration::days(past_days as i64))
            .ok_or_else(|| anyhow!("Cannot convert past duration into query string"))?
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        let mut page = 1;
        let mut recent_commit_infos: Vec<CommitInfo> = Vec::new();

        // Get all recent commits
        loop {
            let api_endpoint = format!(
                "https://api.github.com/repos/{}/commits?since={}&per_page=100&page={}",
                repo_fullname, since_query_string, page
            );
            let response = self.make_github_rest_api_call(&api_endpoint)?;
            if !response.status().is_success() {
                return Err(anyhow!("http request to GitHub failed, {:?}", response));
            }

            let mut response: Vec<CommitInfo> = response.json()?;
            if response.is_empty() {
                break;
            } else {
                recent_commit_infos.append(&mut response);
                page += 1;
            }
        }

        // Analyze recent commits
        if recent_commit_infos.is_empty() {
            return Ok(RecentActivity {
                past_days,
                ..Default::default()
            });
        }

        let commits = recent_commit_infos.len() as u64;
        let mut committers: HashSet<String> = HashSet::new();
        for commit_info in recent_commit_infos {
            committers.insert(commit_info.commit.committer.email);
        }
        let committers = committers.len() as u64;

        Ok(RecentActivity {
            past_days,
            commits,
            committers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use guppy::{graph::PackageGraph, MetadataCommand};
    use std::path::PathBuf;

    fn test_github_analyzer() -> GitHubAnalyzer {
        GitHubAnalyzer::new().unwrap()
    }

    fn get_test_graph() -> PackageGraph {
        MetadataCommand::new()
            .current_dir(PathBuf::from("resources/test/valid_dep"))
            .build_graph()
            .unwrap()
    }

    fn get_test_repo_fullname(package_name: &str) -> String {
        let graph = get_test_graph();
        let pkg = graph.packages().find(|p| p.name() == package_name).unwrap();

        let repository = pkg.repository().unwrap();
        let url = Url::from_str(repository).unwrap();
        GitHubAnalyzer::new()
            .unwrap()
            .get_github_repo_fullname(&url)
            .unwrap()
    }

    fn get_test_repo_default_branch(package_name: &str) -> String {
        let graph = get_test_graph();
        let pkg = graph.packages().find(|p| p.name() == package_name).unwrap();
        let github_analyzer = test_github_analyzer();
        let report = github_analyzer.analyze_github(&pkg).unwrap();
        report.repo_stats.default_branch.unwrap()
    }

    fn get_test_github_report(package_name: &str) -> GitHubReport {
        let github_analyzer = test_github_analyzer();
        let graph = get_test_graph();
        let pkg = graph.packages().find(|p| p.name() == package_name).unwrap();
        github_analyzer.analyze_github(&pkg).unwrap()
    }

    #[test]
    fn test_github_stats_for_libc() {
        let report = get_test_github_report("libc");
        assert!(report.is_github_repo);
        // Relying on Libc to have at least one star on GitHub
        assert!(report.repo_stats.stargazers_count > 0);
    }

    #[test]
    fn test_github_stats_for_gitlab() {
        let report = get_test_github_report("gitlab");
        assert!(!report.is_github_repo);
        assert_eq!(report.repo_stats.stargazers_count, 0);
    }

    #[test]
    fn test_github_time_since_last_commit() {
        let github_analyzer = test_github_analyzer();
        let package_name = "octocrab";
        let fullname = get_test_repo_fullname(package_name);
        let default_branch = get_test_repo_default_branch(package_name);
        let time_since_last_commit = github_analyzer
            .get_time_since_last_commit(&fullname, &default_branch)
            .unwrap();
        assert!(time_since_last_commit.num_nanoseconds().unwrap() > 0)
    }

    #[test]
    fn test_github_time_since_last_open_issue() {
        let package_name = "libc";
        let repo_fullname = get_test_repo_fullname(package_name);
        let report = get_test_github_report(package_name);

        let github_analyzer = test_github_analyzer();
        let time_since_last_open_issue = github_analyzer
            .get_time_since_last_open_issue(&repo_fullname)
            .unwrap();

        if time_since_last_open_issue.is_none() {
            assert_eq!(report.repo_stats.open_issues, 0);
        } else {
            assert!(report.repo_stats.open_issues > 0);
        }
    }

    #[test]
    fn test_github_total_open_issue_count_for_label() {
        let github_analyzer = test_github_analyzer();
        let repo_fullname = get_test_repo_fullname("libc");

        let open_bugs = github_analyzer
            .get_total_open_issue_count_for_label(&repo_fullname, "bug")
            .unwrap();
        let open_security = github_analyzer
            .get_total_open_issue_count_for_label(&repo_fullname, "security")
            .unwrap();

        println!(
            "{} has {} open bugs and {} open security",
            repo_fullname, open_bugs, open_security
        );
    }

    #[test]
    fn test_github_recent_activity() {
        let github_analyzer = test_github_analyzer();
        let fullname = get_test_repo_fullname("libc");
        let past_days = 6 * 30;
        let recent_activity = github_analyzer
            .get_stats_on_recent_activity(&fullname, past_days)
            .unwrap();
        println!("recent_activity for {} is {:?}", fullname, recent_activity);
        assert_eq!(recent_activity.past_days, past_days);
    }
}
