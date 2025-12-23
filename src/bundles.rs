use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::result::Result::Ok;

use anyhow::Result;
use reqwest::blocking::Client;
use reqwest::header::USER_AGENT;
use serde::Deserialize;

use crate::{buckal_log, buckal_warn, user_agent};

type Section = String;
type Lines = Vec<String>;

#[derive(Default)]
pub struct BuckConfig {
    section_order: Vec<Section>,
    sections: HashMap<Section, Lines>,
}

impl BuckConfig {
    pub fn load(path: &std::path::Path) -> std::io::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        Ok(Self::parse(contents))
    }

    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        std::fs::write(path, self.serialize())?;
        Ok(())
    }

    pub fn get_section_mut(&mut self, section: &str) -> &mut Lines {
        self.sections.entry(section.to_string()).or_default()
    }

    fn new_section_after(&mut self, after_section: &str, new_section_name: String) -> &mut Lines {
        self.sections.insert(new_section_name.clone(), Vec::new());

        if let Some(pos) = self.section_order.iter().position(|s| s == after_section) {
            self.section_order
                .insert(pos + 1, new_section_name.to_owned());
        } else {
            self.section_order.push(new_section_name.to_owned());
        }

        self.sections.entry(new_section_name).or_default()
    }

    fn new_section(&mut self, new_section_name: String) -> &mut Lines {
        self.sections.insert(new_section_name.clone(), Vec::new());
        self.section_order.push(new_section_name.to_owned());

        self.sections.entry(new_section_name).or_default()
    }

    fn parse(contents: String) -> BuckConfig {
        let lines: Vec<String> = contents.lines().map(|s| s.to_string()).collect();

        let mut config = BuckConfig::default();
        let mut current_section: Option<String> = None;

        for line in lines {
            let trimmed = line.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                let section_name = trimmed[1..trimmed.len() - 1].to_string();
                config.section_order.push(section_name.clone());
                current_section = Some(section_name);
            } else if trimmed.starts_with('#') {
                continue;
            } else if !line.is_empty()
                && let Some(section) = &current_section
            {
                config
                    .sections
                    .entry(section.clone())
                    .or_default()
                    .push(line);
            }
        }
        config
    }

    fn serialize(&self) -> String {
        let mut output = String::new();

        for section in &self.section_order {
            output.push('[');
            output.push_str(section);
            output.push_str("]\n");
            if let Some(lines) = self.sections.get(section) {
                for line in lines {
                    output.push_str(line);
                    output.push('\n');
                }
                output.push('\n');
            }
        }
        output.pop();

        output
    }

    /// In the [cells] section, return the mapping from cell names to their respective paths
    pub fn parse_cells(&self) -> HashMap<String, String> {
        let mut cells = HashMap::new();

        if let Some(cell_lines) = self.sections.get("cells") {
            for line in cell_lines {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }

                // parse format: "cell_name = path" or "  cell_name = path"
                if let Some(equal_pos) = trimmed.find('=') {
                    let cell_name = trimmed[..equal_pos].trim().to_string();
                    let cell_path = trimmed[equal_pos + 1..].trim().to_string();
                    if !cell_name.is_empty() && !cell_path.is_empty() {
                        cells.insert(cell_name, cell_path);
                    }
                }
            }
        }

        cells
    }

    /// Parse the [cell_aliases] section and return the mapping from aliases to cell names.
    pub fn parse_cell_aliases(&self) -> HashMap<String, String> {
        let mut aliases = HashMap::new();

        if let Some(alias_lines) = self.sections.get("cell_aliases") {
            for line in alias_lines {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }

                // parse format: "alias = cell_name" or "  alias = cell_name"
                if let Some(equal_pos) = trimmed.find('=') {
                    let alias = trimmed[..equal_pos].trim().to_string();
                    let cell_name = trimmed[equal_pos + 1..].trim().to_string();
                    if !alias.is_empty() && !cell_name.is_empty() {
                        aliases.insert(alias, cell_name);
                    }
                }
            }
        }

        aliases
    }

    /// Determine the corresponding cell based on the file path
    pub fn find_cell_for_path(&self, path: &Path, buck2_root: &Path) -> Option<String> {
        let cells = self.parse_cells();
        let aliases = self.parse_cell_aliases();

        // First, parse the complete cell mapping (considering aliases)
        let mut cell_mappings = HashMap::new();
        for (cell_name, cell_path) in &cells {
            cell_mappings.insert(cell_name.clone(), cell_path.clone());
        }

        // Apply the alias mapping
        for (alias, cell_name) in &aliases {
            if let Some(cell_path) = cells.get(cell_name) {
                cell_mappings.insert(alias.clone(), cell_path.clone());
            }
        }

        // Convert the path to a relative path relative to buck2_root
        let relative_path = match path.strip_prefix(buck2_root) {
            Ok(p) => p,
            Err(_) => return None,
        };

        // Search for the matching cell (using the most specific match)
        let mut best_match: Option<(String, usize)> = None;

        for (cell_name, cell_path) in &cell_mappings {
            // Convert the cell path to a Path
            let cell_path_obj = Path::new(cell_path);

            // Check if the path starts with the cell path
            if relative_path.starts_with(cell_path_obj) {
                let match_length = cell_path_obj.components().count();

                // Select the most specific match (the one with the longest path)
                match &best_match {
                    Some((_, current_length)) if match_length > *current_length => {
                        best_match = Some((cell_name.clone(), match_length));
                    }
                    None => {
                        best_match = Some((cell_name.clone(), match_length));
                    }
                    _ => {}
                }
            }
        }

        best_match.map(|(cell_name, _)| cell_name)
    }
}

