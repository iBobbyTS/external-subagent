//! Unit coverage for personal provider materialization: generated shape,
//! degradation paths, the atomic environment group and key redaction.

use std::{fs, path::Path, process::Command};

use serde_json::{json, Value};

use super::provider::{
    apply_provider_environment_for, derive_builtin_provider_config,
    generate_personal_provider_config, BUILTIN_PROVIDER_BUNDLED_CONFIG_ENV,
    BUILTIN_PROVIDER_CONFIG_ENV, DATA_BASE_DIR_ENV, PERSONAL_PROVIDER_CONFIG_ENV,
};

const ZAI_KEY: &str = "fixture-zai-coding-plan-key";

fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

/// Synthetic CLI config mirroring `~/.zcode/cli/config.json`: a zai coding-plan
/// key plus three model keys, one of which (`glm-5.1`) has no builtin template
/// entry and must be excluded from the model order.
fn cli_config(key: Option<&str>) -> Value {
    let mut zai = json!({
        "models": {
            "glm-5.3": {"enabled": true},
            "glm-5.3-flash": {"enabled": true},
            "glm-5.1": {"enabled": true},
        },
        "options": {"baseURL": "https://api.z.ai"},
    });
    if let Some(key) = key {
        zai["options"]["apiKey"] = json!(key);
    }
    json!({"provider": {"zai": zai}})
}

/// Synthetic builtin template bundle with the zai and deepseek templates.
fn builtin_config() -> Value {
    json!({
        "config": {
            "providerConfigRules": {
                "providerRules": [
                    {"providerId": "zai", "templateId": "zai-api", "config": {"builtinModelIds": ["GLM-5.3", "GLM-5.3-Flash"]}},
                    {"providerId": "deepseek", "templateId": "deepseek", "config": {"builtinModelIds": ["deepseek-flash", "deepseek-v4-pro"]}},
                ]
            }
        }
    })
}

fn v2_config(rules: Value, order: Value, model_config_rules: Value) -> Value {
    json!({
        "config": {
            "providerConfigRules": {"providerRules": rules},
            "modelConfigRules": model_config_rules,
            "providerOrder": order,
        }
    })
}

fn env_of(command: &Command, name: &str) -> Option<String> {
    command
        .get_envs()
        .find(|(key, _)| key.to_string_lossy().as_ref() == name)
        .and_then(|(_, value)| value.map(|value| value.to_string_lossy().into_owned()))
}

fn deepseek_rule() -> Value {
    json!({
        "providerId": "deepseek",
        "templateId": "deepseek",
        "providerName": "DeepSeek",
        "config": {"personalModelIds": ["deepseek-flash"], "baseUrl": "https://api.deepseek"}
    })
}

// ① shape: the v2 layer is copied verbatim, the zai rule is synthesized with
// the builtin/template intersection, and the disabled model stays disabled.
#[test]
fn personal_config_copies_v2_verbatim_and_adds_the_zai_rule() {
    let directory = tempfile::tempdir().unwrap();
    let cli = directory.path().join("cli.json");
    let v2 = directory.path().join("v2.json");
    let builtin = directory.path().join("builtin.json");
    let output = directory.path().join("out");
    write_json(&cli, &cli_config(Some(ZAI_KEY)));
    let model_config_rules = json!({"providerModelRules": [{"providerId": "deepseek", "modelId": "deepseek-v4-pro", "enabled": false}]});
    write_json(
        &v2,
        &v2_config(
            json!([deepseek_rule()]),
            json!(["deepseek"]),
            model_config_rules.clone(),
        ),
    );
    write_json(&builtin, &builtin_config());

    let written = generate_personal_provider_config(&cli, &v2, &builtin, &output).unwrap();
    assert_eq!(written, output.join("personal-provider-config.json"));
    let generated: Value = serde_json::from_slice(&fs::read(&written).unwrap()).unwrap();

    assert_eq!(generated["schemaVersion"], json!(1));
    assert_eq!(
        generated["config"]["providerOrder"],
        json!(["deepseek", "zai"])
    );
    let rules = generated["config"]["providerConfigRules"]["providerRules"]
        .as_array()
        .unwrap();
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0]["providerId"], json!("deepseek"));
    let zai = &rules[1];
    assert_eq!(zai["providerId"], json!("zai"));
    assert_eq!(zai["templateId"], json!("zai-api"));
    assert_eq!(zai["providerName"], json!("Z.ai Coding Plan"));
    assert_eq!(zai["config"]["group"], json!("standard-personal"));
    assert_eq!(
        zai["config"]["access"]["type"],
        json!("zhipu-coding-plan-api-key")
    );
    assert_eq!(zai["config"]["access"]["apiKey"], json!(ZAI_KEY));
    assert_eq!(zai["config"]["personalModelIds"], json!([]));
    assert_eq!(
        zai["config"]["modelOrder"],
        json!(["GLM-5.3", "GLM-5.3-Flash"])
    );
    // modelConfigRules is copied byte-for-byte; the user's disable survives.
    assert_eq!(
        serde_json::to_string(&generated["config"]["modelConfigRules"]).unwrap(),
        serde_json::to_string(&model_config_rules).unwrap()
    );
    assert!(generated.to_string().contains("\"enabled\":false"));
}

