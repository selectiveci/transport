use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BuildEnv {
    pub api_key: String,
    pub host: String,
    pub platform: String,
    pub branch: String,
    pub pr_title: String,
    pub target_branch: String,
    pub actor: String,
    pub sha: String,
    pub base_sha: String,
    pub run_id: String,
    pub run_attempt: String,
    pub runner_id: String,
    pub commit_message: String,
    pub committer_name: String,
    pub committer_email: String,
    pub git_repo_full_name: String,
    pub git_provider: String,
}

pub fn detect_build_env() -> BuildEnv {
    let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    detect_build_env_at(&cwd, |key| std::env::var(key).ok())
}

pub fn detect_build_env_at(cwd: &Path, var: impl Fn(&str) -> Option<String>) -> BuildEnv {
    let mut detected = Detected::default();
    if present(&var, "GITHUB_ACTIONS") {
        detected.platform = "github_actions".to_owned();
        detected.branch = first_non_empty(&var, &["GITHUB_HEAD_REF", "GITHUB_REF_NAME"]);
        detected.target_branch = value(&var, "GITHUB_BASE_REF");
        detected.actor = value(&var, "GITHUB_ACTOR");
        detected.sha = value(&var, "GITHUB_SHA");
        detected.run_id = value(&var, "GITHUB_RUN_ID");
        detected.run_attempt = value(&var, "GITHUB_RUN_ATTEMPT");
        detected.runner_id = value(&var, "SELECTIVE_RUNNER_ID");
        detected.commit_message = git_format(cwd, &detected.sha, "%s");
        detected.committer_name = git_show(cwd, &detected.sha, "%an");
        detected.committer_email = git_show(cwd, &detected.sha, "%ae");
        detected.git_repo_full_name = value(&var, "GITHUB_REPOSITORY");
        detected.git_provider = "github".to_owned();
        detected.base_sha = github_base_sha(&var);
    } else if present(&var, "CIRCLECI") {
        detected.platform = "circleci".to_owned();
        detected.branch = value(&var, "CIRCLE_BRANCH");
        detected.actor = first_non_empty(&var, &["CIRCLE_USERNAME", "CIRCLE_PR_USERNAME"]);
        detected.sha = value(&var, "CIRCLE_SHA1");
        detected.run_attempt = value(&var, "CIRCLE_BUILD_NUM");
        detected.runner_id = value(&var, "CIRCLE_NODE_INDEX");
        detected.commit_message = git_format(cwd, &detected.sha, "%s");
        detected.committer_name = git_show(cwd, &detected.sha, "%an");
        detected.committer_email = git_show(cwd, &detected.sha, "%ae");
        let username = value(&var, "CIRCLE_PROJECT_USERNAME");
        let reponame = value(&var, "CIRCLE_PROJECT_REPONAME");
        if !username.is_empty() && !reponame.is_empty() {
            let repo_url = value(&var, "CIRCLE_REPOSITORY_URL");
            let provider = provider_from_url(&repo_url);
            if provider.as_deref() == Some("github") || repo_url.is_empty() {
                detected.git_repo_full_name = format!("{username}/{reponame}");
                detected.git_provider = provider.unwrap_or_else(|| "github".to_owned());
            } else if let Some(provider) = provider {
                detected.git_provider = provider;
            }
        }
    } else if present(&var, "SEMAPHORE") {
        detected.platform = "semaphore".to_owned();
        detected.branch =
            first_non_empty(&var, &["SEMAPHORE_GIT_PR_BRANCH", "SEMAPHORE_GIT_BRANCH"]);
        if !value(&var, "SEMAPHORE_GIT_PR_BRANCH").is_empty() {
            detected.target_branch = value(&var, "SEMAPHORE_GIT_BRANCH");
        }
        detected.actor = value(&var, "SEMAPHORE_GIT_COMMITTER");
        detected.sha = value(&var, "SEMAPHORE_GIT_SHA");
        detected.run_id = value(&var, "SEMAPHORE_WORKFLOW_ID");
        detected.run_attempt = "1".to_owned();
        detected.runner_id = value(&var, "SEMAPHORE_JOB_ID");
        detected.pr_title = value(&var, "SEMAPHORE_GIT_PR_NAME");
        detected.commit_message = git_format(cwd, &detected.sha, "%s");
        detected.committer_name = git_show(cwd, &detected.sha, "%an");
        detected.committer_email = git_show(cwd, &detected.sha, "%ae");
        detected.git_provider = value(&var, "SEMAPHORE_GIT_PROVIDER");
        if detected.git_provider == "github" {
            detected.git_repo_full_name = value(&var, "SEMAPHORE_GIT_REPO_SLUG");
        }
        let range = value(&var, "SEMAPHORE_GIT_COMMIT_RANGE");
        if !range.is_empty() {
            detected.base_sha = range.split("...").next().unwrap_or("").to_owned();
        }
    } else if present(&var, "BUILDKITE") {
        detected.platform = "buildkite".to_owned();
        detected.branch = value(&var, "BUILDKITE_BRANCH");
        detected.target_branch = value(&var, "BUILDKITE_PULL_REQUEST_BASE_BRANCH");
        detected.actor = value(&var, "BUILDKITE_BUILD_AUTHOR");
        detected.sha = value(&var, "BUILDKITE_COMMIT");
        detected.run_id = value(&var, "BUILDKITE_BUILD_ID");
        detected.run_attempt = value(&var, "BUILDKITE_RETRY_COUNT");
        detected.runner_id = value(&var, "BUILDKITE_PARALLEL_JOB");
        detected.pr_title = value(&var, "BUILDKITE_PULL_REQUEST_TITLE");
        detected.commit_message = value(&var, "BUILDKITE_MESSAGE");
        detected.committer_name = value(&var, "BUILDKITE_BUILD_AUTHOR");
        detected.committer_email = value(&var, "BUILDKITE_BUILD_AUTHOR_EMAIL");
        if value(&var, "BUILDKITE_PIPELINE_PROVIDER") == "github" {
            detected.git_provider = "github".to_owned();
            detected.git_repo_full_name =
                parse_github_slug(&value(&var, "BUILDKITE_REPO")).unwrap_or_default();
        } else {
            detected.git_provider = value(&var, "BUILDKITE_PIPELINE_PROVIDER");
        }
    } else if present(&var, "RWX") {
        fill_rwx(&var, &mut detected, "RWX");
    } else if present(&var, "MINT") {
        fill_rwx(&var, &mut detected, "MINT");
    }

    if detected.git_repo_full_name.is_empty() {
        if let Some(origin) = git_origin(cwd) {
            if let Some(slug) = parse_github_slug(&origin) {
                detected.git_repo_full_name = slug;
                if detected.git_provider.is_empty() {
                    detected.git_provider = "github".to_owned();
                }
            } else if detected.git_provider.is_empty() {
                if let Some(provider) = provider_from_url(&origin) {
                    detected.git_provider = provider;
                }
            }
        }
    }

    BuildEnv {
        api_key: value(&var, "SELECTIVE_API_KEY"),
        host: override_var(&var, "SELECTIVE_HOST", "wss://app.selective.ci".to_owned()),
        platform: override_var(&var, "SELECTIVE_PLATFORM", detected.platform),
        branch: override_var(&var, "SELECTIVE_BRANCH", detected.branch),
        pr_title: override_var(&var, "SELECTIVE_PR_TITLE", detected.pr_title),
        target_branch: override_var(&var, "SELECTIVE_TARGET_BRANCH", detected.target_branch),
        actor: override_var(&var, "SELECTIVE_ACTOR", detected.actor),
        sha: override_var(&var, "SELECTIVE_SHA", detected.sha),
        base_sha: override_var(&var, "SELECTIVE_BASE_SHA", detected.base_sha),
        run_id: override_var(&var, "SELECTIVE_RUN_ID", detected.run_id),
        run_attempt: override_var(&var, "SELECTIVE_RUN_ATTEMPT", detected.run_attempt),
        runner_id: override_var(&var, "SELECTIVE_RUNNER_ID", detected.runner_id),
        commit_message: override_var(&var, "SELECTIVE_COMMIT_MESSAGE", detected.commit_message),
        committer_name: override_var(&var, "SELECTIVE_COMMITTER_NAME", detected.committer_name),
        committer_email: override_var(&var, "SELECTIVE_COMMITTER_EMAIL", detected.committer_email),
        git_repo_full_name: override_var(&var, "SELECTIVE_GIT_REPO", detected.git_repo_full_name),
        git_provider: override_var(&var, "SELECTIVE_GIT_PROVIDER", detected.git_provider),
    }
}

