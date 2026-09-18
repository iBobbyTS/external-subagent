use std::path::{Component, Path};

use super::{is_agent_metadata_path, is_credential_path};

macro_rules! define_agent_bash_command_families {
    ($($program:literal),+ $(,)?) => {
        /// The command families enforced by the Rust agent Bash policy and
        /// projected into task capability guidance.
        pub const AGENT_BASH_COMMAND_FAMILIES: &[&str] = &[$($program),+];

        pub(super) fn agent_bash_program_allowed(program: &str) -> bool {
            matches!(program, $($program)|+)
        }
    };
}

define_agent_bash_command_families!(
    "pwd", "ls", "stat", "wc", "head", "tail", "cat", "grep", "rg", "sed", "find", "git", "shasum",
    "cksum",
);

fn valid_agent_git_branch(args: &[String]) -> bool {
    if args.is_empty() || args == ["--show-current"] {
        return true;
    }
    let mut list_mode = false;
    for arg in args {
        match arg.as_str() {
            "--list" => list_mode = true,
            "--all" | "--remotes" | "--verbose" | "-a" | "-r" | "-v" | "-vv" => {}
            value if value.starts_with('-') => return false,
            _ if list_mode => {}
            _ => return false,
        }
    }
    true
}

fn unsafe_agent_git_object_path(arg: &str) -> bool {
    let Some((_, object_path)) = arg.split_once(':') else {
        return false;
    };
    let path = Path::new(object_path);
    object_path.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| component == Component::ParentDir)
        || is_credential_path(path)
        || is_agent_metadata_path(path)
}

fn safe_agent_git_operand(arg: &str) -> bool {
    !arg.is_empty()
        && !arg.starts_with('/')
        && !arg.starts_with('~')
        && !arg
            .split('/')
            .any(|component| component.is_empty() || component == "..")
        && !is_credential_path(Path::new(arg))
        && !is_agent_metadata_path(Path::new(arg))
        && !unsafe_agent_git_object_path(arg)
}

pub(super) fn valid_agent_git(args: &[String]) -> bool {
    let mut index = 0;
    if args.first().is_some_and(|arg| arg == "--no-pager") {
        index = 1;
    }
    let Some(subcommand) = args.get(index).map(String::as_str) else {
        return false;
    };
    let sub_args = &args[index + 1..];
    match subcommand {
        "status" => valid_agent_git_status(sub_args),
        "diff" | "show" | "log" => valid_agent_git_diff_like(subcommand, sub_args),
        "rev-parse" => valid_agent_git_rev_parse(sub_args),
        "cat-file" => valid_agent_git_cat_file(sub_args),
        "ls-files" => valid_agent_git_ls_files(sub_args),
        "branch" => valid_agent_git_branch(sub_args),
        _ => false,
    }
}

fn valid_agent_git_status(args: &[String]) -> bool {
    args.iter().all(|arg| {
        if arg.starts_with('-') {
            matches!(
                arg.as_str(),
                "--short"
                    | "-s"
                    | "--branch"
                    | "-b"
                    | "--porcelain"
                    | "--long"
                    | "--no-ahead-behind"
                    | "--show-stash"
                    | "--no-renames"
            ) || matches!(arg.strip_prefix("--porcelain="), Some("v1") | Some("v2"))
                || matches!(
                    arg.strip_prefix("--untracked-files="),
                    Some("no") | Some("normal") | Some("all")
                )
        } else {
            false
        }
    })
}

