//! Launch-argv assembly and the `agy models` / `agy --version` output parsers.
//!
//! The long-lived launch pins the stream-json framing; the admitted model,
//! effort, permission posture, and (on resume) a conversation id are appended
//! as explicit flags. Effort is the measured closed set `low|medium|high|max`
//! (which includes `max`, unlike the official documentation,
//! `docs/compatibility/antigravity.md` §5/§9-2); the two admitted permission
//! postures map to `--mode accept-edits` (build) and
//! `--dangerously-skip-permissions` (yolo). Model/effort compatibility is not
//! orthogonal in `agy` and stays admission's concern; this module only
//! serializes what it is given.

/// The stream-json framing every long-lived `agy` launch pins.
pub const BASE_ARGS: [&str; 4] = [
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
];

/// The `agy models` subcommand name.
pub const MODELS_SUBCOMMAND: &str = "models";

/// Upper bound on the `agy models` slug list.
pub const MAX_MODELS: usize = 256;

/// The two permission postures the product admits for `agy` (`build` and
/// `yolo`); plan/edit are rejected at admission and have no mapping here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgyPermissionMode {
    Build,
    Yolo,
}

impl AgyPermissionMode {
    /// The argv fragment for this posture.
    pub fn args(self) -> &'static [&'static str] {
        match self {
            Self::Build => &["--mode", "accept-edits"],
            Self::Yolo => &["--dangerously-skip-permissions"],
        }
    }
}

/// The measured `agy` effort closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgyEffort {
    Low,
    Medium,
    High,
    Max,
}

impl AgyEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
        }
    }

    /// Parse one effort token against the closed set.
    pub fn parse(token: &str) -> Option<Self> {
        Some(match token {
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "max" => Self::Max,
            _ => return None,
        })
    }
}

/// Optional launch selections. Every field is omitted from the argv when
/// `None`, so the historical stream-json launch is the bare baseline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgyLaunchOptions {
    pub model: Option<String>,
    pub effort: Option<AgyEffort>,
    pub permission: Option<AgyPermissionMode>,
    pub conversation: Option<String>,
}

/// Assemble the child argv (excluding the executable) for one `agy` launch:
/// the pinned stream framing first, then model, effort, permission, and
/// conversation in that order.
pub fn launch_args(options: &AgyLaunchOptions) -> Vec<String> {
    let mut args: Vec<String> = BASE_ARGS.iter().map(|arg| (*arg).to_owned()).collect();
    if let Some(model) = &options.model {
        args.push("--model".into());
        args.push(model.clone());
    }
    if let Some(effort) = options.effort {
        args.push("--effort".into());
        args.push(effort.as_str().into());
    }
    if let Some(permission) = options.permission {
        args.extend(permission.args().iter().map(|arg| (*arg).to_owned()));
    }
    if let Some(conversation) = &options.conversation {
        args.push("--conversation".into());
        args.push(conversation.clone());
    }
    args
}

/// The `agy models` argv (excluding the executable).
pub fn models_args() -> Vec<String> {
    vec![MODELS_SUBCOMMAND.to_owned()]
}

/// Parse `agy models` stdout into a deduplicated, first-seen-ordered slug list
/// capped at [`MAX_MODELS`]. Each model line is `slug<TAB>display name`; the
/// leading status line (`Fetching available models...`) and any other line
/// without a tab are ignored. A repeated slug keeps its first position.
pub fn parse_models(output: &str) -> Vec<String> {
    let mut models: Vec<String> = Vec::new();
    for line in output.lines() {
        let Some((slug, _display)) = line.split_once('\t') else {
            continue;
        };
        let slug = slug.trim();
        if slug.is_empty() || models.iter().any(|existing| existing == slug) {
            continue;
        }
        if models.len() >= MAX_MODELS {
            break;
        }
        models.push(slug.to_owned());
    }
    models
}