pub fn build_env_json() -> String {
    serde_json::to_string(&detect_build_env()).unwrap_or_else(|_| "{}".to_owned())
}

#[derive(Default)]
struct Detected {
    platform: String,
    branch: String,
    pr_title: String,
    target_branch: String,
    actor: String,
    sha: String,
    base_sha: String,
    run_id: String,
    run_attempt: String,
    runner_id: String,
    commit_message: String,
    committer_name: String,
    committer_email: String,
    git_repo_full_name: String,
    git_provider: String,
}

fn fill_rwx(var: &impl Fn(&str) -> Option<String>, detected: &mut Detected, prefix: &str) {
    detected.platform = "rwx".to_owned();
    detected.branch = value(var, &format!("{prefix}_GIT_REF_NAME"));
    detected.actor = value(var, &format!("{prefix}_ACTOR"));
    detected.sha = value(var, &format!("{prefix}_GIT_COMMIT_SHA"));
    detected.run_id = value(var, &format!("{prefix}_RUN_ID"));
    detected.run_attempt = value(var, &format!("{prefix}_TASK_ATTEMPT_NUMBER"));
    detected.runner_id = value(var, &format!("{prefix}_PARALLEL_INDEX"));
    detected.commit_message = value(var, &format!("{prefix}_GIT_COMMIT_SUMMARY"));
    detected.committer_name = value(var, &format!("{prefix}_GIT_COMMITTER_NAME"));
    detected.committer_email = value(var, &format!("{prefix}_GIT_COMMITTER_EMAIL"));
    let repo_url = value(var, &format!("{prefix}_GIT_REPOSITORY_URL"));
    if let Some(provider) = provider_from_url(&repo_url) {
        detected.git_provider = provider;
    }
    if detected.git_provider == "github" {
        detected.git_repo_full_name = value(var, &format!("{prefix}_GIT_REPOSITORY_NAME"));
    }
}

