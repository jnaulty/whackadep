//! A rust dependency analysis tool.
//!
//! `depdive` provides various analysis metrics for
//! i) Rust crates to aid in dependency selection and monitoring,
//! i) and their version updates, to aid in security review
//! (e.g., for pull requests created by dependabot).
//!
//! # Dependency update review
//! Given two commit points of a repo,
//! or two paths of repos, presumably the same repo checked out at two commit points,
//! depdive can determine the dependencies that have been updated between the two commits
//! and generate a update review report consisting:
//! 1. Presence of known advisories
//! 2. Change in build script files
//! 3. Change in unsafe files
//! 4. If code hosted on crates.io differs from the git source
//! 5. Version diff summary, list of changed files.
//! Depdive also offer the update review report in a markdown formatted string
//! so that when integrated into CI tooling,
//! you can use the output string as it is
//! and post on wherever required
//! (e.g., pull requests updating depndencies, [See this example](https://github.com/diem/diem/blob/main/.github/workflows/dep-update-review.yml)).
//!
//! # Dependency monitoring metrics
//! You can provide the path of your Cargo project
//! and get the dependency monitoring metrics in `json` format,
//! such as usage and activity metrics,
//! lines of code, and unsafe code of your dependency crates.
//! Check impls of DependencyAnalyzer and DependencyGraphAnalyzer at the library root.
//! Note that, code-mterics use (cargo-geiger)[https://github.com/rust-secure-code/cargo-geiger] which cannot be run more than once at a time.

use anyhow::{anyhow, Result};
use git2::{build::CheckoutBuilder, Oid, Repository};
use guppy::graph::PackageGraph;
use guppy::MetadataCommand;
use semver::Version;
use separator::Separatable;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

pub mod advisory;
pub mod code;
pub mod cratesio;
pub mod diff;
pub mod ghcomment;
pub mod github;
mod guppy_wrapper;
pub mod super_toml;
pub mod update;

use cratesio::CratesioReport;
use ghcomment::{Emoji::*, GitHubCommentGenerator, TextStyle::*};
use github::GitHubReport;
use guppy_wrapper::{
    get_all_dependencies, get_dep_kind_map, get_direct_dependencies, DependencyKind,
};
use update::{CrateVersionRustSecAdvisory, UpdateReviewReport, VersionConflict};

/// Usage and Activity metrics for a crate
#[derive(Serialize, Deserialize)]
pub struct PackageMetrics {
    pub name: String,
    pub is_direct: bool,
    pub kind: DependencyKind,
    pub cratesio_metrics: Option<CratesioReport>,
    pub github_metrics: Option<GitHubReport>,
}

pub struct DependencyAnalyzer;

impl DependencyAnalyzer {
    /// Given a cargo project path, outputs usage and activity metrics
    pub fn get_dep_pacakge_metrics_in_json_from_path(
        path: &Path,
        only_direct: bool,
    ) -> Result<String> {
        let graph = MetadataCommand::new().current_dir(path).build_graph()?;
        Self::get_dep_pacakge_metrics_in_json(&graph, only_direct)
    }

    /// Given a guppy graph, outputs usage and activity metrics
    fn get_dep_pacakge_metrics_in_json(graph: &PackageGraph, only_direct: bool) -> Result<String> {
        let mut output: Vec<PackageMetrics> = Vec::new();

        let all_deps = get_all_dependencies(graph);
        let direct_deps: HashSet<(&str, &Version)> = get_direct_dependencies(graph)
            .iter()
            .map(|pkg| (pkg.name(), pkg.version()))
            .collect();
        let dep_kind_map = get_dep_kind_map(graph)?;

        for dep in &all_deps {
            let is_direct = direct_deps.contains(&(dep.name(), dep.version()));
            if only_direct && !is_direct {
                continue;
            }
            let kind = dep_kind_map
                .get(&(dep.name().to_string(), dep.version().clone()))
                .ok_or_else(|| {
                    anyhow!(
                        "fatal error in determining dependency kind for {}:{}",
                        dep.name(),
                        dep.version()
                    )
                })?
                .clone();

            let cratesio_metrics = cratesio::CratesioAnalyzer::new()?;
            let cratesio_metrics: Option<CratesioReport> =
                cratesio_metrics.analyze_cratesio(dep).ok();

            let github_metrics = github::GitHubAnalyzer::new()?;
            let github_metrics: Option<GitHubReport> = github_metrics.analyze_github(dep).ok();

            output.push(PackageMetrics {
                name: dep.name().to_string(),
                is_direct,
                kind,
                cratesio_metrics,
                github_metrics,
            });
        }

        let json_output = serde_json::to_string(&output)?;

        Ok(json_output)
    }
}

