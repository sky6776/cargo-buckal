use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet as Set, HashMap},
    io::{BufWriter, Write},
    path::PathBuf,
    vec,
};

use crate::{buck::Alias, buckal_error};
use cargo_metadata::{
    DepKindInfo, DependencyKind, Node, Package, PackageId, Target, camino::Utf8PathBuf,
};
use itertools::Itertools;
use regex::Regex;

use crate::{
    RUST_CRATES_ROOT,
    buck::{
        BuildscriptRun, CargoManifest, CargoTargetKind, FileGroup, Glob, HttpArchive, Load, Rule,
        RustBinary, RustLibrary, RustRule, RustTest, parse_buck_file, patch_buck_rules,
    },
    buckal_log, buckal_warn,
    cache::{BuckalChange, ChangeType},
    context::BuckalContext,
    platform::lookup_platforms,
    utils::{
        UnwrapOrExit, get_buck2_root, get_cfgs, get_target, get_vendor_dir,
        rewrite_target_if_needed,
    },
};

pub fn buckify_dep_node(node: &Node, ctx: &BuckalContext) -> Vec<Rule> {
    let package = ctx.packages_map.get(&node.id).unwrap().to_owned();

    // emit buck rules for lib target
    let mut buck_rules: Vec<Rule> = Vec::new();

    let manifest_dir = package.manifest_path.parent().unwrap().to_owned();
    let lib_target = package
        .targets
        .iter()
        .find(|t| {
            t.kind.contains(&cargo_metadata::TargetKind::Lib)
                || t.kind.contains(&cargo_metadata::TargetKind::CDyLib)
                || t.kind.contains(&cargo_metadata::TargetKind::DyLib)
                || t.kind.contains(&cargo_metadata::TargetKind::RLib)
                || t.kind.contains(&cargo_metadata::TargetKind::StaticLib)
                || t.kind.contains(&cargo_metadata::TargetKind::ProcMacro)
        })
        .expect("No library target found");

    let http_archive = emit_http_archive(&package, ctx);
    buck_rules.push(Rule::HttpArchive(http_archive));

    let cargo_manifest = emit_cargo_manifest(&package);
    buck_rules.push(Rule::CargoManifest(cargo_manifest));

    let rust_library = emit_rust_library(
        &package,
        node,
        &ctx.packages_map,
        lib_target,
        &manifest_dir,
        &package.name,
        ctx,
    );

    buck_rules.push(Rule::RustLibrary(rust_library));

    // Check if the package has a build script
    let custom_build_target = package
        .targets
        .iter()
        .find(|t| t.kind.contains(&cargo_metadata::TargetKind::CustomBuild));

    if let Some(build_target) = custom_build_target {
        // Patch the rust_library rule to support build scripts
        for rule in &mut buck_rules {
            if let Some(rust_rule) = rule.as_rust_rule_mut() {
                patch_with_buildscript(rust_rule, build_target, &package);
            }
        }

        // create the build script rule
        let buildscript_build = emit_buildscript_build(
            build_target,
            &package,
            node,
            &ctx.packages_map,
            &manifest_dir,
            ctx,
        );
        buck_rules.push(Rule::RustBinary(buildscript_build));

        // create the build script run rule
        let buildscript_run =
            emit_buildscript_run(&package, node, &ctx.packages_map, build_target, ctx);
        buck_rules.push(Rule::BuildscriptRun(buildscript_run));
    }

    buck_rules
}