fn valid_agent_git_diff_like(subcommand: &str, args: &[String]) -> bool {
    let mut paths_mode = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            paths_mode = true;
            index += 1;
            continue;
        }
        if paths_mode {
            if !safe_agent_git_operand(arg) {
                return false;
            }
            index += 1;
            continue;
        }
        if matches!(
            arg.as_str(),
            "--output"
                | "--ext-diff"
                | "--textconv"
                | "--no-index"
                | "--show-signature"
                | "--exec"
                | "--stdin"
        ) || arg.starts_with("--output=")
            || arg.starts_with("--exec=")
            || arg.starts_with("--ext-diff=")
            || arg.starts_with("--textconv=")
            || arg.starts_with("--no-index=")
        {
            return false;
        }
        if arg.starts_with('-') {
            let allowed_boolean = match subcommand {
                "diff" => matches!(
                    arg.as_str(),
                    "--cached"
                        | "--staged"
                        | "--stat"
                        | "--shortstat"
                        | "--numstat"
                        | "--name-only"
                        | "--name-status"
                        | "--check"
                        | "--binary"
                        | "--full-index"
                        | "--no-color"
                        | "--minimal"
                        | "--patience"
                        | "--histogram"
                        | "--ignore-space-change"
                        | "--ignore-all-space"
                        | "--ignore-blank-lines"
                        | "--exit-code"
                        | "--quiet"
                        | "--find-renames"
                        | "--find-copies"
                        | "--relative"
                        | "--merge-base"
                        | "--no-renames"
                        | "--word-diff"
                        | "-p"
                        | "-s"
                ),
                "log" => matches!(
                    arg.as_str(),
                    "--oneline"
                        | "--graph"
                        | "--all"
                        | "--branches"
                        | "--tags"
                        | "--remotes"
                        | "--reverse"
                        | "--topo-order"
                        | "--date-order"
                        | "--author-date-order"
                        | "--first-parent"
                        | "--merges"
                        | "--no-merges"
                        | "--name-only"
                        | "--name-status"
                        | "--stat"
                        | "--shortstat"
                        | "--numstat"
                        | "--patch"
                        | "--no-patch"
                        | "--decorate"
                        | "--no-decorate"
                        | "--full-history"
                        | "--simplify-merges"
                        | "--dense"
                        | "--sparse"
                        | "--boundary"
                        | "--left-right"
                        | "--cherry-pick"
                        | "--cherry-mark"
                        | "--ancestry-path"
                        | "--bisect"
                        | "-p"
                ),
                "show" => matches!(
                    arg.as_str(),
                    "--stat"
                        | "--name-only"
                        | "--name-status"
                        | "--no-color"
                        | "--no-patch"
                        | "-s"
                        | "--oneline"
                        | "--decorate"
                        | "--no-decorate"
                ),
                _ => false,
            };
            if allowed_boolean {
                index += 1;
                continue;
            }
            if subcommand == "log" && (arg == "-n" || arg.starts_with("-n")) {
                let value =
                    if let Some(value) = arg.strip_prefix("-n").filter(|value| !value.is_empty()) {
                        value
                    } else {
                        let Some(value) = args.get(index + 1) else {
                            return false;
                        };
                        index += 1;
                        value.as_str()
                    };
                if !valid_positive_integer(value) {
                    return false;
                }
                index += 1;
                continue;
            }
            let (name, inline_value) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(name, value)| (name, Some(value)));
            let value_option = match subcommand {
                "diff" => matches!(
                    name,
                    "--unified"
                        | "--diff-filter"
                        | "--submodule"
                        | "--word-diff"
                        | "--word-diff-regex"
                        | "--find-renames"
                        | "--find-copies"
                        | "--color"
                        | "--src-prefix"
                        | "--dst-prefix"
                        | "--line-prefix"
                        | "--inter-hunk-context"
                ),
                "log" => matches!(
                    name,
                    "--max-count"
                        | "--skip"
                        | "--since"
                        | "--until"
                        | "--after"
                        | "--before"
                        | "--author"
                        | "--committer"
                        | "--grep"
                        | "--pretty"
                        | "--format"
                        | "--date"
                        | "--decorate"
                        | "--diff-filter"
                        | "--unified"
                        | "--color"
                ),
                "show" => matches!(name, "--format" | "--pretty" | "--color"),
                _ => false,
            };
            if !value_option {
                return false;
            }
            if let Some(value) = inline_value {
                if value.is_empty() {
                    return false;
                }
                if (matches!(name, "--max-count" | "--skip") && !valid_positive_integer(value))
                    || (matches!(name, "--unified" | "--inter-hunk-context")
                        && !valid_nonnegative_integer(value))
                {
                    return false;
                }
                index += 1;
            } else {
                let Some(value) = args.get(index + 1) else {
                    return false;
                };
                if value.starts_with('-') || value.is_empty() {
                    return false;
                }
                if (matches!(name, "--max-count" | "--skip") && !valid_positive_integer(value))
                    || (matches!(name, "--unified" | "--inter-hunk-context")
                        && !valid_nonnegative_integer(value))
                {
                    return false;
                }
                index += 2;
            }
            continue;
        }
        if !safe_agent_git_operand(arg) {
            return false;
        }
        index += 1;
    }
    true
}

