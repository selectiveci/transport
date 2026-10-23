use std::path::Path;

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
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
    detect_build_env_at(Path::new("."), |key| std::env::var(key).ok())
}

pub fn detect_build_env_at(cwd: &Path, var: impl Fn(&str) -> Option<String>) -> BuildEnv {
    let mut env = BuildEnv {
        api_key: value(&var, "SELECTIVE_API_KEY"),
        host: "wss://app.selective.ci".to_owned(),
        ..BuildEnv::default()
    };
    if present(&var, "GITHUB_ACTIONS") {
        env.platform = "github_actions".to_owned();
        env.branch = first_non_empty(&var, &["GITHUB_HEAD_REF", "GITHUB_REF_NAME"]);
        env.target_branch = value(&var, "GITHUB_BASE_REF");
        env.actor = value(&var, "GITHUB_ACTOR");
        env.sha = value(&var, "GITHUB_SHA");
        env.run_id = value(&var, "GITHUB_RUN_ID");
        env.run_attempt = value(&var, "GITHUB_RUN_ATTEMPT");
        env.runner_id = value(&var, "SELECTIVE_RUNNER_ID");
        env.commit_message = git_format(cwd, &env.sha, "%s");
        env.committer_name = git_format(cwd, &env.sha, "%an");
        env.committer_email = git_format(cwd, &env.sha, "%ae");
        env.git_repo_full_name = value(&var, "GITHUB_REPOSITORY");
        env.git_provider = "github".to_owned();
        env.base_sha = github_base_sha(&var).unwrap_or_default();
    } else if present(&var, "CIRCLECI") {
        env.platform = "circleci".to_owned();
        env.branch = value(&var, "CIRCLE_BRANCH");
        env.actor = first_non_empty(&var, &["CIRCLE_USERNAME", "CIRCLE_PR_USERNAME"]);
        env.sha = value(&var, "CIRCLE_SHA1");
        env.run_attempt = value(&var, "CIRCLE_BUILD_NUM");
        env.runner_id = value(&var, "CIRCLE_NODE_INDEX");
        env.commit_message = git_format(cwd, &env.sha, "%s");
        env.committer_name = git_format(cwd, &env.sha, "%an");
        env.committer_email = git_format(cwd, &env.sha, "%ae");
        let username = value(&var, "CIRCLE_PROJECT_USERNAME");
        let reponame = value(&var, "CIRCLE_PROJECT_REPONAME");
        if !username.is_empty() && !reponame.is_empty() {
            let repo_url = value(&var, "CIRCLE_REPOSITORY_URL");
            let provider = provider_from_url(&repo_url);
            if provider.as_deref() == Some("github") || repo_url.is_empty() {
                env.git_repo_full_name = format!("{username}/{reponame}");
                env.git_provider = provider.unwrap_or_else(|| "github".to_owned());
            } else if let Some(provider) = provider {
                env.git_provider = provider;
            }
        }
    } else if present(&var, "SEMAPHORE") {
        env.platform = "semaphore".to_owned();
        env.branch = first_non_empty(&var, &["SEMAPHORE_GIT_PR_BRANCH", "SEMAPHORE_GIT_BRANCH"]);
        if !value(&var, "SEMAPHORE_GIT_PR_BRANCH").is_empty() {
            env.target_branch = value(&var, "SEMAPHORE_GIT_BRANCH");
        }
        env.actor = value(&var, "SEMAPHORE_GIT_COMMITTER");
        env.sha = value(&var, "SEMAPHORE_GIT_SHA");
        env.run_id = value(&var, "SEMAPHORE_WORKFLOW_ID");
        env.run_attempt = "1".to_owned();
        env.runner_id = value(&var, "SEMAPHORE_JOB_ID");
        env.pr_title = value(&var, "SEMAPHORE_GIT_PR_NAME");
        env.commit_message = git_format(cwd, &env.sha, "%s");
        env.committer_name = git_format(cwd, &env.sha, "%an");
        env.committer_email = git_format(cwd, &env.sha, "%ae");
        env.git_provider = value(&var, "SEMAPHORE_GIT_PROVIDER");
        if env.git_provider == "github" {
            env.git_repo_full_name = value(&var, "SEMAPHORE_GIT_REPO_SLUG");
        }
        let range = value(&var, "SEMAPHORE_GIT_COMMIT_RANGE");
        if !range.is_empty() {
            env.base_sha = range.split("...").next().unwrap_or("").to_owned();
        }
    } else if present(&var, "BUILDKITE") {
        env.platform = "buildkite".to_owned();
        env.branch = value(&var, "BUILDKITE_BRANCH");
        env.target_branch = value(&var, "BUILDKITE_PULL_REQUEST_BASE_BRANCH");
        env.actor = value(&var, "BUILDKITE_BUILD_AUTHOR");
        env.sha = value(&var, "BUILDKITE_COMMIT");
        env.run_id = value(&var, "BUILDKITE_BUILD_ID");
        env.run_attempt = value(&var, "BUILDKITE_RETRY_COUNT");
        env.runner_id = value(&var, "BUILDKITE_PARALLEL_JOB");
        env.pr_title = value(&var, "BUILDKITE_PULL_REQUEST_TITLE");
        env.commit_message = value(&var, "BUILDKITE_MESSAGE");
        env.committer_name = value(&var, "BUILDKITE_BUILD_AUTHOR");
        env.committer_email = value(&var, "BUILDKITE_BUILD_AUTHOR_EMAIL");
        if value(&var, "BUILDKITE_PIPELINE_PROVIDER") == "github" {
            env.git_provider = "github".to_owned();
            env.git_repo_full_name =
                parse_github_slug(&value(&var, "BUILDKITE_REPO")).unwrap_or_default();
        } else {
            env.git_provider = value(&var, "BUILDKITE_PIPELINE_PROVIDER");
        }
    } else if present(&var, "RWX") {
        fill_rwx(&var, &mut env, "RWX");
    } else if present(&var, "MINT") {
        fill_rwx(&var, &mut env, "MINT");
    }

    if env.git_repo_full_name.is_empty() {
        if let Some(origin) = git_origin(cwd) {
            if let Some(slug) = parse_github_slug(&origin) {
                env.git_repo_full_name = slug;
                if env.git_provider.is_empty() {
                    env.git_provider = "github".to_owned();
                }
            } else if env.git_provider.is_empty() {
                if let Some(provider) = provider_from_url(&origin) {
                    env.git_provider = provider;
                }
            }
        }
    }

    for (key, field) in [
        ("SELECTIVE_HOST", &mut env.host),
        ("SELECTIVE_PLATFORM", &mut env.platform),
        ("SELECTIVE_BRANCH", &mut env.branch),
        ("SELECTIVE_PR_TITLE", &mut env.pr_title),
        ("SELECTIVE_TARGET_BRANCH", &mut env.target_branch),
        ("SELECTIVE_ACTOR", &mut env.actor),
        ("SELECTIVE_SHA", &mut env.sha),
        ("SELECTIVE_BASE_SHA", &mut env.base_sha),
        ("SELECTIVE_RUN_ID", &mut env.run_id),
        ("SELECTIVE_RUN_ATTEMPT", &mut env.run_attempt),
        ("SELECTIVE_RUNNER_ID", &mut env.runner_id),
        ("SELECTIVE_COMMIT_MESSAGE", &mut env.commit_message),
        ("SELECTIVE_COMMITTER_NAME", &mut env.committer_name),
        ("SELECTIVE_COMMITTER_EMAIL", &mut env.committer_email),
        ("SELECTIVE_GIT_REPO", &mut env.git_repo_full_name),
        ("SELECTIVE_GIT_PROVIDER", &mut env.git_provider),
    ] {
        if let Some(overridden) = var(key).filter(|value| !value.is_empty()) {
            *field = overridden;
        }
    }
    env
}