pub fn buckify_root_node(node: &Node, ctx: &BuckalContext) -> Vec<Rule> {
    let package = ctx.packages_map.get(&node.id).unwrap().to_owned();

    let bin_targets = package
        .targets
        .iter()
        .filter(|t| t.kind.contains(&cargo_metadata::TargetKind::Bin))
        .collect::<Vec<_>>();

    let lib_targets = get_lib_targets(&package);

    let test_targets = package
        .targets
        .iter()
        .filter(|t| t.kind.contains(&cargo_metadata::TargetKind::Test))
        .collect::<Vec<_>>();

    let mut buck_rules: Vec<Rule> = Vec::new();

    let manifest_dir = package.manifest_path.parent().unwrap().to_owned();

    // emit filegroup rule for vendor
    let filegroup = emit_filegroup(&package);
    buck_rules.push(Rule::FileGroup(filegroup));

    let cargo_manifest = emit_cargo_manifest(&package);
    buck_rules.push(Rule::CargoManifest(cargo_manifest));

    // emit buck rules for bin targets
    for bin_target in &bin_targets {
        let buckal_name = bin_target.name.to_owned();

        let mut rust_binary = emit_rust_binary(
            &package,
            node,
            &ctx.packages_map,
            bin_target,
            &manifest_dir,
            &buckal_name,
            ctx,
        );

        if lib_targets.iter().any(|l| l.name == bin_target.name) {
            // Cargo allows `main.rs` to use items from `lib.rs` via the crate's own name by default.
            rust_binary
                .deps_mut()
                .insert(format!(":lib{}", bin_target.name));
        }

        buck_rules.push(Rule::RustBinary(rust_binary));
    }

    // emit buck rules for lib targets
    for lib_target in &lib_targets {
        let buckal_name = if bin_targets.iter().any(|b| b.name == lib_target.name) {
            format!("lib{}", lib_target.name)
        } else {
            lib_target.name.to_owned()
        };

        let rust_library = emit_rust_library(
            &package,
            node,
            &ctx.packages_map,
            lib_target,
            &manifest_dir,
            &buckal_name,
            ctx,
        );

        buck_rules.push(Rule::RustLibrary(rust_library));

        if !ctx.repo_config.ignore_tests && lib_target.test {
            // If the library target has inline tests, emit a rust_test rule for it
            let buckal_name = format!("{}-unittest", lib_target.name);

            let rust_test = emit_rust_test(
                &package,
                node,
                &ctx.packages_map,
                lib_target,
                &manifest_dir,
                &buckal_name,
                ctx,
            );

            buck_rules.push(Rule::RustTest(rust_test));
        }
    }

    // emit buck rules for integration test
    if !ctx.repo_config.ignore_tests {
        for test_target in &test_targets {
            let buckal_name = test_target.name.to_owned();

            let mut rust_test = emit_rust_test(
                &package,
                node,
                &ctx.packages_map,
                test_target,
                &manifest_dir,
                &buckal_name,
                ctx,
            );

            let package_name = package.name.replace("-", "_");
            let mut lib_alias = false;
            if bin_targets.iter().any(|b| b.name == package_name) {
                lib_alias = true;
                rust_test.env_mut().insert(
                    format!("CARGO_BIN_EXE_{}", package_name),
                    format!("$(location :{})", package_name),
                );
            }
            if lib_targets.iter().any(|l| l.name == package_name) {
                if lib_alias {
                    rust_test.deps_mut().insert(format!(":lib{}", package_name));
                } else {
                    rust_test.deps_mut().insert(format!(":{}", package_name));
                }
            }

            buck_rules.push(Rule::RustTest(rust_test));
        }
    }

    // Check if the package has a build script
    let custom_build_target = package
        .targets
        .iter()
        .find(|t| t.kind.contains(&cargo_metadata::TargetKind::CustomBuild));

    if let Some(build_target) = custom_build_target {
        // Patch the rust_library and rust_binary rules to support build scripts
        for rule in &mut buck_rules {
            if let Some(rust_rule) = rule.as_rust_rule_mut() {
                patch_with_buildscript(rust_rule, build_target, &package);
            }
        }

        // create the build script rule
        let buildscript_build = emit_buildscript_build(
            build_target,
            &package,
            node,
            &ctx.packages_map,
            &manifest_dir,
            ctx,
        );
        buck_rules.push(Rule::RustBinary(buildscript_build));

        // create the build script run rule
        let buildscript_run =
            emit_buildscript_run(&package, node, &ctx.packages_map, build_target, ctx);
        buck_rules.push(Rule::BuildscriptRun(buildscript_run));
    }

    buck_rules
}

