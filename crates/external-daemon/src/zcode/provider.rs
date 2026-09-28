//! Personal provider rule generation and the zcode provider environment group.

use std::{
    env, fs, io,
    io::Write as _,
    path::{Path, PathBuf},
    process::Command,
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use serde_json::{json, Map, Value};

/// Builtin template file the child resolves as the first provider layer.
/// When the daemon already carries this variable it is respected verbatim;
/// otherwise the path is derived from the runtime location.
pub(crate) const BUILTIN_PROVIDER_CONFIG_ENV: &str = "ZCODE_BUILTIN_PROVIDER_CONFIG_FILE";
/// Bundled companion of [`BUILTIN_PROVIDER_CONFIG_ENV`]; both point at the
/// same template file.
pub(crate) const BUILTIN_PROVIDER_BUNDLED_CONFIG_ENV: &str =
    "ZCODE_BUILTIN_PROVIDER_BUNDLED_CONFIG_FILE";
/// Generated daemon-owned personal provider file.
pub(crate) const PERSONAL_PROVIDER_CONFIG_ENV: &str = "ZCODE_PERSONAL_PROVIDER_CONFIG_FILE";
/// v2 provider/auth/telemetry state root; deliberately not a storage or
/// session-db variable.
pub(crate) const DATA_BASE_DIR_ENV: &str = "ZCODE_DATA_BASE_DIR";

/// Directory under the daemon data root that holds the generated rules and
/// serves as the child's v2 state root.
const RUNTIME_DIR: &str = "zcode-runtime";
/// Stable name of the generated personal provider file.
const PERSONAL_PROVIDER_FILE: &str = "personal-provider-config.json";
/// Location of the builtin template relative to a ZCode `Resources` root.
const BUILTIN_RELATIVE: &str = "config/provider/zcode-builtin.json";

const ZAI_PROVIDER_ID: &str = "zai";
const ZAI_TEMPLATE_ID: &str = "zai-api";
const ZAI_PROVIDER_NAME: &str = "Z.ai Coding Plan";
const ZAI_ACCESS_TYPE: &str = "zhipu-coding-plan-api-key";
const ZAI_GROUP: &str = "standard-personal";
const SCHEMA_VERSION: u64 = 1;

/// Bound on the number of provider rules copied from the user's v2 config.
const MAX_PROVIDER_RULES: usize = 256;
/// Bound on the serialized generated file.
const MAX_SERIALIZED_BYTES: usize = 64 * 1024;
/// Guard against pathological template nesting during the zai template lookup.
const MAX_TEMPLATE_DEPTH: usize = 8;

/// The daemon data root is the parent directory of the store database path.
/// Shared by the production spawn and the probe so both derive the same
/// stable root.
pub fn data_root_for_store(store: &Path) -> Option<PathBuf> {
    store.parent().map(Path::to_path_buf)
}

/// The daemon data root derived from `EXTERNAL_SUBAGENT_STORE`, the same rule
/// applied by [`data_root_for_store`]; used by callers that only have the
/// environment.
pub fn data_root_from_environment() -> Option<PathBuf> {
    env::var_os("EXTERNAL_SUBAGENT_STORE")
        .map(PathBuf::from)
        .as_deref()
        .and_then(data_root_for_store)
}

/// Resolve the builtin provider template path for a zcode runtime.
///
/// An inherited `ZCODE_BUILTIN_PROVIDER_CONFIG_FILE` wins (the app may have
/// already pinned it); otherwise the runtime (`<Resources>/<dir>/zcode.cjs`)
/// is walked upwards and the first existing `config/provider/zcode-builtin.json`
/// is used. `None` means the template is unavailable and the whole provider
/// environment group must be skipped.
pub fn resolve_builtin_provider_config(runtime_path: &Path) -> Option<PathBuf> {
    if let Some(configured) = env::var_os(BUILTIN_PROVIDER_CONFIG_ENV) {
        let configured = PathBuf::from(configured);
        return configured.is_file().then_some(configured);
    }
    derive_builtin_provider_config(runtime_path)
}

/// Derive the builtin template from the runtime location alone, ignoring any
/// inherited override. The runtime lives at `<Resources>/<dir>/zcode.cjs`, so
/// the template is the first ancestor's `config/provider/zcode-builtin.json`.
pub(crate) fn derive_builtin_provider_config(runtime_path: &Path) -> Option<PathBuf> {
    runtime_path
        .ancestors()
        .map(|ancestor| ancestor.join(BUILTIN_RELATIVE))
        .find(|candidate| candidate.is_file())
}

/// Compose the daemon-owned personal provider file and write it 0600 (atomically)
/// into `output_dir`, returning the written path. Idempotent: regenerating from
/// the same inputs yields the same bytes.
///
/// Degradation is intentional and non-fatal for missing user inputs: an
/// unreadable CLI config or an absent zai key simply omits the zai rule, and an
/// unreadable v2 config omits the v2 layer. Only a malformed/unreadable builtin
/// template or a bound violation refuses generation; no partial file is ever
/// written.
pub fn generate_personal_provider_config(
    cli_config: &Path,
    v2_config: &Path,
    builtin_template: &Path,
    output_dir: &Path,
) -> io::Result<PathBuf> {
    // CLI credentials are best-effort: an absent, unreadable or malformed file
    // only omits the synthesized zai rule and never affects the v2 layer.
    let cli = match load_config(cli_config) {
        Ok(Some(value)) => Some(value),
        Ok(None) => {
            eprintln!(
                "external-subagent: zcode CLI provider config {cli_config:?} is absent; \
                 no zai rule is synthesized"
            );
            None
        }
        Err(error) => {
            eprintln!("external-subagent: zcode CLI provider config ignored: {error}");
            None
        }
    };
    // The v2 layer is the one this file replaces, so an existing-but-unusable
    // file must fail closed instead of silently masking the child's own
    // resolution of the real HOME v2 registry. A genuinely absent file (ENOENT)
    // is a faithful empty state and proceeds.
    let v2 = match load_config(v2_config) {
        Ok(value) => value,
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "zcode v2 provider config is unusable ({:?}): {error}",
                    error.kind()
                ),
            ));
        }
    };
    let builtin = match load_config(builtin_template) {
        Ok(Some(value)) => value,
        Ok(None) | Err(_) => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("zcode builtin provider template {builtin_template:?} is unreadable"),
            ));
        }
    };

    let mut rules = v2
        .as_ref()
        .and_then(|value| value.get("config"))
        .and_then(|config| config.pointer("/providerConfigRules/providerRules"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if rules.len() > MAX_PROVIDER_RULES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "zcode personal provider rules exceed the provider-rule bound",
        ));
    }

    // A user-defined zai rule is authoritative: never synthesize a second one
    // (and never overwrite the user's credential) and never re-append "zai" to
    // the provider order.
    let user_defined_zai = rules
        .iter()
        .any(|rule| rule.get("providerId").and_then(Value::as_str) == Some(ZAI_PROVIDER_ID));
    let mut zai_added = false;
    if user_defined_zai {
        eprintln!(
            "external-subagent: personal provider config already defines a zai rule; \
             keeping the user's definition"
        );
    } else if let Some(api_key) = cli_zai_api_key(cli.as_ref()) {
        let model_order = zai_model_order(
            cli.as_ref(),
            template_builtin_model_ids(&builtin, ZAI_TEMPLATE_ID)
                .as_deref()
                .unwrap_or(&[]),
        );
        rules.push(zai_rule(&api_key, model_order));
        zai_added = true;
    }

    // providerOrder authority is the user's v2 list verbatim; without one the
    // rule appearance order is used. "zai" is appended only when it is absent
    // (it is already part of the fallback order once the rule was added).
    let mut provider_order = v2
        .as_ref()
        .and_then(|value| value.get("config"))
        .and_then(|config| config.get("providerOrder"))
        .and_then(Value::as_array)
        .map(|order| {
            order
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| {
            rules
                .iter()
                .filter_map(|rule| rule.get("providerId").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        });
    if zai_added && !provider_order.iter().any(|id| id == ZAI_PROVIDER_ID) {
        provider_order.push(ZAI_PROVIDER_ID.to_owned());
    }

    let mut config = Map::new();
    config.insert(
        "providerConfigRules".into(),
        json!({ "providerRules": rules }),
    );
    if let Some(model_config_rules) = v2
        .as_ref()
        .and_then(|value| value.get("config"))
        .and_then(|config| config.get("modelConfigRules"))
        .cloned()
    {
        config.insert("modelConfigRules".into(), model_config_rules);
    }
    config.insert("providerOrder".into(), json!(provider_order));

    let root = json!({ "schemaVersion": SCHEMA_VERSION, "config": Value::Object(config) });
    let serialized = serde_json::to_vec(&root).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("zcode personal provider config could not be serialized: {error}"),
        )
    })?;
    if serialized.len() > MAX_SERIALIZED_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "zcode personal provider config exceeds the serialized byte bound",
        ));
    }

    ensure_private_directory(output_dir)?;
    let path = output_dir.join(PERSONAL_PROVIDER_FILE);
    write_private_atomic(&path, &serialized)?;
    Ok(path)
}

