//! This module abstracts various analysis of source code for a given package

use anyhow::{anyhow, Result};
use camino::Utf8Path;
use guppy::{
    graph::{DependencyDirection, PackageGraph, PackageMetadata},
    PackageId,
};
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell, collections::HashMap, collections::HashSet, fs, iter, iter::FromIterator, ops,
    path::PathBuf, process::Command,
};
use tokei::{Config, LanguageType, Languages};

#[derive(Debug, Clone)]
pub struct CodeReport {
    pub name: String,
    pub version: String,
    pub is_direct: bool,
    pub has_build_script: bool,
    pub loc_report: Option<LOCReport>,
    pub unsafe_report: Option<UnsafeReport>,
    pub dep_report: Option<DepReport>,
    pub exclusive_dep_report: Option<DepReport>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct LOCReport {
    pub total_loc: u64, // excludes comment and white lines
    pub rust_loc: u64,  // excludes comment and white lines
}

impl ops::Add<LOCReport> for LOCReport {
    type Output = LOCReport;

    fn add(self, rhs: LOCReport) -> LOCReport {
        LOCReport {
            total_loc: self.total_loc + rhs.total_loc,
            rust_loc: self.rust_loc + rhs.rust_loc,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct DepReport {
    pub total_deps: u64,
    pub deps_total_loc_report: LOCReport,
    pub deps_with_build_script: u64,
    pub deps_analyzed_for_unsafe: u64,
    pub deps_forbidding_unsafe: u64,
    pub deps_using_unsafe: u64,
    pub deps_total_used_unsafe_details: UnsafeDetails,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct UnsafeReport {
    // Unsafe code used by the cargo geiger
    pub forbids_unsafe: bool,
    pub used_unsafe_count: UnsafeDetails,
    pub unused_unsafe_count: UnsafeDetails,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct UnsafeDetails {
    pub functions: u64,
    pub expressions: u64,
    pub impls: u64,
    pub traits: u64,
    pub methods: u64,
}

impl ops::Add<UnsafeDetails> for UnsafeDetails {
    type Output = UnsafeDetails;

    fn add(self, rhs: UnsafeDetails) -> UnsafeDetails {
        UnsafeDetails {
            functions: self.functions + rhs.functions,
            expressions: self.expressions + rhs.expressions,
            impls: self.impls + rhs.expressions,
            traits: self.traits + rhs.traits,
            methods: self.methods + rhs.methods,
        }
    }
}

pub struct CodeAnalyzer {
    loc_cache: RefCell<HashMap<String, LOCReport>>,
    geiger_cache: RefCell<HashMap<(String, String), GeigerPackageInfo>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GeigerReport {
    pub packages: Vec<GeigerPackageInfo>,
    pub used_but_not_scanned_files: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GeigerPackageInfo {
    pub package: GeigerPackage,
    pub unsafety: Unsafety,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GeigerPackage {
    pub id: GeigerPackageId,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GeigerPackageId {
    pub name: String,
    pub version: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Unsafety {
    pub used: UnsafeInfo,
    pub unused: UnsafeInfo,
    pub forbids_unsafe: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UnsafeInfo {
    pub functions: UnsafeCount,
    pub exprs: UnsafeCount,
    pub item_impls: UnsafeCount,
    pub item_traits: UnsafeCount,
    pub methods: UnsafeCount,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UnsafeCount {
    pub safe: u64,
    pub unsafe_: u64,
}

impl CodeAnalyzer {
    pub fn new() -> Self {
        Self {
            loc_cache: RefCell::new(HashMap::new()),
            geiger_cache: RefCell::new(HashMap::new()),
        }
    }

    pub fn analyze_code(self, graph: &PackageGraph) -> Result<Vec<CodeReport>> {
        let mut code_reports: Vec<CodeReport> = Vec::new();

        // Get path to all packages in the workspace
        let package_paths: Vec<&str> = graph
            .workspace()
            .iter()
            .map(|pkg| pkg.manifest_path().as_str())
            .collect();
        // Run Geiger report for each member packages and store result in cache
        // TODO: How to avoid multiple calls for Cargo Geiger and run only once?
        self.get_cargo_geiger_report_for_workspace(package_paths)?;

        // Get direct dependencies of the whole workspace
        let direct_dependencies: Vec<PackageMetadata> = graph
            .query_workspace()
            .resolve_with_fn(|_, link| {
                let (from, to) = link.endpoints();
                from.in_workspace() && !to.in_workspace()
            })
            .packages(guppy::graph::DependencyDirection::Forward)
            .filter(|pkg| !pkg.in_workspace())
            .collect();

        for package in &direct_dependencies {
            let loc_report = self.get_loc_report(package.manifest_path())?;
            let unsafe_report =
                self.get_unsafe_report(package.name().to_string(), package.version().to_string());

            //All dependencies of this package
            let dependencies = self.get_package_dependencies(&graph, &package)?;
            let dep_report = self.get_dep_report(&dependencies)?;

            //Exclusive deps from this package
            let exclusive_dependencies = self.filter_exclusive_deps(package, &dependencies);
            let exclusive_dep_report = self.get_dep_report(&exclusive_dependencies)?;

            let code_report = CodeReport {
                name: package.name().to_string(),
                version: package.version().to_string(),
                is_direct: true,
                has_build_script: package.has_build_script(),
                loc_report: Some(loc_report),
                unsafe_report: unsafe_report,
                dep_report: Some(dep_report),
                exclusive_dep_report: Some(exclusive_dep_report),
            };

            code_reports.push(code_report);
        }

        Ok(code_reports)
    }

    pub fn get_package_dependencies<'a>(
        &self,
        graph: &'a PackageGraph,
        package: &PackageMetadata,
    ) -> Result<Vec<PackageMetadata<'a>>> {
        let dependencies: Vec<PackageMetadata> = graph
            .query_forward(iter::once(package.id()))?
            .resolve()
            .packages(DependencyDirection::Forward)
            .filter(|pkg| pkg.id() != package.id())
            .collect();
        Ok(dependencies)
    }

    pub fn filter_exclusive_deps<'a>(
        &self,
        package: &'a PackageMetadata,
        pacakge_dependencies: &Vec<PackageMetadata<'a>>,
    ) -> Vec<PackageMetadata<'a>> {
        // HashSet for quick lookup in dependency subtree
        let mut package_deps: HashSet<&PackageId> =
            HashSet::from_iter(pacakge_dependencies.iter().map(|dep| dep.id()));
        // Add root to the tree
        package_deps.insert(package.id());

        // Keep track of non-exclusive deps
        let mut common_deps: HashSet<&PackageId> = HashSet::new();
        // and exclusive ones for
        let mut exclusive_deps: HashMap<&PackageId, PackageMetadata> = HashMap::new();

        for dep in pacakge_dependencies {
            let mut unique = true;
            for link in dep.reverse_direct_links() {
                let from_id = link.from().id();
                if !package_deps.contains(from_id) || common_deps.contains(from_id) {
                    unique = false;
                    common_deps.insert(dep.id());
                    break;
                }
            }
            if unique {
                exclusive_deps.insert(dep.id(), *dep);
            }
        }

        exclusive_deps.values().cloned().collect()
    }

    fn get_dep_report(&self, dependencies: &Vec<PackageMetadata>) -> Result<DepReport> {
        let total_deps = dependencies.len() as u64;
        let mut deps_total_loc_report = LOCReport {
            ..Default::default()
        };
        let mut deps_analyzed_for_unsafe = 0;
        let mut deps_forbidding_unsafe = 0;
        let mut deps_using_unsafe = 0;
        let mut deps_with_build_script = 0;
        let mut deps_total_used_unsafe_details = UnsafeDetails {
            ..Default::default()
        };

        for package in dependencies {
            let loc_report = self.get_loc_report(package.manifest_path())?;
            deps_total_loc_report = deps_total_loc_report + loc_report;

            let unsafe_report =
                self.get_unsafe_report(package.name().to_string(), package.version().to_string());
            if !unsafe_report.is_none() {
                deps_analyzed_for_unsafe += 1;
                let unsafe_report = unsafe_report.unwrap();
                if unsafe_report.forbids_unsafe {
                    deps_forbidding_unsafe += 1;
                } else {
                    if unsafe_report.used_unsafe_count.expressions > 0 {
                        deps_using_unsafe += 1;
                    }
                }
                deps_total_used_unsafe_details =
                    deps_total_used_unsafe_details + unsafe_report.used_unsafe_count;
            }

            if package.has_build_script() {
                deps_with_build_script += 1;
            }
        }

        Ok(DepReport {
            total_deps,
            deps_total_loc_report,
            deps_with_build_script,
            deps_analyzed_for_unsafe,
            deps_forbidding_unsafe,
            deps_using_unsafe,
            deps_total_used_unsafe_details,
        })
    }

    fn get_loc_report(&self, manifest_path: &Utf8Path) -> Result<LOCReport> {
        let manifest_path = manifest_path.parent().ok_or_else(|| {
            anyhow!(
                "Cannot find parent directory of Cargo.toml for {}",
                manifest_path
            )
        })?;

        if self.loc_cache.borrow().contains_key(manifest_path.as_str()) {
            let code_report = self
                .loc_cache
                .borrow()
                .get(manifest_path.as_str())
                .ok_or_else(|| anyhow!("Caching error"))?
                .clone();
            return Ok(code_report);
        }

        let paths = &[manifest_path];
        let excluded = &["target"];
        let config = Config {
            hidden: Some(true),
            treat_doc_strings_as_comments: Some(true),
            ..Default::default()
        };

        let mut languages = Languages::new();
        languages.get_statistics(paths, excluded, &config);

        let loc_report = LOCReport {
            total_loc: languages.total().code as u64,
            rust_loc: languages
                .get(&LanguageType::Rust)
                .map(|lang| lang.code as u64)
                .unwrap_or(0),
        };

        // put in loc_cache
        self.loc_cache
            .borrow_mut()
            .insert(manifest_path.to_string(), loc_report);

        let code_report = self
            .loc_cache
            .borrow()
            .get(manifest_path.as_str())
            .ok_or_else(|| anyhow!("Caching error"))?
            .clone();
        Ok(code_report)
    }

    fn get_cargo_geiger_report_for_workspace(&self, package_paths: Vec<&str>) -> Result<()> {
        // Cargo geiger only works with package tomls
        // and not a virtual manifest file
        // Therefore, we run cargo geiger on all member packages
        // TODO: Revisit this design
        let package_paths: Vec<PathBuf> = package_paths
            .iter()
            .map(|path| PathBuf::from(path))
            .collect();

        for path in &package_paths {
            let geiger_report = Self::get_cargo_geiger_report(path)?;
            let geiger_packages = geiger_report.packages;
            for geiger_package in &geiger_packages {
                let package = &geiger_package.package.id;
                let key = (package.name.clone(), package.version.clone());
                if self.get_cargo_geiger_report_from_cache(&key).is_none() {
                    // TODO: can the used unsafe code change for separate builds?
                    self.geiger_cache
                        .borrow_mut()
                        .insert(key, geiger_package.clone());
                }
            }
        }

        Ok(())
    }

    fn get_cargo_geiger_report(absolute_path: &PathBuf) -> Result<GeigerReport> {
        let absolute_path = fs::canonicalize(absolute_path)?;
        let absolute_path = absolute_path
            .to_str()
            .ok_or_else(|| anyhow!("error in parsing absolute path for Cargo.toml"))?;

        let output = Command::new("cargo")
            .args(&[
                "geiger",
                "--output-format",
                "Json",
                "--manifest-path",
                absolute_path, // only accepts absolute path
            ])
            .output()?;

        if !output.status.success() {
            return Err(anyhow!("Error in running cargo geiger"));
        }

        let geiger_report: GeigerReport = serde_json::from_slice(&output.stdout)?;
        Ok(geiger_report)
    }

    fn get_unsafe_report(&self, name: String, version: String) -> Option<UnsafeReport> {
        let key = (name, version);
        let geiger_package_info = self.get_cargo_geiger_report_from_cache(&key)?;

        Some(UnsafeReport {
            forbids_unsafe: geiger_package_info.unsafety.forbids_unsafe,
            used_unsafe_count: UnsafeDetails {
                functions: geiger_package_info.unsafety.used.functions.unsafe_,
                expressions: geiger_package_info.unsafety.used.exprs.unsafe_,
                impls: geiger_package_info.unsafety.used.item_impls.unsafe_,
                traits: geiger_package_info.unsafety.used.item_traits.unsafe_,
                methods: geiger_package_info.unsafety.used.methods.unsafe_,
            },
            unused_unsafe_count: UnsafeDetails {
                functions: geiger_package_info.unsafety.unused.functions.unsafe_,
                expressions: geiger_package_info.unsafety.unused.exprs.unsafe_,
                impls: geiger_package_info.unsafety.unused.item_impls.unsafe_,
                traits: geiger_package_info.unsafety.unused.item_traits.unsafe_,
                methods: geiger_package_info.unsafety.unused.methods.unsafe_,
            },
        })
    }

    fn get_cargo_geiger_report_from_cache(
        &self,
        key: &(String, String),
    ) -> Option<GeigerPackageInfo> {
        // Cargo geiger may not have a result for a valid dependency
        // e.g., openssl not present for geiger report for valid_dep test crate
        self.geiger_cache.borrow().get(&key).cloned()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use guppy::{graph::PackageGraph, MetadataCommand};
    use serial_test::serial;
    use std::path::PathBuf;

    fn get_test_graph() -> PackageGraph {
        MetadataCommand::new()
            .current_dir(PathBuf::from("resources/test/valid_dep"))
            .build_graph()
            .unwrap()
    }

    fn get_test_code_analyzer() -> CodeAnalyzer {
        CodeAnalyzer::new()
    }

    #[test]
    fn test_loc_report_for_valid_package() {
        let graph = get_test_graph();
        let code_analyzer = get_test_code_analyzer();
        let pkg = graph.packages().find(|p| p.name() == "libc").unwrap();
        let report = code_analyzer.get_loc_report(pkg.manifest_path()).unwrap();

        assert!(report.total_loc > 0);
        assert!(report.rust_loc > 0);
    }

    #[test]
    fn test_loc_report_for_non_rust_directory() {
        let non_rust_path = Utf8Path::new("resources/test/non_rust/norust.md");
        let code_analyzer = get_test_code_analyzer();
        let report = code_analyzer.get_loc_report(non_rust_path).unwrap();

        assert_eq!(report.rust_loc, 0);
    }

    #[test]
    fn test_code_dep_report_for_valid_report() {
        let graph = get_test_graph();
        let code_analyzer = get_test_code_analyzer();
        let package = graph.packages().find(|p| p.name() == "octocrab").unwrap();
        let dependencies = code_analyzer
            .get_package_dependencies(&graph, &package)
            .unwrap();
        let report = code_analyzer.get_dep_report(&dependencies).unwrap();

        println!("{:?}", report);
        assert!(report.total_deps > 0);
        assert!(report.deps_total_loc_report.total_loc > 0);
        assert!(report.deps_total_loc_report.rust_loc > 0);
        // No unsafe report at this test, as geiger cache would remain empty here
    }

    #[test]
    #[serial]
    fn test_code_analyzer() {
        let code_analyzer = get_test_code_analyzer();
        let graph = get_test_graph();
        let code_reports = code_analyzer.analyze_code(&graph).unwrap();
        println!("{:?}", code_reports);

        assert!(code_reports.len() > 0);
        let report = &code_reports[0];
        assert_eq!(report.unsafe_report.is_none(), false);
    }

    #[test]
    #[serial]
    #[ignore]
    fn test_code_cargo_geiger() {
        let path = PathBuf::from("resources/test/valid_dep/Cargo.toml");
        let geiger_report = CodeAnalyzer::get_cargo_geiger_report(&path).unwrap();
        println!("{:?}", geiger_report);
        assert!(geiger_report.packages.len() > 0);
    }

    #[test]
    #[serial]
    #[ignore]
    fn test_code_geiger_report_for_workspace() {
        let code_analyzer = get_test_code_analyzer();
        let graph = MetadataCommand::new()
            .current_dir(PathBuf::from(".."))
            .build_graph()
            .unwrap();

        // Get path to all packages in the workspace
        let package_paths: Vec<&str> = graph
            .workspace()
            .iter()
            .map(|pkg| pkg.manifest_path().as_str())
            .collect();

        code_analyzer
            .get_cargo_geiger_report_for_workspace(package_paths)
            .unwrap();
        println!(
            "Total keys in geiger cache: {}",
            code_analyzer.geiger_cache.borrow().len()
        );
        assert!(code_analyzer.geiger_cache.borrow().len() > 0);
    }

    #[test]
    #[ignore]
    fn test_code_operator_overloading_for() {
        let unsafe_details = UnsafeDetails {
            functions: 1,
            expressions: 1,
            impls: 1,
            traits: 1,
            methods: 1,
        };

        let sum = unsafe_details.clone() + unsafe_details.clone();
        assert_eq!(
            sum.functions,
            unsafe_details.functions + unsafe_details.functions
        );
    }

    #[test]
    fn test_code_exclusive_deps() {
        let graph = get_test_graph();
        let code_analyzer = get_test_code_analyzer();
        let package = graph.packages().find(|p| p.name() == "gitlab").unwrap();
        let dependencies = code_analyzer
            .get_package_dependencies(&graph, &package)
            .unwrap();
        let exclusive_deps = code_analyzer.filter_exclusive_deps(&package, &dependencies);
        // Checked by hand that it returns the right count :)
        println!("{}", exclusive_deps.len());
    }
}