fn valid_agent_git_rev_parse(args: &[String]) -> bool {
    if args.is_empty() {
        return false;
    }
    args.iter().all(|arg| {
        if arg.starts_with('-') {
            matches!(
                arg.as_str(),
                "--verify"
                    | "--quiet"
                    | "-q"
                    | "--show-toplevel"
                    | "--show-prefix"
                    | "--is-inside-work-tree"
                    | "--is-bare-repository"
                    | "--is-shallow-repository"
                    | "--show-object-format"
            ) || arg
                .strip_prefix("--short=")
                .is_some_and(|value| value.chars().all(|c| c.is_ascii_digit()))
                || arg == "--short"
        } else {
            safe_agent_git_operand(arg)
        }
    })
}

fn valid_agent_git_cat_file(args: &[String]) -> bool {
    args.len() == 2
        && matches!(args[0].as_str(), "-e" | "-p" | "-t" | "-s")
        && safe_agent_git_operand(&args[1])
}

fn valid_agent_git_ls_files(args: &[String]) -> bool {
    let mut paths_mode = false;
    for arg in args {
        if arg == "--" {
            paths_mode = true;
            continue;
        }
        if paths_mode {
            if !safe_agent_git_operand(arg) {
                return false;
            }
            continue;
        }
        if matches!(
            arg.as_str(),
            "--cached"
                | "--deleted"
                | "--modified"
                | "--others"
                | "--ignored"
                | "--stage"
                | "--unmerged"
                | "--killed"
                | "--directory"
                | "--empty-directory"
                | "--full-name"
                | "--error-unmatch"
                | "--deduplicate"
                | "-c"
                | "-d"
                | "-m"
                | "-o"
                | "-i"
                | "-u"
                | "-s"
                | "-t"
                | "-k"
        ) {
            continue;
        }
        return false;
    }
    true
}

/// Return only operands that the selected command grammar defines as paths.
///
/// This intentionally does not guess from whether an arbitrary token happens
/// to exist: search patterns, revisions and option values are not paths, while
/// every actual path operand is subsequently canonicalized fail-closed.
pub(super) fn agent_bash_path_operands(program: &str, args: &[String]) -> Option<Vec<String>> {
    match program {
        "pwd" => args
            .iter()
            .all(|arg| matches!(arg.as_str(), "-L" | "-P"))
            .then(Vec::new),
        "ls" => simple_path_operands(
            args,
            &[
                "-a",
                "-A",
                "-l",
                "-d",
                "-h",
                "-n",
                "-i",
                "-1",
                "-G",
                "--all",
                "--almost-all",
                "--directory",
                "--human-readable",
                "--inode",
                "--numeric-uid-gid",
                "--color=never",
                "--group-directories-first",
            ],
            &["--time-style"],
        ),
        "stat" => simple_path_operands(
            args,
            &["-L", "-x", "--dereference", "--file-system", "--terse"],
            &["-f", "-c", "--format", "--printf"],
        ),
        "wc" => required_path_operands(
            args,
            &[
                "-c",
                "-l",
                "-m",
                "-w",
                "-L",
                "--bytes",
                "--chars",
                "--lines",
                "--max-line-length",
                "--words",
            ],
            &[],
        ),
        "head" | "tail" => head_tail_path_operands(program, args),
        "cat" => required_path_operands(
            args,
            &[
                "-A",
                "-b",
                "-E",
                "-n",
                "-s",
                "-t",
                "-T",
                "-u",
                "-v",
                "--number",
                "--number-nonblank",
                "--show-all",
                "--show-ends",
                "--show-tabs",
                "--squeeze-blank",
            ],
            &[],
        ),
        "grep" => grep_like_path_operands(args, false),
        "rg" => grep_like_path_operands(args, true),
        "sed" => valid_agent_sed(args).then(|| args[2..].to_vec()),
        "find" => valid_agent_find(args),
        "git" => git_path_operands(args),
        "shasum" | "cksum" => required_path_operands(
            args,
            &["-b", "-p", "-t", "-U", "-0", "--tag", "-l"],
            &["-a", "--algorithm", "--length"],
        ),
        _ => None,
    }
}