/// Parse `agy --version` stdout by trimming surrounding whitespace/newlines.
pub fn parse_version(output: &str) -> String {
    output.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODELS: &str = include_str!("../tests/fixtures/models.txt");

    #[test]
    fn bare_launch_pins_only_the_stream_framing() {
        assert_eq!(
            launch_args(&AgyLaunchOptions::default()),
            vec![
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json"
            ]
        );
    }

    #[test]
    fn launch_args_assemble_options_in_order() {
        let options = AgyLaunchOptions {
            model: Some("claude-sonnet-4-6".into()),
            effort: Some(AgyEffort::High),
            permission: Some(AgyPermissionMode::Build),
            conversation: Some("a48d42bb-a643-476b-b394-f4efac7fff21".into()),
        };
        assert_eq!(
            launch_args(&options),
            vec![
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--model",
                "claude-sonnet-4-6",
                "--effort",
                "high",
                "--mode",
                "accept-edits",
                "--conversation",
                "a48d42bb-a643-476b-b394-f4efac7fff21",
            ]
        );
    }

    #[test]
    fn permission_and_effort_map_to_the_measured_flags() {
        assert_eq!(AgyPermissionMode::Build.args(), ["--mode", "accept-edits"]);
        assert_eq!(
            AgyPermissionMode::Yolo.args(),
            ["--dangerously-skip-permissions"]
        );
        for (effort, token) in [
            (AgyEffort::Low, "low"),
            (AgyEffort::Medium, "medium"),
            (AgyEffort::High, "high"),
            (AgyEffort::Max, "max"),
        ] {
            assert_eq!(effort.as_str(), token);
            assert_eq!(AgyEffort::parse(token), Some(effort));
        }
        assert_eq!(AgyEffort::parse("extreme"), None);
        let yolo = AgyLaunchOptions {
            permission: Some(AgyPermissionMode::Yolo),
            ..AgyLaunchOptions::default()
        };
        assert_eq!(
            launch_args(&yolo).last().unwrap(),
            "--dangerously-skip-permissions"
        );
    }

    #[test]
    fn models_argv_is_the_subcommand() {
        assert_eq!(models_args(), vec!["models"]);
    }

    #[test]
    fn models_parser_reads_the_real_excerpt_and_dedupes() {
        let models = parse_models(MODELS);
        assert_eq!(
            models,
            vec![
                "gemini-3.8-flash-high",
                "gemini-3.8-flash-medium",
                "gemini-3.8-flash-low",
                "gemini-3.7-flash-high",
                "gemini-3.7-flash-medium",
                "gemini-3.7-flash-low",
                "gemini-3.6-flash-high",
                "gemini-3.6-flash-medium",
                "gemini-3.6-flash-low",
                "gemini-3.1-pro-high",
                "gemini-3.1-pro-low",
                "claude-sonnet-4-6",
                "claude-opus-4-6-thinking",
                "gpt-oss-120b-medium",
            ]
        );
        assert_eq!(models.len(), 14);
        // The status line is skipped and repeats keep first-seen order.
        let noisy =
            "Fetching available models...\ngemini-3.8-flash-low\tA\ngemini-3.8-flash-low\tB\n\n";
        assert_eq!(parse_models(noisy), vec!["gemini-3.8-flash-low"]);
    }

    #[test]
    fn models_parser_caps_at_the_bound() {
        let mut output = String::new();
        for index in 0..MAX_MODELS + 10 {
            output.push_str(&format!("slug-{index}\tDisplay {index}\n"));
        }
        let models = parse_models(&output);
        assert_eq!(models.len(), MAX_MODELS);
        assert_eq!(models.first().unwrap(), "slug-0");
        assert_eq!(models.last().unwrap(), &format!("slug-{}", MAX_MODELS - 1));
    }

    #[test]
    fn version_output_is_trimmed() {
        assert_eq!(parse_version("1.2.12\n"), "1.2.12");
        assert_eq!(parse_version("  1.2.12  \n"), "1.2.12");
        assert_eq!(parse_version("\n1.2.12\r\n"), "1.2.12");
        assert_eq!(parse_version(""), "");
    }
}