pub fn vendor_package(package: &Package) -> Utf8PathBuf {
    // Vendor the package sources to `third-party/rust/crates/<package_name>/<version>`
    let vendor_dir = get_vendor_dir(&package.name, &package.version.to_string())
        .unwrap_or_exit_ctx("failed to get vendor directory");
    if !vendor_dir.exists() {
        std::fs::create_dir_all(&vendor_dir).expect("Failed to create target directory");
    }

    vendor_dir
}

pub fn gen_buck_content(rules: &[Rule]) -> String {
    let loads: Vec<Rule> = vec![
        Rule::Load(Load {
            bzl: "@buckal//:cargo_manifest.bzl".to_owned(),
            items: Set::from(["cargo_manifest".to_owned()]),
        }),
        Rule::Load(Load {
            bzl: "@buckal//:wrapper.bzl".to_owned(),
            items: Set::from([
                "buildscript_run".to_owned(),
                "rust_binary".to_owned(),
                "rust_library".to_owned(),
            ]),
        }),
    ];

    let loads_string = loads
        .iter()
        .map(serde_starlark::to_string)
        .map(Result::unwrap)
        .join("");

    let mut content = rules
        .iter()
        .map(serde_starlark::to_string)
        .map(Result::unwrap)
        .join("\n");

    content.insert(0, '\n');
    content.insert_str(0, &loads_string);
    content.insert_str(0, "# @generated by `cargo buckal`\n\n");
    content
}

pub fn check_dep_target(dk: &DepKindInfo) -> bool {
    if dk.target.is_none() {
        return true; // No target specified
    }

    let platform = dk.target.as_ref().unwrap();
    let target = get_target();
    let cfgs = get_cfgs();

    platform.matches(&target, &cfgs[..])
}

fn get_lib_targets(package: &Package) -> Vec<&Target> {
    package
        .targets
        .iter()
        .filter(|t| {
            t.kind.contains(&cargo_metadata::TargetKind::Lib)
                || t.kind.contains(&cargo_metadata::TargetKind::CDyLib)
                || t.kind.contains(&cargo_metadata::TargetKind::DyLib)
                || t.kind.contains(&cargo_metadata::TargetKind::RLib)
                || t.kind.contains(&cargo_metadata::TargetKind::StaticLib)
                || t.kind.contains(&cargo_metadata::TargetKind::ProcMacro)
        })
        .collect()
}