fn github_base_sha(var: &impl Fn(&str) -> Option<String>) -> String {
    let Some(path) = var("GITHUB_EVENT_PATH") else {
        return String::new();
    };
    if path.is_empty() {
        return String::new();
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let Ok(json) = serde_json::from_str::<Value>(&text) else {
        return String::new();
    };
    let sha = json
        .pointer("/pull_request/base/sha")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            json.get("before")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_default();
    if sha.chars().all(|ch| ch == '0') && !sha.is_empty() {
        String::new()
    } else {
        sha
    }
}

fn present(var: &impl Fn(&str) -> Option<String>, key: &str) -> bool {
    var(key).is_some_and(|value| !value.is_empty())
}

fn value(var: &impl Fn(&str) -> Option<String>, key: &str) -> String {
    var(key).unwrap_or_default()
}

fn first_non_empty(var: &impl Fn(&str) -> Option<String>, keys: &[&str]) -> String {
    keys.iter()
        .map(|key| value(var, key))
        .find(|item| !item.is_empty())
        .unwrap_or_default()
}

fn override_var(var: &impl Fn(&str) -> Option<String>, key: &str, detected: String) -> String {
    match var(key) {
        Some(value) if !value.is_empty() => value,
        _ => detected,
    }
}

fn provider_from_url(url: &str) -> Option<String> {
    if url.contains("github.com") {
        Some("github".to_owned())
    } else if url.contains("gitlab.com") {
        Some("gitlab".to_owned())
    } else if url.contains("bitbucket.org") {
        Some("bitbucket".to_owned())
    } else {
        None
    }
}

pub fn parse_github_slug(url: &str) -> Option<String> {
    let rest = if let Some(rest) = url.strip_prefix("git@github.com:") {
        rest
    } else if let Some(rest) = url.strip_prefix("ssh://git@github.com/") {
        rest
    } else if let Some(rest) = url.strip_prefix("git://github.com/") {
        rest
    } else if let Some(rest) = url.strip_prefix("https://github.com/") {
        rest
    } else if let Some(rest) = url.strip_prefix("http://github.com/") {
        rest
    } else {
        return None;
    };
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_owned())
    }
}