pub fn init_modifier(dest: &std::path::Path) -> Result<()> {
    let mut package_file = std::fs::File::create(dest.join("PACKAGE"))?;

    writeln!(package_file, "# @generated by `cargo buckal`")?;
    writeln!(package_file)?;
    writeln!(
        package_file,
        "load(\"@prelude//cfg/modifier:set_cfg_modifiers.bzl\", \"set_cfg_modifiers\")"
    )?;
    writeln!(
        package_file,
        "load(\"@prelude//rust:with_workspace.bzl\", \"with_rust_workspace\")"
    )?;
    writeln!(
        package_file,
        "load(\"@buckal//config:set_cfg_constructor.bzl\", \"set_cfg_constructor\")"
    )?;
    writeln!(package_file)?;
    writeln!(package_file, "ALIASES = {{")?;
    writeln!(
        package_file,
        "    \"debug\": \"buckal//config/mode:debug\","
    )?;
    writeln!(
        package_file,
        "    \"release\": \"buckal//config/mode:release\","
    )?;
    writeln!(package_file, "}}")?;
    writeln!(package_file, "set_cfg_constructor(aliases = ALIASES)")?;
    writeln!(package_file)?;
    writeln!(package_file, "set_cfg_modifiers(")?;
    writeln!(package_file, "    cfg_modifiers = [")?;
    writeln!(package_file, "        \"buckal//config/mode:debug\",")?;
    writeln!(package_file, "    ],")?;
    writeln!(package_file, ")")?;

    Ok(())
}

pub fn init_buckal_cell(dest: &std::path::Path) -> Result<()> {
    let mut buckconfig = BuckConfig::load(&dest.join(".buckconfig"))?;
    let cells = buckconfig.get_section_mut("cells");
    cells.push("  buckal = buckal".to_owned());
    let external_cells = buckconfig.get_section_mut("external_cells");
    external_cells.push("  buckal = git".to_owned());
    let buckal_section =
        buckconfig.new_section_after("external_cells", "external_cell_buckal".to_owned());
    buckal_section.push(format!(
        "  git_origin = https://github.com/{}",
        crate::BUCKAL_BUNDLES_REPO
    ));
    let commit_hash = match fetch() {
        Ok(hash) => hash,
        Err(e) => {
            buckal_warn!(
                "Failed to fetch latest bundle hash ({}), using default hash instead.",
                e
            );
            crate::DEFAULT_BUNDLE_HASH.to_string()
        }
    };
    buckal_section.push(format!("  commit_hash = {}", commit_hash));
    let project = buckconfig.new_section("project".to_owned());
    project.push("  ignore = .git .buckal buck-out target".to_owned());
    buckconfig.save(&dest.join(".buckconfig"))?;

    Ok(())
}

pub fn fetch_buckal_cell(dest: &std::path::Path) -> Result<()> {
    let mut buckconfig = BuckConfig::load(&dest.join(".buckconfig"))?;
    let buckal_section = buckconfig.get_section_mut("external_cell_buckal");
    buckal_section.clear();
    buckal_section.push(format!(
        "  git_origin = https://github.com/{}",
        crate::BUCKAL_BUNDLES_REPO
    ));
    let commit_hash = match fetch() {
        Ok(hash) => hash,
        Err(e) => {
            buckal_warn!(
                "Failed to fetch latest bundle hash ({}), using default hash instead.",
                e
            );
            crate::DEFAULT_BUNDLE_HASH.to_string()
        }
    };
    buckal_section.push(format!("  commit_hash = {}", commit_hash));
    buckconfig.save(&dest.join(".buckconfig"))?;

    Ok(())
}

#[derive(Deserialize)]
struct GithubCommit {
    sha: String,
}

pub fn fetch() -> Result<String> {
    let url = format!(
        "https://api.github.com/repos/{}/commits",
        crate::BUCKAL_BUNDLES_REPO
    );
    buckal_log!(
        "Fetching",
        format!("https://github.com/{}", crate::BUCKAL_BUNDLES_REPO)
    );
    let client = Client::new();
    let response: Vec<GithubCommit> = client
        .get(&url)
        .header(USER_AGENT, user_agent())
        .query(&[("per_page", "1")])
        .send()?
        .json()?;
    Ok(response[0].sha.clone())
}