pub struct DependencyGraphAnalyzer;

impl DependencyGraphAnalyzer {
    /// Given a cargo project path, outputs loc and unsafe loc metrics
    pub fn get_code_metrics_in_json_from_path(path: &Path, only_direct: bool) -> Result<String> {
        let graph = MetadataCommand::new().current_dir(path).build_graph()?;
        Self::get_code_metrics_in_json(&graph, only_direct)
    }

    /// Given a guppy graph, outputs loc and unsafe loc metrics
    fn get_code_metrics_in_json(graph: &PackageGraph, only_direct: bool) -> Result<String> {
        let code_reports = code::CodeAnalyzer::new();
        let reports = code_reports.analyze_code(graph, only_direct)?;
        let json_output = serde_json::to_string(&reports)?;
        Ok(json_output)
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct AdvisoryHighlight {
    pub status: AdvisoryStatus,
    pub crate_name: String,
    pub id: String,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum AdvisoryStatus {
    Fixed,
    Introduced,
    Unfixed, // or, persistent, advisory is in both the versions
             // before and after of an update
}

pub struct UpdateAnalyzer;

impl UpdateAnalyzer {
    /// Given two guppy graph, prior and post,
    /// Analyzed the updated dependencies
    pub fn run_update_analyzer(
        prior_graph: &PackageGraph,
        post_graph: &PackageGraph,
    ) -> Result<UpdateReviewReport> {
        let update_analyzer = update::UpdateAnalyzer::new();
        update_analyzer.analyze_updates(prior_graph, post_graph)
    }

    /// Given two guppy graph, prior and post,
    /// Analyzed the updated dependencies
    /// and outputs a markdown formatted report
    pub fn get_summary_report(
        prior_graph: &PackageGraph,
        post_graph: &PackageGraph,
    ) -> Result<Option<String>> {
        let update_review_report = Self::run_update_analyzer(prior_graph, post_graph)?;
        if update_review_report.dep_update_review_reports.is_empty()
            && update_review_report.version_conflicts.is_empty()
        {
            return Ok(None);
        }

        let mut gh = GitHubCommentGenerator::new();

        // Flags for known and new advisory
        let mut advisory_highlights: HashSet<AdvisoryHighlight> = HashSet::new();

        // Write down info on updated dependencies
        gh.add_header("Dependency update review", 2);
        for report in &update_review_report.dep_update_review_reports {
            // Version update info
            gh.add_header(
                &format!(
                    "{} updated: {} --> {}",
                    report.name, report.prior_version.version, report.updated_version.version
                ),
                3,
            );

            // Advisory
            let mut details: String = String::new();
            let mut checkmark_table: Vec<Vec<&str>> = vec![vec![
                "No known advisories",
                GitHubCommentGenerator::get_checkmark(
                    report.updated_version.known_advisories.is_empty(),
                ),
            ]];

            // Keep track of advisory_highlights

            // a closure to add to advisory highlights set
            let mut add_to_advisory_highlights =
                |a: &CrateVersionRustSecAdvisory, status: AdvisoryStatus| {
                    advisory_highlights.insert(AdvisoryHighlight {
                        status,
                        crate_name: report.name.clone(),
                        id: a.id.clone(),
                        url: a.url.clone().map(|url| url.to_string()),
                    })
                };

            // Add to advisory highlights for current crate
            report
                .updated_version
                .known_advisories
                .iter()
                .for_each(|a| {
                    let status = if report.prior_version.known_advisories.contains(a) {
                        AdvisoryStatus::Unfixed
                    } else {
                        AdvisoryStatus::Introduced
                    };
                    add_to_advisory_highlights(a, status);
                });
            report
                .prior_version
                .known_advisories
                .iter()
                .filter(|a| !report.updated_version.known_advisories.contains(a))
                .for_each(|a| {
                    add_to_advisory_highlights(a, AdvisoryStatus::Fixed);
                });

            // a closure for generating hyperlink of an advisory
            let get_hyperlink = |a: &CrateVersionRustSecAdvisory| {
                if let Some(url) = &a.url {
                    GitHubCommentGenerator::get_hyperlink(&a.id, &url.to_string())
                } else {
                    a.id.clone()
                }
            };

            if !report.updated_version.known_advisories.is_empty() {
                let ids: Vec<String> = report
                    .updated_version
                    .known_advisories
                    .iter()
                    .map(|a| get_hyperlink(a))
                    .collect();
                gh.add_header(":bomb: The updated version contains known advisories", 3);
                gh.add_bulleted_list(&ids, &Plain);
            }

            let fixed_advisories: Vec<String> = report
                .prior_version
                .known_advisories
                .iter()
                .filter(|a| !report.updated_version.known_advisories.contains(a))
                .map(|a| get_hyperlink(a))
                .collect();
            if !fixed_advisories.is_empty() {
                gh.add_header(":tada: This update fixes known advisories", 3);
                gh.add_bulleted_list(&fixed_advisories, &Plain);
            }

            // Diff summary
            match &report.diff_stats {
                None => checkmark_table.push(vec![
                    "Depdive failed to get the diff between versions from crates.io",
                    GitHubCommentGenerator::get_emoji(Warning),
                ]),
                Some(stats) => {
                    // Diff overview
                    details.push_str(&GitHubCommentGenerator::get_collapsible_section(
                        "Click to show version diff summary",
                        &GitHubCommentGenerator::get_html_table(&[
                            vec![
                                "total files changed".to_string(),
                                stats.files_changed.len().separated_string(),
                            ],
                            vec![
                                "total rust files changed".to_string(),
                                stats.rust_files_changed.separated_string(),
                            ],
                            vec![
                                "total loc change".to_string(),
                                (stats.insertions + stats.deletions).separated_string(),
                            ],
                        ]),
                    ));

                    let changed_file_paths: Vec<String> =
                        stats.files_changed.iter().cloned().collect();
                    details.push_str(&GitHubCommentGenerator::get_collapsible_section(
                        "Click to show changed files",
                        &GitHubCommentGenerator::get_bulleted_list(&changed_file_paths, &Code),
                    ));

                    checkmark_table.push(vec![
                        "No change in the build script",
                        GitHubCommentGenerator::get_checkmark(
                            stats.modified_build_scripts.is_empty(),
                        ),
                    ]);
                    if !stats.modified_build_scripts.is_empty() {
                        let paths: Vec<String> =
                            stats.modified_build_scripts.iter().cloned().collect();
                        details.push_str(&GitHubCommentGenerator::get_collapsible_section(
                            "Click to show modified build scripts",
                            &GitHubCommentGenerator::get_bulleted_list(&paths, &Code),
                        ));
                    }

                    checkmark_table.push(vec![
                        "No change in any file with unsafe code",
                        GitHubCommentGenerator::get_checkmark(stats.unsafe_file_changed.is_empty()),
                    ]);
                    if !stats.unsafe_file_changed.is_empty() {
                        let paths: Vec<String> = stats
                            .unsafe_file_changed
                            .iter()
                            .map(|stats| stats.file.clone())
                            .collect();
                        details.push_str(&GitHubCommentGenerator::get_collapsible_section(
                            "Click to show changed files with unsafe code",
                            &GitHubCommentGenerator::get_bulleted_list(&paths, &Code),
                        ));
                    }
                }
            }

            if let Some(crate_source_diff_report) = &report.updated_version.crate_source_diff_report
            {
                match crate_source_diff_report.is_different {
                    None => {
                        checkmark_table.push(vec![
                            "Depdive failed to compare the crates.io code with its git source",
                            GitHubCommentGenerator::get_emoji(Warning),
                        ]);
                    }
                    Some(f) => {
                        checkmark_table.push(vec![
                            "The source and crates.io code are the same",
                            GitHubCommentGenerator::get_checkmark(!f),
                        ]);
                        if f {
                            let changed_files = crate_source_diff_report
                                .file_diff_stats
                                .as_ref()
                                .ok_or_else(|| {
                                anyhow!("Cannot locate file paths in git source diff report")
                            })?;
                            // Only added and modified files are of concern
                            let paths: Vec<String> = changed_files
                                .files_added
                                .union(&changed_files.files_modified)
                                .cloned()
                                .collect();
                            details.push_str(&GitHubCommentGenerator::get_collapsible_section(
                                "Click to show the files that differ in crates.io from the git source",
                                &GitHubCommentGenerator::get_bulleted_list(&paths, &Code),
                            ));
                        }
                    }
                }
            } else {
                return Err(anyhow!("no crates source diff report for the new version"));
            }

            gh.add_html_table(&checkmark_table);
            gh.add_collapsible_section("Cilck to show details", &details);
        }

        if !update_review_report.version_conflicts.is_empty() {
            let mut conflicts: Vec<String> = Vec::new();
            for conflict in &update_review_report.version_conflicts {
                match conflict {
                    VersionConflict::DirectTransitiveVersionConflict {
                        name,
                        direct_dep_version,
                        transitive_dep_version,
                    } => conflicts.push(format!(
                        "{} has version {} as a transitive dep but version {} as a direct dep",
                        name, transitive_dep_version, direct_dep_version
                    )),
                }
            }

            gh.add_collapsible_section(
                ":warning: Possible dependency Conflicts",
                &GitHubCommentGenerator::get_bulleted_list(&conflicts, &Plain),
            );
        }

        // Take advisory highlights to the top
        let advisory_banner = Self::get_advisory_banner(&advisory_highlights);
        Ok(Some(format!("{}\n{}", advisory_banner, gh.get_comment())))
    }

    fn get_advisory_banner(advisory_highlights: &HashSet<AdvisoryHighlight>) -> String {
        let mut advisory_banner: String = String::new();

        let introduced = advisory_highlights
            .iter()
            .filter(|a| a.status == AdvisoryStatus::Introduced)
            .count();
        if introduced > 0 {
            advisory_banner.push_str(&GitHubCommentGenerator::get_header_text(
                &format!(
                    ":bomb: This update introduces {} known {}\n",
                    introduced,
                    advisory_text(introduced)
                ),
                1,
            ));
        }

        let unfixed = advisory_highlights
            .iter()
            .filter(|a| a.status == AdvisoryStatus::Unfixed)
            .count();
        if unfixed > 0 {
            advisory_banner.push_str(&GitHubCommentGenerator::get_header_text(
                &format!(
                    ":bomb: {} known {} still unfixed\n",
                    unfixed,
                    advisory_text(unfixed)
                ),
                1,
            ));
        }

        let fixed = advisory_highlights
            .iter()
            .filter(|a| a.status == AdvisoryStatus::Fixed)
            .count();
        if fixed > 0 {
            advisory_banner.push_str(&GitHubCommentGenerator::get_header_text(
                &format!(
                    ":tada: This update fixes {} known {}\n",
                    fixed,
                    advisory_text(fixed)
                ),
                1,
            ));
        }

        fn advisory_text(n: usize) -> &'static str {
            if n == 1 {
                "advisory"
            } else {
                "advisories"
            }
        }

        advisory_banner
    }

    /// Get update review report in markdown format
    /// for a given repo and prior and post commit
    pub fn run_update_analyzer_from_repo_commits(
        path: &Path,
        commit_a: &str,
        commit_b: &str,
    ) -> Result<Option<String>> {
        let repo = Repository::open(&path)?;
        let starter_commit = repo.head()?.peel_to_commit()?;

        let mut checkout_builder = CheckoutBuilder::new();
        checkout_builder.force();

        // Get prior_graph
        repo.checkout_tree(
            &repo.find_object(Oid::from_str(commit_a)?, None)?,
            Some(&mut checkout_builder),
        )?;
        let prior_graph = MetadataCommand::new().current_dir(path).build_graph()?;

        // Get post_graph
        repo.checkout_tree(
            &repo.find_object(Oid::from_str(commit_b)?, None)?,
            Some(&mut checkout_builder),
        )?;
        let post_graph = MetadataCommand::new().current_dir(path).build_graph()?;

        repo.checkout_tree(starter_commit.as_object(), Some(&mut checkout_builder))?;
        UpdateAnalyzer::get_summary_report(&prior_graph, &post_graph)
    }

    /// Get update review report in markdown format
    /// for two paths, presumably checked out at two commits for a given repo
    pub fn run_update_analyzer_from_paths(path_a: &Path, path_b: &Path) -> Result<Option<String>> {
        let prior_graph = MetadataCommand::new().current_dir(path_a).build_graph()?;
        let post_graph = MetadataCommand::new().current_dir(path_b).build_graph()?;
        UpdateAnalyzer::get_summary_report(&prior_graph, &post_graph)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::diff::DiffAnalyzer;
    use once_cell::sync::Lazy;
    use serial_test::serial;
    use std::sync::Once;

    static DIFF_ANALYZER: Lazy<DiffAnalyzer> = Lazy::new(|| DiffAnalyzer::new().unwrap());

    static INIT_GIT_REPOS: Once = Once::new();
    pub fn setup_git_repos() {
        // Multiple tests work with common git repos.
        // As git2::Repositroy mutable reference is not thread safe,
        // we'd need to run those tests serially.
        // However, in this function, we clone those common repos
        // to avoid redundant set up within the tests
        INIT_GIT_REPOS.call_once(|| {
            let name = "diem";
            let url = "https://github.com/diem/diem";
            DIFF_ANALYZER.get_git_repo(name, url).unwrap();

            let name = "octocrab";
            let url = "https://github.com/XAMPPRocky/octocrab";
            DIFF_ANALYZER.get_git_repo(name, url).unwrap();
        });
    }

    #[test]
    #[serial]
    fn test_lib_update_review_report_from_repo_commits() {
        setup_git_repos();

        let name = "diem";
        let repository = "https://github.com/diem/diem";
        let repo = DIFF_ANALYZER.get_git_repo(name, repository).unwrap();
        let path = repo
            .path()
            .parent()
            .ok_or_else(|| anyhow!("repository path not found for {}", repository))
            .unwrap();
        println!(
            "{}",
            UpdateAnalyzer::run_update_analyzer_from_repo_commits(
                path,
                "20da44ad0918e6f260e9f150a60f28ec3b8665b2",
                "2b2e529d96b6fbd9b5d111ecdd21acb61e95a28f"
            )
            .unwrap()
            .unwrap()
        );
    }

    #[test]
    fn test_lib_update_review_report_from_paths() {
        let mut checkout_builder = CheckoutBuilder::new();
        checkout_builder.force();
        let da = diff::DiffAnalyzer::new().unwrap();

        let repo = da
            .get_git_repo(
                "test_a",
                "https://github.com/nasifimtiazohi/test-version-tag",
            )
            .unwrap();
        repo.checkout_tree(
            &repo
                .find_object(
                    Oid::from_str("43ffefddc15cc21725207e51f4d41d9931d197f2").unwrap(),
                    None,
                )
                .unwrap(),
            Some(&mut checkout_builder),
        )
        .unwrap();
        let path_a = repo.path().parent().unwrap();

        // Get post_graph
        let repo = da
            .get_git_repo(
                "test_b",
                "https://github.com/nasifimtiazohi/test-version-tag",
            )
            .unwrap();
        repo.checkout_tree(
            &repo
                .find_object(
                    Oid::from_str("96a541d081863875b169fc88cd8f58bbd268d377").unwrap(),
                    None,
                )
                .unwrap(),
            Some(&mut checkout_builder),
        )
        .unwrap();
        let path_b = repo.path().parent().unwrap();

        println!(
            "{}",
            UpdateAnalyzer::run_update_analyzer_from_paths(path_a, path_b)
                .unwrap()
                .unwrap()
        );
    }

    #[test]
    #[serial]
    fn test_lib_for_no_updates() {
        setup_git_repos();
        let name = "diem";
        let repository = "https://github.com/diem/diem";
        let repo = DIFF_ANALYZER.get_git_repo(name, repository).unwrap();
        let path = repo
            .path()
            .parent()
            .ok_or_else(|| anyhow!("repository path not found for {}", repository))
            .unwrap();
        assert!(UpdateAnalyzer::run_update_analyzer_from_repo_commits(
            path,
            "516b1d9cb619de459da0ba07e8fd74743d2fa9a0",
            "44f91c93c0d0b522bac22d90028698e392fada41"
        )
        .unwrap()
        .is_none());
    }
}