// ① authority: providerOrder comes from the v2 list, not the rule appearance
// order, with "zai" appended once.
#[test]
fn provider_order_authority_wins_over_rule_appearance_order() {
    let directory = tempfile::tempdir().unwrap();
    let cli = directory.path().join("cli.json");
    let v2 = directory.path().join("v2.json");
    let builtin = directory.path().join("builtin.json");
    let output = directory.path().join("out");
    write_json(&cli, &cli_config(Some(ZAI_KEY)));
    let acme = json!({"providerId": "acme", "templateId": "acme"});
    write_json(
        &v2,
        &v2_config(
            json!([deepseek_rule(), acme]),
            json!(["acme", "deepseek"]),
            json!({}),
        ),
    );
    write_json(&builtin, &builtin_config());

    let written = generate_personal_provider_config(&cli, &v2, &builtin, &output).unwrap();
    let generated: Value = serde_json::from_slice(&fs::read(&written).unwrap()).unwrap();
    assert_eq!(
        generated["config"]["providerOrder"],
        json!(["acme", "deepseek", "zai"])
    );
    let rules = generated["config"]["providerConfigRules"]["providerRules"]
        .as_array()
        .unwrap();
    let ids: Vec<&str> = rules
        .iter()
        .map(|rule| rule["providerId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["deepseek", "acme", "zai"]);
}

// ② missing zai key: no zai rule, v2 layer and order unchanged.
#[test]
fn a_missing_zai_key_omits_the_rule_and_keeps_the_v2_layer() {
    let directory = tempfile::tempdir().unwrap();
    let cli = directory.path().join("cli.json");
    let v2 = directory.path().join("v2.json");
    let builtin = directory.path().join("builtin.json");
    let output = directory.path().join("out");
    write_json(&cli, &cli_config(None));
    write_json(
        &v2,
        &v2_config(json!([deepseek_rule()]), json!(["deepseek"]), json!({})),
    );
    write_json(&builtin, &builtin_config());

    let written = generate_personal_provider_config(&cli, &v2, &builtin, &output).unwrap();
    let generated: Value = serde_json::from_slice(&fs::read(&written).unwrap()).unwrap();
    let rules = generated["config"]["providerConfigRules"]["providerRules"]
        .as_array()
        .unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0]["providerId"], json!("deepseek"));
    assert_eq!(generated["config"]["providerOrder"], json!(["deepseek"]));
}