/// Inject the provider environment group into a zcode child command, or skip
/// the group as a whole. Derives the user config locations from `HOME` and the
/// builtin template from the runtime path (or an inherited override).
pub fn apply_provider_environment(command: &mut Command, runtime_path: &Path, data_root: &Path) {
    let Some(home) = env::var_os("HOME").map(PathBuf::from) else {
        eprintln!(
            "external-subagent: zcode provider materialization degraded: HOME is unavailable"
        );
        return;
    };
    let cli_config = home.join(".zcode/cli/config.json");
    let v2_config = home.join(".zcode/v2/provider_config.json");
    let builtin = resolve_builtin_provider_config(runtime_path);
    if let Err(error) = apply_provider_environment_for(
        command,
        builtin.as_deref(),
        data_root,
        &cli_config,
        &v2_config,
    ) {
        eprintln!("external-subagent: zcode provider materialization degraded: {error}");
    }
}

/// Explicit-input injection used by tests and by [`apply_provider_environment`].
///
/// The four variables are one atomic group: they are set only after the builtin
/// path is available and the personal file is generated, so a failure (including
/// a missing builtin template) leaves the command without any new ZCODE_
/// variable (today's behavior).
pub(crate) fn apply_provider_environment_for(
    command: &mut Command,
    builtin: Option<&Path>,
    data_root: &Path,
    cli_config: &Path,
    v2_config: &Path,
) -> io::Result<()> {
    let builtin = builtin.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "zcode builtin provider config is unavailable",
        )
    })?;
    let runtime_dir = data_root.join(RUNTIME_DIR);
    let generated =
        generate_personal_provider_config(cli_config, v2_config, builtin, &runtime_dir)?;
    command
        .env(BUILTIN_PROVIDER_CONFIG_ENV, builtin)
        .env(BUILTIN_PROVIDER_BUNDLED_CONFIG_ENV, builtin)
        .env(PERSONAL_PROVIDER_CONFIG_ENV, &generated)
        .env(DATA_BASE_DIR_ENV, &runtime_dir);
    Ok(())
}