fn set_deps(
    rust_rule: &mut dyn RustRule,
    node: &Node,
    packages_map: &HashMap<PackageId, Package>,
    kind: CargoTargetKind,
    ctx: &BuckalContext,
) {
    for dep in &node.deps {
        if let Some(dep_package) = packages_map.get(&dep.pkg) {
            let dep_package_name = dep_package.name.to_string();
            if dep.dep_kinds.iter().any(|dk| {
                (kind != CargoTargetKind::CustomBuild && dk.kind == DependencyKind::Normal
                    || kind == CargoTargetKind::CustomBuild && dk.kind == DependencyKind::Build
                    || kind == CargoTargetKind::Test && dk.kind == DependencyKind::Development)
                    && check_dep_target(dk)
            }) {
                // Normal dependencies and build dependencies for `build.rs` on current arch
                if dep_package.source.is_none() {
                    // first-party dependency
                    let buck2_root =
                        get_buck2_root().unwrap_or_exit_ctx("failed to get buck2 root");
                    let manifest_path = PathBuf::from(&dep_package.manifest_path);
                    let manifest_dir = manifest_path.parent().unwrap();
                    let relative_path = manifest_dir
                        .strip_prefix(&buck2_root)
                        .unwrap_or_exit_ctx(
                            "Current directory is not inside the Buck2 project root",
                        )
                        .to_string_lossy();

                    let dep_bin_targets = dep_package
                        .targets
                        .iter()
                        .filter(|t| t.kind.contains(&cargo_metadata::TargetKind::Bin))
                        .collect::<Vec<_>>();

                    let dep_lib_targets = get_lib_targets(dep_package);

                    if dep_lib_targets.len() != 1 {
                        buckal_error!(
                            "Expected exactly one library target for dependency {}, but found {}",
                            dep_package.name,
                            dep_lib_targets.len()
                        );
                        std::process::exit(1);
                    }

                    let buckal_name = if dep_bin_targets
                        .iter()
                        .any(|b| b.name == dep_lib_targets[0].name)
                    {
                        format!("lib{}", dep_lib_targets[0].name)
                    } else {
                        dep_lib_targets[0].name.to_owned()
                    };

                    let target_label = format!("{relative_path}:{buckal_name}");

                    let rewritten_target = rewrite_target_if_needed(
                        &target_label,
                        buck2_root.as_std_path(),
                        ctx.repo_config.align_cells,
                    )
                    .unwrap_or_else(|e| {
                        buckal_warn!("Failed to rewrite target label '{}': {}", target_label, e);
                        target_label
                    });

                    if dep.name != dep_package_name.replace("-", "_") {
                        // renamed dependency
                        rust_rule
                            .named_deps_mut()
                            .insert(dep.name.clone(), rewritten_target);
                    } else {
                        rust_rule.deps_mut().insert(rewritten_target);
                    }
                } else {
                    // third-party dependency

                    let use_alias =
                        ctx.repo_config.inherit_workspace_deps && node.id == ctx.root.id;

                    let dep_target = if use_alias {
                        // only workspace root direct deps use alias
                        format!("//third-party/rust:{}", dep_package.name)
                    } else {
                        // default: concrete crate target
                        format!(
                            "//{RUST_CRATES_ROOT}/{}/{}:{}",
                            dep_package.name, dep_package.version, dep_package.name
                        )
                    };

                    let rewritten_target = rewrite_target_if_needed(
                        &dep_target,
                        get_buck2_root()
                            .unwrap_or_exit_ctx("failed to get buck2 root")
                            .as_std_path(),
                        ctx.repo_config.align_cells,
                    )
                    .unwrap_or_else(|e| {
                        buckal_warn!("Failed to rewrite target label '{}': {}", dep_target, e);
                        dep_target.clone()
                    });

                    if dep.name != dep_package_name.replace("-", "_") {
                        rust_rule
                            .named_deps_mut()
                            .insert(dep.name.clone(), rewritten_target);
                    } else {
                        rust_rule.deps_mut().insert(rewritten_target);
                    }
                }
            }
        }
    }
}

/// Emit `rust_library` rule for the given lib target
fn emit_rust_library(
    package: &Package,
    node: &Node,
    packages_map: &HashMap<PackageId, Package>,
    lib_target: &Target,
    manifest_dir: &Utf8PathBuf,
    buckal_name: &str,
    ctx: &BuckalContext,
) -> RustLibrary {
    let mut rust_library = RustLibrary {
        name: buckal_name.to_owned(),
        srcs: Set::from([get_vendor_target(package)]),
        crate_name: lib_target.name.to_owned().replace("-", "_"),
        edition: package.edition.to_string(),
        features: Set::from_iter(node.features.iter().map(|f| f.to_string())),
        rustc_flags: Set::from([format!(
            "@$(location :{}-manifest[env_flags])",
            package.name
        )]),
        visibility: Set::from(["PUBLIC".to_owned()]),
        ..Default::default()
    };

    if lib_target
        .kind
        .contains(&cargo_metadata::TargetKind::ProcMacro)
    {
        rust_library.proc_macro = Some(true);
    }

    // Set the crate root path
    rust_library.crate_root = format!(
        "vendor/{}",
        lib_target
            .src_path
            .to_owned()
            .strip_prefix(manifest_dir)
            .expect("Failed to get library source path")
    );

    // look up platform compatibility
    if let Some(platform) = lookup_platforms(&package.name) {
        rust_library.compatible_with = platform.to_buck();
    }

    // Set dependencies
    set_deps(
        &mut rust_library,
        node,
        packages_map,
        CargoTargetKind::Lib,
        ctx,
    );
    rust_library
}