// ② missing v2: only the zai rule remains and the order flips to ["zai"].
#[test]
fn a_missing_v2_config_leaves_only_the_zai_rule_and_a_zai_order() {
    let directory = tempfile::tempdir().unwrap();
    let cli = directory.path().join("cli.json");
    let builtin = directory.path().join("builtin.json");
    let output = directory.path().join("out");
    write_json(&cli, &cli_config(Some(ZAI_KEY)));
    write_json(&builtin, &builtin_config());
    let missing_v2 = directory.path().join("absent-v2.json");

    let written = generate_personal_provider_config(&cli, &missing_v2, &builtin, &output).unwrap();
    let generated: Value = serde_json::from_slice(&fs::read(&written).unwrap()).unwrap();
    let rules = generated["config"]["providerConfigRules"]["providerRules"]
        .as_array()
        .unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0]["providerId"], json!("zai"));
    assert_eq!(generated["config"]["providerOrder"], json!(["zai"]));
    assert!(generated["config"].get("modelConfigRules").is_none());
}

// ② user-defined zai rule: kept verbatim, never duplicated, order untouched.
#[test]
fn a_user_defined_zai_rule_is_kept_verbatim_without_duplication() {
    let directory = tempfile::tempdir().unwrap();
    let cli = directory.path().join("cli.json");
    let v2 = directory.path().join("v2.json");
    let builtin = directory.path().join("builtin.json");
    let output = directory.path().join("out");
    write_json(&cli, &cli_config(Some(ZAI_KEY)));
    let user_zai = json!({
        "providerId": "zai",
        "templateId": "zai-api",
        "providerName": "User Zai",
        "config": {"access": {"type": "zhipu-coding-plan-api-key", "apiKey": "user-owned-key"}}
    });
    write_json(
        &v2,
        &v2_config(
            json!([user_zai, deepseek_rule()]),
            json!(["zai", "deepseek"]),
            json!({}),
        ),
    );
    write_json(&builtin, &builtin_config());

    let written = generate_personal_provider_config(&cli, &v2, &builtin, &output).unwrap();
    let generated: Value = serde_json::from_slice(&fs::read(&written).unwrap()).unwrap();
    let rules = generated["config"]["providerConfigRules"]["providerRules"]
        .as_array()
        .unwrap();
    assert_eq!(rules.len(), 2);
    assert_eq!(
        rules[0]["config"]["access"]["apiKey"],
        json!("user-owned-key")
    );
    assert_eq!(rules[1]["providerId"], json!("deepseek"));
    assert_eq!(
        generated["config"]["providerOrder"],
        json!(["zai", "deepseek"])
    );
}

// ②③ missing builtin: the whole four-variable group is skipped and nothing is
// created on disk.
#[test]
fn a_missing_builtin_degrades_the_whole_environment_group() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("runtime/zcode.cjs");
    fs::create_dir_all(runtime.parent().unwrap()).unwrap();
    fs::write(&runtime, b"fixture").unwrap();
    assert_eq!(derive_builtin_provider_config(&runtime), None);
    let cli = directory.path().join("cli.json");
    let v2 = directory.path().join("v2.json");
    write_json(&cli, &cli_config(Some(ZAI_KEY)));
    write_json(
        &v2,
        &v2_config(json!([deepseek_rule()]), json!(["deepseek"]), json!({})),
    );
    let data_root = directory.path().join("data");

    let mut command = Command::new("zcode-fixture");
    let error =
        apply_provider_environment_for(&mut command, None, &data_root, &cli, &v2).unwrap_err();
    assert!(!error.to_string().contains(ZAI_KEY));
    assert_eq!(command.get_envs().count(), 0);
    assert!(!data_root.join("zcode-runtime").exists());
}