fn required_path_operands(
    args: &[String],
    boolean_options: &[&str],
    value_options: &[&str],
) -> Option<Vec<String>> {
    let paths = simple_path_operands(args, boolean_options, value_options)?;
    (!paths.is_empty()).then_some(paths)
}

fn simple_path_operands(
    args: &[String],
    boolean_options: &[&str],
    value_options: &[&str],
) -> Option<Vec<String>> {
    let mut paths = Vec::new();
    let mut index = 0;
    let mut options_ended = false;
    while index < args.len() {
        let arg = &args[index];
        if arg == "-" {
            return None;
        }
        if !options_ended && arg == "--" {
            options_ended = true;
        } else if !options_ended
            && (boolean_options.contains(&arg.as_str())
                || value_options
                    .iter()
                    .any(|option| arg.starts_with(&format!("{option}="))))
        {
        } else if !options_ended && value_options.contains(&arg.as_str()) {
            index += 1;
            if index >= args.len() {
                return None;
            }
        } else if !options_ended && arg == "-" {
            paths.push(arg.clone());
        } else if !options_ended && arg.starts_with('-') {
            return None;
        } else {
            paths.push(arg.clone());
        }
        index += 1;
    }
    Some(paths)
}

fn head_tail_path_operands(program: &str, args: &[String]) -> Option<Vec<String>> {
    let mut paths = Vec::new();
    let mut index = 0;
    let mut options_ended = false;
    while index < args.len() {
        let arg = &args[index];
        if !options_ended && arg == "--" {
            options_ended = true;
        } else if !options_ended
            && matches!(
                arg.as_str(),
                "-q" | "-v" | "--quiet" | "--silent" | "--verbose"
            )
        {
        } else if !options_ended
            && program == "tail"
            && matches!(arg.as_str(), "-f" | "-F" | "--follow" | "--retry")
        {
            return None;
        } else if !options_ended
            && (arg.strip_prefix('-').is_some_and(|value| {
                !value.is_empty() && value.chars().all(|character| character.is_ascii_digit())
            }) || ["-n", "--lines", "-c", "--bytes"]
                .iter()
                .any(|option| arg.starts_with(&format!("{option}="))))
        {
        } else if !options_ended && matches!(arg.as_str(), "-n" | "--lines" | "-c" | "--bytes") {
            index += 1;
            if index >= args.len() {
                return None;
            }
        } else if !options_ended && arg.starts_with('-') {
            return None;
        } else {
            paths.push(arg.clone());
        }
        index += 1;
    }
    (!paths.is_empty()).then_some(paths)
}