/// Emit `rust_binary` rule for the given bin target
fn emit_rust_binary(
    package: &Package,
    node: &Node,
    packages_map: &HashMap<PackageId, Package>,
    bin_target: &Target,
    manifest_dir: &Utf8PathBuf,
    buckal_name: &str,
    ctx: &BuckalContext,
) -> RustBinary {
    let mut rust_binary = RustBinary {
        name: buckal_name.to_owned(),
        srcs: Set::from([get_vendor_target(package)]),
        crate_name: bin_target.name.to_owned().replace("-", "_"),
        edition: package.edition.to_string(),
        features: Set::from_iter(node.features.iter().map(|f| f.to_string())),
        rustc_flags: Set::from([format!(
            "@$(location :{}-manifest[env_flags])",
            package.name
        )]),
        visibility: Set::from(["PUBLIC".to_owned()]),
        ..Default::default()
    };

    // Set the crate root path
    rust_binary.crate_root = format!(
        "vendor/{}",
        bin_target
            .src_path
            .to_owned()
            .strip_prefix(manifest_dir)
            .expect("Failed to get binary source path")
    );

    // Set dependencies
    set_deps(
        &mut rust_binary,
        node,
        packages_map,
        CargoTargetKind::Bin,
        ctx,
    );
    rust_binary
}

/// Emit `rust_test` rule for the given bin target
fn emit_rust_test(
    package: &Package,
    node: &Node,
    packages_map: &HashMap<PackageId, Package>,
    test_target: &Target,
    manifest_dir: &Utf8PathBuf,
    buckal_name: &str,
    ctx: &BuckalContext,
) -> RustTest {
    let mut rust_test = RustTest {
        name: buckal_name.to_owned(),
        srcs: Set::from([get_vendor_target(package)]),
        crate_name: test_target.name.to_owned().replace("-", "_"),
        edition: package.edition.to_string(),
        features: Set::from_iter(node.features.iter().map(|f| f.to_string())),
        rustc_flags: Set::from([format!(
            "@$(location :{}-manifest[env_flags])",
            package.name
        )]),
        visibility: Set::from(["PUBLIC".to_owned()]),
        ..Default::default()
    };

    // Set the crate root path
    rust_test.crate_root = format!(
        "vendor/{}",
        test_target
            .src_path
            .to_owned()
            .strip_prefix(manifest_dir)
            .expect("Failed to get binary source path")
    );

    // Set dependencies
    set_deps(
        &mut rust_test,
        node,
        packages_map,
        CargoTargetKind::Test,
        ctx,
    );
    rust_test
}

/// Emit `buildscript_build` rule for the given build target
fn emit_buildscript_build(
    build_target: &Target,
    package: &Package,
    node: &Node,
    packages_map: &HashMap<PackageId, Package>,
    manifest_dir: &Utf8PathBuf,
    ctx: &BuckalContext,
) -> RustBinary {
    // create the build script rule
    let mut buildscript_build = RustBinary {
        name: format!("{}-{}", package.name, build_target.name),
        srcs: Set::from([get_vendor_target(package)]),
        crate_name: build_target.name.to_owned().replace("-", "_"),
        edition: package.edition.to_string(),
        features: Set::from_iter(node.features.iter().map(|f| f.to_string())),
        rustc_flags: Set::from([format!(
            "@$(location :{}-manifest[env_flags])",
            package.name
        )]),
        ..Default::default()
    };

    // Set the crate root path for the build script
    buildscript_build.crate_root = format!(
        "vendor/{}",
        build_target
            .src_path
            .to_owned()
            .strip_prefix(manifest_dir)
            .expect("Failed to get library source path")
    );

    // Set dependencies for the build script
    set_deps(
        &mut buildscript_build,
        node,
        packages_map,
        CargoTargetKind::CustomBuild,
        ctx,
    );

    buildscript_build
}