fn cli_zai_api_key(cli: Option<&Value>) -> Option<String> {
    cli?.pointer("/provider/zai/options/apiKey")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
        .map(str::to_owned)
}

/// Template model IDs intersected with the CLI model keys, using the template's
/// exact spelling. Matching is case-insensitive; the iteration order follows
/// the parsed CLI config key order (serde_json's deterministic object order).
fn zai_model_order(cli: Option<&Value>, builtin_model_ids: &[String]) -> Vec<String> {
    let Some(models) = cli
        .and_then(|value| value.pointer("/provider/zai/models"))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };
    let mut order = Vec::new();
    for key in models.keys() {
        if let Some(id) = builtin_model_ids
            .iter()
            .find(|id| id.eq_ignore_ascii_case(key))
        {
            if !order.contains(id) {
                order.push(id.clone());
            }
        }
    }
    order
}

fn zai_rule(api_key: &str, model_order: Vec<String>) -> Value {
    json!({
        "providerId": ZAI_PROVIDER_ID,
        "templateId": ZAI_TEMPLATE_ID,
        "providerName": ZAI_PROVIDER_NAME,
        "config": {
            "group": ZAI_GROUP,
            "access": { "type": ZAI_ACCESS_TYPE, "apiKey": api_key },
            "personalModelIds": [],
            "modelOrder": model_order,
        }
    })
}

/// Find the builtin template with `template_id` and return its
/// `config.builtinModelIds`, wherever the template is nested. The builtin file
/// shape has varied across versions, so the lookup is shape-tolerant.
fn template_builtin_model_ids(builtin: &Value, template_id: &str) -> Option<Vec<String>> {
    let template = find_template(builtin, template_id, 0)?;
    let ids = template
        .get("config")?
        .get("builtinModelIds")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    Some(ids)
}

fn find_template<'a>(value: &'a Value, template_id: &str, depth: usize) -> Option<&'a Value> {
    if depth > MAX_TEMPLATE_DEPTH {
        return None;
    }
    match value {
        Value::Object(map) => {
            if map.get("templateId").and_then(Value::as_str) == Some(template_id) {
                return Some(value);
            }
            map.values()
                .find_map(|child| find_template(child, template_id, depth + 1))
        }
        Value::Array(items) => items
            .iter()
            .find_map(|child| find_template(child, template_id, depth + 1)),
        _ => None,
    }
}

/// Read a JSON config file, distinguishing genuine absence from a present but
/// unusable file: `Ok(None)` is ENOENT, `Err` is any other read error or a JSON
/// parse failure. The error text names the path and reason but never echoes the
/// file content.
fn load_config(path: &Path) -> io::Result<Option<Value>> {
    match fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => Ok(Some(value)),
            Err(error) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{path:?} is not valid JSON ({error})"),
            )),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("{path:?} could not be read ({error})"),
        )),
    }
}

/// Create `path` (and parents) and force the private 0700 mode. The directory
/// holds the plaintext key, so the mode is enforced on every call.
fn ensure_private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Write `bytes` to `path` through a same-directory temporary file, then
/// rename, so readers never observe a partial file. The file is 0600.
fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let directory = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "zcode personal provider config path has no parent directory",
        )
    })?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".zcode-personal-provider-")
        .tempfile_in(directory)?;
    temporary.as_file_mut().write_all(bytes)?;
    #[cfg(unix)]
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}