// ③ normal path: four variables set, private modes enforced, regeneration is
// idempotent.
#[test]
fn the_environment_group_is_injected_with_private_paths() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("Resources/app/zcode.cjs");
    fs::create_dir_all(runtime.parent().unwrap()).unwrap();
    fs::write(&runtime, b"fixture").unwrap();
    let builtin = directory
        .path()
        .join("Resources/config/provider/zcode-builtin.json");
    fs::create_dir_all(builtin.parent().unwrap()).unwrap();
    write_json(&builtin, &builtin_config());
    let cli = directory.path().join("cli.json");
    let v2 = directory.path().join("v2.json");
    write_json(&cli, &cli_config(Some(ZAI_KEY)));
    write_json(
        &v2,
        &v2_config(json!([deepseek_rule()]), json!(["deepseek"]), json!({})),
    );
    let data_root = directory.path().join("data");
    assert_eq!(
        derive_builtin_provider_config(&runtime),
        Some(builtin.clone()),
        "the builtin template is derived from the runtime ancestors"
    );

    let mut command = Command::new("zcode-fixture");
    apply_provider_environment_for(&mut command, Some(&builtin), &data_root, &cli, &v2).unwrap();

    let runtime_dir = data_root.join("zcode-runtime");
    assert_eq!(
        env_of(&command, BUILTIN_PROVIDER_CONFIG_ENV),
        Some(builtin.to_string_lossy().into_owned())
    );
    assert_eq!(
        env_of(&command, BUILTIN_PROVIDER_BUNDLED_CONFIG_ENV),
        Some(builtin.to_string_lossy().into_owned())
    );
    assert_eq!(
        env_of(&command, PERSONAL_PROVIDER_CONFIG_ENV),
        Some(
            runtime_dir
                .join("personal-provider-config.json")
                .to_string_lossy()
                .into_owned()
        )
    );
    assert_eq!(
        env_of(&command, DATA_BASE_DIR_ENV),
        Some(runtime_dir.to_string_lossy().into_owned())
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let directory_mode = fs::metadata(&runtime_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(directory_mode, 0o700);
        let file_mode = fs::metadata(runtime_dir.join("personal-provider-config.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
    }

    // Idempotent regeneration over the same inputs.
    let first = fs::read(runtime_dir.join("personal-provider-config.json")).unwrap();
    let mut second_command = Command::new("zcode-fixture");
    apply_provider_environment_for(&mut second_command, Some(&builtin), &data_root, &cli, &v2)
        .unwrap();
    let second = fs::read(runtime_dir.join("personal-provider-config.json")).unwrap();
    assert_eq!(first, second);
}

// ④ every generator refusal path reports an error without echoing the key.
#[test]
fn generator_error_paths_never_echo_the_api_key() {
    let directory = tempfile::tempdir().unwrap();
    let cli = directory.path().join("cli.json");
    write_json(&cli, &cli_config(Some(ZAI_KEY)));
    let output = directory.path().join("out");
    let builtin = directory.path().join("builtin.json");
    write_json(&builtin, &builtin_config());

    // Unreadable builtin template.
    let missing_builtin = directory.path().join("absent-builtin.json");
    let empty_v2 = directory.path().join("empty-v2.json");
    write_json(&empty_v2, &v2_config(json!([]), json!([]), json!({})));
    let error =
        generate_personal_provider_config(&cli, &empty_v2, &missing_builtin, &output).unwrap_err();
    assert!(!error.to_string().contains(ZAI_KEY));

    // Provider-rule bound.
    let rules: Vec<Value> = (0..257)
        .map(|index| json!({"providerId": format!("provider-{index}")}))
        .collect();
    let bounded_v2 = directory.path().join("bounded-v2.json");
    write_json(
        &bounded_v2,
        &v2_config(Value::Array(rules), json!([]), json!({})),
    );
    let error =
        generate_personal_provider_config(&cli, &bounded_v2, &builtin, &output).unwrap_err();
    assert!(!error.to_string().contains(ZAI_KEY));

    // Serialized byte bound.
    let oversized_v2 = directory.path().join("oversized-v2.json");
    write_json(
        &oversized_v2,
        &v2_config(
            json!([{"providerId": "big", "blob": "x".repeat(70 * 1024)}]),
            json!(["big"]),
            json!({}),
        ),
    );
    let error =
        generate_personal_provider_config(&cli, &oversized_v2, &builtin, &output).unwrap_err();
    assert!(!error.to_string().contains(ZAI_KEY));

    // Output path cannot be created as a private directory.
    let blocked = directory.path().join("blocked");
    fs::write(&blocked, b"not a directory").unwrap();
    let error = generate_personal_provider_config(&cli, &empty_v2, &builtin, &blocked).unwrap_err();
    assert!(!error.to_string().contains(ZAI_KEY));
}

// ② strict v2 distinction: an existing-but-unreadable v2 file fails generation
// closed, so the environment group is skipped and the child keeps resolving the
// real HOME v2 registry instead of a silently masked one.
#[test]
fn an_unreadable_v2_config_degrades_the_environment_group() {
    let directory = tempfile::tempdir().unwrap();
    let cli = directory.path().join("cli.json");
    let v2 = directory.path().join("v2.json");
    let builtin = directory.path().join("builtin.json");
    let output = directory.path().join("out");
    write_json(&cli, &cli_config(Some(ZAI_KEY)));
    write_json(&builtin, &builtin_config());
    write_json(
        &v2,
        &v2_config(json!([deepseek_rule()]), json!(["deepseek"]), json!({})),
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&v2, fs::Permissions::from_mode(0o000)).unwrap();
    }
    // A privileged runner bypasses the 000 mode; a directory is equally an
    // existing-but-unreadable config path and keeps the assertion decidable.
    if fs::read(&v2).is_ok() {
        fs::remove_file(&v2).unwrap();
        fs::create_dir(&v2).unwrap();
    }

    let error = generate_personal_provider_config(&cli, &v2, &builtin, &output).unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("v2.json"),
        "diagnostic must name the unreadable path: {message}"
    );
    assert!(!message.contains(ZAI_KEY));

    let data_root = directory.path().join("data");
    let mut command = Command::new("zcode-fixture");
    let error = apply_provider_environment_for(&mut command, Some(&builtin), &data_root, &cli, &v2)
        .unwrap_err();
    assert!(!error.to_string().contains(ZAI_KEY));
    assert_eq!(command.get_envs().count(), 0);
    assert!(!data_root.join("zcode-runtime").exists());
}