/// Emit `buildscript_run` rule for the given build target
fn emit_buildscript_run(
    package: &Package,
    node: &Node,
    packages_map: &HashMap<PackageId, Package>,
    build_target: &Target,
    ctx: &BuckalContext,
) -> BuildscriptRun {
    // create the build script run rule
    let build_name = get_build_name(&build_target.name);
    let mut buildscript_run = BuildscriptRun {
        name: format!("{}-{}-run", package.name, build_name),
        package_name: package.name.to_string(),
        buildscript_rule: format!(":{}-{}", package.name, build_target.name),
        env_srcs: Set::from([format!(":{}-manifest[env_dict]", package.name)]),
        features: Set::from_iter(node.features.iter().map(|f| f.to_string())),
        version: package.version.to_string(),
        manifest_dir: format!(":{}-vendor", package.name),
        visibility: Set::from(["PUBLIC".to_owned()]),
        ..Default::default()
    };

    // Set environment variables from dependencies
    // See https://doc.rust-lang.org/cargo/reference/build-scripts.html#the-links-manifest-key
    for dep in &node.deps {
        if let Some(dep_package) = packages_map.get(&dep.pkg)
            && dep_package.links.is_some()
            && dep
                .dep_kinds
                .iter()
                .any(|dk| dk.kind == DependencyKind::Normal && check_dep_target(dk))
        {
            // Only normal dependencies with The links Manifest Key for current arch are considered
            let custom_build_target_dep = dep_package
                .targets
                .iter()
                .find(|t| t.kind.contains(&cargo_metadata::TargetKind::CustomBuild));
            if let Some(build_target_dep) = custom_build_target_dep {
                let build_name_dep = get_build_name(&build_target_dep.name);
                let target_label = format!(
                    "//{RUST_CRATES_ROOT}/{}/{}:{}-{build_name_dep}-run[metadata]",
                    dep_package.name, dep_package.version, dep_package.name
                );

                let rewritten_target = rewrite_target_if_needed(
                    &target_label,
                    get_buck2_root()
                        .unwrap_or_exit_ctx("failed to get buck2 root")
                        .as_std_path(),
                    ctx.repo_config.align_cells,
                )
                .unwrap_or_else(|e| {
                    buckal_warn!("Failed to rewrite target label '{}': {}", target_label, e);
                    target_label.clone()
                });
                buildscript_run.env_srcs.insert(rewritten_target);
            } else {
                panic!(
                    "Dependency {} has links key but no build script target",
                    dep_package.name
                );
            }
        }
    }

    buildscript_run
}

/// Patch the given `rust_library` or `rust_binary` rule to support build scripts
fn patch_with_buildscript(rust_rule: &mut dyn RustRule, build_target: &Target, package: &Package) {
    let build_name = get_build_name(&build_target.name);
    rust_rule.env_mut().insert(
        "OUT_DIR".to_owned(),
        format!("$(location :{}-{build_name}-run[out_dir])", package.name).to_owned(),
    );
    rust_rule.rustc_flags_mut().insert(
        format!(
            "@$(location :{}-{build_name}-run[rustc_flags])",
            package.name
        )
        .to_owned(),
    );
}

/// Emit `http_archive` rule for the given package
fn emit_http_archive(package: &Package, ctx: &BuckalContext) -> HttpArchive {
    let vendor_name = format!("{}-vendor", package.name);
    let url = format!(
        "https://static.crates.io/crates/{}/{}-{}.crate",
        package.name, package.name, package.version
    );
    let buckal_name = format!("{}-{}", package.name, package.version);
    let checksum = ctx
        .checksums_map
        .get(&format!("{}-{}", package.name, package.version))
        .unwrap();

    HttpArchive {
        name: vendor_name,
        urls: Set::from([url]),
        sha256: checksum.to_string(),
        _type: "tar.gz".to_owned(),
        strip_prefix: buckal_name,
        out: Some("vendor".to_owned()),
    }
}