fn fill_rwx(var: &impl Fn(&str) -> Option<String>, env: &mut BuildEnv, prefix: &str) {
    env.platform = "rwx".to_owned();
    env.branch = value(var, &format!("{prefix}_GIT_REF_NAME"));
    env.actor = value(var, &format!("{prefix}_ACTOR"));
    env.sha = value(var, &format!("{prefix}_GIT_COMMIT_SHA"));
    env.run_id = value(var, &format!("{prefix}_RUN_ID"));
    env.run_attempt = value(var, &format!("{prefix}_TASK_ATTEMPT_NUMBER"));
    env.runner_id = value(var, &format!("{prefix}_PARALLEL_INDEX"));
    env.commit_message = value(var, &format!("{prefix}_GIT_COMMIT_SUMMARY"));
    env.committer_name = value(var, &format!("{prefix}_GIT_COMMITTER_NAME"));
    env.committer_email = value(var, &format!("{prefix}_GIT_COMMITTER_EMAIL"));
    let repo_url = value(var, &format!("{prefix}_GIT_REPOSITORY_URL"));
    if let Some(provider) = provider_from_url(&repo_url) {
        env.git_provider = provider;
    }
    if env.git_provider == "github" {
        env.git_repo_full_name = value(var, &format!("{prefix}_GIT_REPOSITORY_NAME"));
    }
}

fn github_base_sha(var: &impl Fn(&str) -> Option<String>) -> Option<String> {
    let text = std::fs::read_to_string(var("GITHUB_EVENT_PATH")?).ok()?;
    let json: Value = serde_json::from_str(&text).ok()?;
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
        })?;
    if sha.chars().all(|ch| ch == '0') {
        None
    } else {
        Some(sha)
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
    let rest = [
        "git@github.com:",
        "ssh://git@github.com/",
        "git://github.com/",
        "https://github.com/",
        "http://github.com/",
    ]
    .iter()
    .find_map(|prefix| url.strip_prefix(prefix))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_owned())
    }
}

fn git_origin(cwd: &Path) -> Option<String> {
    crate::snapshot::git(cwd, &["config", "--get", "remote.origin.url"])
        .map(|url| url.trim().to_owned())
        .filter(|url| !url.is_empty())
}

fn git_format(cwd: &Path, sha: &str, format: &str) -> String {
    let spec = format!("--format={format}");
    let mut args = vec!["log", spec.as_str(), "-n", "1"];
    if !sha.is_empty() {
        args.push(sha);
    }
    crate::snapshot::git(cwd, &args)
        .unwrap_or_default()
        .trim_end_matches(['\n', '\r'])
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;

    fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

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