fn git_origin(cwd: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .ok()?;
    if output.status.success() {
        let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if url.is_empty() {
            None
        } else {
            Some(url)
        }
    } else {
        None
    }
}

fn git_format(cwd: &Path, sha: &str, format: &str) -> String {
    let spec = format!("--format={format}");
    let mut args = vec!["log", spec.as_str(), "-n", "1"];
    if !sha.is_empty() {
        args.push(sha);
    }
    git_stdout(cwd, &args)
        .unwrap_or_default()
        .trim_end_matches(['\n', '\r'])
        .to_owned()
}

fn git_show(cwd: &Path, sha: &str, format: &str) -> String {
    let spec = format!("--format={format}");
    let mut args = vec!["show", "-s", spec.as_str(), "-n", "1"];
    if !sha.is_empty() {
        args.push(sha);
    }
    git_stdout(cwd, &args)
        .unwrap_or_default()
        .trim_end_matches(['\n', '\r'])
        .to_owned()
}

fn git_stdout(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    }
}

pub fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn lookup<'a>(map: &'a HashMap<String, String>) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| map.get(key).cloned()
    }

    fn isolated() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "transport-build-env-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn github_actions_slug_and_base_sha() {
        let dir = isolated();
        let event = dir.join("event.json");
        fs::write(
            &event,
            r#"{"pull_request":{"base":{"sha":"basefeedfeed"}},"before":"ignored"}"#,
        )
        .unwrap();
        let mut map = HashMap::new();
        map.insert("GITHUB_ACTIONS".to_owned(), "true".to_owned());
        map.insert("GITHUB_REPOSITORY".to_owned(), "acme/widgets".to_owned());
        map.insert("GITHUB_SHA".to_owned(), "headfeedfeed".to_owned());
        map.insert("GITHUB_REF_NAME".to_owned(), "main".to_owned());
        map.insert(
            "GITHUB_EVENT_PATH".to_owned(),
            event.to_string_lossy().into_owned(),
        );
        let env = detect_build_env_at(&dir, lookup(&map));
        assert_eq!(env.platform, "github_actions");
        assert_eq!(env.git_repo_full_name, "acme/widgets");
        assert_eq!(env.git_provider, "github");
        assert_eq!(env.base_sha, "basefeedfeed");
        assert_eq!(env.host, "wss://app.selective.ci");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn github_before_sha_and_zero_sha() {
        let dir = isolated();
        let event = dir.join("event.json");
        fs::write(&event, r#"{"before":"beforesha1234","after":"head"}"#).unwrap();
        let mut map = HashMap::new();
        map.insert("GITHUB_ACTIONS".to_owned(), "true".to_owned());
        map.insert(
            "GITHUB_EVENT_PATH".to_owned(),
            event.to_string_lossy().into_owned(),
        );
        let env = detect_build_env_at(&dir, lookup(&map));
        assert_eq!(env.base_sha, "beforesha1234");

        fs::write(&event, format!("{{\"before\":\"{}\"}}", "0".repeat(40))).unwrap();
        let env = detect_build_env_at(&dir, lookup(&map));
        assert_eq!(env.base_sha, "");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn circle_buildkite_semaphore_rwx_and_mint() {
        let dir = isolated();
        let cases = [
            (
                vec![
                    ("CIRCLECI", "true"),
                    ("CIRCLE_PROJECT_USERNAME", "acme"),
                    ("CIRCLE_PROJECT_REPONAME", "widgets"),
                    ("CIRCLE_REPOSITORY_URL", "git@github.com:acme/widgets.git"),
                    ("CIRCLE_SHA1", "abc123"),
                ],
                "circleci",
                "acme/widgets",
                "github",
            ),
            (
                vec![
                    ("CIRCLECI", "true"),
                    ("CIRCLE_PROJECT_USERNAME", "acme"),
                    ("CIRCLE_PROJECT_REPONAME", "widgets"),
                    (
                        "CIRCLE_REPOSITORY_URL",
                        "git@bitbucket.org:acme/widgets.git",
                    ),
                ],
                "circleci",
                "",
                "bitbucket",
            ),
            (
                vec![
                    ("SEMAPHORE", "true"),
                    ("SEMAPHORE_GIT_PROVIDER", "github"),
                    ("SEMAPHORE_GIT_REPO_SLUG", "acme/widgets"),
                    ("SEMAPHORE_GIT_COMMIT_RANGE", "basefeedfeed...headfeedfeed"),
                ],
                "semaphore",
                "acme/widgets",
                "github",
            ),
            (
                vec![
                    ("BUILDKITE", "true"),
                    ("BUILDKITE_PIPELINE_PROVIDER", "github"),
                    ("BUILDKITE_REPO", "https://github.com/acme/widgets.git"),
                ],
                "buildkite",
                "acme/widgets",
                "github",
            ),
            (
                vec![
                    ("BUILDKITE", "true"),
                    ("BUILDKITE_PIPELINE_PROVIDER", "gitlab"),
                    ("BUILDKITE_REPO", "git@gitlab.com:acme/widgets.git"),
                ],
                "buildkite",
                "",
                "gitlab",
            ),
            (
                vec![
                    ("RWX", "true"),
                    ("RWX_GIT_REPOSITORY_NAME", "acme/widgets"),
                    (
                        "RWX_GIT_REPOSITORY_URL",
                        "https://github.com/acme/widgets.git",
                    ),
                ],
                "rwx",
                "acme/widgets",
                "github",
            ),
            (
                vec![
                    ("RWX", "true"),
                    ("RWX_GIT_REPOSITORY_NAME", "acme/widgets"),
                    (
                        "RWX_GIT_REPOSITORY_URL",
                        "https://gitlab.com/acme/widgets.git",
                    ),
                ],
                "rwx",
                "",
                "gitlab",
            ),
            (
                vec![
                    ("MINT", "true"),
                    ("MINT_GIT_REPOSITORY_NAME", "acme/widgets"),
                    ("MINT_GIT_REPOSITORY_URL", "git@github.com:acme/widgets.git"),
                ],
                "rwx",
                "acme/widgets",
                "github",
            ),
        ];
        for (pairs, platform, slug, provider) in cases {
            let map = env_map(&pairs);
            let env = detect_build_env_at(&dir, lookup(&map));
            assert_eq!(env.platform, platform, "{pairs:?}");
            assert_eq!(env.git_repo_full_name, slug, "{pairs:?}");
            assert_eq!(env.git_provider, provider, "{pairs:?}");
            if pairs
                .iter()
                .any(|(key, _)| *key == "SEMAPHORE_GIT_COMMIT_RANGE")
            {
                assert_eq!(env.base_sha, "basefeedfeed");
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn overrides_and_slug_parser() {
        let dir = isolated();
        let map = env_map(&[
            ("GITHUB_ACTIONS", "true"),
            ("GITHUB_REPOSITORY", "detected/repo"),
            ("SELECTIVE_GIT_REPO", "override/repo"),
            ("SELECTIVE_BASE_SHA", "overridesha"),
            ("GITHUB_SHA", "abc"),
        ]);
        let env = detect_build_env_at(&dir, lookup(&map));
        assert_eq!(env.git_repo_full_name, "override/repo");
        assert_eq!(env.base_sha, "overridesha");

        let empty = detect_build_env_at(&dir, lookup(&HashMap::new()));
        assert_eq!(empty.platform, "");
        assert_eq!(empty.git_repo_full_name, "");
        assert_eq!(empty.git_provider, "");
        assert_eq!(empty.base_sha, "");

        let urls = [
            ("git@github.com:acme/widgets.git", "acme/widgets"),
            ("git@github.com:acme/widgets", "acme/widgets"),
            ("https://github.com/acme/widgets.git", "acme/widgets"),
            ("https://github.com/acme/widgets", "acme/widgets"),
            ("http://github.com/acme/widgets.git", "acme/widgets"),
            ("ssh://git@github.com/acme/widgets.git", "acme/widgets"),
            ("git://github.com/acme/widgets.git", "acme/widgets"),
        ];
        for (url, expected) in urls {
            assert_eq!(parse_github_slug(url).as_deref(), Some(expected));
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