/// Emit `filegroup` rule for the given package
fn emit_filegroup(package: &Package) -> FileGroup {
    let vendor_name = format!("{}-vendor", package.name);
    FileGroup {
        name: vendor_name,
        srcs: Glob {
            include: Set::from(["**/**".to_owned()]),
            ..Default::default()
        },
        out: Some("vendor".to_owned()),
    }
}

// Emit `cargo_manifest` rule for the given package
fn emit_cargo_manifest(package: &Package) -> CargoManifest {
    CargoManifest {
        name: format!("{}-manifest", package.name),
        vendor: get_vendor_target(package),
    }
}

fn get_build_name(s: &str) -> Cow<'_, str> {
    if let Some(stripped) = s.strip_suffix("-build") {
        Cow::Owned(stripped.to_string())
    } else {
        Cow::Borrowed(s)
    }
}

fn get_vendor_target(package: &Package) -> String {
    format!(":{}-vendor", package.name)
}

impl BuckalChange {
    pub fn apply(&self, ctx: &BuckalContext) {
        // This function applies changes to the BUCK files of detected packages in the cache diff, but skips the root package.
        let re = Regex::new(r"^([^+#]+)\+([^#]+)#([^@]+)@([^+#]+)(?:\+(.+))?$")
            .expect("error creating regex");
        let skip_pattern = format!("path+file://{}", ctx.workspace_root);

        for (id, change_type) in &self.changes {
            match change_type {
                ChangeType::Added | ChangeType::Changed => {
                    // Skip root package
                    if id == &ctx.root.id {
                        continue;
                    }

                    if let Some(node) = ctx.nodes_map.get(id) {
                        let package = ctx.packages_map.get(id).unwrap();

                        if ctx.separate && package.source.is_none() {
                            // Skip first-party packages if `--separate` is set
                            continue;
                        }

                        buckal_log!(
                            if let ChangeType::Added = change_type {
                                "Adding"
                            } else {
                                "Flushing"
                            },
                            format!("{} v{}", package.name, package.version)
                        );

                        // Vendor package sources
                        let vendor_dir = if package.source.is_none() {
                            package.manifest_path.parent().unwrap().to_owned()
                        } else {
                            vendor_package(package)
                        };

                        // Generate BUCK rules
                        let mut buck_rules = if package.source.is_none() {
                            buckify_root_node(node, ctx)
                        } else {
                            buckify_dep_node(node, ctx)
                        };

                        // Patch BUCK Rules
                        let buck_path = vendor_dir.join("BUCK");
                        if buck_path.exists() {
                            // Skip merging manual changes if `--no-merge` is set
                            if !ctx.no_merge && !ctx.repo_config.patch_fields.is_empty() {
                                let existing_rules = parse_buck_file(&buck_path)
                                    .expect("Failed to parse existing BUCK file");
                                patch_buck_rules(
                                    &existing_rules,
                                    &mut buck_rules,
                                    &ctx.repo_config.patch_fields,
                                );
                            }
                        } else {
                            std::fs::File::create(&buck_path).expect("Failed to create BUCK file");
                        }

                        // Generate the BUCK file
                        let buck_content = gen_buck_content(&buck_rules);
                        std::fs::write(&buck_path, buck_content)
                            .expect("Failed to write BUCK file");
                    }
                }
                ChangeType::Removed => {
                    // Skip workspace_root package
                    if id.repr.starts_with(skip_pattern.as_str()) {
                        continue;
                    }

                    let caps = re.captures(&id.repr).expect("Failed to parse package ID");
                    let name = &caps[3];
                    let version = &caps[4];

                    buckal_log!("Removing", format!("{} v{}", name, version));
                    let vendor_dir = get_vendor_dir(name, version)
                        .unwrap_or_exit_ctx("failed to get vendor directory");
                    if vendor_dir.exists() {
                        std::fs::remove_dir_all(&vendor_dir)
                            .expect("Failed to remove vendor directory");
                    }
                    if let Some(package_dir) = vendor_dir.parent()
                        && package_dir.exists()
                        && package_dir.read_dir().unwrap().next().is_none()
                    {
                        std::fs::remove_dir_all(package_dir)
                            .expect("Failed to remove empty package directory");
                    }
                }
            }
        }
    }
}