fn grep_like_path_operands(args: &[String], ripgrep: bool) -> Option<Vec<String>> {
    let boolean_short = if ripgrep {
        "nHhIiSsFUlcvqwo"
    } else {
        "HhIiLnqsvEFPowx"
    };
    let value_short = if ripgrep { "egtmABC" } else { "efmABC" };
    let boolean_long: &[&str] = if ripgrep {
        &[
            "--line-number",
            "--with-filename",
            "--no-filename",
            "--ignore-case",
            "--case-sensitive",
            "--smart-case",
            "--fixed-strings",
            "--files",
            "--files-with-matches",
            "--files-without-match",
            "--count",
            "--count-matches",
            "--only-matching",
            "--word-regexp",
            "--line-regexp",
            "--json",
            "--stats",
            "--heading",
            "--no-heading",
            "--column",
            "--pcre2",
            "--color=never",
        ]
    } else {
        &[
            "--with-filename",
            "--no-filename",
            "--ignore-case",
            "--no-messages",
            "--invert-match",
            "--line-number",
            "--files-with-matches",
            "--files-without-match",
            "--count",
            "--only-matching",
            "--word-regexp",
            "--line-regexp",
            "--fixed-strings",
            "--extended-regexp",
            "--perl-regexp",
            "--binary-files=without-match",
            "--binary-files=text",
            "--color=never",
        ]
    };
    let value_long: &[&str] = if ripgrep {
        &[
            "--regexp",
            "--glob",
            "--type",
            "--type-not",
            "--max-count",
            "--after-context",
            "--before-context",
            "--context",
            "--file",
            "--ignore-file",
            "--encoding",
            "--sort",
            "--sortr",
        ]
    } else {
        &[
            "--regexp",
            "--file",
            "--max-count",
            "--after-context",
            "--before-context",
            "--context",
            "--include",
            "--exclude",
            "--exclude-dir",
            "--exclude-from",
            "--directories",
            "--devices",
        ]
    };
    let mut paths = Vec::new();
    let mut pattern_specified = false;
    let mut files_mode = false;
    let mut options_ended = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if !options_ended && arg == "--" {
            options_ended = true;
        } else if !options_ended
            && ripgrep
            && [
                "--pre",
                "--pre-glob",
                "--hostname-bin",
                "--search-zip",
                "--follow",
            ]
            .iter()
            .any(|option| arg == option || arg.starts_with(&format!("{option}=")))
        {
            return None;
        } else if !options_ended && arg.starts_with("--") {
            if boolean_long.contains(&arg.as_str()) {
                files_mode |= arg == "--files";
            } else {
                let option = arg.split_once('=').map_or(arg.as_str(), |(name, _)| name);
                if !value_long.contains(&option) {
                    return None;
                }
                let inline_value = arg.contains('=');
                if !inline_value {
                    index += 1;
                    if index >= args.len() {
                        return None;
                    }
                }
                if matches!(option, "--regexp" | "--file") {
                    pattern_specified = true;
                }
                if matches!(option, "--file" | "--ignore-file" | "--exclude-from") {
                    let value = arg
                        .split_once('=')
                        .map_or(args[index].as_str(), |(_, value)| value);
                    paths.push(value.to_owned());
                }
            }
        } else if !options_ended && arg == "-" {
            paths.push(arg.clone());
        } else if !options_ended && arg.starts_with('-') {
            let cluster = &arg[1..];
            if cluster.len() == 1 && value_short.contains(cluster) {
                index += 1;
                if index >= args.len() {
                    return None;
                }
                if cluster == "e" || cluster == "f" {
                    pattern_specified = true;
                }
                if cluster == "f" {
                    paths.push(args[index].clone());
                }
            } else if !cluster.chars().all(|flag| boolean_short.contains(flag)) {
                return None;
            }
        } else if files_mode || pattern_specified {
            paths.push(arg.clone());
        } else {
            pattern_specified = true;
        }
        index += 1;
    }
    if (!files_mode && !pattern_specified) || (!ripgrep && paths.is_empty()) {
        None
    } else {
        Some(paths)
    }
}

fn git_path_operands(args: &[String]) -> Option<Vec<String>> {
    let command_index = args
        .iter()
        .position(|arg| arg == "--no-pager")
        .map_or(0, |_| 1);
    let subcommand = args.get(command_index)?.as_str();
    let operands = &args[command_index + 1..];
    if let Some(separator) = operands.iter().position(|arg| arg == "--") {
        return Some(operands[separator + 1..].to_vec());
    }
    let _ = subcommand;
    Some(Vec::new())
}

