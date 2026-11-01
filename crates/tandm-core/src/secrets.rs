//! The script's push guard: refuse to push when added lines in the outgoing diff look like secrets.
//! Same three regexes as the original `git-autosync.sh`, except that a hit reports `file:line (rule)`
//! and never the matched text, because records travel to other machines and into the GUI.

use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretHit {
    pub file: String,
    pub line: u32,
    pub rule: String,
    /// Short id of the commit that adds the line, when scanning `git log -p`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

impl std::fmt::Display for SecretHit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{} ({})", self.file, self.line, self.rule)?;
        if let Some(c) = &self.commit {
            write!(f, " in {c}")?;
        }
        Ok(())
    }
}

fn rules() -> &'static [(&'static str, Regex)] {
    static RULES: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    RULES.get_or_init(|| {
        // Each is the script's `grep -E '^\+.*(…)'` alternative, applied to an added line.
        [
            ("private key", r"-----BEGIN [A-Z ]*PRIVATE KEY"),
            ("mongodb uri with password", r"mongodb(\+srv)?://[^<$ /]+:[^<$@ ]+@"),
            (
                "credential assignment",
                r#"(KEY|SECRET|TOKEN|PASSWORD|PW)[A-Z_]*[[:space:]]*[=:][[:space:]]*["']?[A-Za-z0-9/+_-]{24,}"#,
            ),
        ]
        .into_iter()
        .map(|(name, re)| (name, Regex::new(re).expect("secret regex")))
        .collect()
    })
}

/// Rule name of the first rule an added line (without its leading `+`) matches.
pub fn match_line(line: &str) -> Option<&'static str> {
    rules()
        .iter()
        .find(|(_, re)| re.is_match(line))
        .map(|(n, _)| *n)
}

/// Scan a unified diff, or `git log -p --format='commit %H'` output, and return hits on added
/// lines.
pub fn scan_diff(diff: &str) -> Vec<SecretHit> {
    let mut hits = Vec::new();
    let mut file = String::new();
    let mut commit: Option<String> = None;
    let mut new_line: u32 = 0;
    for line in diff.lines() {
        // Diff body lines always start with ' ', '+', '-' or '\\', so this can't be content.
        if let Some(id) = line.strip_prefix("commit ") {
            commit = Some(id.chars().take(8).collect());
            continue;
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            file = rest.strip_prefix("b/").unwrap_or(rest).to_string();
            continue;
        }
        if line.starts_with("--- ") || line.starts_with("diff --git ") {
            continue;
        }
        if let Some(h) = line.strip_prefix("@@ ") {
            // @@ -a,b +c,d @@
            new_line = h
                .split_whitespace()
                .find_map(|t| t.strip_prefix('+'))
                .and_then(|t| t.split(',').next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            continue;
        }
        if let Some(added) = line.strip_prefix('+') {
            if let Some(rule) = match_line(added) {
                hits.push(SecretHit {
                    file: file.clone(),
                    line: new_line,
                    rule: rule.to_string(),
                    commit: commit.clone(),
                });
            }
            new_line += 1;
        } else if line.starts_with(' ') {
            new_line += 1;
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_match_like_the_script() {
        assert_eq!(
            match_line("-----BEGIN OPENSSH PRIVATE KEY-----"),
            Some("private key")
        );
        assert_eq!(
            match_line("uri: mongodb+srv://app:s3cr3t@cluster0.example.net/db"),
            Some("mongodb uri with password")
        );
        // Placeholders the script deliberately lets through.
        assert_eq!(match_line("mongodb://<user>:<pass>@host"), None);
        assert_eq!(match_line("mongodb://$USER:$PASS@host"), None);
        assert_eq!(
            match_line("export API_KEY=\"abcdefghijklmnopqrstuvwxyz012345\""),
            Some("credential assignment")
        );
        assert_eq!(
            match_line("GITEA_TOKEN: abcdefghijklmnopqrstuvwx"),
            Some("credential assignment")
        );
        assert_eq!(match_line("API_KEY=short"), None);
        assert_eq!(match_line("api_key=abcdefghijklmnopqrstuvwxyz012345"), None);
        assert_eq!(match_line("just some text"), None);
    }

    #[test]
    fn diff_positions() {
        let diff = "diff --git a/x.env b/x.env\n--- a/x.env\n+++ b/x.env\n@@ -1,2 +1,3 @@\n a=1\n-b=2\n+DB_PASSWORD=abcdefghijklmnopqrstuvwxyz\n c=3\n";
        let hits = scan_diff(diff);
        assert_eq!(
            hits,
            vec![SecretHit {
                file: "x.env".into(),
                line: 2,
                rule: "credential assignment".into(),
                commit: None,
            }]
        );
        assert!(!hits[0].to_string().contains("abcdef"));
    }
}