pub fn flush_root(ctx: &BuckalContext) {
    buckal_log!(
        "Flushing",
        format!("{} v{}", ctx.root.name, ctx.root.version)
    );
    let root_node = ctx
        .nodes_map
        .get(&ctx.root.id)
        .expect("Root node not found");
    if ctx.repo_config.inherit_workspace_deps {
        buckal_log!(
            "Generating",
            "third-party alias rules (inherit_workspace_deps=true)"
        );
        generate_third_party_aliases(ctx);
    }

    let cwd = std::env::current_dir().expect("Failed to get current directory");
    let buck_path = Utf8PathBuf::from(cwd.to_str().unwrap()).join("BUCK");

    // Generate BUCK rules
    let buck_rules = buckify_root_node(root_node, ctx);

    // Generate the BUCK file
    let buck_content = gen_buck_content(&buck_rules);
    std::fs::write(&buck_path, buck_content).expect("Failed to write BUCK file");
}

pub fn generate_third_party_aliases(ctx: &BuckalContext) {
    let root = get_buck2_root().expect("failed to get buck2 root");
    let dir = root.join("third-party/rust");
    std::fs::create_dir_all(&dir).expect("failed to create third-party/rust dir");

    let buck_file = dir.join("BUCK");

    let mut grouped: BTreeMap<String, Vec<&cargo_metadata::Package>> = BTreeMap::new();

    for (pkg_id, pkg) in &ctx.packages_map {
        // only workspace members (first-party)
        if pkg.source.is_some() {
            continue;
        }

        let node = match ctx.nodes_map.get(pkg_id) {
            Some(n) => n,
            None => continue,
        };

        for dep in &node.deps {
            let dep_pkg = ctx.packages_map.get(&dep.pkg).unwrap();
            if dep_pkg.source.is_some() {
                grouped
                    .entry(dep_pkg.name.to_string())
                    .or_default()
                    .push(dep_pkg);
            }
        }
    }

    let file = std::fs::File::create(&buck_file).expect("failed to create third-party/rust/BUCK");
    let mut writer = BufWriter::new(file);

    writeln!(writer, "# @generated by cargo-buckal\n").expect("failed to write header");

    for (crate_name, mut versions) in grouped {
        versions.sort_by(|a, b| a.version.cmp(&b.version));
        let latest = versions.last().expect("empty version list");

        let actual = format!(
            "//third-party/rust/crates/{}/{}:{}",
            crate_name, latest.version, crate_name
        );
        let rewritten_actual =
            rewrite_target_if_needed(&actual, root.as_std_path(), ctx.repo_config.align_cells)
                .unwrap_or_else(|e| {
                    buckal_warn!("Failed to rewrite target label '{}': {}", actual, e);
                    actual.clone()
                });

        let rule = Alias {
            name: crate_name.clone(),
            actual: rewritten_actual,
            visibility: ["PUBLIC"].into_iter().map(String::from).collect(),
        };
        let rendered = serde_starlark::to_string(&rule).expect("failed to serialize alias");
        writeln!(writer, "{}", rendered).expect("write failed");
    }

    writer.flush().expect("failed to flush alias rules");

    buckal_log!(
        "Generated",
        format!("third-party alias rules at {}", buck_file)
    );
}