fn valid_agent_find(args: &[String]) -> Option<Vec<String>> {
    let mut paths = Vec::new();
    let mut index = 0;
    while index < args.len() && !args[index].starts_with('-') {
        paths.push(args[index].clone());
        index += 1;
    }
    if paths.is_empty() {
        return None;
    }
    while index < args.len() {
        match args[index].as_str() {
            "-L" | "-H" | "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir" | "-prune"
            | "-ls" | "-print" | "-print0" | "-fprint" | "-fprint0" | "-fprintf" | "-fls"
            | "-printf" | "-writable" => return None,
            "-maxdepth" | "-mindepth" => {
                let value = args.get(index + 1)?;
                if value.parse::<u8>().ok().is_none_or(|value| value > 50) {
                    return None;
                }
                index += 2;
            }
            "-type" => {
                let value = args.get(index + 1)?;
                if value.len() != 1
                    || !matches!(
                        value.as_bytes()[0],
                        b'f' | b'd' | b'l' | b'p' | b's' | b'b' | b'c'
                    )
                {
                    return None;
                }
                index += 2;
            }
            "-name" | "-iname" | "-path" | "-ipath" | "-size" | "-mtime" | "-mmin" => {
                let value = args.get(index + 1)?;
                if value.is_empty() || value.starts_with('-') {
                    return None;
                }
                index += 2;
            }
            "-newer" => {
                let value = args.get(index + 1)?;
                if value.is_empty() {
                    return None;
                }
                paths.push(value.clone());
                index += 2;
            }
            "-empty" | "-readable" | "-quit" => index += 1,
            _ => return None,
        }
    }
    Some(paths)
}

fn valid_positive_integer(value: &str) -> bool {
    value.parse::<u32>().is_ok_and(|value| value > 0)
}

fn valid_nonnegative_integer(value: &str) -> bool {
    value.parse::<u16>().is_ok()
}

pub(super) fn tokenize_agent_bash(command: &str) -> Option<Vec<String>> {
    if command.is_empty()
        || command.len() > 16 * 1024
        || command.chars().any(|c| matches!(c, '\n' | '\r'))
    {
        return None;
    }
    let mut result = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for character in command.chars() {
        match quote {
            Some(delimiter) if character == delimiter => quote = None,
            Some(_) => current.push(character),
            None if character == '\'' || character == '"' => quote = Some(character),
            None if character.is_whitespace() => {
                if !current.is_empty() {
                    result.push(std::mem::take(&mut current));
                }
            }
            None if matches!(
                character,
                ';' | '&'
                    | '|'
                    | '>'
                    | '<'
                    | '$'
                    | '`'
                    | '('
                    | ')'
                    | '{'
                    | '}'
                    | '*'
                    | '?'
                    | '['
                    | ']'
                    | '#'
                    | '!'
                    | '\\'
            ) =>
            {
                return None
            }
            None => current.push(character),
        }
    }
    if quote.is_some() {
        return None;
    }
    if !current.is_empty() {
        result.push(current);
    }
    if result.len() > 128 {
        return None;
    }
    (!result.is_empty()).then_some(result)
}

pub(super) fn valid_agent_sed(args: &[String]) -> bool {
    if args.len() < 3 || args[0] != "-n" {
        return false;
    }
    let range = &args[1];
    let valid_number = |value: &str| {
        !value.is_empty()
            && value.chars().all(|c| c.is_ascii_digit())
            && value
                .parse::<u64>()
                .ok()
                .is_some_and(|number| number <= 10_000)
    };
    let mut pieces = range.strip_suffix('p').unwrap_or("").split(',');
    let first = pieces.next().unwrap_or("");
    let second = pieces.next();
    if pieces.next().is_some()
        || !valid_number(first)
        || second.is_some_and(|value| !valid_number(value))
    {
        return false;
    }
    args[2..].iter().all(|arg| !arg.starts_with('-'))
}