// ② strict v2 distinction: a present but malformed v2 file degrades the whole
// group rather than masking the real registry with a partial copy.
#[test]
fn a_malformed_v2_config_degrades_the_environment_group() {
    let directory = tempfile::tempdir().unwrap();
    let cli = directory.path().join("cli.json");
    let v2 = directory.path().join("v2.json");
    let builtin = directory.path().join("builtin.json");
    let output = directory.path().join("out");
    write_json(&cli, &cli_config(Some(ZAI_KEY)));
    write_json(&builtin, &builtin_config());
    fs::write(&v2, b"{\"config\": {").unwrap();

    let error = generate_personal_provider_config(&cli, &v2, &builtin, &output).unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("v2.json"),
        "diagnostic must name the malformed path: {message}"
    );
    assert!(!message.contains(ZAI_KEY));

    let data_root = directory.path().join("data");
    let mut command = Command::new("zcode-fixture");
    let error = apply_provider_environment_for(&mut command, Some(&builtin), &data_root, &cli, &v2)
        .unwrap_err();
    assert!(!error.to_string().contains(ZAI_KEY));
    assert_eq!(command.get_envs().count(), 0);
    assert!(!data_root.join("zcode-runtime").exists());
}

// ② an absent v2 file is a faithful empty state: zai-only generation still
// injects the group.
#[test]
fn an_absent_v2_config_still_injects_the_environment_group() {
    let directory = tempfile::tempdir().unwrap();
    let cli = directory.path().join("cli.json");
    let builtin = directory.path().join("builtin.json");
    write_json(&cli, &cli_config(Some(ZAI_KEY)));
    write_json(&builtin, &builtin_config());
    let missing_v2 = directory.path().join("absent-v2.json");
    let data_root = directory.path().join("data");

    let mut command = Command::new("zcode-fixture");
    apply_provider_environment_for(&mut command, Some(&builtin), &data_root, &cli, &missing_v2)
        .unwrap();
    assert!(env_of(&command, PERSONAL_PROVIDER_CONFIG_ENV).is_some());
    assert!(env_of(&command, DATA_BASE_DIR_ENV).is_some());

    let generated: Value = serde_json::from_slice(
        &fs::read(data_root.join("zcode-runtime/personal-provider-config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(generated["config"]["providerOrder"], json!(["zai"]));
}

/// A structurally malformed v2 file must refuse generation so the whole group
/// degrades instead of masking the real registry with a defaulted layer.
fn assert_malformed_v2_degrades(v2: &Value) {
    let directory = tempfile::tempdir().unwrap();
    let cli = directory.path().join("cli.json");
    let v2_path = directory.path().join("v2.json");
    let builtin = directory.path().join("builtin.json");
    let output = directory.path().join("out");
    write_json(&cli, &cli_config(Some(ZAI_KEY)));
    write_json(&builtin, &builtin_config());
    write_json(&v2_path, v2);

    let error = generate_personal_provider_config(&cli, &v2_path, &builtin, &output).unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("v2.json"),
        "diagnostic must name the malformed path: {message}"
    );
    assert!(!message.contains(ZAI_KEY));

    let data_root = directory.path().join("data");
    let mut command = Command::new("zcode-fixture");
    let error =
        apply_provider_environment_for(&mut command, Some(&builtin), &data_root, &cli, &v2_path)
            .unwrap_err();
    assert!(!error.to_string().contains(ZAI_KEY));
    assert_eq!(command.get_envs().count(), 0);
    assert!(!data_root.join("zcode-runtime").exists());
}

// A1: providerRules present but not an array is refused.
#[test]
fn a_non_array_provider_rules_degrades_the_environment_group() {
    assert_malformed_v2_degrades(&json!({
        "config": {"providerConfigRules": {"providerRules": "invalid"}}
    }));
}

// A2: modelConfigRules present but not an object is refused (its disable rules
// must never be silently dropped).
#[test]
fn a_non_object_model_config_rules_degrades_the_environment_group() {
    assert_malformed_v2_degrades(&json!({"config": {"modelConfigRules": "invalid"}}));
}

// A3: a providerOrder containing a non-string entry is refused.
#[test]
fn a_non_string_provider_order_entry_degrades_the_environment_group() {
    assert_malformed_v2_degrades(&json!({
        "config": {"providerOrder": ["deepseek", 7]}
    }));
}

// B1: a user-defined zai rule whose providerOrder omits "zai" still gets "zai"
// appended, and the group is injected.
#[test]
fn a_user_defined_zai_rule_missing_from_provider_order_is_appended() {
    let directory = tempfile::tempdir().unwrap();
    let cli = directory.path().join("cli.json");
    let v2 = directory.path().join("v2.json");
    let builtin = directory.path().join("builtin.json");
    write_json(&cli, &cli_config(Some(ZAI_KEY)));
    let user_zai = json!({
        "providerId": "zai",
        "templateId": "zai-api",
        "providerName": "User Zai",
        "config": {"access": {"type": "zhipu-coding-plan-api-key", "apiKey": "user-owned-key"}}
    });
    write_json(
        &v2,
        &v2_config(
            json!([user_zai, deepseek_rule()]),
            json!(["deepseek"]),
            json!({}),
        ),
    );
    write_json(&builtin, &builtin_config());
    let data_root = directory.path().join("data");

    let mut command = Command::new("zcode-fixture");
    apply_provider_environment_for(&mut command, Some(&builtin), &data_root, &cli, &v2).unwrap();
    assert!(env_of(&command, PERSONAL_PROVIDER_CONFIG_ENV).is_some());
    assert!(env_of(&command, DATA_BASE_DIR_ENV).is_some());

    let generated: Value = serde_json::from_slice(
        &fs::read(data_root.join("zcode-runtime/personal-provider-config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        generated["config"]["providerOrder"],
        json!(["deepseek", "zai"])
    );
    let rules = generated["config"]["providerConfigRules"]["providerRules"]
        .as_array()
        .unwrap();
    assert_eq!(rules.len(), 2);
    let zai = rules
        .iter()
        .find(|rule| rule["providerId"] == json!("zai"))
        .unwrap();
    assert_eq!(zai["providerName"], json!("User Zai"));
    assert_eq!(zai["config"]["access"]["apiKey"], json!("user-owned-key"));
}
